//! Menu, pack browsing, and command line wiring, against synthetic files.
//!
//! Port of `tests/test_menu.py`. `TestPacks` lives in `tests/test_packs.rs`
//! (ported earlier); every other class is here.
//!
//! Python patches `menu.convert_folder` / `menu._convert_now`; here a
//! `MenuDriver` fake records the calls (the plan's test seam). The CLI tests
//! drive `cli::run` with `cli::RealDriver`, and the Ctrl+C case uses the
//! `CliDriver::interrupt` hook exactly as the plan prescribes.

mod common;

use std::cell::RefCell;
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde_json::{json, Value};
use vgm2fur::cli::{self, CliDriver};
use vgm2fur::convert::{ConvertInfo, ConvertOptions, ConvertRow, ReportOptions};
use vgm2fur::edit::Chip;
use vgm2fur::error::Error;
use vgm2fur::menu::{self, MenuDriver, ReadResult};
use vgm2fur::paths::clean_text;

/// `Reader`: pops scripted answers, records prompts.
struct Reader {
    answers: RefCell<VecDeque<String>>,
    prompts: RefCell<Vec<String>>,
}

impl Reader {
    fn new(answers: &[&str]) -> Self {
        Reader {
            answers: RefCell::new(answers.iter().map(|s| s.to_string()).collect()),
            prompts: RefCell::new(Vec::new()),
        }
    }

    fn read(&self) -> ReadResult {
        self.prompts.borrow_mut().push(String::new());
        match self.answers.borrow_mut().pop_front() {
            Some(answer) => {
                if answer == "INT" {
                    return ReadResult::Interrupt;
                }
                ReadResult::Line(answer)
            }
            None => ReadResult::Eof,
        }
    }

    fn texts(&self) -> Vec<String> {
        self.prompts.borrow().clone()
    }
}

fn tmp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("vgm2fur_menu_{tag}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir
}

fn music_vgm() -> Vec<u8> {
    let mut cmds: Vec<u8> = Vec::new();
    for (reg, val) in common::PATCH {
        cmds.extend_from_slice(&[0x54, reg as u8, val as u8]);
    }
    for _ in 0..24 {
        cmds.extend_from_slice(&[0x54, 0x08, 0x78]);
        cmds.push(0x61);
        cmds.extend_from_slice(&5000u16.to_le_bytes());
    }
    common::raw_vgm(&cmds, 3_579_545)
}

fn make_pack(root: &Path, tracks: usize) -> PathBuf {
    let pack = root.join("Test Pack (Arcade)");
    std::fs::create_dir_all(&pack).expect("pack dir");
    let names: Vec<String> = (1..=tracks).map(|i| format!("0{i} Song {i}.vgz")).collect();
    for name in &names {
        std::fs::write(pack.join(name), music_vgm()).expect("track");
    }
    let mut playlist: Vec<String> = names.clone();
    playlist.reverse();
    std::fs::write(
        pack.join("Test Pack.m3u"),
        playlist.join("\n") + "\n",
    )
    .expect("playlist");
    pack
}

/// `_run_menu`: scripted answers, captured output, the given state path.
fn run_menu(answers: &[&str], log: &mut Vec<u8>, state: &Path) -> Result<i32, Error> {
    let mut reader = Reader::new(answers);
    menu::run_menu(
        &mut || reader.read(),
        log,
        state,
        &menu::RealDriver,
    )
}

/// `cli.main([...])`: the reference maps KeyboardInterrupt to 130, `_Usage` to
/// its code, and OSError/ValueError to `ERROR: …` with status 1.
fn cli_main(args: &[&str]) -> i32 {
    cli_main_with(args, &cli::RealDriver)
}

fn cli_main_with(args: &[&str], driver: &dyn CliDriver) -> i32 {
    let mut out: Vec<u8> = Vec::new();
    let mut err: Vec<u8> = Vec::new();
    let argv: Vec<String> = args.iter().map(|s| s.to_string()).collect();
    match cli::run(argv, &mut out, &mut err, driver) {
        Ok(code) => code,
        Err(Error::Interrupt) => {
            eprintln!("cancelled");
            130
        }
        Err(Error::Exit(code)) => code,
        Err(other) => {
            eprintln!("ERROR: {other}");
            1
        }
    }
}

// ---------------------------------------------------------------------------
// Drivers that record what the menu asks them to do.
// ---------------------------------------------------------------------------

