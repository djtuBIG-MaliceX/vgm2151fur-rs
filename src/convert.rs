//! Convert VGMs to .fur files, one at a time or as a folder.
//!
//! Literal port of `vgm2151fur/convert.py` at v0.11.0. Every Python name is
//! kept public (leading underscore included) so the CLI, the menu and the
//! condense/variant tests can call the same seams the Python tests patch.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use std::sync::mpsc;

use indexmap::IndexMap;
use serde_json::{Value, json};

use crate::analyze::{AnalyzeOptions, analyze, format_report};
use crate::error::{Error, Result};
use crate::furwrite::{
    WriteOptions, compress_fur, dev_max, keyon_loss, loss_total, write_fur,
};
use crate::levels::normalize_module;
use crate::loopfind::apply_loop;
use crate::model::Song;
use crate::parity::{fmt_g, format_f};
use crate::vgm::{VgmFile, load_vgm};

/// The desktop default: logical CPUs minus one or two threads, capped at 16.
pub static CONVERT_WORKERS: LazyLock<i64> =
    LazyLock::new(|| crate::priority::resolve_workers(None, 1_000_000).0);
/// A rendition may carry this share of its played time as divergent pitch.
pub const DIVERGENCE_SHARE: f64 = 0.15;
pub const VGM_SUFFIXES: [&str; 2] = [".vgm", ".vgz"];
pub const VGM_GLOBS: [&str; 4] = ["*.vgm", "*.vgz", "*.VGM", "*.VGZ"];

/// Every flag of the Python `convert_vgm` / `convert_variants` /
/// `convert_folder` surfaces.
#[derive(Clone, Debug)]
pub struct ConvertOptions {
    pub speed: Option<i32>,
    pub tick_hz: Option<f64>,
    pub pcm: bool,
    pub tick_samples: Option<f64>,
    pub min_row: Option<f64>,
    pub variants: bool,
    pub condense: f64,
    pub factors: Option<Vec<f64>>,
    pub optimize: String,
    pub include_fm: bool,
    pub normalize: bool,
    pub loop_find: bool,
    pub verify: bool,
    pub layout: String,
    pub c352_quad: bool,
    pub lfo_loop: bool,
    pub lossy_samples: bool,
    pub workers: Option<usize>,
}

impl Default for ConvertOptions {
    fn default() -> Self {
        ConvertOptions {
            speed: None,
            tick_hz: None,
            pcm: true,
            tick_samples: None,
            min_row: None,
            variants: false,
            condense: 1.0,
            factors: None,
            optimize: "all".to_string(),
            include_fm: true,
            normalize: false,
            loop_find: true,
            verify: false,
            layout: "flat".to_string(),
            c352_quad: false,
            lfo_loop: true,
            lossy_samples: false,
            workers: None,
        }
    }
}

impl ConvertOptions {
    fn analyze_options<'a>(
        &self,
        condense: f64,
        grid_cache: Option<&'a mut crate::timing::GridCache>,
    ) -> AnalyzeOptions<'a> {
        AnalyzeOptions {
            speed: self.speed,
            pcm: self.pcm,
            tick_hz: self.tick_hz,
            tick_samples: self.tick_samples,
            min_row: self.min_row,
            condense,
            grid_cache,
            c352_quad: self.c352_quad,
            lfo_loop: self.lfo_loop,
            lossy_samples: self.lossy_samples,
            ..Default::default()
        }
    }
}

/// `report_files` options (the `report` subcommand's flags).
#[derive(Clone, Copy, Debug)]
pub struct ReportOptions {
    pub speed: Option<i32>,
    pub pcm: bool,
    pub tick_samples: Option<f64>,
    pub min_row: Option<f64>,
    pub workers: Option<usize>,
}

impl Default for ReportOptions {
    fn default() -> Self {
        ReportOptions {
            speed: None,
            pcm: true,
            tick_samples: None,
            min_row: None,
            workers: None,
        }
    }
}

/// The dict `convert_vgm` returns. Count fields follow the Python names.
#[derive(Clone, Debug, Default)]
pub struct ConvertInfo {
    pub src: String,
    pub dst: String,
    pub bytes: usize,
    pub speed: i32,
    pub hz: f64,
    pub bpm: f64,
    pub rows_per_s: f64,
    pub condense: f64,
    pub loss: IndexMap<String, i64>,
    pub loss_total: i64,
    pub keyons: i64,
    pub missed: i64,
    pub dev_max: i64,
    pub fm_patches: usize,
    pub pcm_samples: usize,
    pub oki_samples: usize,
    pub msm6258_samples: usize,
    /// Python's CLI `_sample_counts` queries these keys with `info.get`, but
    /// `convert_vgm` at v0.11.0 never writes them. They stay 0 so those
    /// labels remain absent from CLI output exactly as in Python.
    pub ymz280b_samples: usize,
    pub multipcm_samples: usize,
    pub upd7759_samples: usize,
    pub pwm_samples: usize,
    pub y8950_b_samples: usize,
    pub segapcm_samples: usize,
    pub rf5c68_samples: usize,
    pub rf5c164_samples: usize,
    pub c140_samples: usize,
    pub c219_samples: usize,
    pub c219_notes: i32,
    pub c352_samples: usize,
    pub k053260_samples: usize,
    pub k054539_samples: usize,
    pub k054539_notes: i32,
    pub msm5205_samples: usize,
    pub msm5205_notes: i32,
    pub ga20_samples: usize,
    pub ga20_notes: i32,
    pub msm5232_notes: i32,
    pub mikey_samples: usize,
    pub mikey_notes: i32,
    pub x1_010_samples: usize,
    pub x1_010_notes: i32,
    pub qsound_samples: usize,
    pub qsound_notes: i32,
    pub wonderswan_pcm_samples: usize,
    pub wonderswan_notes: i32,
    pub ym2610_a_samples: usize,
    pub ym2610_b_samples: usize,
    pub ym2610_a_notes: i32,
    pub ym2610_b_notes: i32,
    pub ym2610_ssg_notes: i32,
    pub ym2608_chips: usize,
    pub ym2608_ext: bool,
    pub ym2608_delta_t_samples: usize,
    pub ym2608_rhythm_notes: i32,
    pub ym2608_delta_t_notes: i32,
    pub ym2608_ssg_notes: i32,
    pub scc_notes: i32,
    pub ay_notes: i32,
    pub saa_notes: i32,
    pub saa_chips: usize,
    pub ym2203_chips: usize,
    pub ym2203_ext: bool,
    pub ym2203_ssg_notes: i32,
    pub psg_notes: i32,
    pub dac_notes: i32,
    pub dac_samples: usize,
    pub events: usize,
    pub legato_steps: i32,
    pub legato_ramps: i32,
    pub warnings: Vec<String>,
    pub name: String,
    pub peak: Option<f64>,
    pub gain: Option<f64>,
    pub loop_: Option<String>,
    /// Only `_variant_info` fills this: the whole 1x..Nx result set.
    pub variants: Option<Vec<VariantInfo>>,
}

/// One row of a `convert_variants` result set.
#[derive(Clone, Debug, Default)]
pub struct VariantInfo {
    pub factor: f64,
    pub dst: String,
    pub bytes: usize,
    pub speed: i32,
    pub hz: f64,
    pub bpm: f64,
    pub rows_per_s: f64,
    pub grid: String,
    pub loss: IndexMap<String, i64>,
    pub loss_total: i64,
    pub keyons: i64,
    pub missed: i64,
    pub dev_max: i64,
    pub legato_steps: i32,
    pub legato_ramps: i32,
    pub warnings: Vec<String>,
    pub loop_: Option<String>,
    pub peak: Option<f64>,
    pub gain: Option<f64>,
    pub div_ms: Option<f64>,
    pub audit: Option<IndexMap<String, Value>>,
    pub verify_failed: bool,
    pub lossless: bool,
    pub pick: bool,
}

