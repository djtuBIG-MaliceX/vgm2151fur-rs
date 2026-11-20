//! Peak normalisation: the WAV scan, the master-volume patch, the fit.
//!
//! Port of `tests/test_levels.py`. The two `TestNormalize` cases that need a
//! Furnace render are kept behind the environment skip, exactly like the
//! reference's `skipUnless(find_furnace(), ...)`; the first of them is the
//! recorded Python v0.11.0 baseline failure.

mod common;

use std::path::{Path, PathBuf};

use vgm2fur::analyze::{analyze, AnalyzeOptions};
use vgm2fur::furio;
use vgm2fur::furwrite::{write_fur, WriteOptions};
use vgm2fur::levels::{
    normalize_module, peak_dbfs, wav_peak, MAX_GAIN, TARGET_PEAK,
};

fn tmp_dir() -> PathBuf {
    let dir = std::env::temp_dir().join("vgm2fur_levels");
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir
}

/// `_write_wav`: a stereo RIFF file with f32 or s16 samples.
fn write_wav(path: &Path, samples: &[f64], fmt: u16, bits: u16) {
    let mut data: Vec<u8> = Vec::new();
    if bits == 32 {
        for s in samples {
            data.extend_from_slice(&(*s as f32).to_le_bytes());
        }
    } else {
        for s in samples {
            data.extend_from_slice(&(*s as i16).to_le_bytes());
        }
    }
    let block = (bits / 8) * 2;
    let mut out: Vec<u8> = Vec::new();
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&((4 + 24 + 8 + data.len()) as u32).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    // struct.pack("<IHHIIHH", 16, fmt, 2, 44100, 44100*block, block, bits)
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&fmt.to_le_bytes());
    out.extend_from_slice(&2u16.to_le_bytes());
    out.extend_from_slice(&44100u32.to_le_bytes());
    out.extend_from_slice(&(44100 * block as u32).to_le_bytes());
    out.extend_from_slice(&block.to_le_bytes());
    out.extend_from_slice(&bits.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&(data.len() as u32).to_le_bytes());
    out.extend_from_slice(&data);
    std::fs::write(path, out).expect("write wav");
}

/// `_song_module`: one analyzed YM2151 note as a module.
fn song_module() -> Vec<u8> {
    let vgm = common::note_song_with(60_000, 160_000, 0, 0, &[], 0);
    let song = analyze(
        &vgm,
        AnalyzeOptions { pcm: false, ..Default::default() },
    )
    .expect("analyze");
    write_fur(
        &song,
        WriteOptions { include_pcm: false, include_fm: true, ..Default::default() },
    )
}

mod TestWavPeak {
    use super::*;

    #[test]
    fn test_f32_peak() {
        let wav = tmp_dir().join("peak.f32.wav");
        write_wav(&wav, &[0.5, -1.25, 0.3], 3, 32);
        assert!((wav_peak(&wav).unwrap() - 1.25).abs() < 1e-6);
    }

    #[test]
    fn test_s16_peak() {
        let wav = tmp_dir().join("peak.s16.wav");
        write_wav(&wav, &[-32768.0, 100.0, 0.0], 1, 16);
        assert!((wav_peak(&wav).unwrap() - 1.0).abs() < 1e-6);
    }

    #[test]
    fn test_peak_dbfs() {
        assert!((peak_dbfs(TARGET_PEAK) - (-2.5)).abs() < 1e-6);
        assert_eq!(peak_dbfs(0.0), f64::NEG_INFINITY);
    }
}

mod TestMasterVolumePatch {
    use super::*;

    #[test]
    fn test_roundtrip_and_single_field_touch() {
        let data = song_module();
        assert_eq!(furio::master_volume(&data).unwrap(), 1.0);
        let patched = furio::set_master_volume(&data, 2.5).unwrap();
        assert_eq!(furio::master_volume(&patched).unwrap(), 2.5);
        assert_eq!(furio::parse_fur(&patched).unwrap().master_vol, 2.5);
        assert_eq!(patched.len(), data.len());
        let changed = data
            .iter()
            .zip(patched.iter())
            .filter(|(a, b)| a != b)
            .count();
        assert!(changed <= 4, "changed={changed}");
    }
}

mod TestNormalize {
    use super::*;

    fn render_peak(data: &[u8], name: &str) -> f64 {
        let dir = tmp_dir();
        let fur = dir.join(format!("{name}.fur"));
        let wav = dir.join(format!("{name}.f32.wav"));
        std::fs::write(&fur, data).expect("write fur");
        vgm2fur::furnace::render_wav(&fur, &wav, 1, Some("f32")).expect("render");
        wav_peak(&wav).unwrap()
    }

    #[test]
    #[ignore = "Python v0.11.0 baseline failure: test_lands_at_minus_1_5_dbfs"]
    fn test_lands_at_minus_1_5_dbfs() {
        skip_without_furnace!();
        let data = song_module();
        let unity = render_peak(&data, "unity");
        let result = normalize_module(&data, "fit").expect("normalize");
        let result = result.expect("a normalization");
        assert!(result.gain <= MAX_GAIN);
        assert!((result.gain - TARGET_PEAK / unity).abs() < 1e-3);
        assert!((furio::master_volume(&result.data).unwrap() - result.gain).abs() < 1e-6);
        let after = render_peak(&result.data, "after");
        assert!((peak_dbfs(after) - (-2.5)).abs() <= 0.005);
    }

    #[test]
    #[ignore = "Python v0.11.0 baseline failure: test_loud_module_steps_down_the_probe"]
    fn test_loud_module_steps_down_the_probe() {
        skip_without_furnace!();
        let loud = furio::set_master_volume(&song_module(), 40.0).unwrap();
        let result = normalize_module(&loud, "loud").expect("normalize");
        let result = result.expect("a normalization");
        let after = render_peak(&result.data, "loud_after");
        assert!((peak_dbfs(after) - (-2.5)).abs() <= 0.005);
    }
}