#[derive(Default)]
struct RecordingMenuDriver {
    folders: RefCell<Vec<ConvertOptions>>,
    nows: RefCell<Vec<(Option<i32>, bool, bool)>>,
    reports: RefCell<usize>,
    rendered: RefCell<usize>,
}

impl MenuDriver for RecordingMenuDriver {
    fn convert_folder(
        &self,
        _files: &[PathBuf],
        _dest: &Path,
        opts: &ConvertOptions,
        _log: &mut dyn FnMut(&str),
        _on_row: &mut dyn FnMut(ConvertRow),
    ) -> Result<(), Error> {
        self.folders.borrow_mut().push(opts.clone());
        Ok(())
    }

    fn convert_now(
        &self,
        _files: &[PathBuf],
        _dest: &Path,
        speed: Option<i32>,
        lfo_loop: bool,
        lossy_samples: bool,
        _log: &mut dyn FnMut(&str),
        _on_row: &mut dyn FnMut(ConvertRow),
    ) -> Result<(), Error> {
        self.nows.borrow_mut().push((speed, lfo_loop, lossy_samples));
        Ok(())
    }

    fn report_files(
        &self,
        _files: &[PathBuf],
        _log: &mut dyn FnMut(&str),
    ) -> Vec<(PathBuf, Option<String>, Option<String>)> {
        *self.reports.borrow_mut() += 1;
        Vec::new()
    }

    fn render_wav(&self, _fur: &Path, _out: &Path, _loops: i64) -> Result<(), Error> {
        *self.rendered.borrow_mut() += 1;
        Ok(())
    }
}

/// Yields one row, then a log line: the menu must print the row first.
struct StreamingMenuDriver;

impl MenuDriver for StreamingMenuDriver {
    fn convert_folder(
        &self,
        _files: &[PathBuf],
        _dest: &Path,
        _opts: &ConvertOptions,
        log: &mut dyn FnMut(&str),
        on_row: &mut dyn FnMut(ConvertRow),
    ) -> Result<(), Error> {
        on_row(ConvertRow {
            src: PathBuf::from("01 Song.vgm"),
            info: Some(ConvertInfo {
                dst: "01 Song.fur".to_string(),
                bytes: 1234,
                speed: 6,
                events: 42,
                ..ConvertInfo::default()
            }),
            error: None,
        });
        log("tail marker");
        Ok(())
    }

    fn convert_now(
        &self,
        _files: &[PathBuf],
        _dest: &Path,
        _speed: Option<i32>,
        _lfo_loop: bool,
        _lossy_samples: bool,
        log: &mut dyn FnMut(&str),
        on_row: &mut dyn FnMut(ConvertRow),
    ) -> Result<(), Error> {
        self.convert_folder(&[], Path::new("out"), &ConvertOptions::default(), log, on_row)
    }

    fn report_files(
        &self,
        _files: &[PathBuf],
        _log: &mut dyn FnMut(&str),
    ) -> Vec<(PathBuf, Option<String>, Option<String>)> {
        Vec::new()
    }

    fn render_wav(&self, _fur: &Path, _out: &Path, _loops: i64) -> Result<(), Error> {
        Ok(())
    }
}

mod TestMenuStreaming {
    use super::*;

    #[test]
    fn rows_print_before_the_batch_finishes() {
        let mut out: Vec<u8> = Vec::new();
        let driver = StreamingMenuDriver;
        let files = vec![PathBuf::from("01 Song.vgm")];
        let rc = menu::_convert_now(
            &mut out,
            &driver,
            &files,
            Path::new("out"),
            None,
            true,
            false,
        )
        .unwrap();
        assert_eq!(rc, 0);
        let text = String::from_utf8(out).unwrap();
        let row = text.find("01 Song.vgm -> 01 Song.fur").expect("row line");
        let tail = text.find("tail marker").expect("driver log line");
        assert!(
            row < tail,
            "converted row must print on completion, before later work: {text}"
        );
    }
}

/// Delegates to the real CLI driver but reports an interrupt first.
struct InterruptDriver(cli::RealDriver);