/// One finished unit of `convert_folder` / `report_files`.
#[derive(Clone, Debug)]
pub struct ConvertRow {
    pub src: PathBuf,
    pub info: Option<ConvertInfo>,
    pub error: Option<String>,
}

/// The test seams Python gets from patching module globals.
pub trait ConvertDriver {
    fn load_vgm(&self, path: &Path) -> Result<VgmFile>;
    fn analyze(&self, vgm: &VgmFile, opts: AnalyzeOptions<'_>) -> Result<Song>;
    fn apply_loop(&self, song: &mut Song) -> Option<String>;
    fn verify_runs(
        &self,
        src_vgm: &VgmFile,
        fur_path: &Path,
        channels: Option<i32>,
        threshold_st: Option<f64>,
        min_run_ms: Option<f64>,
    ) -> Result<IndexMap<String, Value>>;
}

/// Production driver: forwards to the real library functions.
pub struct RealDriver;

impl ConvertDriver for RealDriver {
    fn load_vgm(&self, path: &Path) -> Result<VgmFile> {
        load_vgm(path)
    }

    fn analyze(&self, vgm: &VgmFile, opts: AnalyzeOptions<'_>) -> Result<Song> {
        analyze(vgm, opts)
    }

    fn apply_loop(&self, song: &mut Song) -> Option<String> {
        apply_loop(song)
    }

    fn verify_runs(
        &self,
        src_vgm: &VgmFile,
        fur_path: &Path,
        channels: Option<i32>,
        threshold_st: Option<f64>,
        min_run_ms: Option<f64>,
    ) -> Result<IndexMap<String, Value>> {
        _verify_runs(src_vgm, fur_path, channels, threshold_st, min_run_ms)
    }
}

/// Python `Path.stem` text.
fn _stem(path: &Path) -> String {
    path.file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// Python `Path.name` text.
fn _name(path: &Path) -> String {
    path.file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default()
}

fn _replace_path(from: &Path, to: &Path) -> std::io::Result<()> {
    if to.exists() {
        let _ = fs::remove_file(to);
    }
    fs::rename(from, to)
}

/// Python `Path.unlink(missing_ok=True)`: absence is fine, other errors raise.
fn _unlink_missing_ok(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(Error::io(path, err)),
    }
}

fn _json_i64(value: &Value, key: &str) -> i64 {
    value.get(key).and_then(Value::as_i64).unwrap_or(0)
}

fn _scalar_text(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Number(n) => n.to_string(),
        Value::Bool(b) => {
            if *b {
                "True".to_string()
            } else {
                "False".to_string()
            }
        }
        Value::Null => "None".to_string(),
        _ => String::new(),
    }
}

/// Python `repr()` of the `single_track` token as used in `select_tracks`.
fn _track_repr(token: &str) -> String {
    let quote = if token.contains('\'') && !token.contains('"') {
        '"'
    } else {
        '\''
    };
    let mut out = String::new();
    out.push(quote);
    for ch in token.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c == quote => {
                out.push('\\');
                out.push(c);
            }
            c => out.push(c),
        }
    }
    out.push(quote);
    out
}

pub fn track_number(path: &Path) -> Option<i64> {
    let stem = _stem(path);
    let digits: String = stem.chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() {
        None
    } else {
        digits.parse::<i64>().ok()
    }
}

/// `.vgm`/`.vgz` files in `path`, or `[path]` when that file is one.
///
/// A directory is not walked. Nested packs are a separate scan.
pub fn iter_vgms(path: &Path) -> Vec<PathBuf> {
    if path.is_file() {
        let suffix = path
            .extension()
            .map(|s| format!(".{}", s.to_string_lossy().to_lowercase()))
            .unwrap_or_default();
        return if VGM_SUFFIXES.contains(&suffix.as_str()) {
            vec![path.to_path_buf()]
        } else {
            Vec::new()
        };
    }
    let mut files: Vec<PathBuf> = Vec::new();
    // `Path.glob` in a directory is the four non-recursive patterns in
    // VGM_GLOBS; on Windows it matches case-insensitively, which is the
    // behavior below. `read_dir` also keeps path metacharacters literal.
    if let Ok(entries) = fs::read_dir(path) {
        for entry in entries.flatten() {
            let p = entry.path();
            let suffix = p
                .extension()
                .map(|s| format!(".{}", s.to_string_lossy().to_lowercase()))
                .unwrap_or_default();
            if VGM_SUFFIXES.contains(&suffix.as_str()) {
                files.push(p);
            }
        }
    }
    files.sort_by(|a, b| _name(a).to_lowercase().cmp(&_name(b).to_lowercase()));
    let mut seen: HashSet<String> = HashSet::new();
    let mut out: Vec<PathBuf> = Vec::new();
    for p in files {
        let key = p
            .canonicalize()
            .unwrap_or_else(|_| p.clone())
            .to_string_lossy()
            .to_lowercase();
        if seen.insert(key) {
            out.push(p);
        }
    }
    out
}

/// `15` matches a leading track number, any other token a stem substring.
pub fn select_tracks(files: &[PathBuf], single_track: Option<&str>) -> Result<Vec<PathBuf>> {
    let Some(raw) = single_track else {
        return Ok(files.to_vec());
    };
    if raw.is_empty() {
        return Ok(files.to_vec());
    }
    let token = raw.trim().to_string();
    let hit: Vec<PathBuf> = if !token.is_empty() && token.chars().all(|c| c.is_ascii_digit()) {
        let n: i64 = token.parse().unwrap_or(i64::MAX);
        files
            .iter()
            .filter(|p| track_number(p) == Some(n))
            .cloned()
            .collect()
    } else {
        let low = token.to_lowercase();
        files
            .iter()
            .filter(|p| _stem(p).to_lowercase().contains(&low))
            .cloned()
            .collect()
    };
    if hit.is_empty() {
        let names = files
            .iter()
            .take(8)
            .map(|p| _name(p))
            .collect::<Vec<_>>()
            .join(", ");
        let ellipsis = if files.len() > 8 { "..." } else { "" };
        return Err(Error::msg(format!(
            "no track matching {} (looked at: {}{})",
            _track_repr(raw),
            names,
            ellipsis
        )));
    }
    Ok(hit)
}

pub fn report_vgm(path: &Path, opts: &ReportOptions) -> Result<String> {
    let vgm = load_vgm(path)?;
    let song = analyze(
        &vgm,
        AnalyzeOptions {
            speed: opts.speed,
            pcm: opts.pcm,
            tick_samples: opts.tick_samples,
            min_row: opts.min_row,
            ..Default::default()
        },
    )?;
    Ok(format_report(&song))
}

pub fn _report_one(
    path_str: &str,
    speed: Option<i32>,
    pcm: bool,
    tick_samples: Option<f64>,
    min_row: Option<f64>,
) -> Result<String> {
    report_vgm(
        Path::new(path_str),
        &ReportOptions {
            speed,
            pcm,
            tick_samples,
            min_row,
            workers: None,
        },
    )
}

