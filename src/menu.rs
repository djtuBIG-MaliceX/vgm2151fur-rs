//! Line menu. Numbered commands, and a pasted path does the obvious thing.
//!
//! Not a full-screen interface. [`run_menu`] is the testable entry: every
//! action calls the same functions the command line uses.
//!
//! Ctrl+C cancels a running command and returns to the menu. At the menu
//! prompt it quits.
//!
//! A path typed at the prompt can be quoted (Explorer's Copy as path), a file,
//! a pack folder, or a folder of packs. A file converts. A pack opens. A
//! folder of packs lists them.

use std::cell::RefCell;
use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

use crate::convert::{self, ConvertOptions, ConvertRow, ReportOptions};
use crate::error::{Error, Result};
use crate::packs::{self, Pack};
use crate::paths;

pub const FUR_FOLDER: &str = "fur";
/// A scan from a big collection still returns. The line under the list says so
/// when this many packs came back.
pub const PACK_SCAN_LIMIT: usize = 400;

/// One answer from the menu's reader.
///
/// The reference passes a callable (Python's `input`). A line, end of input,
/// and Ctrl+C are the three outcomes the menu distinguishes.
pub enum ReadResult {
    Line(String),
    Eof,
    Interrupt,
}

/// The operations the menu invokes.
///
/// The reference menu calls `convert_folder`, `_convert_now`, `report_files`
/// and `render_wav` from the other modules; tests replace them by patching
/// those names. Rust gets the same seam from this trait. [`RealDriver`]
/// forwards to the library functions.
pub trait MenuDriver {
    /// `convert_folder(...)` with the given options. Rows arrive in
    /// completion order.
    fn convert_folder(
        &self,
        files: &[PathBuf],
        dest: &Path,
        opts: &ConvertOptions,
        log: &mut dyn FnMut(&str),
        on_row: &mut dyn FnMut(ConvertRow),
    ) -> Result<()>;

    /// `_convert_now`: a direct conversion with the menu defaults
    /// (`optimize="all"`, the 1x..4x ladder, dirs layout, no Furnace).
    fn convert_now(
        &self,
        files: &[PathBuf],
        dest: &Path,
        speed: Option<i32>,
        lfo_loop: bool,
        lossy_samples: bool,
        log: &mut dyn FnMut(&str),
        on_row: &mut dyn FnMut(ConvertRow),
    ) -> Result<()>;

    /// `report_files(...)`: `(src, report, error)` in input order.
    fn report_files(
        &self,
        files: &[PathBuf],
        log: &mut dyn FnMut(&str),
    ) -> Vec<(PathBuf, Option<String>, Option<String>)>;

    /// `render_wav(fur, out, loops=nloops)`.
    fn render_wav(&self, fur: &Path, out: &Path, loops: i64) -> Result<()>;
}

/// Production driver: forwards to the real library functions.
pub struct RealDriver;

impl MenuDriver for RealDriver {
    fn convert_folder(
        &self,
        files: &[PathBuf],
        dest: &Path,
        opts: &ConvertOptions,
        log: &mut dyn FnMut(&str),
        on_row: &mut dyn FnMut(ConvertRow),
    ) -> Result<()> {
        let deps = convert::RealDriver;
        convert::convert_folder(files, dest, opts, &deps, log, on_row)
    }

    fn convert_now(
        &self,
        files: &[PathBuf],
        dest: &Path,
        speed: Option<i32>,
        lfo_loop: bool,
        lossy_samples: bool,
        log: &mut dyn FnMut(&str),
        on_row: &mut dyn FnMut(ConvertRow),
    ) -> Result<()> {
        // `_run_convert` with optimize="all": the 4x variant ladder.
        let opts = ConvertOptions {
            speed,
            variants: true,
            condense: 4.0,
            optimize: "all".to_string(),
            layout: "dirs".to_string(),
            lfo_loop,
            lossy_samples,
            ..ConvertOptions::default()
        };
        self.convert_folder(files, dest, &opts, log, on_row)
    }

    fn report_files(
        &self,
        files: &[PathBuf],
        log: &mut dyn FnMut(&str),
    ) -> Vec<(PathBuf, Option<String>, Option<String>)> {
        convert::report_files(files, &ReportOptions::default(), log)
    }

    fn render_wav(&self, fur: &Path, out: &Path, loops: i64) -> Result<()> {
        crate::furnace::render_wav(fur, out, loops, None).map(|_| ())
    }
}

/// `Path.home() / ".vgm2151fur.json"`.
pub fn default_state_path() -> PathBuf {
    let home = std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
        .unwrap_or_default();
    home.join(".vgm2151fur.json")
}

struct Tone {
    on: bool,
}

impl Tone {
    fn wrap(&self, code: &str, text: &str) -> String {
        if !self.on {
            return text.to_string();
        }
        format!("\x1b[{code}m{text}\x1b[0m")
    }

    fn title(&self, text: &str) -> String {
        self.wrap("1", text)
    }

    fn dim(&self, text: &str) -> String {
        self.wrap("2", text)
    }

    #[allow(dead_code)]
    fn bad(&self, text: &str) -> String {
        self.wrap("31", text)
    }
}