impl CliDriver for InterruptDriver {
    fn convert_folder(
        &self,
        files: &[PathBuf],
        dest: &Path,
        opts: &ConvertOptions,
        log: &mut dyn FnMut(&str),
        on_row: &mut dyn FnMut(ConvertRow),
    ) -> Result<(), Error> {
        self.0.convert_folder(files, dest, opts, log, on_row)
    }
    fn report_files(
        &self,
        files: &[PathBuf],
        opts: &ReportOptions,
        log: &mut dyn FnMut(&str),
    ) -> Vec<(PathBuf, Option<String>, Option<String>)> {
        self.0.report_files(files, opts, log)
    }
    fn render_wav(&self, fur: &Path, out: &Path, loops: i64) -> Result<PathBuf, Error> {
        self.0.render_wav(fur, out, loops)
    }
    fn read_chips(&self, fur: &Path) -> Result<Vec<Chip>, Error> {
        self.0.read_chips(fur)
    }
    fn set_chip_volume(
        &self,
        fur: &Path,
        factor: Option<f64>,
        value: Option<f64>,
        targets: Option<&[usize]>,
    ) -> Result<Vec<Chip>, Error> {
        self.0.set_chip_volume(fur, factor, value, targets)
    }
    fn interrupt(&self) -> bool {
        true
    }
}

// ---------------------------------------------------------------------------
// CLI
// ---------------------------------------------------------------------------

mod TestCli {
    use super::*;

    #[test]
    fn test_no_lfo_loop_flag_parses() {
        let argv: Vec<String> = ["convert", "Track.vgm", "--no-lfo-loop"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert!(cli::parse_for_test(&argv).unwrap().no_lfo_loop);
        let plain: Vec<String> =
            ["convert", "Track.vgm"].iter().map(|s| s.to_string()).collect();
        assert!(!cli::parse_for_test(&plain).unwrap().no_lfo_loop);
    }

    #[test]
    fn test_convert_writes_a_fur() {
        let dir = tmp_dir("cli_convert");
        let src = dir.join("Song.vgz");
        std::fs::write(&src, music_vgm()).expect("write");
        let out = dir.join("fur");
        let rc = cli_main(&["convert", &src.to_string_lossy(), "-o", &out.to_string_lossy()]);
        assert_eq!(rc, 0);
        assert!(out.join("Song.fur").is_file());
    }

    #[test]
    fn test_edit_factor_and_list() {
        let dir = tmp_dir("cli_edit");
        let src = dir.join("Song.vgm");
        std::fs::write(&src, music_vgm()).expect("write");
        assert_eq!(
            cli_main(&["convert", &src.to_string_lossy(), "-o", &dir.to_string_lossy()]),
            0
        );
        let fur = dir.join("Song.fur");
        let rc = cli_main(&[
            "edit",
            &fur.to_string_lossy(),
            "--chip",
            "YM2151",
            "--factor",
            "0.5",
        ]);
        assert_eq!(rc, 0);
        let chips = vgm2fur::edit::read_chips_file(&fur).expect("read chips");
        assert!((chips[0].volume - 0.5).abs() < 1e-3);
    }

    #[test]
    fn test_edit_needs_a_change() {
        let dir = tmp_dir("cli_edit_noop");
        let src = dir.join("Song.vgm");
        std::fs::write(&src, music_vgm()).expect("write");
        assert_eq!(
            cli_main(&["convert", &src.to_string_lossy(), "-o", &dir.to_string_lossy()]),
            0
        );
        assert_eq!(
            cli_main(&["edit", &dir.join("Song.fur").to_string_lossy()]),
            2
        );
    }

    #[test]
    fn test_missing_input_reports_cleanly() {
        assert_eq!(cli_main(&["report", "no-such-file.vgm"]), 1);
    }

    #[test]
    fn test_bare_and_quoted_paths_convert() {
        let dir = tmp_dir("cli_quoted");
        let src = dir.join("My Song.vgz");
        std::fs::write(&src, music_vgm()).expect("write");
        let quoted = format!("\"{}\"", src.display());
        let rc = cli_main(&[&quoted, "--no-normalize", "--workers", "1"]);
        assert_eq!(rc, 0);
        assert!(dir.join("fur").join("My Song.fur").is_file());
    }

    #[test]
    fn test_parent_directory_converts_the_pack() {
        let dir = tmp_dir("cli_pack");
        let pack = make_pack(&dir, 3);
        let rc = cli_main(&[
            "convert",
            &dir.to_string_lossy(),
            "--workers",
            "1",
            "--no-normalize",
        ]);
        assert_eq!(rc, 0);
        assert!(pack.join("fur").join("01 Song 1.fur").is_file());
        assert!(pack.join("fur").join("03 Song 3.fur").is_file());
    }

    #[test]
    fn test_glob_converts() {
        let dir = tmp_dir("cli_glob");
        std::fs::write(dir.join("A.vgz"), music_vgm()).expect("a");
        std::fs::write(dir.join("B.vgz"), music_vgm()).expect("b");
        let out = dir.join("out");
        let pattern = format!("{}/*.vgz", dir.display());
        let rc = cli_main(&[
            "convert",
            &pattern,
            "-o",
            &out.to_string_lossy(),
            "--workers",
            "1",
            "--no-normalize",
        ]);
        assert_eq!(rc, 0);
        assert!(out.join("A.fur").is_file());
        assert!(out.join("B.fur").is_file());
    }

    #[test]
    fn test_edit_finds_fur_child() {
        let dir = tmp_dir("cli_edit_child");
        let src = dir.join("Song.vgz");
        std::fs::write(&src, music_vgm()).expect("write");
        let out = dir.join("fur");
        assert_eq!(
            cli_main(&[
                "convert",
                &src.to_string_lossy(),
                "-o",
                &out.to_string_lossy(),
                "--no-normalize",
            ]),
            0
        );
        let rc = cli_main(&[
            "edit",
            &dir.to_string_lossy(),
            "--chip",
            "YM2151",
            "--factor",
            "0.5",
        ]);
        assert_eq!(rc, 0);
        let chips = vgm2fur::edit::read_chips_file(&out.join("Song.fur")).expect("read");
        assert!((chips[0].volume - 0.5).abs() < 1e-3);
    }

    #[test]
    fn test_bare_fur_lists_chips() {
        let dir = tmp_dir("cli_bare_fur");
        let src = dir.join("Song.vgz");
        std::fs::write(&src, music_vgm()).expect("write");
        assert_eq!(
            cli_main(&[
                "convert",
                &src.to_string_lossy(),
                "-o",
                &dir.to_string_lossy(),
                "--no-normalize",
            ]),
            0
        );
        let fur = dir.join("Song.fur");
        let mut out: Vec<u8> = Vec::new();
        let mut err: Vec<u8> = Vec::new();
        let rc = cli::run(
            vec![fur.to_string_lossy().into_owned()],
            &mut out,
            &mut err,
            &cli::RealDriver,
        )
        .expect("run");
        assert_eq!(rc, 0);
        let text = String::from_utf8_lossy(&out);
        assert!(text.contains("YM2151"), "{text}");
        let chips = vgm2fur::edit::read_chips_file(&fur).expect("read");
        assert!((chips[0].volume - 1.0).abs() < 1e-3);
    }

    #[test]
    fn test_report_on_a_collection_lists_packs() {
        let dir = tmp_dir("cli_report");
        make_pack(&dir, 3);
        let mut out: Vec<u8> = Vec::new();
        let mut err: Vec<u8> = Vec::new();
        let rc = cli::run(
            vec!["report".to_string(), dir.to_string_lossy().into_owned()],
            &mut out,
            &mut err,
            &cli::RealDriver,
        )
        .expect("run");
        assert_eq!(rc, 0);
        let text = String::from_utf8_lossy(&out);
        assert!(text.contains("Test Pack"), "{text}");
        assert!(text.contains("pass a pack folder"), "{text}");
    }
}

mod TestPaths {
    use super::*;