/// Analyze each file. Rows come back in input order.
///
/// One track per worker thread, at low priority. The default count follows
/// the logical CPUs (see `priority.default_workers`).
pub fn report_files(
    files: &[PathBuf],
    opts: &ReportOptions,
    log: &mut dyn FnMut(&str),
) -> Vec<(PathBuf, Option<String>, Option<String>)> {
    let mut rows: Vec<(PathBuf, Option<String>, Option<String>)> = Vec::new();
    if files.is_empty() {
        return rows;
    }
    let (n_workers, note) =
        crate::priority::resolve_workers(opts.workers.map(|w| w as i64), files.len() as i64);
    if n_workers <= 1 {
        let _low = crate::priority::low_priority();
        for f in files {
            match report_vgm(f, opts) {
                Ok(report) => rows.push((f.clone(), Some(report), None)),
                Err(exc) => rows.push((f.clone(), None, Some(exc.to_string()))),
            }
        }
        return rows;
    }
    log(&format!("workers: {n_workers} ({note})"));
    let options = *opts;
    let mut pairs: Vec<(usize, Result<String>)> = std::thread::scope(|scope| {
        let handles: Vec<_> = files
            .iter()
            .enumerate()
            .map(|(i, f)| {
                let f = f.clone();
                (
                    i,
                    scope.spawn(move || {
                        crate::priority::set_low_priority();
                        (i, report_vgm(&f, &options))
                    }),
                )
            })
            .collect();
        handles
            .into_iter()
            .map(|(i, h)| {
                h.join()
                    .unwrap_or_else(|_| (i, Err(Error::msg("report worker panicked"))))
            })
            .collect()
    });
    pairs.sort_by_key(|(i, _)| *i);
    for (i, result) in pairs {
        let src = files[i].clone();
        match result {
            Ok(report) => rows.push((src, Some(report), None)),
            Err(exc) => rows.push((src, None, Some(exc.to_string()))),
        }
    }
    rows
}

pub fn dump_pcm(path: &Path, out_dir: &Path) -> Result<IndexMap<String, Value>> {
    let vgm = load_vgm(path)?;
    crate::pcm::dump_pcm(&vgm, &out_dir.join(_stem(path)), 8)
}

/// The BPM Furnace displays (`calcBPM`: `60*hz/(hilightA*speed)`).
pub fn furnace_bpm(song: &Song) -> f64 {
    let hl = if song.hilight_a != 0 { song.hilight_a } else { 4 };
    60.0 * song.hz / (hl as f64 * 1.max(song.speed) as f64)
}

/// Milliseconds of divergent pitch between the source and the export.
pub fn _verify_runs(
    src_vgm: &VgmFile,
    fur_path: &Path,
    channels: Option<i32>,
    threshold_st: Option<f64>,
    min_run_ms: Option<f64>,
) -> Result<IndexMap<String, Value>> {
    let mut channels = channels.unwrap_or(8);
    let threshold_st = threshold_st.unwrap_or(0.25);
    let min_run_ms = min_run_ms.unwrap_or(60.0);

    let ym_ids: HashSet<u8> = src_vgm
        .writes
        .iter()
        .filter(|w| w.chip == "ym2203" || w.chip == "ym2203_ssg")
        .map(|w| w.chip_id)
        .collect();
    let ym2612_ids: HashSet<u8> = src_vgm
        .writes
        .iter()
        .filter(|w| w.chip == "ym2612")
        .map(|w| w.chip_id)
        .collect();
    let ym2610_ids: HashSet<u8> = src_vgm
        .writes
        .iter()
        .filter(|w| w.chip == "ym2610" || w.chip == "ym2610_ssg")
        .map(|w| w.chip_id)
        .collect();
    let ym2608_ids: HashSet<u8> = src_vgm
        .writes
        .iter()
        .filter(|w| w.chip == "ym2608" || w.chip == "ym2608_ssg")
        .map(|w| w.chip_id)
        .collect();
    let has_opm = src_vgm.writes.iter().any(|w| w.chip == "ym2151");

    let opl_kind = ["opl3", "opl2", "opl"]
        .into_iter()
        .find(|k| src_vgm.writes.iter().any(|w| w.chip == *k));
    if let Some(opl_kind) = opl_kind {
        let drums = src_vgm.writes.iter().any(|w| {
            w.chip == opl_kind && (w.reg & 0xFF) == 0xBD && (w.val & 0x20) != 0
        });
        let ids: HashSet<u8> = src_vgm
            .writes
            .iter()
            .filter(|w| w.chip == opl_kind)
            .map(|w| w.chip_id)
            .collect();
        channels = crate::opl::channel_count(opl_kind, drums) * 1.max(ids.len() as i32);
    } else if !ym2608_ids.is_empty() && !has_opm {
        let ext = src_vgm.writes.iter().any(|w| {
            w.chip == "ym2608"
                && (w.reg & 0x100) == 0
                && (w.reg & 0xFF) == 0x27
                && (w.val & 0xC0) != 0
        });
        channels = (if ext { 19 } else { 16 }) * ym2608_ids.len() as i32;
    } else if !ym2610_ids.is_empty() && !has_opm {
        channels = (if src_vgm.ym2610_b { 16 } else { 14 }) * ym2610_ids.len() as i32;
    } else if !ym2612_ids.is_empty() && !has_opm {
        let ext = src_vgm
            .writes
            .iter()
            .any(|w| w.chip == "ym2612" && (w.reg & 0xFF) == 0x27 && (w.val & 0xC0) != 0);
        channels = (if ext { 9 } else { 6 }) * ym2612_ids.len() as i32;
    } else if !ym_ids.is_empty() && !has_opm {
        let ext = src_vgm
            .writes
            .iter()
            .any(|w| w.chip == "ym2203" && (0xA8..=0xAE).contains(&w.reg));
        channels = (if ext { 9 } else { 6 }) * ym_ids.len() as i32;
    }

    let exp = fur_path.with_extension("verify.vgm");
    let result = (|| -> Result<IndexMap<String, Value>> {
        crate::diag::export_fur(fur_path, Some(&exp))?;
        let other = load_vgm(&exp)?;
        let mut total: i64 = 0;
        for ch in 0..channels {
            for run in crate::diag::pitch_runs(
                src_vgm,
                &other,
                ch,
                threshold_st,
                min_run_ms,
            ) {
                total += run.t1 - run.t0;
            }
        }
        let mut verdict = crate::audit::compare_keyons(src_vgm, &other);
        verdict.insert("div_ms".to_string(), json!(total as f64 / 44.1));
        Ok(verdict)
    })();
    let _ = fs::remove_file(&exp);
    result
}

/// Write one Song to `out_path`. Returns (bytes, peak, gain).
///
/// `gain` reuses a master volume measured on another rendition of the track.
pub fn _emit(
    song: &Song,
    out_path: &Path,
    pcm: bool,
    include_fm: bool,
    normalize: bool,
    warnings: &mut Vec<String>,
    stats: &mut IndexMap<String, i64>,
    gain: Option<f64>,
) -> Result<(Vec<u8>, Option<f64>, Option<f64>)> {
    // An empty YM2151 adds a second device to the export and VGM players
    // rescale the mix, dropping the sample chip's level. The YM2203 SSG
    // belongs to the same system, and an SSG-only track keeps the chip.
    let use_fm = include_fm
        && (!song.fm_patches.is_empty()
            || !song.ym2203_ids.is_empty()
            || (pcm && !song.y8950_b_samples.is_empty()));
    let mut data = write_fur(
        song,
        WriteOptions {
            include_pcm: pcm,
            include_fm: use_fm,
            stats: Some(stats),
            compress: false,
        },
    );
    if let Some(parent) = out_path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)?;
        }
    }
    fs::write(out_path, &data)?;
    if normalize {
        if let Some(gain) = gain {
            data = crate::furio::set_master_volume(&data, gain)?;
            data = compress_fur(&data, 5);
            fs::write(out_path, &data)?;
            return Ok((data, None, Some(gain)));
        }
    }
    let mut peak = None;
    let mut applied = None;
    if normalize {
        let name = _stem(out_path);
        match normalize_module(&data, &name) {
            Err(Error::Furnace(text)) | Err(Error::Msg(text)) => {
                if text.contains("unrecognized system ID") {
                    warnings.push(
                        "volume normalization skipped: this Furnace build cannot load the \
                         module (unrecognized system ID)"
                            .to_string(),
                    );
                } else {
                    let first = text.lines().next().unwrap_or("");
                    warnings.push(format!("volume normalization skipped: {first}"));
                }
            }
            Err(other) => return Err(other),
            Ok(Some(result)) => {
                data = result.data;
                peak = Some(result.peak);
                applied = Some(result.gain);
            }
            Ok(None) => {}
        }
    }
    data = compress_fur(&data, 5);
    fs::write(out_path, &data)?;
    Ok((data, peak, applied))
}

