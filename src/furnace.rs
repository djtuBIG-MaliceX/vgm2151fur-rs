//! Run the Furnace console build: render `.fur` to WAV, export to VGM.
//!
//! Furnace is not vendored in the package. The tools look for the console
//! build in this order: `VGM2151FUR_FURNACE` (an exe or a folder holding it), a
//! `furnace` folder next to the project (an unpacked release or a source
//! checkout with `build/furnace.exe`), a `third_party/furnace-console/`
//! drop-in, a `third_party/furnace` drop-in, the same folders one level up,
//! then `PATH`.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use crate::error::{Error, Result};
use crate::priority::creationflags;

/// Furnace's `-console` path clears line input on the console it is attached
/// to. Launch it detached, with stdin closed, so the menu keeps that console.
pub const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// Ctrl+C, line input, echo.
pub const CONSOLE_LINE: u32 = 0x0001 | 0x0002 | 0x0004;

// Furnace failures are `crate::Error::Furnace`; this alias keeps the Python
// exception name visible at call sites.
pub type FurnaceError = Error;

/// Turn line input, echo, and Ctrl+C back on for this console.
pub fn restore_console() {
    #[cfg(not(windows))]
    {
        return;
    }
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::Console::{
            GetConsoleMode, GetStdHandle, SetConsoleMode, STD_INPUT_HANDLE,
        };

        // SAFETY: the handle and mode pointer are the same ones the console
        // API expects; a null or closed handle makes either call fail and we
        // simply return, exactly like the ctypes version.
        unsafe {
            let handle = GetStdHandle(STD_INPUT_HANDLE);
            if handle.is_null() {
                return;
            }
            let mut mode: u32 = 0;
            if GetConsoleMode(handle, &mut mode) == 0 {
                return;
            }
            if mode & CONSOLE_LINE != CONSOLE_LINE {
                let _ = SetConsoleMode(handle, mode | CONSOLE_LINE);
            }
        }
    }
}

/// The console build's file name on this platform.
pub fn binary_name() -> &'static str {
    if cfg!(windows) { "furnace.exe" } else { "furnace" }
}

/// The project root, the Rust equivalent of `Path(__file__).parents[1]`.
fn project_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn _candidates() -> Vec<PathBuf> {
    let here = project_root();
    let exe = binary_name();
    let mut out: Vec<PathBuf> = Vec::new();
    if let Some(raw) = env::var_os("VGM2151FUR_FURNACE") {
        if !raw.is_empty() {
            let p = PathBuf::from(raw);
            if p.is_dir() {
                out.push(p.join(exe));
            } else {
                out.push(p);
            }
        }
    }
    out.push(here.join("furnace").join(exe));
    out.push(here.join("furnace").join("build").join(exe));
    out.push(here.parent().unwrap_or(&here).join("furnace").join(exe));
    out.push(
        here.parent()
            .unwrap_or(&here)
            .join("furnace")
            .join("build")
            .join(exe),
    );
    out.push(here.join("third_party").join("furnace-console").join(exe));
    out.push(
        here.parent()
            .unwrap_or(&here)
            .join("third_party")
            .join("furnace-console")
            .join(exe),
    );
    out.push(here.join("third_party").join("furnace").join(exe));
    out.push(
        here.parent()
            .unwrap_or(&here)
            .join("third_party")
            .join("furnace")
            .join(exe),
    );
    out
}

/// Candidate paths in probe order, mirroring Python's `_candidates`.
pub fn candidates() -> Vec<PathBuf> {
    _candidates()
}