/// True when the console will actually render ANSI color.
fn vt_on() -> bool {
    if !std::io::stdout().is_terminal() {
        return false;
    }
    #[cfg(not(windows))]
    {
        true
    }
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::Console::{GetConsoleMode, GetStdHandle, SetConsoleMode};

        const STD_OUTPUT_HANDLE: u32 = -11i32 as u32;
        const ENABLE_VIRTUAL_TERMINAL_PROCESSING: u32 = 0x0004;
        // SAFETY: the console APIs take the process's standard handle and a
        // pointer to a local `u32`; neither call retains either.
        unsafe {
            let handle = GetStdHandle(STD_OUTPUT_HANDLE);
            if handle.is_null() {
                return false;
            }
            let mut mode: u32 = 0;
            if GetConsoleMode(handle, &mut mode) == 0 {
                return false;
            }
            SetConsoleMode(handle, mode | ENABLE_VIRTUAL_TERMINAL_PROCESSING) != 0
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Nav {
    Quit,
    Menu,
}

struct Menu<'a> {
    read: &'a mut dyn FnMut() -> ReadResult,
    out: &'a mut dyn Write,
    driver: &'a dyn MenuDriver,
    state: Value,
    state_path: PathBuf,
    tone: Tone,
}

/// The interactive line menu.
///
/// Returns the process exit code: `0` on quit or end of input, `130` on
/// Ctrl+C at the prompt. A command interrupted with Ctrl+C prints `cancelled`
/// and returns to the loop.
pub fn run_menu(
    read: &mut dyn FnMut() -> ReadResult,
    out: &mut dyn Write,
    state_path: &Path,
    driver: &dyn MenuDriver,
) -> Result<i32> {
    let tone = Tone { on: vt_on() };
    let mut menu = Menu {
        read,
        out,
        driver,
        state: load_state(state_path),
        state_path: state_path.to_path_buf(),
        tone,
    };
    menu.show_banner();
    loop {
        crate::furnace::restore_console();
        menu.emit("> ");
        let _ = menu.out.flush();
        let raw = match (menu.read)() {
            ReadResult::Line(line) => line.trim().to_string(),
            ReadResult::Eof => return Ok(0),
            ReadResult::Interrupt => {
                menu.emit_line("");
                menu.emit_line("  quit (Ctrl+C)");
                return Ok(130);
            }
        };
        if raw.is_empty() {
            continue;
        }
        let key = raw.to_lowercase();
        if matches!(key.as_str(), "q" | "quit" | "exit") {
            return Ok(0);
        }
        if matches!(key.as_str(), "m" | "menu" | "?" | "help") {
            menu.show_banner();
            continue;
        }
        match menu.dispatch(&raw, &key) {
            Ok(Some(Nav::Quit)) => return Ok(0),
            Ok(Some(Nav::Menu)) => menu.show_banner(),
            Ok(None) => {}
            Err(Error::Interrupt) => {
                menu.emit_line("");
                menu.emit_line("  cancelled");
            }
            Err(e @ (Error::Io { .. } | Error::Msg(_))) => {
                menu.emit_line(&format!("  ERROR: {e}"));
            }
            Err(e) => return Err(e),
        }
    }
}

impl<'a> Menu<'a> {
    fn emit(&mut self, text: &str) {
        let _ = self.out.write_all(text.as_bytes());
    }

    fn emit_line(&mut self, text: &str) {
        self.emit(text);
        self.emit("\n");
    }

    fn banner(&self) -> String {
        let mut lines: Vec<String> = vec![
            self.tone
                .title(&format!("vgm2151fur {}", crate::CONVERTER_VERSION)),
            "  1  packs      scan a folder, then convert, volumes, render".to_string(),
            "  2  convert    a file, a folder, or a glob".to_string(),
            "  3  volumes    a .fur file or a folder of them".to_string(),
            "  4  compare    a .fur against its source VGM".to_string(),
            "  5  render     .fur to WAV".to_string(),
            "  6  report     analysis, nothing written".to_string(),
            "  7  condense   write copies of tracks with 1x..4x lower BPM".to_string(),
            self.tone
                .dim("  paste a path, or a number.  m menu, q quit, Ctrl+C quit"),
        ];
        if let Some(last) = self
            .state
            .get("browse_root")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
        {
            lines.insert(1, format!("  last folder  {last}"));
        }
        let mut text = lines.join("\n");
        text.push('\n');
        text
    }

    fn show_banner(&mut self) {
        let text = self.banner();
        self.emit(&text);
        // `out(_banner(state))` is `print`, which adds the blank line after.
        self.emit("\n");
    }

    fn dispatch(&mut self, raw: &str, key: &str) -> Result<Option<Nav>> {
        match key {
            "1" => self.cmd_packs(),
            "2" => {
                self.cmd_convert_file()?;
                Ok(None)
            }
            "3" => {
                self.cmd_edit()?;
                Ok(None)
            }
            "4" => {
                self.cmd_compare()?;
                Ok(None)
            }
            "5" => {
                self.cmd_render()?;
                Ok(None)
            }
            "6" => {
                self.cmd_report()?;
                Ok(None)
            }
            "7" => {
                self.cmd_condense()?;
                Ok(None)
            }
            _ => self.open_typed_path(raw),
        }
    }

    // ------------------------------------------------------------ prompts

    fn ask(&mut self, prompt: &str, default: &str) -> Result<String> {
        let suffix = if default.is_empty() {
            String::new()
        } else {
            format!(" [{default}]")
        };
        self.emit(&format!("{prompt}{suffix}: "));
        let _ = self.out.flush();
        crate::furnace::restore_console();
        match (self.read)() {
            ReadResult::Line(line) => {
                let answer = line.trim().to_string();
                if answer.is_empty() {
                    Ok(default.to_string())
                } else {
                    Ok(answer)
                }
            }
            ReadResult::Eof => Ok(default.to_string()),
            ReadResult::Interrupt => Err(Error::Interrupt),
        }
    }

    fn ask_optimise(&mut self) -> Result<String> {
        let answer = self
            .ask("optimise for pattern density? (Y/n)", "y")?
            .to_lowercase();
        Ok(if matches!(answer.as_str(), "n" | "no") {
            "none".to_string()
        } else {
            "all".to_string()
        })
    }

    fn ask_lfo_loop(&mut self) -> Result<bool> {
        let answer = self
            .ask("detect and optimise for LFO loops? (Y/n)", "y")?
            .to_lowercase();
        Ok(!matches!(answer.as_str(), "n" | "no"))
    }

    fn ask_lossy_samples(&mut self) -> Result<bool> {
        let answer = self
            .ask(
                "experimental aggressive (lossy) sample clustering? (y/N)",
                "n",
            )?
            .to_lowercase();
        Ok(matches!(answer.as_str(), "y" | "yes"))
    }

    fn ask_existing(&mut self, prompt: &str) -> Result<Option<Vec<PathBuf>>> {
        loop {
            let raw = self.ask(prompt, "")?;
            if raw.is_empty() {
                return Ok(None);
            }
            let matches = paths::expand_token(&raw);
            let existing: Vec<PathBuf> = matches.into_iter().filter(|p| p.exists()).collect();
            if !existing.is_empty() {
                return Ok(Some(existing));
            }
            let shown = paths::clean_text(&raw);
            let again = paths::expand_token(&raw);
            if again.is_empty() {
                self.emit_line(&format!("  no match for {shown}"));
            } else {
                self.emit_line(&format!("  not found: {shown}"));
            }
            self.emit_line("  blank cancels. Quotes around a pasted path are fine.");
        }
    }

    fn ask_file(&mut self, prompt: &str) -> Result<Option<PathBuf>> {
        loop {
            let found = match self.ask_existing(prompt)? {
                Some(found) => found,
                None => return Ok(None),
            };
            let files: Vec<&PathBuf> = found.iter().filter(|p| p.is_file()).collect();
            if files.len() == 1 {
                return Ok(Some(files[0].clone()));
            }
            if found.len() == 1 && found[0].is_dir() {
                self.emit_line("  that is a folder. Give the file.");
                continue;
            }
            let count = found.len();
            self.emit_line(&format!("  {count} matches. Give one file."));
        }
    }

    fn coerce_folder(&mut self, raw: &str) -> Result<Option<PathBuf>> {
        let matches: Vec<PathBuf> = paths::expand_token(raw)
            .into_iter()
            .filter(|p| p.exists())
            .collect();
        if matches.is_empty() {
            let shown = paths::clean_text(raw);
            if paths::expand_token(raw).is_empty() {
                self.emit_line(&format!("  no match for {shown}"));
            } else {
                self.emit_line(&format!("  not found: {shown}"));
            }
            return Ok(None);
        }
        if matches.len() == 1 {
            let path = &matches[0];
            return Ok(Some(if path.is_file() {
                parent_or_dot(path)
            } else {
                path.clone()
            }));
        }
        let parents: Vec<PathBuf> = paths::dedupe(
            &matches
                .iter()
                .map(|p| {
                    if p.is_file() {
                        parent_or_dot(p)
                    } else {
                        p.clone()
                    }
                })
                .collect::<Vec<_>>(),
        );
        if parents.len() == 1 {
            return Ok(Some(parents[0].clone()));
        }
        let count = matches.len();
        self.emit_line(&format!("  {count} matches. Give one folder."));
        Ok(None)
    }

    fn ask_factor(&mut self) -> Result<(Option<f64>, Option<f64>)> {
        let raw = self.ask("volume factor (1.2 = +20%, 0.75 = -25%, =0.9 = set)", "")?;
        if raw.is_empty() {
            return Ok((None, None));
        }
        if let Some(rest) = raw.strip_prefix('=') {
            return Ok((None, Some(py_float(rest)?)));
        }
        Ok((Some(py_float(&raw)?), None))
    }

    // ------------------------------------------------------------ state

    fn save(&self) {
        save_state(&self.state_path, &self.state);
    }

    fn remember(&mut self, folder: &Path) {
        set_state_str(&mut self.state, "browse_root", &folder.display().to_string());
        self.save();
    }

    fn set_out_dir(&mut self, key: &Path, dir: &Path) {
        let map = ensure_object(&mut self.state);
        let entry = map
            .entry("out_dirs".to_string())
            .or_insert_with(|| Value::Object(Map::new()));
        if !entry.is_object() {
            *entry = Value::Object(Map::new());
        }
        if let Some(out_dirs) = entry.as_object_mut() {
            out_dirs.insert(
                key.display().to_string(),
                Value::String(dir.display().to_string()),
            );
        }
    }

    // ------------------------------------------------------------ packs

    fn cmd_packs(&mut self) -> Result<Option<Nav>> {
        let last = self
            .state
            .get("browse_root")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        let default = match last {
            Some(last) => last,
            None => std::env::current_dir()?.display().to_string(),
        };
        let root_raw = self.ask("folder to scan", &default)?;
        if root_raw.is_empty() {
            return Ok(None);
        }
        let root = match self.coerce_folder(&root_raw)? {
            Some(root) => root,
            None => return Ok(None),
        };
        self.open_dir(&root, true)
    }

    fn open_dir(&mut self, root: &Path, remember: bool) -> Result<Option<Nav>> {
        if let Some(mut pack) = paths::folder_pack(root) {
            ensure_chips(std::slice::from_mut(&mut pack));
            if remember {
                self.remember(root);
            }
            return self.pack_view(&pack);
        }
        let mut found = packs::find_packs_with(root, 5, Some(PACK_SCAN_LIMIT));
        if !found.is_empty() {
            ensure_chips(&mut found);
            if remember {
                self.remember(root);
            }
            let truncated = found.len() >= PACK_SCAN_LIMIT;
            return self.browse_packs(&found, root, truncated);
        }
        let furs = paths::fur_targets(root);
        if !furs.is_empty() {
            self.show_furs(&furs);
            return Ok(None);
        }
        self.emit_line(&format!("  no packs under {}", root.display()));
        self.emit_line("  a pack is a folder with .vgm or .vgz files in it");
        Ok(None)
    }

    fn browse_packs(
        &mut self,
        found: &[Pack],
        root: &Path,
        truncated: bool,
    ) -> Result<Option<Nav>> {
        self.show_packs(found, root, truncated);
        loop {
            let answer = self.ask("pack number (m = list again, blank = main menu)", "")?;
            if answer.is_empty() {
                return Ok(None);
            }
            let low = answer.to_lowercase();
            if matches!(low.as_str(), "q" | "quit" | "exit") {
                return Ok(Some(Nav::Quit));
            }
            if matches!(low.as_str(), "m" | "menu") {
                self.show_packs(found, root, truncated);
                continue;
            }
            let pick = if py_isdigit(&answer) {
                answer.parse::<usize>().ok()
            } else {
                None
            };
            let chosen = pick.filter(|n| *n >= 1 && *n <= found.len());
            let chosen = match chosen {
                Some(n) => n,
                None => {
                    self.emit_line(&format!("  pick 1..{}", found.len()));
                    continue;
                }
            };
            let result = self.pack_view(&found[chosen - 1])?;
            if result.is_some() {
                return Ok(result);
            }
            self.show_packs(found, root, truncated);
        }
    }

    fn show_packs(&mut self, found: &[Pack], root: &Path, truncated: bool) {
        self.emit_line(&format!("{} packs in {}", found.len(), root.display()));
        for (i, pack) in found.iter().enumerate() {
            let chips = {
                let label = pack.chip_label();
                if label.is_empty() {
                    "no supported chip".to_string()
                } else {
                    label
                }
            };
            let playlist = if pack.playlist.is_some() {
                ", playlist"
            } else {
                ""
            };
            self.emit_line(&format!(
                "  {:3}  {:4} tracks  {}  ({}{})",
                i + 1,
                pack.tracks.len(),
                pack.name(),
                chips,
                playlist
            ));
        }
        if truncated {
            self.emit_line("  scan stopped here. Point at a narrower folder for the rest.");
        }
    }

    fn fur_dir(&self, pack: &Pack) -> PathBuf {
        let remembered = self
            .state
            .get("out_dirs")
            .and_then(Value::as_object)
            .and_then(|m| m.get(&pack.path.display().to_string()))
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty());
        match remembered {
            Some(dir) => PathBuf::from(dir),
            None => pack.path.join(FUR_FOLDER),
        }
    }

    fn pack_view(&mut self, pack: &Pack) -> Result<Option<Nav>> {
        let (tracks, from_playlist) = packs::pack_tracks(pack)?;
        loop {
            let fur_dir = self.fur_dir(pack);
            let chips = {
                let label = pack.chip_label();
                if label.is_empty() {
                    String::new()
                } else {
                    format!(", {label}")
                }
            };
            let order = if from_playlist {
                "playlist order"
            } else {
                "name order"
            };
            self.emit_line(&format!(
                "pack {}  ({} tracks{}, {})",
                pack.path.display(),
                tracks.len(),
                chips,
                order
            ));
            self.emit_line(&format!("output  {}", fur_dir.display()));
            for (i, track) in tracks.iter().enumerate() {
                let mark = if fur_path(&fur_dir, &file_stem(track)).is_file() {
                    ""
                } else {
                    "  -"
                };
                self.emit_line(&format!("  {:3}. {}{}", i + 1, file_stem(track), mark));
            }
            let answer = self
                .ask(
                    "c convert, e volumes, r render, b back (a '-' means no .fur yet)",
                    "b",
                )?
                .to_lowercase();
            if matches!(answer.as_str(), "" | "b" | "back") {
                return Ok(None);
            }
            if matches!(answer.as_str(), "q" | "quit" | "exit") {
                return Ok(Some(Nav::Quit));
            }
            if matches!(answer.as_str(), "m" | "menu") {
                return Ok(Some(Nav::Menu));
            }
            match answer.as_str() {
                "c" => self.pack_convert(pack, &tracks, &fur_dir)?,
                "e" => self.pack_volumes(&tracks, &fur_dir)?,
                "r" => self.pack_render(&tracks, &fur_dir, None)?,
                _ => self.emit_line("  c, e, r or b"),
            }
        }
    }

    fn run_convert(
        &mut self,
        files: &[PathBuf],
        dest: &Path,
        speed: Option<i32>,
        optimize: &str,
        verify: bool,
        lfo_loop: bool,
        lossy_samples: bool,
        tick_hz: Option<f64>,
    ) -> Result<i32> {
        if optimize != "none" {
            return self.run_variants(
                files,
                dest,
                4,
                optimize,
                verify,
                lfo_loop,
                lossy_samples,
                speed,
                tick_hz,
            );
        }
        let opts = ConvertOptions {
            speed,
            tick_hz,
            normalize: verify,
            lfo_loop,
            lossy_samples,
            ..ConvertOptions::default()
        };
        self.stream_plain(files, dest, &opts)
    }

    #[allow(clippy::too_many_arguments)]
    fn run_variants(
        &mut self,
        files: &[PathBuf],
        dest: &Path,
        factor: i64,
        mode: &str,
        verify: bool,
        lfo_loop: bool,
        lossy_samples: bool,
        speed: Option<i32>,
        tick_hz: Option<f64>,
    ) -> Result<i32> {
        let use_verify = verify && mode != "all";
        if use_verify {
            self.emit_line("  rendering/verifying each variant through Furnace...");
        }
        let opts = ConvertOptions {
            speed,
            tick_hz,
            variants: true,
            condense: factor as f64,
            optimize: mode.to_string(),
            verify,
            layout: "dirs".to_string(),
            normalize: verify,
            lfo_loop,
            lossy_samples,
            ..ConvertOptions::default()
        };
        self.stream_variants(files, dest, &opts)
    }

    /// Print each converted track the moment its result arrives, the way the
    /// reference consumes `convert_folder`'s generator inside the menu.
    fn stream_plain(
        &mut self,
        files: &[PathBuf],
        dest: &Path,
        opts: &ConvertOptions,
    ) -> Result<i32> {
        let Self { out, driver, .. } = self;
        let cell = RefCell::new(out);
        let mut done = 0usize;
        let mut rc = 0i32;
        {
            let mut log = |message: &str| {
                let _ = writeln!(*cell.borrow_mut(), "  {message}");
            };
            let mut on_row = |row: ConvertRow| {
                let mut sink = cell.borrow_mut();
                print_plain_row(&mut **sink, row, &mut done, &mut rc);
            };
            driver.convert_folder(files, dest, opts, &mut log, &mut on_row)?;
        }
        let _ = writeln!(
            *cell.borrow_mut(),
            "  {done}/{} converted to {}",
            files.len(),
            dest.display()
        );
        Ok(rc)
    }

    fn stream_variants(
        &mut self,
        files: &[PathBuf],
        dest: &Path,
        opts: &ConvertOptions,
    ) -> Result<i32> {
        let Self { out, driver, .. } = self;
        let cell = RefCell::new(out);
        let mut done = 0usize;
        let mut rc = 0i32;
        {
            let mut log = |message: &str| {
                let _ = writeln!(*cell.borrow_mut(), "  {message}");
            };
            let mut on_row = |row: ConvertRow| {
                let mut sink = cell.borrow_mut();
                print_variant_row(&mut **sink, row, dest, &mut done, &mut rc);
            };
            driver.convert_folder(files, dest, opts, &mut log, &mut on_row)?;
        }
        let _ = writeln!(
            *cell.borrow_mut(),
            "  {done}/{} converted to {}",
            files.len(),
            dest.display()
        );
        Ok(rc)
    }

    fn cmd_condense(&mut self) -> Result<()> {
        let raw = self.ask("VGM/VGZ file, folder, or glob", "")?;
        if raw.is_empty() {
            return Ok(());
        }
        let existing: Vec<PathBuf> = paths::expand_token(&raw)
            .into_iter()
            .filter(|p| p.exists())
            .collect();
        if existing.is_empty() {
            self.emit_line("  no match");
            return Ok(());
        }
        let dirs: Vec<PathBuf> = existing.iter().filter(|p| p.is_dir()).cloned().collect();
        let files: Vec<PathBuf> = existing.iter().filter(|p| p.is_file()).cloned().collect();
        let mut vgms: Vec<PathBuf> = files
            .iter()
            .filter(|p| has_vgm_suffix(p))
            .cloned()
            .collect();
        for folder in &dirs {
            vgms.extend(paths::vgm_targets(folder, None).0);
        }
        let vgms = paths::dedupe(&vgms);
        if vgms.is_empty() {
            self.emit_line("  no .vgm/.vgz in that path");
            return Ok(());
        }
        let default_dst = vgms[0]
            .parent()
            .map(|p| p.join(FUR_FOLDER))
            .unwrap_or_else(|| PathBuf::from(FUR_FOLDER));
        let dst = PathBuf::from(self.ask("output folder", &default_dst.display().to_string())?);
        let factor = self.ask("max condense factor (2-4)", "4")?;
        let mode = self.ask_optimise()?;
        let lfo_loop = self.ask_lfo_loop()?;
        let lossy_samples = self.ask_lossy_samples()?;
        let verify = matches!(
            self.ask("normalize/verify through Furnace? (y/N)", "n")?
                .to_lowercase()
                .as_str(),
            "y" | "yes"
        );
        set_state_bool(&mut self.state, "lfo_loop", lfo_loop);
        set_state_bool(&mut self.state, "lossy_samples", lossy_samples);
        let factor = if py_isdigit(&factor) {
            factor.parse::<i64>().unwrap_or(4)
        } else {
            4
        };
        self.run_variants(
            &vgms,
            &dst,
            factor,
            &mode,
            verify,
            lfo_loop,
            lossy_samples,
            None,
            None,
        )?;
        let key = parent_or_dot(&vgms[0]);
        self.set_out_dir(&key, &dst);
        self.save();
        Ok(())
    }

    fn pack_convert(
        &mut self,
        pack: &Pack,
        tracks: &[PathBuf],
        default_dir: &Path,
        ) -> Result<()> {
        let spec = self.ask("tracks (all, 3, 2-6, 1,4)", "all")?;
        let picks = match packs::parse_selection(&spec, tracks.len()) {
            Ok(picks) => picks,
            Err(exc) => {
                self.emit_line(&format!("  {exc}"));
                return Ok(());
            }
        };
        let dst = PathBuf::from(self.ask("output folder", &default_dir.display().to_string())?);
        let optimize = self.ask_optimise()?;
        let lfo_loop = self.ask_lfo_loop()?;
        let lossy_samples = self.ask_lossy_samples()?;
        let verify = matches!(
            self.ask("normalize/verify through Furnace? (y/N)", "n")?
                .to_lowercase()
                .as_str(),
            "y" | "yes"
        );
        set_state_bool(&mut self.state, "lfo_loop", lfo_loop);
        set_state_bool(&mut self.state, "lossy_samples", lossy_samples);
        let files: Vec<PathBuf> = picks.iter().map(|i| tracks[*i].clone()).collect();
        self.run_convert(
            &files,
            &dst,
            None,
            &optimize,
            verify,
            lfo_loop,
            lossy_samples,
            None,
        )?;
        self.set_out_dir(&pack.path, &dst);
        set_state_str(&mut self.state, "pack", &pack.path.display().to_string());
        self.save();
        Ok(())
    }

    fn pack_volumes(&mut self, tracks: &[PathBuf], fur_dir: &Path) -> Result<()> {
        let spec = self.ask("tracks to adjust (all, 3, 1,4)", "all")?;
        let picks = match packs::parse_selection(&spec, tracks.len()) {
            Ok(picks) => picks,
            Err(exc) => {
                self.emit_line(&format!("  {exc}"));
                return Ok(());
            }
        };
        let furs: Vec<PathBuf> = picks
            .iter()
            .map(|i| fur_path(fur_dir, &file_stem(&tracks[*i])))
            .collect();
        let missing: Vec<&PathBuf> = furs.iter().filter(|f| !f.is_file()).collect();
        if !missing.is_empty() {
            let shown: Vec<String> = missing
                .iter()
                .take(4)
                .map(|m| file_name(m))
                .collect();
            self.emit_line(&format!(
                "  convert these first (c): {}",
                shown.join(", ")
            ));
            return Ok(());
        }
        let (names, factor, value) = match self.read_volume_choices(&furs) {
            Ok(choices) => choices,
            Err(e @ (Error::FurEdit(_) | Error::Io { .. } | Error::Msg(_))) => {
                self.emit_line(&format!("  {e}"));
                return Ok(());
            }
            Err(e) => return Err(e),
        };
        if factor.is_none() && value.is_none() {
            self.emit_line("  nothing changed");
            return Ok(());
        }
        if self.apply_volume(&furs, &names, factor, value)? > 0 {
            let answer = self.ask("render one to WAV now (track number, blank = no)", "")?;
            if !answer.is_empty() {
                self.pack_render(tracks, fur_dir, Some(&answer))?;
            }
        }
        Ok(())
    }

    fn read_volume_choices(
        &mut self,
        furs: &[PathBuf],
    ) -> Result<(Vec<String>, Option<f64>, Option<f64>)> {
        let chips = crate::edit::read_chips_file(&furs[0])?;
        self.print_chips(&chips);
        let spec = self.ask("chip number, name, or 'all'", "all")?;
        let targets = parse_targets(&spec, &chips)?;
        let names: Vec<String> = chips
            .iter()
            .filter(|c| targets.contains(&c.index))
            .map(|c| c.name())
            .collect();
        let (factor, value) = self.ask_factor()?;
        Ok((names, factor, value))
    }

    fn pack_render(
        &mut self,
        tracks: &[PathBuf],
        fur_dir: &Path,
        spec: Option<&str>,
    ) -> Result<()> {
        let spec = match spec {
            Some(spec) => spec.to_string(),
            None => self.ask("track number", "1")?,
        };
        let picks = match packs::parse_selection(&spec, tracks.len()) {
            Ok(picks) => picks,
            Err(exc) => {
                self.emit_line(&format!("  {exc}"));
                return Ok(());
            }
        };
        let loops = self.ask("loops", "1")?;
        let nloops = match py_int_str(if loops.is_empty() { "1" } else { &loops }) {
            Some(n) => n,
            None => {
                self.emit_line("  loops should be a number");
                return Ok(());
            }
        };
        for i in picks {
            let fur = fur_path(fur_dir, &file_stem(&tracks[i]));
            if !fur.is_file() {
                self.emit_line(&format!(
                    "  no .fur at {}, convert it first",
                    file_name(&fur)
                ));
                continue;
            }
            let out_path = with_extension(&fur, "wav");
            if let Err(exc) = self.driver.render_wav(&fur, &out_path, nloops) {
                self.emit_line(&format!("  {exc}"));
                return Ok(());
            }
            let size = std::fs::metadata(&out_path).map(|m| m.len()).unwrap_or(0);
            self.emit_line(&format!("  {}  ({size} bytes)", file_name(&out_path)));
        }
        Ok(())
    }

    // ------------------------------------------------------------ direct commands

    fn cmd_convert_file(&mut self) -> Result<i32> {
        let raw = self.ask("VGM/VGZ file, folder, or glob", "")?;
        if raw.is_empty() {
            return Ok(0);
        }
        let matches = paths::expand_token(&raw);
        let existing: Vec<PathBuf> = matches.iter().filter(|p| p.exists()).cloned().collect();
        if existing.is_empty() {
            let shown = paths::clean_text(&raw);
            if matches.is_empty() {
                self.emit_line(&format!("  no match for {shown}"));
            } else {
                self.emit_line(&format!("  not found: {shown}"));
            }
            return Ok(0);
        }
        let dirs: Vec<PathBuf> = existing.iter().filter(|p| p.is_dir()).cloned().collect();
        let files: Vec<PathBuf> = existing.iter().filter(|p| p.is_file()).cloned().collect();
        if dirs.len() == 1 && files.is_empty() {
            self.open_dir(&dirs[0], true)?;
            return Ok(0);
        }
        if !dirs.is_empty() && files.is_empty() {
            let count = dirs.len();
            self.emit_line(&format!("  {count} folders. Give one."));
            for folder in dirs.iter().take(20) {
                self.emit_line(&format!("  {}", folder.display()));
            }
            return Ok(0);
        }
        let mut vgms: Vec<PathBuf> = files
            .iter()
            .filter(|p| has_vgm_suffix(p))
            .cloned()
            .collect();
        for folder in &dirs {
            vgms.extend(paths::vgm_targets(folder, None).0);
        }
        let vgms = paths::dedupe(&vgms);
        if vgms.is_empty() {
            self.emit_line("  no .vgm/.vgz in that path");
            return Ok(0);
        }
        let default_out = vgms[0]
            .parent()
            .map(|p| p.join(FUR_FOLDER))
            .unwrap_or_else(|| PathBuf::from(FUR_FOLDER));
        let dst = PathBuf::from(self.ask("output folder", &default_out.display().to_string())?);
        let speed = self.ask("ticks per row (blank = auto)", "")?;
        let mut tick_hz: Option<f64> = None;
        if py_isdigit(&speed) {
            let raw_hz = self.ask("tick rate Hz (blank = 60)", "")?;
            if !raw_hz.is_empty() {
                match py_float(&raw_hz) {
                    Ok(hz) => tick_hz = Some(hz),
                    Err(_) => self.emit_line(&format!(
                        "  '{raw_hz}' is not a number; tick rate stays at 60 Hz"
                    )),
                }
            }
        }
        let optimize = self.ask_optimise()?;
        let lfo_loop = self.ask_lfo_loop()?;
        let lossy_samples = self.ask_lossy_samples()?;
        let verify = matches!(
            self.ask("normalize/verify through Furnace? (y/N)", "n")?
                .to_lowercase()
                .as_str(),
            "y" | "yes"
        );
        set_state_bool(&mut self.state, "lfo_loop", lfo_loop);
        set_state_bool(&mut self.state, "lossy_samples", lossy_samples);
        self.save();
        let speed = if py_isdigit(&speed) {
            speed.parse::<i32>().ok()
        } else {
            None
        };
        self.run_convert(
            &vgms,
            &dst,
            speed,
            &optimize,
            verify,
            lfo_loop,
            lossy_samples,
            tick_hz,
        )
    }

    fn cmd_edit(&mut self) -> Result<()> {
        loop {
            let found = match self.ask_existing(".fur file or folder")? {
                Some(found) => found,
                None => return Ok(()),
            };
            let files = paths::dedupe(
                &found
                    .iter()
                    .flat_map(|path| paths::fur_targets(path))
                    .collect::<Vec<_>>(),
            );
            if files.is_empty() {
                self.emit_line("  no .fur under that path");
                return Ok(());
            }
            if files.len() > 1 {
                let count = files.len();
                self.emit_line(&format!("  {count} files here. The same change goes to each"));
            }
            let choices = match self.read_volume_choices(&files) {
                Ok(choices) => choices,
                Err(e @ (Error::FurEdit(_) | Error::Io { .. } | Error::Msg(_))) => {
                    self.emit_line(&format!("  {e}"));
                    return Ok(());
                }
                Err(e) => return Err(e),
            };
            let (names, factor, value) = choices;
            if factor.is_none() && value.is_none() {
                self.emit_line("  nothing changed");
            } else {
                self.apply_volume(&files, &names, factor, value)?;
            }
            let again = self.ask("adjust another file? (y/N)", "n")?;
            if !matches!(again.to_lowercase().as_str(), "y" | "yes") {
                return Ok(());
            }
        }
    }

    fn cmd_compare(&mut self) -> Result<()> {
        let src = match self.ask_file("source VGM/VGZ")? {
            Some(src) => src,
            None => return Ok(()),
        };
        let fur = match self.ask_file(".fur file")? {
            Some(fur) => fur,
            None => return Ok(()),
        };
        let ch = self.ask("YM2151 channel", "0")?;
        let channel = if ch.is_empty() {
            0
        } else {
            match ch.parse::<i32>() {
                Ok(value) => value,
                Err(_) => {
                    self.emit_line("  channel should be a number");
                    return Ok(());
                }
            }
        };
        match crate::diag::compare_fur_to_source(&src, &fur, channel, 0.5, 100.0, None) {
            Err(exc) => self.emit_line(&format!("  {exc}")),
            Ok((runs, exported)) => {
                self.emit_line(&format!("  export: {}", file_name(&exported)));
                if runs.is_empty() {
                    self.emit_line("  no sustained divergence above 0.5 st");
                    return Ok(());
                }
                self.emit_line(&format!("  {} divergence run(s):", runs.len()));
                for run in runs {
                    self.emit_line(&format!("   {run}"));
                }
            }
        }
        Ok(())
    }

    fn cmd_render(&mut self) -> Result<()> {
        let found = match self.ask_existing(".fur file or folder")? {
            Some(found) => found,
            None => return Ok(()),
        };
        let files = paths::dedupe(
            &found
                .iter()
                .flat_map(|path| paths::fur_targets(path))
                .collect::<Vec<_>>(),
        );
        if files.is_empty() {
            self.emit_line("  no .fur under that path");
            return Ok(());
        }
        let mut one = String::new();
        if files.len() == 1 {
            one = self.ask(
                "output WAV",
                &with_extension(&files[0], "wav").display().to_string(),
            )?;
        } else {
            let count = files.len();
            self.emit_line(&format!("  {count} files, WAV written next to each"));
        }
        let loops = self.ask("loops", "1")?;
        let nloops = match py_int_str(if loops.is_empty() { "1" } else { &loops }) {
            Some(n) => n,
            None => {
                self.emit_line("  loops should be a number");
                return Ok(());
            }
        };
        for fur in &files {
            let dest = if one.is_empty() {
                with_extension(fur, "wav")
            } else {
                PathBuf::from(&one)
            };
            if let Err(exc) = self.driver.render_wav(fur, &dest, nloops) {
                self.emit_line(&format!("  {exc}"));
                return Ok(());
            }
            self.emit_line(&format!("  {} -> {}", file_name(fur), dest.display()));
        }
        Ok(())
    }

    fn cmd_report(&mut self) -> Result<()> {
        let found = match self.ask_existing("VGM/VGZ file or folder")? {
            Some(found) => found,
            None => return Ok(()),
        };
        let mut direct_all: Vec<PathBuf> = Vec::new();
        for path in &found {
            let (direct, packs) = paths::vgm_targets(path, None);
            if !direct.is_empty() {
                direct_all.extend(direct);
                continue;
            }
            if !packs.is_empty() {
                self.emit_line(&format!("{} packs in {}", packs.len(), path.display()));
                for pack in &packs {
                    self.emit_line(&format!("  {:4}  {}", pack.tracks.len(), pack.name()));
                }
                self.emit_line("  pass a pack folder to report every track");
                continue;
            }
            self.emit_line(&format!("  no .vgm/.vgz under {}", path.display()));
        }
        if direct_all.is_empty() {
            return Ok(());
        }
        let files = paths::dedupe(&direct_all);
        let rows = {
            let out = &mut self.out;
            let mut log = |message: &str| {
                let _ = writeln!(out, "  {message}");
            };
            self.driver.report_files(&files, &mut log)
        };
        for (src, text, error) in rows {
            match error {
                Some(error) => {
                    self.emit_line(&format!("  ERROR {}: {error}", file_name(&src)))
                }
                None => {
                    if let Some(text) = text {
                        self.emit_line(&text);
                    }
                }
            }
        }
        Ok(())
    }

    fn show_furs(&mut self, files: &[PathBuf]) {
        self.emit_line(&format!("{} .fur", files.len()));
        for fur in files.iter().take(12) {
            let chips = match crate::edit::read_chips_file(fur) {
                Ok(chips) => chips,
                Err(exc) => {
                    self.emit_line(&format!("  ERROR {}: {exc}", file_name(fur)));
                    continue;
                }
            };
            let joined = chips
                .iter()
                .map(|c| format!("{} {}", c.name(), crate::parity::format_f(c.volume, 3)))
                .collect::<Vec<_>>()
                .join(", ");
            self.emit_line(&format!("  {}  {joined}", file_name(fur)));
        }
        if files.len() > 12 {
            self.emit_line(&format!("  ... {} more", files.len() - 12));
        }
        self.emit_line("  3 volumes, 5 render");
    }

    fn open_typed_path(&mut self, raw: &str) -> Result<Option<Nav>> {
        let matches = paths::expand_token(raw);
        let existing: Vec<PathBuf> = matches.iter().filter(|p| p.exists()).cloned().collect();
        if existing.is_empty() {
            let shown = paths::clean_text(raw);
            if looks_like_path(raw) {
                if matches.is_empty() {
                    self.emit_line(&format!("  no match for {shown}"));
                } else {
                    self.emit_line(&format!("  not found: {shown}"));
                }
            } else {
                self.emit_line("  unknown command. 'm' lists them, or paste a path");
            }
            return Ok(None);
        }
        let dirs: Vec<PathBuf> = existing.iter().filter(|p| p.is_dir()).cloned().collect();
        let files: Vec<PathBuf> = existing.iter().filter(|p| p.is_file()).cloned().collect();
        if existing.len() == 1 && existing[0].is_dir() {
            return self.open_dir(&existing[0], true);
        }
        let mut vgms: Vec<PathBuf> = files
            .iter()
            .filter(|p| has_vgm_suffix(p))
            .cloned()
            .collect();
        let furs: Vec<PathBuf> = files
            .iter()
            .filter(|p| suffix_lower(p) == ".fur")
            .cloned()
            .collect();
        for folder in &dirs {
            let (direct, packs) = paths::vgm_targets(folder, None);
            if !direct.is_empty() {
                vgms.extend(direct);
            } else if !packs.is_empty() {
                self.emit_line(&format!(
                    "  {}: {} packs. Paste that folder on its own.",
                    file_name(folder),
                    packs.len()
                ));
            }
        }
        if !vgms.is_empty() && furs.is_empty() {
            let unique = paths::dedupe(&vgms);
            let dest = vgms[0]
                .parent()
                .map(|p| p.join(FUR_FOLDER))
                .unwrap_or_else(|| PathBuf::from(FUR_FOLDER));
            let lfo_loop = state_bool(&self.state, "lfo_loop", true);
            let lossy_samples = state_bool(&self.state, "lossy_samples", false);
            let Self { out, driver, .. } = self;
            let cell = RefCell::new(out);
            let mut done = 0usize;
            let mut rc = 0i32;
            {
                let mut log = |message: &str| {
                    let _ = writeln!(*cell.borrow_mut(), "  {message}");
                };
                let mut on_row = |row: ConvertRow| {
                    let mut sink = cell.borrow_mut();
                    print_variant_row(&mut **sink, row, &dest, &mut done, &mut rc);
                };
                driver.convert_now(
                    &unique,
                    &dest,
                    None,
                    lfo_loop,
                    lossy_samples,
                    &mut log,
                    &mut on_row,
                )?;
            }
            let _ = writeln!(
                *cell.borrow_mut(),
                "  {done}/{} converted to {}",
                unique.len(),
                dest.display()
            );
            return Ok(None);
        }
        if !furs.is_empty() && vgms.is_empty() {
            self.show_furs(&paths::dedupe(&furs));
            return Ok(None);
        }
        if !vgms.is_empty() && !furs.is_empty() {
            self.emit_line("  mixed VGM and .fur files. 2 converts, 3 edits volumes.");
            return Ok(None);
        }
        self.emit_line("  nothing to convert or edit there");
        Ok(None)
    }

    fn print_chips(&mut self, chips: &[crate::edit::Chip]) {
        for chip in chips {
            self.emit_line(&format!(
                "   {}. {:<8} volume {}",
                chip.index + 1,
                chip.name(),
                crate::parity::format_f(chip.volume, 3)
            ));
        }
    }

    fn apply_volume(
        &mut self,
        files: &[PathBuf],
        names: &[String],
        factor: Option<f64>,
        value: Option<f64>,
    ) -> Result<usize> {
        let mut changed = 0usize;
        for fur in files {
            let outcome = (|| -> Result<bool> {
                let chips = crate::edit::read_chips_file(fur)?;
                let targets: Vec<usize> = chips
                    .iter()
                    .filter(|c| names.iter().any(|n| *n == c.name()))
                    .map(|c| c.index)
                    .collect();
                if targets.is_empty() {
                    self.emit_line(&format!(
                        "   {}: no {} here",
                        file_name(fur),
                        names.join(", ")
                    ));
                    return Ok(false);
                }
                crate::edit::set_chip_volume_file(fur, factor, value, Some(targets.as_slice()), false)?;
                let after: Vec<crate::edit::Chip> = crate::edit::read_chips_file(fur)?
                    .into_iter()
                    .filter(|c| targets.contains(&c.index))
                    .collect();
                let parts = after
                    .iter()
                    .map(|c| format!("{} {}", c.name(), crate::parity::format_f(c.volume, 3)))
                    .collect::<Vec<_>>()
                    .join(", ");
                self.emit_line(&format!("   {}: {parts}", file_name(fur)));
                Ok(true)
            })();
            match outcome {
                Ok(true) => changed += 1,
                Ok(false) => {}
                Err(e @ (Error::FurEdit(_) | Error::Io { .. } | Error::Msg(_))) => {
                    self.emit_line(&format!("   ERROR {}: {e}", file_name(fur)));
                }
                Err(e) => return Err(e),
            }
        }
        Ok(changed)
    }
}