pub fn convert_vgm(
    path: &Path,
    out_path: &Path,
    opts: &ConvertOptions,
    deps: &dyn ConvertDriver,
) -> Result<ConvertInfo> {
    let vgm = deps.load_vgm(path)?;
    let mut song = deps.analyze(
        &vgm,
        opts.analyze_options(opts.condense, None),
    )?;
    let loop_note = if opts.loop_find {
        deps.apply_loop(&mut song)
    } else {
        None
    };
    let mut warnings = song.warnings.clone();
    let mut stats: IndexMap<String, i64> = IndexMap::new();
    let (data, peak, gain) = _emit(
        &song,
        out_path,
        opts.pcm,
        opts.include_fm,
        opts.normalize,
        &mut warnings,
        &mut stats,
        None,
    )?;
    let pcm = opts.pcm;
    Ok(ConvertInfo {
        src: path.to_string_lossy().into_owned(),
        dst: out_path.to_string_lossy().into_owned(),
        bytes: data.len(),
        speed: song.speed,
        hz: song.hz,
        bpm: furnace_bpm(&song),
        rows_per_s: song.hz / song.speed as f64,
        condense: opts.condense,
        loss: stats.clone(),
        loss_total: loss_total(&stats),
        keyons: *stats.get("keyons").unwrap_or(&0),
        missed: keyon_loss(Some(&stats)),
        dev_max: dev_max(&stats),
        fm_patches: if opts.include_fm && !song.fm_patches.is_empty() {
            song.fm_patches.len()
        } else {
            0
        },
        pcm_samples: if pcm {
            song.pcm_samples.iter().filter(|s| s.length() >= 8).count()
        } else {
            0
        },
        oki_samples: if pcm { song.oki_samples.len() } else { 0 },
        msm6258_samples: if pcm { song.msm6258_samples.len() } else { 0 },
        // Not part of Python's `convert_vgm` return dict: always absent.
        ymz280b_samples: 0,
        multipcm_samples: 0,
        upd7759_samples: 0,
        pwm_samples: 0,
        y8950_b_samples: 0,
        segapcm_samples: if pcm { song.segapcm_samples.len() } else { 0 },
        rf5c68_samples: if pcm { song.rf5c68_samples.len() } else { 0 },
        rf5c164_samples: if pcm { song.rf5c164_samples.len() } else { 0 },
        c140_samples: if pcm { song.c140_samples.len() } else { 0 },
        c219_samples: if pcm { song.c219_samples.len() } else { 0 },
        c219_notes: song.c219_count,
        c352_samples: if pcm { song.c352_samples.len() } else { 0 },
        k053260_samples: if pcm { song.k053260_samples.len() } else { 0 },
        k054539_samples: if pcm { song.k054539_samples.len() } else { 0 },
        k054539_notes: song.k054539_count,
        msm5205_samples: if pcm { song.msm5205_samples.len() } else { 0 },
        msm5205_notes: song.msm5205_count,
        ga20_samples: if pcm { song.ga20_samples.len() } else { 0 },
        ga20_notes: song.ga20_count,
        msm5232_notes: song.msm5232_count,
        mikey_samples: if pcm { song.mikey_samples.len() } else { 0 },
        mikey_notes: song.mikey_count,
        x1_010_samples: if pcm { song.x1_010_samples.len() } else { 0 },
        x1_010_notes: song.x1_010_count,
        qsound_samples: if pcm { song.qsound_samples.len() } else { 0 },
        qsound_notes: song.qsound_count + song.qsound_adpcm_count,
        wonderswan_pcm_samples: if pcm {
            song.wonderswan_pcm_samples.len()
        } else {
            0
        },
        wonderswan_notes: song.wonderswan_count,
        ym2610_a_samples: if pcm { song.ym2610_a_samples.len() } else { 0 },
        ym2610_b_samples: if pcm { song.ym2610_b_samples.len() } else { 0 },
        ym2610_a_notes: song.ym2610_a_count,
        ym2610_b_notes: song.ym2610_b_count,
        ym2610_ssg_notes: song.ym2610_ssg_count,
        ym2608_chips: song.ym2608_ids.len(),
        ym2608_ext: song.ym2608_ext,
        ym2608_delta_t_samples: if pcm { song.ym2608_b_samples.len() } else { 0 },
        ym2608_rhythm_notes: song.ym2608_rhythm_notes,
        ym2608_delta_t_notes: song.ym2608_b_count,
        ym2608_ssg_notes: song.ym2608_ssg_count,
        scc_notes: song.scc_count,
        ay_notes: song.ay_count,
        saa_notes: song.saa_count,
        saa_chips: song.saa_ids.len(),
        ym2203_chips: song.ym2203_ids.len(),
        ym2203_ext: song.ym2203_ext,
        ym2203_ssg_notes: song.ym2203_ssg_count,
        psg_notes: song.psg_count,
        dac_notes: song.dac_count,
        dac_samples: song.dac_samples.len(),
        events: song.events.iter().filter(|e| e.on).count(),
        legato_steps: song.legato_steps,
        legato_ramps: song.legato_ramps,
        warnings,
        name: vgm.name(),
        peak,
        gain,
        loop_: loop_note,
        variants: None,
    })
}

/// The per-factor subfolder: `default/`, `x2 optimised/`, `x3 optimised/`.
pub fn _optimised_dir(factor: f64) -> String {
    if factor == 1.0 {
        "default".to_string()
    } else {
        format!("x{} optimised", fmt_g(factor, 6))
    }
}

pub fn _lossy_name(path: &Path) -> PathBuf {
    let stem = _stem(path);
    let suffix = path
        .extension()
        .map(|e| format!(".{}", e.to_string_lossy()))
        .unwrap_or_default();
    path.with_file_name(format!("{stem} (lossy){suffix}"))
}

pub fn _plain_name(path: &Path) -> PathBuf {
    let stem = _stem(path);
    let stem = stem.strip_suffix(" (lossy)").unwrap_or(&stem);
    let suffix = path
        .extension()
        .map(|e| format!(".{}", e.to_string_lossy()))
        .unwrap_or_default();
    path.with_file_name(format!("{stem}{suffix}"))
}

/// Add ` (lossy)` to each rejected variant and drop stale marked names.
pub fn _mark_lossy(results: &mut [VariantInfo]) -> Result<()> {
    for r in results.iter_mut() {
        if r.factor == 1.0 {
            continue;
        }
        let path = PathBuf::from(&r.dst);
        let marked = _lossy_name(&path);
        if r.lossless {
            _unlink_missing_ok(&marked)?;
            continue;
        }
        _replace_path(&path, &marked)?;
        r.dst = marked.to_string_lossy().into_owned();
    }
    Ok(())
}

/// The 1x..Nx ladder with half steps: 1, 1.5, 2, 2.5, ... N.
pub fn default_factors(condense: f64) -> Vec<f64> {
    let top = 1.0f64.max(condense);
    let mut out: Vec<f64> = Vec::new();
    let mut step: i64 = 1;
    while step as f64 <= top {
        out.push(step as f64);
        if step as f64 + 0.5 <= top {
            out.push(step as f64 + 0.5);
        }
        step += 1;
    }
    out
}