    #[test]
    fn test_clean_text_strips_paste_wrapping() {
        assert_eq!(clean_text("\"C:\\Music\\Song.vgz\""), "C:\\Music\\Song.vgz");
        assert_eq!(clean_text("& 'C:\\Music\\Song.vgz'"), "C:\\Music\\Song.vgz");
        assert_eq!(
            clean_text("file:///C:/Music/My%20Song.vgz"),
            "C:/Music/My Song.vgz"
        );
        let home = std::env::var_os("USERPROFILE")
            .or_else(|| std::env::var_os("HOME"))
            .map(PathBuf::from);
        if let Some(home) = home {
            assert_eq!(PathBuf::from(clean_text("~")), home);
        }
    }

    #[test]
    fn test_iter_vgms_skips_a_fur_file() {
        let dir = tmp_dir("paths_iter");
        let fur = dir.join("Song.fur");
        std::fs::write(&fur, b"not a vgm").expect("write");
        assert!(vgm2fur::convert::iter_vgms(&fur).is_empty());
    }

    #[test]
    fn test_parse_targets_all() {
        let chip = Chip {
            index: 0,
            id: 0x82,
            volume: 1.0,
            pan: 0.0,
            pan_fr: 0.0,
        };
        assert_eq!(menu::_parse_targets("all", &[chip]).unwrap(), vec![0]);
    }
}

// ---------------------------------------------------------------------------
// Menu flows
// ---------------------------------------------------------------------------

mod TestMenu {
    use super::*;