// ---------------------------------------------------------------- state

fn load_state(path: &Path) -> Value {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(_) => return Value::Object(Map::new()),
    };
    let mut state: Value = match serde_json::from_str(&text) {
        Ok(state) => state,
        Err(_) => return Value::Object(Map::new()),
    };
    // The experimental lossy clustering choice is a session-only latch.
    // Pasting a path later in the same menu run may reuse the answer, but a
    // new session must default back to the normal (non-lossy) conversion.
    if let Some(map) = state.as_object_mut() {
        map.remove("lossy_samples");
    }
    state
}

fn save_state(path: &Path, state: &Value) {
    let mut payload = state.clone();
    if let Some(map) = payload.as_object_mut() {
        map.remove("lossy_samples");
    }
    let text = match serde_json::to_string_pretty(&payload) {
        Ok(text) => text,
        Err(_) => return,
    };
    let _ = std::fs::write(path, format!("{text}\n"));
}

fn ensure_object(state: &mut Value) -> &mut Map<String, Value> {
    if !state.is_object() {
        *state = Value::Object(Map::new());
    }
    state.as_object_mut().expect("state is an object")
}

fn set_state_str(state: &mut Value, key: &str, value: &str) {
    ensure_object(state).insert(key.to_string(), Value::String(value.to_string()));
}