/// Find the console build, or [`None`] when it is not installed.
pub fn find_furnace() -> Option<PathBuf> {
    for candidate in _candidates() {
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    which("furnace").map(PathBuf::from)
}

/// Windows `shutil._WIN_DEFAULT_PATHEXT`.
const WIN_DEFAULT_PATHEXT: &str = ".COM;.EXE;.BAT;.CMD;.VBS;.JS;.WS;.MSC";

/// `shutil.which` for the default `F_OK | X_OK` mode.
fn which(command: &str) -> Option<String> {
    let mut dirs: Vec<PathBuf> = match env::var_os("PATH") {
        // PATH='' doesn't match, whereas PATH=':' looks in the current
        // directory.
        Some(path) if path.is_empty() => return None,
        Some(path) => env::split_paths(&path).collect(),
        None => {
            let fallback = if cfg!(windows) { ".;C:\\bin" } else { "/bin:/usr/bin" };
            env::split_paths(&std::ffi::OsString::from(fallback)).collect()
        }
    };
    let files: Vec<String> = if cfg!(windows) {
        // NeedCurrentDirectoryForExePath: a bare name searches the current
        // directory unless NoDefaultCurrentDirectoryInExePath is set.
        let needs_curdir = !command.contains('\\')
            && env::var_os("NoDefaultCurrentDirectoryInExePath").is_none();
        let curdir = PathBuf::from(".");
        if needs_curdir && !dirs.iter().any(|dir| dir == &curdir) {
            dirs.insert(0, curdir);
        }
        let pathext_source = env::var("PATHEXT")
            .ok()
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| WIN_DEFAULT_PATHEXT.to_string());
        let extensions: Vec<String> = pathext_source
            .split(';')
            .filter(|ext| !ext.is_empty())
            .map(|ext| ext.trim_end_matches('.').to_string())
            .collect();
        let mut files: Vec<String> = extensions
            .iter()
            .map(|ext| format!("{command}{ext}"))
            .collect();
        let normcmd = command.to_ascii_uppercase();
        if extensions
            .iter()
            .any(|ext| normcmd.ends_with(&ext.to_ascii_uppercase()))
        {
            files.insert(0, command.to_string());
        }
        files
    } else {
        vec![command.to_string()]
    };
    let mut seen: Vec<PathBuf> = Vec::new();
    for dir in dirs {
        if seen.contains(&dir) {
            continue;
        }
        seen.push(dir.clone());
        for file in &files {
            let candidate = dir.join(file);
            if is_executable(&candidate) {
                return Some(candidate.to_string_lossy().into_owned());
            }
        }
    }
    None
}

fn is_executable(path: &Path) -> bool {
    if !path.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        match fs::metadata(path) {
            Ok(meta) => meta.permissions().mode() & 0o111 != 0,
            Err(_) => false,
        }
    }
    #[cfg(not(unix))]
    {
        true
    }
}

/// The file name Python's `Path.name` reports.
fn path_name(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// `Path.parent`, with Python's `.` for a bare file name.
fn parent_dir(path: &Path) -> PathBuf {
    match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent.to_path_buf(),
        _ => PathBuf::from("."),
    }
}

/// `Path.resolve`: absolute, without requiring the file to exist.
fn resolve(path: &Path) -> Result<PathBuf> {
    std::path::absolute(path).map_err(|exc| Error::io(path, exc))
}

static CAPTURE_SEQ: AtomicU64 = AtomicU64::new(0);

/// A temporary file that receives the child's merged stdout/stderr.
struct CaptureFile {
    path: PathBuf,
    file: fs::File,
}

impl CaptureFile {
    fn new() -> Result<Self> {
        let mut last: Option<(PathBuf, std::io::Error)> = None;
        for _ in 0..100 {
            let seq = CAPTURE_SEQ.fetch_add(1, Ordering::Relaxed);
            let path = env::temp_dir().join(format!(
                "vgm2fur-furnace-{}-{seq}.log",
                std::process::id()
            ));
            match fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
            {
                Ok(file) => return Ok(CaptureFile { path, file }),
                Err(exc) if exc.kind() == std::io::ErrorKind::AlreadyExists => {
                    last = Some((path, exc));
                }
                Err(exc) => return Err(Error::io(&path, exc)),
            }
        }
        match last {
            Some((path, exc)) => Err(Error::io(&path, exc)),
            None => Err(Error::Furnace(
                "could not create a Furnace output capture file".to_string(),
            )),
        }
    }