    #[test]
    fn test_pack_convert_flow() {
        let dir = tmp_dir("menu_pack");
        let pack = make_pack(&dir, 3);
        let mut log: Vec<u8> = Vec::new();
        let rc = run_menu(
            &["1", &dir.to_string_lossy(), "1", "c", "all", "", "", "", "n", "b", "", "q"],
            &mut log,
            &dir.join("state.json"),
        )
        .expect("menu");
        assert_eq!(rc, 0);
        assert!(pack.join("fur").join("default").join("01 Song 1.fur").is_file());
        assert!(pack.join("fur").join("default").join("03 Song 3.fur").is_file());
        let text = String::from_utf8_lossy(&log);
        assert!(text.contains("3/3 converted"), "{text}");
        let state: Value =
            serde_json::from_str(&std::fs::read_to_string(dir.join("state.json")).unwrap())
                .expect("state");
        assert_eq!(state["browse_root"], json!(dir.to_string_lossy().into_owned()));
        assert_eq!(state["pack"], json!(pack.to_string_lossy().into_owned()));
    }

    #[test]
    fn test_optimise_prompt_is_yes_no_and_defaults_to_all() {
        let dir = tmp_dir("menu_optimise");
        let pack = make_pack(&dir, 3);
        let mut log: Vec<u8> = Vec::new();
        let rc = run_menu(
            &["1", &dir.to_string_lossy(), "1", "c", "1", "", "", "", "n", "b", "", "q"],
            &mut log,
            &dir.join("state.json"),
        )
        .expect("menu");
        assert_eq!(rc, 0);
        let x2 = pack.join("fur").join("x2 optimised");
        let hits = std::fs::read_dir(&x2)
            .map(|entries| {
                entries
                    .filter_map(|e| e.ok())
                    .filter(|e| e.file_name().to_string_lossy().starts_with("03 Song 3 x2"))
                    .count()
            })
            .unwrap_or(0);
        assert_eq!(hits, 1);
    }

    #[test]
    fn test_optimise_no_writes_only_the_1x_file() {
        let dir = tmp_dir("menu_optimise_no");
        let pack = make_pack(&dir, 3);
        let mut log: Vec<u8> = Vec::new();
        run_menu(
            &["1", &dir.to_string_lossy(), "1", "c", "1", "", "n", "", "b", "", "q"],
            &mut log,
            &dir.join("state.json"),
        )
        .expect("menu");
        assert!(pack.join("fur").join("03 Song 3.fur").is_file());
        assert!(!pack.join("fur").join("x2 optimised").exists());
    }

    #[test]
    fn test_pack_convert_a_range() {
        let dir = tmp_dir("menu_range");
        let pack = make_pack(&dir, 3);
        let mut log: Vec<u8> = Vec::new();
        run_menu(
            &["1", &dir.to_string_lossy(), "1", "c", "2-3", "", "", "", "n", "b", "", "q"],
            &mut log,
            &dir.join("state.json"),
        )
        .expect("menu");
        assert!(!pack.join("fur").join("default").join("03 Song 3.fur").exists());
        assert!(pack.join("fur").join("default").join("02 Song 2.fur").is_file());
        assert!(pack.join("fur").join("default").join("01 Song 1.fur").is_file());
    }

    #[test]
    fn test_pack_volume_flow() {
        let dir = tmp_dir("menu_volume");
        let pack = make_pack(&dir, 3);
        cli_main(&[
            "convert",
            &pack.join("01 Song 1.vgz").to_string_lossy(),
            "-o",
            &pack.join("fur").to_string_lossy(),
        ]);
        let mut log: Vec<u8> = Vec::new();
        let rc = run_menu(
            &["1", &dir.to_string_lossy(), "1", "e", "3", "1", "0.5", "", "b", "", "q"],
            &mut log,
            &dir.join("state.json"),
        )
        .expect("menu");
        assert_eq!(rc, 0);
        let chips =
            vgm2fur::edit::read_chips_file(&pack.join("fur").join("01 Song 1.fur")).expect("read");
        assert!((chips[0].volume - 0.5).abs() < 1e-3);
        assert!(String::from_utf8_lossy(&log).contains("YM2151 0.500"));
    }