/// Move each kept factor into its subfolder; delete the ones that lost.
///
/// `keep` lists the factors to retain. 1x is always kept.
pub fn _place_dirs(results: &mut [VariantInfo], out_dir: &Path, keep: &[f64]) -> Result<()> {
    for r in results.iter_mut() {
        let path = PathBuf::from(&r.dst);
        if keep.iter().any(|k| *k == r.factor) {
            let dest = out_dir.join(_optimised_dir(r.factor)).join(_name(&path));
            if let Some(parent) = dest.parent() {
                if !parent.as_os_str().is_empty() {
                    fs::create_dir_all(parent)?;
                }
            }
            if path != dest {
                _replace_path(&path, &dest)?;
            }
            r.dst = dest.to_string_lossy().into_owned();
            if r.factor != 1.0 {
                // Drop the name this verdict no longer uses from an earlier run.
                let alternate = if _stem(&dest).ends_with(" (lossy)") {
                    _plain_name(&dest)
                } else {
                    _lossy_name(&dest)
                };
                _unlink_missing_ok(&alternate)?;
            }
        } else if path.exists() {
            let _ = fs::remove_file(&path);
        }
    }
    Ok(())
}

pub const _MANIFEST: &str = "condense.tsv";
pub const _MANIFEST_HEADER: &str = "track\tfactor\tfile\tbpm\tloss\tdev\tdiv\tkeys\tmiss\taudit_miss\tfields\ttl\tpsg_miss\tdac_miss\tlossless\tpick\tgrid\n";

pub fn _manifest_line(src: &Path, v: &VariantInfo) -> String {
    let div = match v.div_ms {
        None => String::new(),
        Some(value) => format_f(value, 1),
    };
    let audit = v.audit.as_ref().filter(|map| !map.is_empty());
    let fields: Option<i64> = audit.map(|map| {
        map.get("fields")
            .and_then(Value::as_object)
            .map(|fields| fields.values().filter_map(Value::as_i64).sum())
            .unwrap_or(0)
    });
    let audit_text = |key: &str| -> String {
        match audit.and_then(|map| map.get(key)) {
            Some(value) => _scalar_text(value),
            None => String::new(),
        }
    };
    [
        _stem(src),
        fmt_g(v.factor, 6),
        _name(&PathBuf::from(&v.dst)),
        format_f(v.bpm, 1),
        v.loss_total.to_string(),
        v.dev_max.to_string(),
        div,
        v.keyons.to_string(),
        v.missed.to_string(),
        audit_text("missed"),
        fields.map(|f| f.to_string()).unwrap_or_default(),
        audit_text("tl"),
        audit_text("psg_missed"),
        audit_text("dac_missed"),
        if v.lossless { "1" } else { "0" }.to_string(),
        if v.pick { "1" } else { "0" }.to_string(),
        v.grid.clone(),
    ]
    .join("\t")
        + "\n"
}

/// Write one .fur per condense factor and pick one.
pub fn convert_variants(
    path: &Path,
    out_dir: &Path,
    opts: &ConvertOptions,
    deps: &dyn ConvertDriver,
) -> Result<Vec<VariantInfo>> {
    let factors = opts
        .factors
        .clone()
        .unwrap_or_else(|| vec![1.0, 2.0, 3.0, 4.0]);
    _convert_variants_with_factors(path, out_dir, &factors, opts, deps)
}