fn set_state_bool(state: &mut Value, key: &str, value: bool) {
    ensure_object(state).insert(key.to_string(), Value::Bool(value));
}

fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(flag) => *flag,
        Value::Number(number) => number.as_f64().map(|n| n != 0.0).unwrap_or(true),
        Value::String(text) => !text.is_empty(),
        Value::Array(items) => !items.is_empty(),
        Value::Object(map) => !map.is_empty(),
    }
}

fn state_bool(state: &Value, key: &str, default: bool) -> bool {
    match state.get(key) {
        Some(value) => truthy(value),
        None => default,
    }
}

// ---------------------------------------------------------------- helpers

fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default()
}

fn file_stem(path: &Path) -> String {
    path.file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
        .unwrap_or_default()
}

fn suffix_lower(path: &Path) -> String {
    path.extension()
        .map(|ext| format!(".{}", ext.to_string_lossy().to_lowercase()))
        .unwrap_or_default()
}

fn has_vgm_suffix(path: &Path) -> bool {
    let suffix = suffix_lower(path);
    convert::VGM_SUFFIXES
        .iter()
        .any(|known| *known == suffix.as_str())
}

/// `path.parent()` with Python's `Path("x").parent == Path(".")` spelling.
fn parent_or_dot(path: &Path) -> PathBuf {
    match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent.to_path_buf(),
        _ => PathBuf::from("."),
    }
}