    #[test]
    fn test_direct_edit_flow() {
        let dir = tmp_dir("menu_direct_edit");
        let src = dir.join("Song.vgm");
        std::fs::write(&src, music_vgm()).expect("write");
        cli_main(&["convert", &src.to_string_lossy(), "-o", &dir.to_string_lossy()]);
        let fur = dir.join("Song.fur");
        let quoted = format!("\"{}\"", fur.display());
        let mut log: Vec<u8> = Vec::new();
        run_menu(
            &["3", &quoted, "1", "0.8", "", "q"],
            &mut log,
            &dir.join("state.json"),
        )
        .expect("menu");
        let chips = vgm2fur::edit::read_chips_file(&fur).expect("read");
        assert!((chips[0].volume - 0.8).abs() < 1e-3);
    }

    #[test]
    fn test_unknown_command_then_quit() {
        let dir = tmp_dir("menu_unknown");
        let mut log: Vec<u8> = Vec::new();
        run_menu(&["nope", "q"], &mut log, &dir.join("state.json")).expect("menu");
        assert!(String::from_utf8_lossy(&log).contains("unknown command"));
    }

    #[test]
    fn test_paste_quoted_file_converts() {
        let dir = tmp_dir("menu_paste_file");
        let src = dir.join("My Song.vgz");
        std::fs::write(&src, music_vgm()).expect("write");
        let quoted = format!("\"{}\"", src.display());
        let mut log: Vec<u8> = Vec::new();
        run_menu(&[&quoted, "q"], &mut log, &dir.join("state.json")).expect("menu");
        assert!(dir.join("fur").join("default").join("My Song.fur").is_file());
        assert!(String::from_utf8_lossy(&log).contains("1/1 converted"));
    }

    #[test]
    fn test_paste_pack_folder_opens_it() {
        let dir = tmp_dir("menu_paste_pack");
        let pack = make_pack(&dir, 3);
        let quoted = format!("\"{}\"", pack.display());
        let mut log: Vec<u8> = Vec::new();
        run_menu(&[&quoted, "b", "q"], &mut log, &dir.join("state.json")).expect("menu");
        let text = String::from_utf8_lossy(&log);
        assert!(text.contains("01 Song 1"), "{text}");
        assert!(text.contains("playlist order"), "{text}");
    }

    #[test]
    fn test_q_inside_a_pack_quits() {
        let dir = tmp_dir("menu_q_pack");
        let pack = make_pack(&dir, 3);
        let mut log: Vec<u8> = Vec::new();
        let rc = run_menu(
            &["1", &dir.to_string_lossy(), "1", "q"],
            &mut log,
            &dir.join("state.json"),
        )
        .expect("menu");
        assert_eq!(rc, 0);
        let text = String::from_utf8_lossy(&log);
        assert!(text.contains(&pack.file_name().unwrap().to_string_lossy().to_string()));
        assert!(!text.contains("c, e, r or b"));
    }

    #[test]
    fn test_quoted_collection_lists_packs() {
        let dir = tmp_dir("menu_collection");
        let pack = make_pack(&dir, 3);
        let quoted = format!("\"{}\"", dir.display());
        let mut log: Vec<u8> = Vec::new();
        run_menu(&["1", &quoted, "1", "b", "", "q"], &mut log, &dir.join("state.json"))
            .expect("menu");
        assert!(String::from_utf8_lossy(&log)
            .contains(&pack.file_name().unwrap().to_string_lossy().to_string()));
    }

    #[test]
    fn test_eof_exits_quietly() {
        let dir = tmp_dir("menu_eof");
        let mut log: Vec<u8> = Vec::new();
        let rc = run_menu(&[], &mut log, &dir.join("state.json")).expect("menu");
        assert_eq!(rc, 0);
    }

    #[test]
    fn test_ctrl_c_at_the_prompt_quits() {
        let dir = tmp_dir("menu_ctrl_c");
        let mut log: Vec<u8> = Vec::new();
        let rc = run_menu(&["nope", "INT"], &mut log, &dir.join("state.json")).expect("menu");
        assert_eq!(rc, 130);
        let text = String::from_utf8_lossy(&log);
        assert!(text.contains("unknown command"), "{text}");
        assert!(text.contains("quit (Ctrl+C)"), "{text}");
    }