fn _convert_variants_with_factors(
    path: &Path,
    out_dir: &Path,
    factors: &[f64],
    opts: &ConvertOptions,
    deps: &dyn ConvertDriver,
) -> Result<Vec<VariantInfo>> {
    let vgm = deps.load_vgm(path)?;
    let src = path.to_path_buf();
    let out_dir = out_dir.to_path_buf();
    let mut results: Vec<VariantInfo> = Vec::new();
    let mut shared_gain: Option<f64> = None;
    let mut grid_cache = Default::default();
    let mut base_opts = opts.analyze_options(1.0, Some(&mut grid_cache));
    // `convert_variants` has no tick_samples argument (Python omits it).
    base_opts.tick_samples = None;
    let mut base = deps.analyze(&vgm, base_opts)?;
    let loop_note = if opts.loop_find {
        deps.apply_loop(&mut base)
    } else {
        None
    };
    let mode = opts.optimize.as_str();
    let mut use_verify = false;
    for &factor in factors {
        let variant_loop;
        let fresh: Option<Song>;
        if factor == 1.0 {
            fresh = None;
            variant_loop = loop_note.clone();
        } else {
            let mut factor_opts = opts.analyze_options(factor, Some(&mut grid_cache));
            factor_opts.tick_samples = None;
            let mut new_song = deps.analyze(&vgm, factor_opts)?;
            // A condensed grid has longer rows. Re-seat the loop on those
            // rows and recalibrate them. Copying the 1x sample points would
            // floor the start one row early and stretch the loop by a row.
            variant_loop = if opts.loop_find {
                deps.apply_loop(&mut new_song)
            } else {
                None
            };
            fresh = Some(new_song);
        }
        let song: &Song = match &fresh {
            Some(song) => song,
            None => &base,
        };
        let mut warnings = song.warnings.clone();
        use_verify = opts.verify && mode != "all";
        if opts.verify && mode == "all" {
            warnings.push(
                "verification skipped: --optimize all keeps the largest factor".to_string(),
            );
        }
        if factor > 1.0 && song.grid_note.contains("low confidence") {
            // The row grid is ambiguous, but the user asked for the factor
            // anyway. Keep the structural loss counters as the only warning.
            warnings.push(format!("condensed x{}: low-confidence source grid", fmt_g(factor, 6)));
        }
        let mut stats: IndexMap<String, i64> = IndexMap::new();
        let name = if factor == 1.0 {
            format!("{}.fur", _stem(&src))
        } else {
            format!("{} x{}.fur", _stem(&src), fmt_g(factor, 6))
        };
        let out_path = out_dir.join(name);
        let (data, peak, gain) = _emit(
            song,
            &out_path,
            opts.pcm,
            opts.include_fm,
            opts.normalize,
            &mut warnings,
            &mut stats,
            shared_gain,
        )?;
        if shared_gain.is_none() && gain.is_some() {
            shared_gain = gain;
        }
        let row_ms = song.samples_per_row() / 44.1;
        // Furnace rejects a module over its one-byte instrument limit.
        let over_limit = warnings.iter().any(|w| w.contains("0xFF instrument limit"));
        let mut verify_failed = use_verify && over_limit;
        let mut verdict: Option<IndexMap<String, Value>> = None;
        if use_verify && over_limit {
            warnings.push(
                "verification skipped: the module exceeds Furnace's instrument limit".to_string(),
            );
        } else if use_verify {
            let min_run = 4.0f64.max(60.0f64.min(row_ms * 0.75));
            match deps.verify_runs(&vgm, &out_path, None, None, Some(min_run)) {
                Ok(value) => verdict = Some(value),
                Err(Error::Furnace(text)) => {
                    verify_failed = true;
                    warnings.push(format!("verification skipped: {text}"));
                }
                Err(other) => return Err(other),
            }
        }
        let div_ms = verdict
            .as_ref()
            .and_then(|v| v.get("div_ms"))
            .and_then(Value::as_f64);
        results.push(VariantInfo {
            factor,
            dst: out_path.to_string_lossy().into_owned(),
            bytes: data.len(),
            speed: song.speed,
            hz: song.hz,
            bpm: furnace_bpm(song),
            rows_per_s: song.hz / song.speed as f64,
            grid: song.grid_note.clone(),
            loss: stats.clone(),
            loss_total: loss_total(&stats),
            keyons: *stats.get("keyons").unwrap_or(&0),
            missed: keyon_loss(Some(&stats)),
            dev_max: dev_max(&stats),
            legato_steps: song.legato_steps,
            legato_ramps: song.legato_ramps,
            warnings,
            loop_: variant_loop,
            peak,
            gain,
            div_ms,
            audit: verdict,
            verify_failed,
            lossless: false,
            pick: false,
        });
    }
    // The variants sharing a measurement all report the track's peak.
    let measured_peak = results.iter().find(|r| r.peak.is_some()).and_then(|r| r.peak);
    for r in results.iter_mut() {
        if r.peak.is_none() {
            r.peak = measured_peak;
        }
    }
    let base_index = results
        .iter()
        .position(|r| r.factor == 1.0)
        .or(if results.is_empty() { None } else { Some(0) })
        .ok_or_else(|| Error::msg("list index out of range"))?;
    // Structural counters that change content and that the rendered pitch
    // comparison cannot see: a dropped note, a swallowed note-off, an evicted
    // sample or a lost effect. The sweep/fade counters describe the pitch
    // model, which --verify measures directly. Under verify they are advisory:
    // a factor whose render matches 1x should not be blocked by a clipped
    // sweep step the render shows is inaudible.
    let hard_keys = [
        "note_collision",
        "off_swallowed",
        "pcm_evicted",
        "pcm_unplaced",
        "fx_dropped",
        "orders_truncated",
    ];
    let tol = 6;
    let base_div = results[base_index].div_ms;
    let base_hard: i64 = hard_keys
        .iter()
        .map(|k| *results[base_index].loss.get(*k).unwrap_or(&0))
        .sum();
    let base_loss_total = results[base_index].loss_total;
    let base_dev_max = results[base_index].dev_max;
    let play_ms = (if vgm.loop_samples != 0 {
        vgm.loop_samples
    } else {
        vgm.total_samples
    }) as f64
        / 44.1;
    let div_cap = play_ms * DIVERGENCE_SHARE;

    let lossless = |r: &VariantInfo| -> bool {
        if use_verify {
            // An uninspected rendition is not lossless.
            if r.verify_failed {
                return false;
            }
            if hard_keys
                .iter()
                .map(|k| *r.loss.get(*k).unwrap_or(&0))
                .sum::<i64>()
                > base_hard
            {
                return false;
            }
            if let (Some(base_div), Some(div_ms)) = (base_div, r.div_ms) {
                // Divergent time scales with the length of the piece. The
                // slack is relative, with a floor for near-perfect baselines.
                if div_ms > base_div + 20.0f64.max(base_div * 0.25) {
                    return false;
                }
                if div_ms > div_cap {
                    return false;
                }
            }
            return true;
        }
        // Without a render the analytic counters are all there is.
        if r.loss_total > base_loss_total {
            return false;
        }
        if r.dev_max > base_dev_max + tol {
            return false;
        }
        true
    };
    for r in results.iter_mut() {
        r.lossless = lossless(r);
    }
    if mode == "all" {
        _mark_lossy(&mut results)?;
    }
    let ok: Vec<usize> = results
        .iter()
        .enumerate()
        .filter(|(_, r)| r.lossless)
        .map(|(i, _)| i)
        .collect();
    let pick_index: usize = if mode == "none" || ok.is_empty() {
        base_index
    } else if mode == "lossless" {
        let measured: Vec<usize> = ok
            .iter()
            .copied()
            .filter(|i| results[*i].div_ms.is_some())
            .collect();
        if !measured.is_empty() {
            // Divergent time is not monotone in the factor, and the largest
            // accepted factor can walk past its minimum. Pick the largest
            // factor still within a tenth of the best.
            let best = measured
                .iter()
                .map(|i| results[*i].div_ms.unwrap())
                .fold(f64::INFINITY, f64::min);
            let near: Vec<usize> = measured
                .into_iter()
                .filter(|i| results[*i].div_ms.unwrap() <= best + 20.0f64.max(best * 0.10))
                .collect();
            near.into_iter()
                .max_by(|a, b| {
                    results[*a]
                        .factor
                        .partial_cmp(&results[*b].factor)
                        .unwrap_or(std::cmp::Ordering::Equal)
                })
                .unwrap()
        } else {
            *ok.iter()
                .max_by(|a, b| {
                    results[**a]
                        .factor
                        .partial_cmp(&results[**b].factor)
                        .unwrap_or(std::cmp::Ordering::Equal)
                })
                .unwrap()
        }
    } else {
        (0..results.len())
            .max_by(|a, b| {
                results[*a]
                    .factor
                    .partial_cmp(&results[*b].factor)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .unwrap()
    };
    for (i, r) in results.iter_mut().enumerate() {
        r.pick = i == pick_index;
    }
    if opts.layout == "dirs" {
        // 1x is the canonical file and the reference every verdict is measured
        // against. Write it even when nothing else survives.
        let mut keep: Vec<f64> = if mode == "all" {
            results.iter().map(|r| r.factor).collect()
        } else {
            ok.iter().map(|i| results[*i].factor).collect()
        };
        keep.push(results[base_index].factor);
        _place_dirs(&mut results, &out_dir, &keep)?;
    }
    Ok(results)
}

/// Aligned one-line summaries of one `convert_variants` result set.
pub fn format_variant_lines(variants: &[VariantInfo], dest: &Path) -> Vec<String> {
    if variants.is_empty() {
        return Vec::new();
    }
    let mut factors: Vec<String> = Vec::new();
    let mut wheres: Vec<String> = Vec::new();
    for v in variants {
        let dst = PathBuf::from(&v.dst);
        let where_ = match dst.strip_prefix(dest) {
            Ok(rel) => rel.to_string_lossy().into_owned(),
            Err(_) => _name(&dst),
        };
        factors.push(format!("x{}", fmt_g(v.factor, 6)));
        wheres.push(where_);
    }
    let factor_w = factors.iter().map(|f| f.chars().count()).max().unwrap_or(0);
    let where_w = wheres.iter().map(|w| w.chars().count()).max().unwrap_or(0);
    let mut lines: Vec<String> = Vec::new();
    for ((factor, where_), v) in factors.iter().zip(wheres.iter()).zip(variants.iter()) {
        let mark = if v.pick { "  <= pick" } else { "" };
        let dropped = if v.lossless { "" } else { "  (dropped)" };
        let div = match v.div_ms {
            Some(value) => format!("  div {:>7}ms", format_f(value, 1)),
            None => String::new(),
        };
        let keep = format!("  keys {:>4} miss {:>3}", v.keyons, v.missed);
        let audit = v.audit.as_ref().filter(|map| !map.is_empty());
        let mut check = String::new();
        if let Some(audit) = audit {
            let matched = audit.get("matched").and_then(Value::as_i64).unwrap_or(0);
            let missed = audit.get("missed").and_then(Value::as_i64).unwrap_or(0);
            if matched + missed != 0 {
                let nfields: i64 = audit
                    .get("fields")
                    .and_then(Value::as_object)
                    .map(|fields| fields.values().filter_map(Value::as_i64).sum())
                    .unwrap_or(0);
                check.push_str(&format!(
                    "  aud {}/{} miss {} fld {}",
                    matched,
                    matched + missed,
                    missed,
                    nfields
                ));
            }
            let psg_missed = audit.get("psg_missed").and_then(Value::as_i64).unwrap_or(0);
            if psg_missed != 0 {
                check.push_str(&format!(" psgmiss {psg_missed}"));
            }
            let dac_matched = audit.get("dac_matched").and_then(Value::as_i64).unwrap_or(0);
            let dac_missed = audit.get("dac_missed").and_then(Value::as_i64).unwrap_or(0);
            let dac_total = dac_matched + dac_missed;
            if dac_total != 0 {
                check.push_str(&format!(" dac {dac_matched}/{dac_total}"));
            }
            if let Some(ym) = audit.get("ym2610") {
                for kind in ["a", "b"] {
                    let Some(d) = ym.get(kind) else { continue };
                    let mt = _json_i64(d, "matched");
                    let ms = _json_i64(d, "missed");
                    if mt + ms != 0 {
                        check.push_str(&format!(" {kind} {mt}/{}", mt + ms));
                        let nf: i64 = ["data", "length", "vol", "pan", "rate"]
                            .iter()
                            .map(|k| _json_i64(d, k))
                            .sum();
                        if nf != 0 {
                            check.push_str(&format!(" fld {nf}"));
                        }
                    }
                }
            }
            if let Some(opna) = audit.get("ym2608") {
                for (tag, kind) in [("rhy", "rhythm"), ("dt", "delta_t")] {
                    let Some(d) = opna.get(kind) else { continue };
                    let mt = _json_i64(d, "matched");
                    let ms = _json_i64(d, "missed");
                    if mt + ms != 0 {
                        check.push_str(&format!(" {tag} {mt}/{}", mt + ms));
                        let nf: i64 = ["data", "length", "vol", "pan", "rate"]
                            .iter()
                            .map(|k| _json_i64(d, k))
                            .sum();
                        if nf != 0 {
                            check.push_str(&format!(" fld {nf}"));
                        }
                    }
                }
            }
        }
        let bpm = format!("{:>7}", format_f(v.bpm, 0));
        lines.push(format!(
            "{factor:<factor_w$}  BPM {bpm}  {where_:<where_w$}  loss {:>4}{keep}{check}  dev {:>4}{div}{mark}{dropped}",
            v.loss_total, v.dev_max
        ));
    }
    lines
}

/// Worker configuration for the folder batch (Python's `_CFG` dict).
#[derive(Clone, Debug)]
pub struct WorkerCfg {
    pub out_dir: PathBuf,
    pub speed: Option<i32>,
    pub tick_hz: Option<f64>,
    pub pcm: bool,
    pub tick_samples: Option<f64>,
    pub min_row: Option<f64>,
    pub variants: bool,
    pub factors: Vec<f64>,
    pub optimize: String,
    pub include_fm: bool,
    pub normalize: bool,
    pub loop_find: bool,
    pub verify: bool,
    pub layout: String,
    pub c352_quad: bool,
    pub lfo_loop: bool,
    pub lossy_samples: bool,
}

impl WorkerCfg {
    fn to_options(&self) -> ConvertOptions {
        ConvertOptions {
            speed: self.speed,
            tick_hz: self.tick_hz,
            pcm: self.pcm,
            tick_samples: self.tick_samples,
            min_row: self.min_row,
            variants: self.variants,
            condense: 1.0,
            factors: Some(self.factors.clone()),
            optimize: self.optimize.clone(),
            include_fm: self.include_fm,
            normalize: self.normalize,
            loop_find: self.loop_find,
            verify: self.verify,
            layout: self.layout.clone(),
            c352_quad: self.c352_quad,
            lfo_loop: self.lfo_loop,
            lossy_samples: self.lossy_samples,
            workers: None,
        }
    }
}

/// Write the 1x/2x/3x/4x set and return the pick's info plus the set.
pub fn _variant_info(
    src: &Path,
    out_dir: &Path,
    factors: &[f64],
    opts: &ConvertOptions,
    deps: &dyn ConvertDriver,
) -> Result<ConvertInfo> {
    let results = _convert_variants_with_factors(src, out_dir, factors, opts, deps)?;
    let pick = results
        .iter()
        .find(|r| r.pick)
        .cloned()
        .ok_or_else(|| Error::msg("no variant was picked"))?;
    Ok(_info_from_variant(src, &pick, results))
}

fn _info_from_variant(
    src: &Path,
    pick: &VariantInfo,
    results: Vec<VariantInfo>,
) -> ConvertInfo {
    ConvertInfo {
        src: src.to_string_lossy().into_owned(),
        dst: pick.dst.clone(),
        bytes: pick.bytes,
        speed: pick.speed,
        hz: pick.hz,
        bpm: pick.bpm,
        rows_per_s: pick.rows_per_s,
        condense: pick.factor,
        loss: pick.loss.clone(),
        loss_total: pick.loss_total,
        keyons: pick.keyons,
        missed: pick.missed,
        dev_max: pick.dev_max,
        legato_steps: pick.legato_steps,
        legato_ramps: pick.legato_ramps,
        warnings: pick.warnings.clone(),
        name: _stem(src),
        peak: pick.peak,
        gain: pick.gain,
        loop_: pick.loop_.clone(),
        variants: Some(results),
        ..Default::default()
    }
}

/// Per-process worker initializer: lower this thread's priority.
pub fn _init_worker(_cfg: &WorkerCfg) {
    crate::priority::set_low_priority();
}

/// Convert one file with the process-global configuration.
pub fn _worker(
    path: &Path,
    cfg: &WorkerCfg,
    deps: &dyn ConvertDriver,
) -> Result<ConvertInfo> {
    if cfg.variants {
        return _variant_info(path, &cfg.out_dir, &cfg.factors, &cfg.to_options(), deps);
    }
    let mut opts = cfg.to_options();
    // Python's `_worker` does not forward tick_samples; keep that behavior.
    opts.tick_samples = None;
    convert_vgm(
        path,
        &cfg.out_dir.join(format!("{}.fur", _stem(path))),
        &opts,
        deps,
    )
}

fn _convert_serial(
    files: &[PathBuf],
    out_dir: &Path,
    factors: &[f64],
    opts: &ConvertOptions,
    deps: &dyn ConvertDriver,
    on_row: &mut dyn FnMut(ConvertRow) -> Result<()>,
) -> Result<()> {
    for f in files {
        let result = if opts.variants {
            _variant_info(f, out_dir, factors, opts, deps)
        } else {
            // `_convert_serial` calls `convert_vgm` without `condense`, so the
            // folder path always uses the 1x default; `condense` only shapes
            // the variant ladder.
            let mut conv_opts = opts.clone();
            conv_opts.condense = 1.0;
            convert_vgm(
                f,
                &out_dir.join(format!("{}.fur", _stem(f))),
                &conv_opts,
                deps,
            )
        };
        match result {
            Ok(info) => on_row(ConvertRow {
                src: f.clone(),
                info: Some(info),
                error: None,
            })?,
            Err(exc) => on_row(ConvertRow {
                src: f.clone(),
                info: None,
                error: Some(exc.to_string()),
            })?,
        }
    }
    Ok(())
}

fn _note(manifest: Option<&Path>, src: &Path, info: Option<&ConvertInfo>) -> Result<()> {
    let Some(manifest) = manifest else {
        return Ok(());
    };
    let Some(info) = info else {
        return Ok(());
    };
    let Some(variants) = info.variants.as_ref() else {
        return Ok(());
    };
    if variants.is_empty() {
        return Ok(());
    }
    use std::io::Write;
    let mut fh = fs::OpenOptions::new()
        .append(true)
        .create(true)
        .open(manifest)?;
    for v in variants {
        fh.write_all(_manifest_line(src, v).as_bytes())?;
    }
    Ok(())
}

/// Convert every VGM/VGZ to `out_dir/<name>.fur`.
///
/// Calls `on_row` as each file finishes, or with an error for a file that
/// raised. The rest still convert. With `variants`, each file is written as
/// the 1x..`condense`x set and `info` carries the whole set plus the pick;
/// `layout="dirs"` lands the kept factors in `default/` and `xN optimised/`
/// with a `condense.tsv` manifest.
pub fn convert_folder(
    files: &[PathBuf],
    out_dir: &Path,
    opts: &ConvertOptions,
    deps: &(dyn ConvertDriver + Sync),
    log: &mut dyn FnMut(&str),
    on_row: &mut dyn FnMut(ConvertRow),
) -> Result<()> {
    let outcome = _convert_folder_inner(files, out_dir, opts, deps, log, on_row);
    crate::furnace::restore_console();
    outcome
}

fn _convert_folder_inner(
    files: &[PathBuf],
    out_dir: &Path,
    opts: &ConvertOptions,
    deps: &(dyn ConvertDriver + Sync),
    log: &mut dyn FnMut(&str),
    on_row: &mut dyn FnMut(ConvertRow),
) -> Result<()> {
    if files.is_empty() {
        return Ok(());
    }
    let factor_list: Vec<f64> = if let Some(factors) = &opts.factors {
        factors.iter().map(|f| *f as f64).collect()
    } else if opts.variants {
        default_factors(opts.condense)
    } else {
        vec![1.0]
    };
    let manifest = if opts.variants && opts.layout == "dirs" {
        Some(out_dir.join(_MANIFEST))
    } else {
        None
    };
    if let Some(manifest) = manifest.as_deref() {
        if !manifest.exists() {
            if let Some(parent) = manifest.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::write(manifest, _MANIFEST_HEADER)?;
        }
    }

    let (n_workers, note) =
        crate::priority::resolve_workers(opts.workers.map(|w| w as i64), files.len() as i64);
    if n_workers <= 1 {
        let _low = crate::priority::low_priority();
        _convert_serial(files, out_dir, &factor_list, opts, deps, &mut |row| {
            _note(manifest.as_deref(), &row.src, row.info.as_ref())?;
            on_row(row);
            Ok(())
        })?;
        return Ok(());
    }

    log(&format!("workers: {n_workers} ({note})"));
    let cfg = WorkerCfg {
        out_dir: out_dir.to_path_buf(),
        speed: opts.speed,
        tick_hz: opts.tick_hz,
        pcm: opts.pcm,
        tick_samples: opts.tick_samples,
        min_row: opts.min_row,
        variants: opts.variants,
        factors: factor_list,
        optimize: opts.optimize.clone(),
        include_fm: opts.include_fm,
        normalize: opts.normalize,
        loop_find: opts.loop_find,
        verify: opts.verify,
        layout: opts.layout.clone(),
        c352_quad: opts.c352_quad,
        lfo_loop: opts.lfo_loop,
        lossy_samples: opts.lossy_samples,
    };
    let (tx, rx) = mpsc::channel::<(usize, Result<ConvertInfo>)>();
    let outcome = std::thread::scope(|scope| -> Result<()> {
        for (i, f) in files.iter().enumerate() {
            let tx = tx.clone();
            let cfg = &cfg;
            scope.spawn(move || {
                crate::priority::set_low_priority();
                let result = _worker(f, cfg, deps);
                let _ = tx.send((i, result));
            });
        }
        drop(tx);
        // `as_completed`: emit in the order workers finish, not input order.
        for (i, result) in rx {
            let src = files[i].clone();
            match result {
                Ok(info) => {
                    _note(manifest.as_deref(), &src, Some(&info))?;
                    on_row(ConvertRow {
                        src,
                        info: Some(info),
                        error: None,
                    });
                }
                Err(exc) => on_row(ConvertRow {
                    src,
                    info: None,
                    error: Some(exc.to_string()),
                }),
            }
        }
        Ok(())
    });
    // If a worker panicked, surface it the way Python's BrokenProcessPool
    // would: the batch fails rather than pretending the file succeeded.
    outcome
}

#[cfg(test)]
mod tests {
    use super::*;
    use indexmap::IndexMap;
    use serde_json::json;

    #[test]
    fn default_factors_half_steps_to_top() {
        assert_eq!(default_factors(4.0), vec![1.0, 1.5, 2.0, 2.5, 3.0, 3.5, 4.0]);
        assert_eq!(default_factors(1.0), vec![1.0]);
        assert_eq!(default_factors(0.25), vec![1.0]);
        assert_eq!(default_factors(3.2), vec![1.0, 1.5, 2.0, 2.5, 3.0]);
    }

    #[test]
    fn optimised_dir_and_lossy_names() {
        assert_eq!(_optimised_dir(1.0), "default");
        assert_eq!(_optimised_dir(2.0), "x2 optimised");
        assert_eq!(_optimised_dir(1.5), "x1.5 optimised");
        let p = Path::new("out/song.fur");
        assert_eq!(_lossy_name(p), Path::new("out/song (lossy).fur"));
        assert_eq!(_plain_name(p), p);
        assert_eq!(
            _plain_name(Path::new("out/song (lossy).fur")),
            Path::new("out/song.fur")
        );
    }

    #[test]
    fn track_number_and_iter_vgms() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("01 Alpha.vgm"), b"").unwrap();
        fs::write(dir.path().join("02 Beta.vgz"), b"").unwrap();
        fs::write(dir.path().join("notes.txt"), b"").unwrap();
        let files = iter_vgms(dir.path());
        assert_eq!(
            files.iter().map(|p| _name(p)).collect::<Vec<_>>(),
            vec!["01 Alpha.vgm", "02 Beta.vgz"]
        );
        assert_eq!(track_number(&files[0]), Some(1));
        assert_eq!(track_number(Path::new("no digits.vgm")), None);
        assert_eq!(iter_vgms(&files[0]), vec![files[0].clone()]);
        assert!(iter_vgms(Path::new("missing.vgm")).is_empty());
    }

    #[test]
    fn select_tracks_by_number_substring_and_error() {
        let files = vec![
            PathBuf::from("01 Alpha.vgm"),
            PathBuf::from("02 Beta.vgm"),
        ];
        let hit = select_tracks(&files, Some("2")).unwrap();
        assert_eq!(hit, vec![PathBuf::from("02 Beta.vgm")]);
        let hit = select_tracks(&files, Some("alph")).unwrap();
        assert_eq!(hit, vec![PathBuf::from("01 Alpha.vgm")]);
        assert_eq!(select_tracks(&files, None).unwrap(), files);
        let err = select_tracks(&files, Some("9")).unwrap_err().to_string();
        assert_eq!(
            err,
            "no track matching '9' (looked at: 01 Alpha.vgm, 02 Beta.vgm)"
        );
    }

    #[test]
    fn manifest_line_exact_columns() {
        let mut v = VariantInfo {
            factor: 2.0,
            dst: "out/song x2.fur".to_string(),
            bpm: 150.0,
            loss_total: 3,
            dev_max: 7,
            keyons: 42,
            missed: 1,
            grid: "16th".to_string(),
            lossless: true,
            pick: true,
            ..Default::default()
        };
        let mut fields = serde_json::Map::new();
        fields.insert("tl".to_string(), json!(2));
        fields.insert("dt".to_string(), json!(3));
        let mut audit = IndexMap::new();
        audit.insert("fields".to_string(), Value::Object(fields));
        audit.insert("missed".to_string(), json!(4));
        v.audit = Some(audit);
        assert_eq!(
            _manifest_line(Path::new("in/song.vgm"), &v),
            "song\t2\tsong x2.fur\t150.0\t3\t7\t\t42\t1\t4\t5\t\t\t\t1\t1\t16th\n"
        );
    }

    #[test]
    fn variant_lines_align_and_mark_pick() {
        let variants = vec![
            VariantInfo {
                factor: 1.0,
                dst: "out/song.fur".to_string(),
                bpm: 120.0,
                loss_total: 0,
                dev_max: 1,
                keyons: 10,
                missed: 0,
                lossless: true,
                pick: true,
                ..Default::default()
            },
            VariantInfo {
                factor: 2.0,
                dst: "out/song x2.fur".to_string(),
                bpm: 120.0,
                loss_total: 5,
                dev_max: 9,
                keyons: 10,
                missed: 2,
                div_ms: Some(12.5),
                ..Default::default()
            },
        ];
        let lines = format_variant_lines(&variants, Path::new("out"));
        assert_eq!(
            lines,
            vec![
                "x1  BPM     120  song.fur     loss    0  keys   10 miss   0  dev    1  <= pick"
                    .to_string(),
                "x2  BPM     120  song x2.fur  loss    5  keys   10 miss   2  dev    9  div    12.5ms  (dropped)"
                    .to_string(),
            ]
        );
    }
}