fn with_extension(path: &Path, extension: &str) -> PathBuf {
    let mut out = path.to_path_buf();
    out.set_extension(extension);
    out
}

/// The 1x .fur: `default/<stem>.fur` in the dir layout, else `<stem>.fur`.
fn fur_path(fur_dir: &Path, stem: &str) -> PathBuf {
    let sub = fur_dir.join("default").join(format!("{stem}.fur"));
    if sub.is_file() {
        sub
    } else {
        fur_dir.join(format!("{stem}.fur"))
    }
}

fn ensure_chips(packs: &mut [Pack]) {
    for pack in packs {
        if pack.chips.is_empty() {
            let chips = packs::scan_pack_chips(pack);
            pack.chips = chips;
        }
    }
}

fn looks_like_path(text: &str) -> bool {
    let cleaned = paths::clean_text(text);
    if cleaned.is_empty() {
        return false;
    }
    let low = cleaned.to_lowercase();
    if [".vgm", ".vgz", ".fur", ".wav", ".m3u"]
        .iter()
        .any(|suffix| low.ends_with(suffix))
    {
        return true;
    }
    cleaned
        .chars()
        .any(|ch| matches!(ch, '\\' | '/' | '*' | '?' | '['))
}

fn py_isdigit(text: &str) -> bool {
    !text.is_empty() && text.chars().all(|ch| ch.is_ascii_digit())
}