    fn stdio(&self) -> Result<Stdio> {
        self.file
            .try_clone()
            .map(Stdio::from)
            .map_err(|exc| Error::io(&self.path, exc))
    }
}

impl Drop for CaptureFile {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

/// What a finished Furnace run left behind: its exit code and merged output.
#[derive(Clone, Debug, Default)]
pub struct Capture {
    pub code: i64,
    pub text: String,
}

/// The process seam: production uses [`SystemRunner`], tests a fake.
pub trait FurnaceRunner {
    fn run(&self, exe: &Path, args: &[String], timeout_secs: u64) -> Result<Capture>;
}

/// Production runner: starts the real console build and collects its output.
pub struct SystemRunner;

impl FurnaceRunner for SystemRunner {
    fn run(&self, exe: &Path, args: &[String], timeout_secs: u64) -> Result<Capture> {
        let Some(out_arg) = args.len().checked_sub(2) else {
            return Err(Error::Furnace(
                "furnace command is missing the output path".to_string(),
            ));
        };
        let out = PathBuf::from(&args[out_arg]);
        let fur = PathBuf::from(args.last().map(String::as_str).unwrap_or(""));
        let parent = parent_dir(&out);
        fs::create_dir_all(&parent).map_err(|exc| Error::io(&parent, exc))?;
        if out.exists() {
            fs::remove_file(&out).map_err(|exc| Error::io(&out, exc))?;
        }
        let capture = CaptureFile::new()?;

        let mut cmd = _command(exe, args);
        cmd.current_dir(&parent);
        cmd.stdout(capture.stdio()?);
        cmd.stderr(capture.stdio()?);

        let mut child = match cmd.spawn() {
            Ok(child) => child,
            Err(exc) => return Err(Error::io(exe, exc)),
        };
        let start = Instant::now();
        let mut status: Option<ExitStatus> = None;
        let mut wait_error: Option<std::io::Error> = None;
        let mut timed_out = false;
        loop {
            match child.try_wait() {
                Ok(Some(done)) => {
                    status = Some(done);
                    break;
                }
                Ok(None) => {
                    if start.elapsed() >= Duration::from_secs(timeout_secs) {
                        timed_out = true;
                        let _ = child.kill();
                        let _ = wait_for_exit(&mut child, Duration::from_secs(5));
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(exc) => {
                    wait_error = Some(exc);
                    let _ = child.kill();
                    let _ = wait_for_exit(&mut child, Duration::from_secs(5));
                    break;
                }
            }
        }
        restore_console();
        if let Some(exc) = wait_error {
            return Err(Error::io(exe, exc));
        }
        let text = fs::read(&capture.path)
            .map(|raw| String::from_utf8_lossy(&raw).into_owned())
            .unwrap_or_default();
        if timed_out {
            return Err(Error::Furnace(format!(
                "{}: furnace timed out after {timeout_secs}s",
                path_name(&fur)
            )));
        }
        let status = match status {
            Some(status) => status,
            None => {
                return Err(Error::Furnace(format!(
                    "{}: furnace exited without a status",
                    path_name(&fur)
                )));
            }
        };
        let exists = out.exists();
        let size = if exists {
            fs::metadata(&out).map(|meta| meta.len()).unwrap_or(0)
        } else {
            0
        };
        if !status.success() || !exists || size == 0 {
            let lines = py_splitlines(&text);
            let start = lines.len().saturating_sub(6);
            let tail = lines[start..].join("\n");
            return Err(Error::Furnace(format!(
                "{}: furnace exit {}\n{}",
                path_name(&fur),
                returncode(&status),
                tail
            )));
        }
        Ok(Capture {
            code: returncode(&status),
            text,
        })
    }
}

/// The command line and launch attributes a Furnace invocation gets.
#[derive(Clone, Debug)]
pub struct CommandSpec {
    pub exe: PathBuf,
    /// The complete argument list, `-console` first.
    pub args: Vec<String>,
    pub stdin_null: bool,
    pub creation_flags: u32,
}

/// Build the inspectable description of a Furnace launch.
pub fn _command_spec(exe: &Path, args: &[String]) -> CommandSpec {
    let mut all = Vec::with_capacity(args.len() + 1);
    all.push("-console".to_string());
    all.extend_from_slice(args);
    #[allow(unused_mut)]
    let mut flags = creationflags();
    #[cfg(windows)]
    {
        flags |= CREATE_NO_WINDOW;
    }
    CommandSpec {
        exe: exe.to_path_buf(),
        args: all,
        stdin_null: true,
        creation_flags: flags,
    }
}

/// The `Command` a Furnace launch uses: stdin closed, low priority, detached
/// from the console where the platform supports it.
pub fn _command(exe: &Path, args: &[String]) -> Command {
    let spec = _command_spec(exe, args);
    let mut cmd = Command::new(&spec.exe);
    cmd.args(&spec.args);
    if spec.stdin_null {
        cmd.stdin(Stdio::null());
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        if spec.creation_flags != 0 {
            cmd.creation_flags(spec.creation_flags);
        }
    }
    cmd
}

/// Run through an injected runner; the seam used by tests.
pub fn _run_with(
    runner: &dyn FurnaceRunner,
    exe: &Path,
    args: &[String],
    timeout_secs: u64,
) -> Result<Capture> {
    runner.run(exe, args, timeout_secs)
}

fn _run(fur: &Path, out: &Path, extra: &[String], timeout: Option<f64>) -> Result<PathBuf> {
    let Some(exe) = find_furnace() else {
        return Err(Error::Furnace(format!(
            "{} not found. Put a Furnace build next to the project as \
             furnace/, drop it in third_party/furnace/, or point \
             VGM2151FUR_FURNACE at it",
            binary_name()
        )));
    };
    let out_abs = resolve(out)?;
    let fur_abs = resolve(fur)?;
    let mut args = extra.to_vec();
    args.push(out_abs.to_string_lossy().into_owned());
    args.push(fur_abs.to_string_lossy().into_owned());
    let timeout_secs = timeout.map(|limit| limit.max(0.0) as u64).unwrap_or(u64::MAX);
    _run_with(&SystemRunner, &exe, &args, timeout_secs)?;
    Ok(out.to_path_buf())
}

fn wait_for_exit(child: &mut Child, timeout: Duration) -> Option<ExitStatus> {
    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Some(status),
            Ok(None) => {
                if start.elapsed() >= timeout {
                    return None;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(_) => return None,
        }
    }
}

/// `subprocess.Popen.returncode` as an integer.
fn returncode(status: &ExitStatus) -> i64 {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(signal) = status.signal() {
            return -(signal as i64);
        }
    }
    status.code().map(i64::from).unwrap_or(0)
}

/// Python `str.splitlines` for the output tail.
fn py_splitlines(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        let boundary = matches!(
            ch,
            '\n' | '\r' | '\u{000b}' | '\u{000c}' | '\u{001c}' | '\u{001d}'
                | '\u{001e}' | '\u{0085}' | '\u{2028}' | '\u{2029}'
        );
        if boundary {
            out.push(std::mem::take(&mut current));
            if ch == '\r' && chars.peek() == Some(&'\n') {
                chars.next();
            }
        } else {
            current.push(ch);
        }
    }
    if !current.is_empty() {
        out.push(current);
    }
    out
}

/// Render `fur` to `out`, optionally as a specific sample format.
pub fn render_wav(
    fur: &Path,
    out: &Path,
    loops: i64,
    outformat: Option<&str>,
) -> Result<PathBuf> {
    let Some(exe) = find_furnace() else {
        return Err(Error::Furnace(format!(
            "{} not found. Put a Furnace build next to the project as \
             furnace/, drop it in third_party/furnace/, or point \
             VGM2151FUR_FURNACE at it",
            binary_name()
        )));
    };
    render_wav_with(&SystemRunner, &exe, fur, out, loops, outformat)
}

fn _wav_args(loops: i64, outformat: Option<&str>) -> Vec<String> {
    let mut extra: Vec<String> = vec![
        "-noreport".to_string(),
        "-loops".to_string(),
        loops.to_string(),
    ];
    if let Some(outformat) = outformat {
        if !outformat.is_empty() {
            extra.push("-outformat".to_string());
            extra.push(outformat.to_string());
        }
    }
    extra.push("-output".to_string());
    extra
}

/// [`render_wav`] with the executable and runner supplied for tests.
pub fn render_wav_with(
    runner: &dyn FurnaceRunner,
    exe: &Path,
    fur: &Path,
    out: &Path,
    loops: i64,
    outformat: Option<&str>,
) -> Result<PathBuf> {
    let extra = _wav_args(loops, outformat);
    let parent = parent_dir(out);
    fs::create_dir_all(&parent).map_err(|exc| Error::io(&parent, exc))?;
    if out.exists() {
        fs::remove_file(out).map_err(|exc| Error::io(out, exc))?;
    }
    let out_abs = resolve(out)?;
    let fur_abs = resolve(fur)?;
    let mut args = extra;
    args.push(out_abs.to_string_lossy().into_owned());
    args.push(fur_abs.to_string_lossy().into_owned());
    _run_with(runner, exe, &args, 900)?;
    Ok(out.to_path_buf())
}

/// Export `fur` to a `.vgm` beside it.
pub fn export_vgm(fur: &Path, out: &Path) -> Result<PathBuf> {
    // A malformed module opens a modal dialog instead of exiting. A register
    // walk exports in far less than a minute.
    _run(fur, out, &["-vgmout".to_string()], Some(60.0))
}

/// Largest absolute sample of a rendered WAV (f32 or s16).
pub fn wav_peak(path: &Path) -> Result<f64> {
    let raw = fs::read(path).map_err(|exc| Error::io(path, exc))?;
    let mut pos = 12usize;
    let mut fmt: Option<&[u8]> = None;
    let mut data: Option<&[u8]> = None;
    while pos + 8 <= raw.len() {
        let cid = &raw[pos..pos + 4];
        let size = u32::from_le_bytes([
            raw[pos + 4],
            raw[pos + 5],
            raw[pos + 6],
            raw[pos + 7],
        ]) as usize;
        let start = pos + 8;
        let end = start.saturating_add(size).min(raw.len());
        if cid == b"fmt " {
            fmt = Some(&raw[start..end]);
        } else if cid == b"data" {
            data = Some(&raw[start..end]);
        }
        pos = start
            .saturating_add(size)
            .saturating_add(size & 1);
    }
    let name = path_name(path);
    let (Some(fmt), Some(data)) = (fmt, data) else {
        return Err(Error::Msg(format!("{name}: no fmt/data chunk")));
    };
    if fmt.len() < 16 {
        return Err(Error::Msg(format!("{name}: no fmt/data chunk")));
    }
    let audio_format = u16::from_le_bytes([fmt[0], fmt[1]]);
    let bits = u16::from_le_bytes([fmt[14], fmt[15]]);
    if audio_format == 3 && bits == 32 {
        let usable = data.len() - data.len() % 4;
        let mut samples = data[..usable].chunks_exact(4);
        let mut high: f64 = 0.0;
        let mut low: f64 = 0.0;
        let mut any = false;
        for chunk in &mut samples {
            let value = f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]) as f64;
            if !any {
                high = value;
                low = value;
                any = true;
            } else {
                if value > high {
                    high = value;
                }
                if value < low {
                    low = value;
                }
            }
        }
        if !any {
            high = 0.0;
            low = 0.0;
        }
        let low_abs = -low;
        return Ok(if low_abs > high { low_abs } else { high });
    }
    if audio_format == 1 && bits == 16 {
        let usable = data.len() - data.len() % 2;
        let mut samples = data[..usable].chunks_exact(2);
        let mut high: i32 = 0;
        let mut low: i32 = 0;
        let mut any = false;
        for chunk in &mut samples {
            let value = i16::from_le_bytes([chunk[0], chunk[1]]) as i32;
            if !any {
                high = value;
                low = value;
                any = true;
            } else {
                if value > high {
                    high = value;
                }
                if value < low {
                    low = value;
                }
            }
        }
        if !any {
            high = 0;
            low = 0;
        }
        let low_abs = -low;
        let peak = if low_abs > high { low_abs } else { high };
        return Ok(peak as f64 / 32768.0);
    }
    Err(Error::Msg(format!(
        "{name}: unsupported WAV format {audio_format}/{bits}"
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn write_wav(path: &Path, audio_format: u16, bits: u16, data: &[u8]) {
        let block = bits / 8 * 2;
        let mut out = Vec::new();
        out.extend_from_slice(b"RIFF");
        out.extend_from_slice(&(4u32 + 24 + 8 + data.len() as u32).to_le_bytes());
        out.extend_from_slice(b"WAVE");
        out.extend_from_slice(b"fmt ");
        out.extend_from_slice(&16u32.to_le_bytes());
        out.extend_from_slice(&audio_format.to_le_bytes());
        out.extend_from_slice(&2u16.to_le_bytes());
        out.extend_from_slice(&44100u32.to_le_bytes());
        out.extend_from_slice(&(44100u32 * block as u32).to_le_bytes());
        out.extend_from_slice(&block.to_le_bytes());
        out.extend_from_slice(&bits.to_le_bytes());
        out.extend_from_slice(b"data");
        out.extend_from_slice(&(data.len() as u32).to_le_bytes());
        out.extend_from_slice(data);
        fs::write(path, out).unwrap();
    }

    #[test]
    fn wav_peak_reads_s16_peak() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("peak.s16.wav");
        let mut data = Vec::new();
        for sample in [-32768i16, 100, 0] {
            data.extend_from_slice(&sample.to_le_bytes());
        }
        write_wav(&path, 1, 16, &data);
        assert_eq!(wav_peak(&path).unwrap(), 1.0);
    }

    #[test]
    fn wav_peak_reads_f32_peak() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("peak.f32.wav");
        let mut data = Vec::new();
        for sample in [0.5f32, -1.25, 0.3] {
            data.extend_from_slice(&sample.to_le_bytes());
        }
        write_wav(&path, 3, 32, &data);
        assert_eq!(wav_peak(&path).unwrap(), 1.25);
    }

    #[test]
    fn wav_peak_rejects_unsupported_format() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("peak.u8.wav");
        write_wav(&path, 1, 8, &[0, 1]);
        let error = wav_peak(&path).unwrap_err();
        assert_eq!(
            error.to_string(),
            "peak.u8.wav: unsupported WAV format 1/8"
        );
    }

    #[test]
    fn wav_peak_rejects_missing_chunks() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("empty.wav");
        fs::write(&path, b"RIFF\x00\x00\x00\x00WAVE").unwrap();
        let error = wav_peak(&path).unwrap_err();
        assert_eq!(error.to_string(), "empty.wav: no fmt/data chunk");
    }

    #[test]
    fn find_furnace_honours_the_env_override() {
        let dir = tempdir().unwrap();
        let exe = dir.path().join(binary_name());
        fs::write(&exe, b"").unwrap();
        let previous = env::var_os("VGM2151FUR_FURNACE");
        // SAFETY: this unit test owns the variable for the duration of the
        // call; no other furnace test reads it concurrently.
        let from_exe = unsafe {
            env::set_var("VGM2151FUR_FURNACE", &exe);
            find_furnace()
        };
        let from_folder = unsafe {
            env::set_var("VGM2151FUR_FURNACE", dir.path());
            find_furnace()
        };
        unsafe {
            match previous {
                Some(value) => env::set_var("VGM2151FUR_FURNACE", value),
                None => env::remove_var("VGM2151FUR_FURNACE"),
            }
        }
        assert_eq!(from_exe, Some(exe.clone()));
        assert_eq!(from_folder, Some(exe));
    }
}
