//! Measure a converted module and normalise it to a fixed peak headroom.
//!
//! Furnace applies the song master volume linearly to the whole mix, so the
//! level is one float and one render tells the truth: [`normalize_module`]
//! renders at a quarter volume (below any clipping), reads the peak, and writes
//! the gain that lands the track at -2.5 dBFS, probing 24 dB lower if the
//! render still clips.
//!
//! A build without the module's chips refuses it ("unrecognized system ID"),
//! which callers report as a skipped fit rather than measure silence.
//!
//! Port of `vgm2151fur/levels.py` (0.11.0).

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::error::{Error, Result};
use crate::{furio, furnace};

/// -2.5 dBFS: loudness first, 1.5 dB of headroom.
pub const TARGET_PEAK: f64 = 0.7498942093324559; // 10 ** (-2.5 / 20)
/// Quieter than this is a broken render, not a track.
pub const MAX_GAIN: f64 = 15.848931924611133; // 10 ** (24 / 20)
/// Furnace's song master volume is capped at 3.0.
pub const MAX_MASTER: f64 = 3.0;

/// Probe volumes, loudest first; step down when the render sits at the clamp.
pub const PROBES: (f64, f64, f64) = (0.25, 0.25 / 16.0, 0.25 / 256.0);

/// A peak this close to 1.0 may be a clamp, not the signal.
const CLIPPED: f64 = 0.98;

/// Module bytes with the master volume applied, the measured full-volume peak,
/// and the master volume that was written.
#[derive(Clone, Debug, PartialEq)]
pub struct Normalization {
    /// Module bytes with the master volume applied.
    pub data: Vec<u8>,
    /// Full-volume peak (linear, 1.0 = full scale).
    pub peak: f64,
    /// The master volume that was written.
    pub gain: f64,
}

/// Largest absolute sample of a rendered WAV (f32 or s16).
pub fn wav_peak(path: &Path) -> Result<f64> {
    let raw = fs::read(path).map_err(|exc| Error::io(path, exc))?;
    let file_name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let mut pos = 12usize;
    let mut fmt: Option<&[u8]> = None;
    let mut data: Option<&[u8]> = None;
    while pos + 8 <= raw.len() {
        let cid = &raw[pos..pos + 4];
        let size = u32::from_le_bytes(raw[pos + 4..pos + 8].try_into().unwrap()) as usize;
        let start = pos + 8;
        let end = start.saturating_add(size).min(raw.len());
        if cid == b"fmt " {
            fmt = Some(&raw[start..end]);
        } else if cid == b"data" {
            data = Some(&raw[start..end]);
        }
        pos += 8 + size + (size & 1);
    }
    let (Some(fmt), Some(data)) = (fmt, data) else {
        return Err(Error::msg(format!("{file_name}: no fmt/data chunk")));
    };
    if fmt.len() < 16 {
        return Err(Error::msg(format!("{file_name}: no fmt/data chunk")));
    }
    let audio_format = u16::from_le_bytes([fmt[0], fmt[1]]);
    let bits = u16::from_le_bytes([fmt[14], fmt[15]]);
    if audio_format == 3 && bits == 32 {
        let usable = data.len() - data.len() % 4;
        let mut max_sample: Option<f64> = None;
        let mut min_sample: Option<f64> = None;
        let mut off = 0;
        while off + 4 <= usable {
            let sample = f32::from_le_bytes(data[off..off + 4].try_into().unwrap()) as f64;
            max_sample = Some(match max_sample {
                Some(current) if current >= sample => current,
                _ => sample,
            });
            min_sample = Some(match min_sample {
                Some(current) if current <= sample => current,
                _ => sample,
            });
            off += 4;
        }
        return Ok(max_sample.unwrap_or(0.0).max(-min_sample.unwrap_or(0.0)));
    }
    if audio_format == 1 && bits == 16 {
        let usable = data.len() - data.len() % 2;
        let mut max_sample: Option<i32> = None;
        let mut min_sample: Option<i32> = None;
        let mut off = 0;
        while off + 2 <= usable {
            let sample = i16::from_le_bytes([data[off], data[off + 1]]) as i32;
            max_sample = Some(match max_sample {
                Some(current) if current >= sample => current,
                _ => sample,
            });
            min_sample = Some(match min_sample {
                Some(current) if current <= sample => current,
                _ => sample,
            });
            off += 2;
        }
        let peak = max_sample.unwrap_or(0).max(-min_sample.unwrap_or(0));
        return Ok(peak as f64 / 32768.0);
    }
    Err(Error::msg(format!(
        "{file_name}: unsupported WAV format {audio_format}/{bits}"
    )))
}

/// Render `fur` at its own volume and scan the peak, retrying once.
///
/// Concurrent Furnace consoles occasionally lose a race on the shared config
/// file and exit without rendering; the retry is for that transient.
fn _render_raw_peak(fur: &Path, wav: &Path) -> Result<f64> {
    match furnace::render_wav(fur, wav, 0, Some("f32")) {
        Ok(_) => {}
        Err(Error::Furnace(_)) => {
            match fs::remove_file(wav) {
                Ok(()) => {}
                Err(exc) if exc.kind() == std::io::ErrorKind::NotFound => {}
                Err(exc) => return Err(Error::io(wav, exc)),
            }
            furnace::render_wav(fur, wav, 0, Some("f32"))?;
        }
        Err(err) => return Err(err),
    }
    let raw = wav_peak(wav)?;
    fs::remove_file(wav)?;
    Ok(raw)
}