fn py_int_str(text: &str) -> Option<i64> {
    text.trim().parse::<i64>().ok()
}

fn py_float(text: &str) -> Result<f64> {
    let trimmed = text.trim();
    let cleaned = if trimmed.contains('_') {
        // Python accepts single underscores between digits; rewriting them
        // away reproduces that for every spelling the menu receives.
        trimmed.replace('_', "")
    } else {
        trimmed.to_string()
    };
    cleaned
        .parse::<f64>()
        .map_err(|_| Error::Msg(format!("could not convert string to float: {}", py_repr(trimmed))))
}

/// Python `repr()` for the strings the menu puts in errors.
fn py_repr(text: &str) -> String {
    let quote = if text.contains('\'') && !text.contains('"') {
        '"'
    } else {
        '\''
    };
    let mut out = String::new();
    out.push(quote);
    for ch in text.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c == quote => {
                out.push('\\');
                out.push(c);
            }
            c if (c as u32) < 0x20 || c as u32 == 0x7f => {
                out.push_str(&format!("\\x{:02x}", c as u32));
            }
            c => out.push(c),
        }
    }
    out.push(quote);
    out
}

/// `all`, `*`, a number, a name, or a comma list of those. Indices are
/// 0-based and sorted; out-of-range numbers and unmatched names are errors.
fn parse_targets(spec: &str, chips: &[crate::edit::Chip]) -> Result<Vec<usize>> {
    let mut targets: Vec<usize> = Vec::new();
    for token in spec.split(',') {
        let token = token.trim().to_lowercase();
        if token.is_empty() {
            continue;
        }
        if matches!(token.as_str(), "all" | "*") {
            targets.extend(0..chips.len());
            continue;
        }
        if py_isdigit(&token) {
            let index = token.parse::<usize>().unwrap_or(0);
            if index == 0 || index > chips.len() {
                return Err(Error::Msg(format!(
                    "chip {token} is outside 1..{}",
                    chips.len()
                )));
            }
            targets.push(index - 1);
            continue;
        }
        let matched: Vec<usize> = chips
            .iter()
            .filter(|c| c.name().to_lowercase().contains(&token))
            .map(|c| c.index)
            .collect();
        if matched.is_empty() {
            return Err(Error::Msg(format!("no chip matching {}", py_repr(&token))));
        }
        targets.extend(matched);
    }
    targets.sort_unstable();
    targets.dedup();
    Ok(targets)
}