    #[test]
    fn test_ctrl_c_in_a_command_returns_to_the_menu() {
        let dir = tmp_dir("menu_ctrl_c_cmd");
        make_pack(&dir, 3);
        let mut log: Vec<u8> = Vec::new();
        let rc = run_menu(
            &["1", &dir.to_string_lossy(), "1", "INT", "q"],
            &mut log,
            &dir.join("state.json"),
        )
        .expect("menu");
        assert_eq!(rc, 0);
        assert!(String::from_utf8_lossy(&log).contains("cancelled"));
    }

    #[test]
    fn test_ctrl_c_in_a_cli_run_exits_130() {
        let code = cli_main_with(&["convert", "whatever.vgm"], &InterruptDriver(cli::RealDriver));
        assert_eq!(code, 130);
    }
}

// ---------------------------------------------------------------------------
// Prompt seams
// ---------------------------------------------------------------------------

mod TestLfoLoopPrompt {
    use super::*;

    #[test]
    fn test_prompt_defaults_to_yes() {
        let seen = Mutex::new(Vec::new());
        let mut read = |_prompt: &str| {
            seen.lock().unwrap().push(String::new());
            String::new()
        };
        assert!(menu::_ask_lfo_loop(&mut read));
    }

    #[test]
    fn test_no_disables_loop_search() {
        assert!(!menu::_ask_lfo_loop(&mut |_p| "n".to_string()));
    }

    #[test]
    fn test_pasted_path_uses_the_saved_setting() {
        let dir = tmp_dir("lfo_state");
        let src = dir.join("a.vgm");
        std::fs::write(&src, music_vgm()).expect("write");
        let driver = RecordingMenuDriver::default();
        let mut log: Vec<u8> = Vec::new();
        let state = json!({"lfo_loop": false});
        menu::_open_typed_path(
            &mut |_p| String::new(),
            &mut log,
            state,
            &dir.join("state.json"),
            &src.to_string_lossy(),
            &driver,
        )
        .expect("open typed path");
        let nows = driver.nows.borrow();
        assert_eq!(nows.len(), 1);
        assert!(!nows[0].1);
    }

    #[test]
    fn test_conversion_carries_the_flag() {
        let driver = RecordingMenuDriver::default();
        let mut log: Vec<u8> = Vec::new();
        menu::_run_convert(
            &mut log,
            &driver,
            &[PathBuf::from("a.vgm")],
            Path::new("out"),
            None,
            "none",
            false,
            false,
            false,
            None,
        )
        .expect("run convert");
        assert!(!driver.folders.borrow()[0].lfo_loop);
    }
}

mod TestLossySamplesPrompt {
    use super::*;

    #[test]
    fn test_prompt_defaults_to_no() {
        assert!(!menu::_ask_lossy_samples(&mut |_p| String::new()));
    }

    #[test]
    fn test_yes_enables_it() {
        assert!(menu::_ask_lossy_samples(&mut |_p| "y".to_string()));
    }

    #[test]
    fn test_pasted_path_uses_the_saved_setting() {
        let dir = tmp_dir("lossy_state");
        let src = dir.join("a.vgm");
        std::fs::write(&src, music_vgm()).expect("write");
        let driver = RecordingMenuDriver::default();
        let mut log: Vec<u8> = Vec::new();
        menu::_open_typed_path(
            &mut |_p| String::new(),
            &mut log,
            json!({"lossy_samples": true}),
            &dir.join("state.json"),
            &src.to_string_lossy(),
            &driver,
        )
        .expect("open typed path");
        let nows = driver.nows.borrow();
        assert!(nows[0].2);
    }

    #[test]
    fn test_conversion_carries_the_flag() {
        let driver = RecordingMenuDriver::default();
        let mut log: Vec<u8> = Vec::new();
        menu::_run_convert(
            &mut log,
            &driver,
            &[PathBuf::from("a.vgm")],
            Path::new("out"),
            None,
            "none",
            false,
            true,
            true,
            None,
        )
        .expect("run convert");
        assert!(driver.folders.borrow()[0].lossy_samples);
    }