/// Render `data` and return it with the master volume set to -2.5 dBFS peak.
///
/// `None` when the render is silent, so callers keep the module untouched.
pub fn normalize_module(data: &[u8], name: &str) -> Result<Option<Normalization>> {
    let tmpdir = TempDir::new("vgm2151fur-norm-")?;
    let probe_fur = tmpdir.path.join(format!("{name}.fur"));
    let probe_wav = tmpdir.path.join(format!("{name}.f32.wav"));
    let mut peak = 0.0;
    for probe in [PROBES.0, PROBES.1, PROBES.2] {
        let patched = furio::set_master_volume(data, probe)?;
        fs::write(&probe_fur, patched).map_err(|exc| Error::io(&probe_fur, exc))?;
        // -loops 0 renders one pass: intro plus one loop body. A second pass
        // only repeats, and the peak of a repeat is the same peak.
        let raw = _render_raw_peak(&probe_fur, &probe_wav)?;
        peak = raw / probe;
        if raw < CLIPPED {
            break;
        }
    }
    drop(tmpdir);
    if peak <= 0.0 {
        return Ok(None);
    }
    let gain = MAX_GAIN.min(MAX_MASTER).min(TARGET_PEAK / peak);
    Ok(Some(Normalization {
        data: furio::set_master_volume(data, gain)?,
        peak,
        gain,
    }))
}

/// The peak in dBFS, or negative infinity for silence.
pub fn peak_dbfs(peak: f64) -> f64 {
    if peak > 0.0 {
        20.0 * peak.log10()
    } else {
        f64::NEG_INFINITY
    }
}

/// A temporary directory with the same cleanup contract as Python's
/// `tempfile.TemporaryDirectory(prefix=...)`.
struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new(prefix: &str) -> Result<TempDir> {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let base = std::env::temp_dir();
        let mut last_err = None;
        for _ in 0..100 {
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|dur| dur.as_nanos())
                .unwrap_or(0);
            let seq = NEXT.fetch_add(1, Ordering::Relaxed);
            let path = base.join(format!("{prefix}{}-{nanos:x}-{seq:x}", std::process::id()));
            match fs::create_dir(&path) {
                Ok(()) => return Ok(TempDir { path }),
                Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
                    last_err = Some(err);
                }
                Err(err) => return Err(Error::io(path, err)),
            }
        }
        Err(Error::io(
            base,
            last_err.unwrap_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::AlreadyExists,
                    "could not create temporary directory",
                )
            }),
        ))
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_wav(path: &Path, samples: &[f64], fmt: u16, bits: u16) {
        let data: Vec<u8> = if bits == 32 {
            samples
                .iter()
                .flat_map(|sample| (*sample as f32).to_le_bytes())
                .collect()
        } else {
            samples
                .iter()
                .flat_map(|sample| (*sample as i16).to_le_bytes())
                .collect()
        };
        let block = bits / 8 * 2;
        let mut out = Vec::new();
        out.extend_from_slice(b"RIFF");
        out.extend_from_slice(&(4u32 + 24 + 8 + data.len() as u32).to_le_bytes());
        out.extend_from_slice(b"WAVEfmt ");
        out.extend_from_slice(&16u32.to_le_bytes());
        out.extend_from_slice(&fmt.to_le_bytes());
        out.extend_from_slice(&2u16.to_le_bytes());
        out.extend_from_slice(&44100u32.to_le_bytes());
        out.extend_from_slice(&(44100u32 * block as u32).to_le_bytes());
        out.extend_from_slice(&block.to_le_bytes());
        out.extend_from_slice(&bits.to_le_bytes());
        out.extend_from_slice(b"data");
        out.extend_from_slice(&(data.len() as u32).to_le_bytes());
        out.extend_from_slice(&data);
        fs::write(path, out).unwrap();
    }

    #[test]
    fn f32_peak() {
        let dir = tempfile::tempdir().unwrap();
        let wav = dir.path().join("peak.f32.wav");
        write_wav(&wav, &[0.5, -1.25, 0.3], 3, 32);
        let peak = wav_peak(&wav).unwrap();
        assert!((peak - 1.25).abs() < 1e-6);
    }

    #[test]
    fn s16_peak() {
        let dir = tempfile::tempdir().unwrap();
        let wav = dir.path().join("peak.s16.wav");
        write_wav(&wav, &[-32768.0, 100.0, 0.0], 1, 16);
        let peak = wav_peak(&wav).unwrap();
        assert!((peak - 1.0).abs() < 1e-6);
    }

    #[test]
    fn peak_dbfs_at_target() {
        assert!((peak_dbfs(TARGET_PEAK) - -2.5).abs() < 1e-6);
        assert_eq!(peak_dbfs(0.0), f64::NEG_INFINITY);
    }

    #[test]
    fn unsupported_wav_format() {
        let dir = tempfile::tempdir().unwrap();
        let wav = dir.path().join("peak.s24.wav");
        write_wav(&wav, &[1.0], 1, 24);
        let err = wav_peak(&wav).unwrap_err();
        assert_eq!(err.to_string(), "peak.s24.wav: unsupported WAV format 1/24");
    }

    #[test]
    fn missing_chunks() {
        let dir = tempfile::tempdir().unwrap();
        let wav = dir.path().join("empty.wav");
        fs::write(&wav, b"RIFF0000WAVE").unwrap();
        let err = wav_peak(&wav).unwrap_err();
        assert_eq!(err.to_string(), "empty.wav: no fmt/data chunk");
    }
}