/// One line per converted track (plain conversion).
fn print_plain_row(
    out: &mut dyn Write,
    row: ConvertRow,
    done: &mut usize,
    rc: &mut i32,
) {
    if let Some(error) = row.error.as_deref() {
        let _ = writeln!(out, "  ERROR {}: {error}", file_name(&row.src));
        *rc = 1;
        return;
    }
    *done += 1;
    let info = match row.info {
        Some(info) => info,
        None => return,
    };
    let vol = match info.gain {
        Some(gain) => format!("  vol {}x", crate::parity::format_f(gain, 2)),
        None => String::new(),
    };
    let loop_text = match &info.loop_ {
        Some(text) if !text.is_empty() => format!("  {text}"),
        _ => String::new(),
    };
    let _ = writeln!(
        out,
        "  {} -> {}  {} bytes  speed {}  {} notes{vol}{loop_text}",
        file_name(&row.src),
        file_name(Path::new(&info.dst)),
        info.bytes,
        info.speed,
        info.events
    );
    let mut shown = 0;
    for warning in &info.warnings {
        if Some(warning.as_str()) == info.loop_.as_deref() {
            continue;
        }
        let _ = writeln!(out, "     warning: {warning}");
        shown += 1;
        if shown == 2 {
            break;
        }
    }
}

