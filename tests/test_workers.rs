//! Worker count and low priority for convert and report.

mod common;

use common::*;
use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;

use vgm2fur::convert::{
    convert_folder, report_files, ConvertOptions, ConvertRow, RealDriver, ReportOptions,
};
use vgm2fur::furnace::{
    _command_spec, binary_name, candidates, render_wav_with, restore_console, Capture,
    CommandSpec, FurnaceRunner, CONSOLE_LINE, CREATE_NO_WINDOW,
};
use vgm2fur::priority::{
    creationflags, current_priority, default_workers, resolve_workers, resolve_workers_for,
    set_low_priority, set_priority, visible_cpus, worker_cap, worker_limit, LOW_PRIORITY_CLASS,
    WORKER_CEILING,
};

const _PRIORITY_CLASSES: [u32; 6] = [0x20, 0x40, 0x80, 0x100, 0x4000, 0x8000];

/// Priority is process-global, so tests that change or observe it run alone.
static PRIORITY_LOCK: Mutex<()> = Mutex::new(());

/// Child side of `test_python_worker_drops_itself`: the Rust library's
/// `set_low_priority` in a fresh process. Ignored so only the parent runs it.
#[test]
#[ignore = "helper for test_python_worker_drops_itself"]
fn __priority_child_probe() {
    if std::env::var_os("VGM2FUR_WORKERS_CHILD").is_none() {
        return;
    }
    assert!(set_low_priority());
    println!("{}", current_priority().unwrap_or(0));
}

mod TestWorkerCount {
    use super::*;

    #[test]
    fn test_default_follows_the_thread_rule() {
        // One thread stays free below six, two at six or more, ceiling 16.
        assert_eq!(default_workers(1), 1);
        assert_eq!(default_workers(2), 1);
        assert_eq!(default_workers(5), 4);
        assert_eq!(default_workers(6), 4);
        assert_eq!(default_workers(12), 10); // 6 core/12 thread desktop
        assert_eq!(default_workers(16), 14);
        assert_eq!(default_workers(22), 16); // P+E cores; ceiling applies
        assert_eq!(default_workers(64), 16);
        assert_eq!(worker_limit(), default_workers(visible_cpus()));
    }

    #[test]
    fn test_worker_cap_for_explicit_requests() {
        // Every thread below eight, one free from eight up, safety cap at 31.
        assert_eq!(WORKER_CEILING, 31);
        assert_eq!(worker_cap(1), 1);
        assert_eq!(worker_cap(2), 2);
        assert_eq!(worker_cap(7), 7);
        assert_eq!(worker_cap(8), 7);
        assert_eq!(worker_cap(12), 11);
        assert_eq!(worker_cap(32), 31);
        assert_eq!(worker_cap(64), 31);
        let cpus = visible_cpus();
        assert_eq!(
            worker_cap(cpus),
            if cpus >= 8 {
                std::cmp::max(1, std::cmp::min(cpus - 1, 31))
            } else {
                cpus
            }
        );
    }

    #[test]
    fn test_resolve_workers_honors_a_request_up_to_the_cap() {
        let limit = worker_limit();
        let cap = worker_cap(visible_cpus());
        let (count, note) = resolve_workers(None, 100);
        assert_eq!(count, limit);
        assert!(note.contains("low priority"));
        assert!(note.contains("threads"));
        assert_eq!(resolve_workers(Some(1), 100).0, 1);
        let (count, _note) = resolve_workers(Some(1_000_000), 3);
        assert_eq!(count, 3); // never more workers than jobs
        if cap > 1 {
            let (count, _note) = resolve_workers(Some(cap - 1), cap + 5);
            assert_eq!(count, cap - 1); // within the cap: as given
        }
        let (count, note) = resolve_workers(Some(cap + 5), cap + 20);
        assert_eq!(count, cap); // beyond it: clamped, with a reason
        assert!(note.contains("capped"));
    }

    #[test]
    fn test_safety_cap_note_on_a_many_thread_cpu() {
        // Python patches visible_cpus to 64; here the thread count is an
        // explicit argument.
        let (count, note) = resolve_workers_for(64, Some(64), 100);
        assert_eq!(worker_cap(64), 31);
        assert_eq!(count, 31);
        assert!(note.contains("safety cap"));
        assert!(note.contains("slower"));
    }
}

mod TestLowPriority {
    use super::*;

    #[test]
    fn test_set_and_restore() {
        if !cfg!(windows) {
            eprintln!("skipping: Windows priority classes");
            return;
        }
        let _guard = PRIORITY_LOCK.lock().unwrap();
        let before = current_priority();
        assert!(_PRIORITY_CLASSES.contains(&before.expect("Windows priority")));
        struct Restore(Option<u32>);
        impl Drop for Restore {
            fn drop(&mut self) {
                if let Some(previous) = self.0 {
                    set_priority(previous);
                }
            }
        }
        {
            let _restore = Restore(before);
            assert!(set_low_priority());
            assert_eq!(current_priority(), Some(LOW_PRIORITY_CLASS));
        }
        assert_eq!(current_priority(), before);
    }