    #[test]
    fn test_cli_flag_parses() {
        let argv: Vec<String> = ["convert", "Track.vgm", "--lossy-sample-clustering"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert!(cli::parse_for_test(&argv).unwrap().lossy_sample_clustering);
        let plain: Vec<String> =
            ["convert", "Track.vgm"].iter().map(|s| s.to_string()).collect();
        assert!(!cli::parse_for_test(&plain).unwrap().lossy_sample_clustering);
    }

    #[test]
    fn test_latch_is_not_persisted() {
        let dir = tmp_dir("lossy_persist");
        let path = dir.join("state.json");
        std::fs::write(
            &path,
            serde_json::to_string(&json!({"lossy_samples": true, "browse_root": "x"})).unwrap(),
        )
        .expect("write state");
        let state = menu::_load_state(&path);
        assert!(state.get("lossy_samples").is_none());
        assert_eq!(state["browse_root"], json!("x"));

        menu::_save_state(&path, &json!({"lossy_samples": true, "browse_root": "x"}));
        let saved: Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).expect("saved");
        assert!(saved.get("lossy_samples").is_none());
        assert_eq!(saved["browse_root"], json!("x"));
    }
}

mod TestSpeedPrompt {
    use super::*;

    #[test]
    fn test_optimised_path_keeps_the_speed() {
        let driver = RecordingMenuDriver::default();
        let mut log: Vec<u8> = Vec::new();
        menu::_run_convert(
            &mut log,
            &driver,
            &[PathBuf::from("a.vgm")],
            Path::new("out"),
            Some(8),
            "all",
            false,
            true,
            false,
            None,
        )
        .expect("run convert");
        let folders = driver.folders.borrow();
        assert_eq!(folders[0].speed, Some(8));
        assert!(folders[0].variants);
    }

    #[test]
    fn test_plain_path_keeps_the_speed() {
        let driver = RecordingMenuDriver::default();
        let mut log: Vec<u8> = Vec::new();
        menu::_run_convert(
            &mut log,
            &driver,
            &[PathBuf::from("a.vgm")],
            Path::new("out"),
            Some(8),
            "none",
            false,
            true,
            false,
            None,
        )
        .expect("run convert");
        let folders = driver.folders.borrow();
        assert_eq!(folders[0].speed, Some(8));
        assert!(!folders[0].variants);
    }

    #[test]
    fn test_optimised_path_keeps_the_tick_rate() {
        let driver = RecordingMenuDriver::default();
        let mut log: Vec<u8> = Vec::new();
        menu::_run_convert(
            &mut log,
            &driver,
            &[PathBuf::from("a.vgm")],
            Path::new("out"),
            Some(8),
            "all",
            false,
            true,
            false,
            Some(120.0),
        )
        .expect("run convert");
        let folders = driver.folders.borrow();
        assert_eq!(folders[0].speed, Some(8));
        assert_eq!(folders[0].tick_hz, Some(120.0));
        assert!(folders[0].variants);
    }

    #[test]
    fn test_plain_path_keeps_the_tick_rate() {
        let driver = RecordingMenuDriver::default();
        let mut log: Vec<u8> = Vec::new();
        menu::_run_convert(
            &mut log,
            &driver,
            &[PathBuf::from("a.vgm")],
            Path::new("out"),
            Some(8),
            "none",
            false,
            true,
            false,
            Some(120.0),
        )
        .expect("run convert");
        let folders = driver.folders.borrow();
        assert_eq!(folders[0].speed, Some(8));
        assert_eq!(folders[0].tick_hz, Some(120.0));
        assert!(!folders[0].variants);
    }
}

mod TestMenuFurnaceDefault {
    use super::*;

    #[test]
    fn test_direct_conversion_defaults_to_no_normalize() {
        // The menu's direct path never passes `normalize`/`verify`; it hands
        // the driver only speed, the LFO latch and the lossy latch.
        let driver = RecordingMenuDriver::default();
        let mut log: Vec<u8> = Vec::new();
        menu::_convert_now(
            &mut log,
            &driver,
            &[PathBuf::from("a.vgm")],
            Path::new("out"),
            None,
            true,
            false,
        )
        .expect("convert now");
        assert_eq!(driver.nows.borrow().len(), 1);
    }

    #[test]
    fn test_plain_conversion_defaults_to_no_normalize() {
        let driver = RecordingMenuDriver::default();
        let mut log: Vec<u8> = Vec::new();
        menu::_run_convert(
            &mut log,
            &driver,
            &[PathBuf::from("a.vgm")],
            Path::new("out"),
            None,
            "none",
            false,
            true,
            false,
            None,
        )
        .expect("run convert");
        let folders = driver.folders.borrow();
        assert_eq!(folders.len(), 1);
        assert!(!folders[0].normalize);
        assert!(!folders[0].verify);
    }
}