fn print_variant_row(
    out: &mut dyn Write,
    row: ConvertRow,
    dest: &Path,
    done: &mut usize,
    rc: &mut i32,
) {
    if let Some(error) = row.error.as_deref() {
        let _ = writeln!(out, "  ERROR {}: {error}", file_name(&row.src));
        *rc = 1;
        return;
    }
    *done += 1;
    let _ = writeln!(out, "  {}", file_name(&row.src));
    let variants = row.info.and_then(|info| info.variants).unwrap_or_default();
    for line in convert::format_variant_lines(&variants, dest) {
        let _ = writeln!(out, "     {line}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    struct Fake;

    impl MenuDriver for Fake {
        fn convert_folder(
            &self,
            _files: &[PathBuf],
            _dest: &Path,
            _opts: &ConvertOptions,
            _log: &mut dyn FnMut(&str),
            _on_row: &mut dyn FnMut(ConvertRow),
        ) -> Result<()> {
            Ok(())
        }

        fn convert_now(
            &self,
            _files: &[PathBuf],
            _dest: &Path,
            _speed: Option<i32>,
            _lfo_loop: bool,
            _lossy_samples: bool,
            _log: &mut dyn FnMut(&str),
            _on_row: &mut dyn FnMut(ConvertRow),
        ) -> Result<()> {
            Ok(())
        }

        fn report_files(
            &self,
            _files: &[PathBuf],
            _log: &mut dyn FnMut(&str),
        ) -> Vec<(PathBuf, Option<String>, Option<String>)> {
            Vec::new()
        }

        fn render_wav(&self, _fur: &Path, _out: &Path, _loops: i64) -> Result<()> {
            Ok(())
        }
    }

    fn run(answers: Vec<ReadResult>) -> (i32, String) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        let mut answers: VecDeque<ReadResult> = answers.into();
        let mut read = || answers.pop_front().unwrap_or(ReadResult::Eof);
        let mut out: Vec<u8> = Vec::new();
        let rc = run_menu(&mut read, &mut out, &path, &Fake).unwrap();
        (rc, String::from_utf8(out).unwrap())
    }

    #[test]
    fn unknown_command_then_quit() {
        let (rc, text) = run(vec![
            ReadResult::Line("nope".to_string()),
            ReadResult::Line("q".to_string()),
        ]);
        assert_eq!(rc, 0);
        assert!(text.contains("unknown command"), "{text}");
    }

    #[test]
    fn eof_exits_zero() {
        let (rc, _text) = run(Vec::new());
        assert_eq!(rc, 0);
    }

    #[test]
    fn ctrl_c_at_the_prompt_quits_130() {
        let (rc, text) = run(vec![
            ReadResult::Line("nope".to_string()),
            ReadResult::Interrupt,
        ]);
        assert_eq!(rc, 130);
        assert!(text.contains("unknown command"), "{text}");
        assert!(text.contains("quit (Ctrl+C)"), "{text}");
    }

    #[test]
    fn parse_targets_all_number_and_name() {
        let chips = vec![
            crate::edit::Chip { index: 0, id: 0, volume: 1.0, pan: 0.0, pan_fr: 0.0 },
            crate::edit::Chip { index: 1, id: 0, volume: 1.0, pan: 0.0, pan_fr: 0.0 },
        ];
        assert_eq!(parse_targets("all", &chips).unwrap(), vec![0, 1]);
        assert_eq!(parse_targets("2", &chips).unwrap(), vec![1]);
        assert_eq!(parse_targets("1,1", &chips).unwrap(), vec![0]);
        assert!(parse_targets("3", &chips).is_err());
        assert!(parse_targets("zzz", &chips).is_err());
    }

    #[test]
    fn lossy_latch_is_not_persisted() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        std::fs::write(
            &path,
            r#"{"lossy_samples": true, "browse_root": "x"}"#,
        )
        .unwrap();
        let state = load_state(&path);
        assert!(state.get("lossy_samples").is_none());
        assert_eq!(state.get("browse_root").unwrap().as_str(), Some("x"));

        let mut payload = Value::Object(Map::new());
        set_state_bool(&mut payload, "lossy_samples", true);
        set_state_str(&mut payload, "browse_root", "x");
        save_state(&path, &payload);
        let saved: Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert!(saved.get("lossy_samples").is_none());
        assert_eq!(saved.get("browse_root").unwrap().as_str(), Some("x"));
    }
}

// ---------------------------------------------------------------------------
// Test seams
//
// The reference tests call the module's private helpers directly
// (`menu._ask_lfo_loop`, `menu._run_convert`, `menu._convert_now`,
// `menu._open_typed_path`, `menu._parse_targets`, `menu._load_state`,
// `menu._save_state`). The plan makes those seams part of the library API.
// ---------------------------------------------------------------------------

/// `_ask_lfo_loop(read)`: the default answer is yes.
pub fn _ask_lfo_loop(read: &mut dyn FnMut(&str) -> String) -> bool {
    let mut r = || ReadResult::Line(read(""));
    let mut out: Vec<u8> = Vec::new();
    let mut menu = Menu {
        read: &mut r,
        out: &mut out,
        driver: &RealDriver,
        state: Value::Object(Map::new()),
        state_path: PathBuf::new(),
        tone: Tone { on: false },
    };
    menu.ask_lfo_loop().unwrap_or(false)
}

/// `_ask_lossy_samples(read)`: the default answer is no.
pub fn _ask_lossy_samples(read: &mut dyn FnMut(&str) -> String) -> bool {
    let mut r = || ReadResult::Line(read(""));
    let mut out: Vec<u8> = Vec::new();
    let mut menu = Menu {
        read: &mut r,
        out: &mut out,
        driver: &RealDriver,
        state: Value::Object(Map::new()),
        state_path: PathBuf::new(),
        tone: Tone { on: false },
    };
    menu.ask_lossy_samples().unwrap_or(false)
}

/// `_load_state(path)`.
pub fn _load_state(path: &Path) -> Value {
    load_state(path)
}

/// `_save_state(path, state)`.
pub fn _save_state(path: &Path, state: &Value) {
    save_state(path, state)
}

/// `_parse_targets(spec, chips)`.
pub fn _parse_targets(spec: &str, chips: &[crate::edit::Chip]) -> Result<Vec<usize>> {
    parse_targets(spec, chips)
}

/// `_run_convert(emit, files, dest, speed, optimize, verify, lfo_loop,
/// lossy_samples, tick_hz)`: returns the CLI-style status of the batch.
#[allow(clippy::too_many_arguments)]
pub fn _run_convert(
    out: &mut dyn Write,
    driver: &dyn MenuDriver,
    files: &[PathBuf],
    dest: &Path,
    speed: Option<i32>,
    optimize: &str,
    verify: bool,
    lfo_loop: bool,
    lossy_samples: bool,
    tick_hz: Option<f64>,
) -> Result<i32> {
    let mut read = || ReadResult::Eof;
    let mut menu = Menu {
        read: &mut read,
        out,
        driver,
        state: Value::Object(Map::new()),
        state_path: PathBuf::new(),
        tone: Tone { on: false },
    };
    menu.run_convert(
        files,
        dest,
        speed,
        optimize,
        verify,
        lfo_loop,
        lossy_samples,
        tick_hz,
    )
}

/// `_convert_now(emit, files, dest, speed)`: a direct conversion with the
/// menu defaults (no Furnace).
pub fn _convert_now(
    out: &mut dyn Write,
    driver: &dyn MenuDriver,
    files: &[PathBuf],
    dest: &Path,
    speed: Option<i32>,
    lfo_loop: bool,
    lossy_samples: bool,
) -> Result<i32> {
    let mut read = || ReadResult::Eof;
    let mut menu = Menu {
        read: &mut read,
        out,
        driver,
        state: Value::Object(Map::new()),
        state_path: PathBuf::new(),
        tone: Tone { on: false },
    };
    const EMPTY_STATE: &str = "";
    let _ = EMPTY_STATE;
    {
        let Menu { out, driver, .. } = &mut menu;
        let cell = RefCell::new(out);
        let mut done = 0usize;
        let mut rc = 0i32;
        {
            let mut emit = |line: &str| {
                let _ = writeln!(*cell.borrow_mut(), "{}", line);
            };
            let mut on_row = |row: ConvertRow| {
                let mut sink = cell.borrow_mut();
                print_plain_row(&mut **sink, row, &mut done, &mut rc);
            };
            driver.convert_now(
                files,
                dest,
                speed,
                lfo_loop,
                lossy_samples,
                &mut emit,
                &mut on_row,
            )?;
        }
        let _ = writeln!(
            *cell.borrow_mut(),
            "  {done}/{} converted to {}",
            files.len(),
            dest.display()
        );
        Ok(rc)
    }
}

/// `_open_typed_path(read, emit, state, state_path, raw)`.
pub fn _open_typed_path(
    read: &mut dyn FnMut(&str) -> String,
    out: &mut dyn Write,
    state: Value,
    state_path: &Path,
    raw: &str,
    driver: &dyn MenuDriver,
) -> Result<()> {
    let mut r = || ReadResult::Line(read(""));
    let mut menu = Menu {
        read: &mut r,
        out,
        driver,
        state,
        state_path: state_path.to_path_buf(),
        tone: Tone { on: false },
    };
    menu.open_typed_path(raw)?;
    Ok(())
}