    #[test]
    fn test_spawned_process_starts_low() {
        if !cfg!(windows) {
            eprintln!("skipping: Windows priority classes");
            return;
        }
        let flags = creationflags();
        assert_eq!(flags, LOW_PRIORITY_CLASS);
        let mut cmd = Command::new("powershell");
        cmd.args([
            "-NoProfile",
            "-Command",
            "[int][System.Diagnostics.Process]::GetCurrentProcess().PriorityClass",
        ]);
        {
            use std::os::windows::process::CommandExt;
            cmd.creation_flags(flags);
        }
        let proc = cmd.output().expect("spawn powershell");
        assert!(
            proc.status.success(),
            "{}",
            String::from_utf8_lossy(&proc.stderr)
        );
        assert_eq!(String::from_utf8_lossy(&proc.stdout).trim(), "64");
    }

    #[test]
    fn test_python_worker_drops_itself() {
        if !cfg!(windows) {
            eprintln!("skipping: Windows priority classes");
            return;
        }
        let _guard = PRIORITY_LOCK.lock().unwrap();
        let before = current_priority();
        let proc = Command::new(std::env::current_exe().unwrap())
            .args([
                "--ignored",
                "--exact",
                "__priority_child_probe",
                "--nocapture",
            ])
            .env("VGM2FUR_WORKERS_CHILD", "1")
            .output()
            .expect("spawn probe");
        assert!(
            proc.status.success(),
            "{}",
            String::from_utf8_lossy(&proc.stderr)
        );
        let stdout = String::from_utf8_lossy(&proc.stdout);
        assert!(
            stdout.lines().any(|line| line.trim() == "64"),
            "child output: {stdout}"
        );
        assert_eq!(current_priority(), before);
    }
}

mod TestBatch {
    use super::*;
    use indexmap::IndexMap;
    use serde_json::Value;
    use vgm2fur::analyze::AnalyzeOptions;
    use vgm2fur::convert::ConvertDriver;
    use vgm2fur::error::Result;
    use vgm2fur::model::Song;
    use vgm2fur::vgm::VgmFile;

    /// Real loader, but every input analyzes to the standard synthetic note.
    /// Python's empty raw VGM has no FM data; the Rust writer rejects a Song
    /// with neither FM nor PCM, so the seam supplies a renderable song.
    struct SongDriver;

    impl ConvertDriver for SongDriver {
        fn load_vgm(&self, path: &Path) -> Result<VgmFile> {
            RealDriver.load_vgm(path)
        }

        fn analyze(&self, _vgm: &VgmFile, opts: AnalyzeOptions<'_>) -> Result<Song> {
            RealDriver.analyze(&note_song(), opts)
        }

        fn apply_loop(&self, song: &mut Song) -> Option<String> {
            RealDriver.apply_loop(song)
        }

        fn verify_runs(
            &self,
            src_vgm: &VgmFile,
            fur_path: &Path,
            channels: Option<i32>,
            threshold_st: Option<f64>,
            min_run_ms: Option<f64>,
        ) -> Result<IndexMap<String, Value>> {
            RealDriver.verify_runs(src_vgm, fur_path, channels, threshold_st, min_run_ms)
        }
    }

    fn _two_files(root: &Path) -> Vec<PathBuf> {
        let mut files = Vec::new();
        for name in ["A Song.vgz", "B Song.vgz"] {
            let path = root.join(name);
            std::fs::write(&path, raw_vgm(&[], 3_579_545)).unwrap();
            files.push(path);
        }
        files
    }

    #[test]
    fn test_one_worker_keeps_order_and_restores_priority() {
        let _guard = PRIORITY_LOCK.lock().unwrap();
        let before = current_priority();
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let files = _two_files(root);
        let mut notes: Vec<String> = Vec::new();
        let mut rows: Vec<ConvertRow> = Vec::new();
        let opts = ConvertOptions {
            workers: Some(1),
            ..Default::default()
        };
        convert_folder(
            &files,
            &root.join("out"),
            &opts,
            &SongDriver,
            &mut |line| notes.push(line.to_string()),
            &mut |row| rows.push(row),
        )
        .unwrap();
        assert_eq!(notes, Vec::<String>::new());
        assert_eq!(
            rows.iter().map(|row| row.src.clone()).collect::<Vec<_>>(),
            files
        );
        assert!(rows.iter().all(|row| row.error.is_none()));
        assert_eq!(current_priority(), before);
    }

    #[test]
    fn test_two_workers_log_the_cap() {
        let _guard = PRIORITY_LOCK.lock().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let files = _two_files(root);
        let mut notes: Vec<String> = Vec::new();
        let mut rows: Vec<ConvertRow> = Vec::new();
        let opts = ConvertOptions {
            workers: Some(2),
            ..Default::default()
        };
        convert_folder(
            &files,
            &root.join("out"),
            &opts,
            &SongDriver,
            &mut |line| notes.push(line.to_string()),
            &mut |row| rows.push(row),
        )
        .unwrap();
        let reports = report_files(
            &files,
            &ReportOptions {
                workers: Some(2),
                ..Default::default()
            },
            &mut |line| notes.push(line.to_string()),
        );
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().all(|row| row.error.is_none()));
        assert_eq!(
            reports
                .iter()
                .map(|(src, _text, _err)| src.clone())
                .collect::<Vec<_>>(),
            files
        );
        assert!(reports.iter().all(|(_src, text, err)| {
            err.is_none()
                && text
                    .as_deref()
                    .is_some_and(|text| text.contains("converter"))
        }));
        if worker_limit() >= 2 {
            assert!(
                notes
                    .iter()
                    .any(|line| line.starts_with("workers:") && line.contains("low priority"))
            );
        } else {
            assert_eq!(notes, Vec::<String>::new());
        }
    }
}

mod TestFurnaceConsole {
    use super::*;

    #[test]
    fn test_candidates_use_the_platform_binary_name() {
        assert_eq!(
            binary_name(),
            if cfg!(windows) { "furnace.exe" } else { "furnace" }
        );
        let had_env = std::env::var_os("VGM2151FUR_FURNACE");
        // SAFETY: this test file does not read the variable concurrently; the
        // render test below passes its executable explicitly.
        unsafe { std::env::remove_var("VGM2151FUR_FURNACE") };
        let found = candidates();
        if let Some(value) = had_env {
            // SAFETY: same as the removal above.
            unsafe { std::env::set_var("VGM2151FUR_FURNACE", value) };
        }
        assert!(!found.is_empty());
        assert!(found.iter().all(|path| path
            .file_name()
            .is_some_and(|name| name == std::ffi::OsStr::new(binary_name()))));
    }

    struct FakeRunner {
        seen: RefCell<Option<CommandSpec>>,
    }

    impl FurnaceRunner for FakeRunner {
        fn run(&self, exe: &Path, args: &[String], _timeout_secs: u64) -> vgm2fur::error::Result<Capture> {
            let spec = _command_spec(exe, args);
            let out = &args[args.len() - 2];
            std::fs::write(out, b"RIFF").unwrap();
            *self.seen.borrow_mut() = Some(spec);
            Ok(Capture {
                code: 0,
                text: String::new(),
            })
        }
    }

    #[test]
    fn test_render_does_not_take_the_console() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let fur = root.join("Song.fur");
        std::fs::write(&fur, b"fur").unwrap();
        let runner = FakeRunner {
            seen: RefCell::new(None),
        };
        render_wav_with(
            &runner,
            &root.join("furnace.exe"),
            &fur,
            &root.join("Song.wav"),
            1,
            None,
        )
        .unwrap();
        let seen = runner.seen.borrow();
        let spec = seen.as_ref().unwrap();
        assert!(spec.stdin_null); // subprocess.DEVNULL
        if cfg!(windows) {
            assert_ne!(spec.creation_flags & CREATE_NO_WINDOW, 0);
            assert_ne!(spec.creation_flags & LOW_PRIORITY_CLASS, 0);
        }
        assert!(spec.args.iter().any(|arg| arg == "-console"));
    }

    #[test]
    fn test_restore_console_puts_line_input_back() {
        if !cfg!(windows) {
            eprintln!("skipping: Windows console");
            return;
        }
        #[cfg(windows)]
        {
            use windows_sys::Win32::System::Console::{
                GetConsoleMode, GetStdHandle, SetConsoleMode, STD_INPUT_HANDLE,
            };
            // SAFETY: console handles and mode words are plain OS values; a
            // missing console makes these calls fail and the test skip.
            unsafe {
                let handle = GetStdHandle(STD_INPUT_HANDLE);
                let mut original: u32 = 0;
                if handle.is_null() || GetConsoleMode(handle, &mut original) == 0 {
                    eprintln!("skipping: no console input");
                    return;
                }
                let _ = SetConsoleMode(handle, original & !CONSOLE_LINE);
                let mut cleared: u32 = 0;
                assert_ne!(GetConsoleMode(handle, &mut cleared), 0);
                assert_eq!(cleared & CONSOLE_LINE, 0);
                restore_console();
                let mut restored: u32 = 0;
                assert_ne!(GetConsoleMode(handle, &mut restored), 0);
                assert_eq!(restored & CONSOLE_LINE, CONSOLE_LINE);
                let _ = SetConsoleMode(handle, original);
            }
        }
    }
}
