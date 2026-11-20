//! A synthetic YM2151 edge-case log through the converter and back.
//!
//! The driver keys only the last operator of a four-operator voice, leaving
//! the other three un-keyed. The converter has to mute those operators in the
//! instrument (they never start an envelope) while keeping the carrier level
//! that was written at the key-on. The test runs a self-contained log through
//! `convert_vgm`/`export_fur` and checks the exported register writes.
//!
//! Port of `tests/test_cbt_litmus.py` at vgm2151fur a8edb6e.

mod common;

use std::collections::HashSet;

use vgm2fur::convert::{convert_vgm, ConvertOptions, RealDriver};
use vgm2fur::diag::export_fur;
use vgm2fur::vgm::{load_vgm, VgmFile};

const CLOCK: u32 = 3_579_545;
const PULSE: i64 = 20_000;
const TOTAL_SAMPLES: i64 = 188_000;

/// `_last_tl`: the last value written to `reg` at or before `sample`.
fn last_tl(v: &VgmFile, sample: i64, reg: u32) -> Option<u32> {
    let mut last = None;
    for w in &v.writes {
        if w.sample > sample {
            break;
        }
        if w.chip == "ym2151" && w.reg == reg {
            last = Some(w.val);
        }
    }
    last
}

fn ym(reg: u32, val: u32) -> Vec<u8> {
    vec![0x54, (reg & 0xFF) as u8, (val & 0xFF) as u8]
}

fn wait(samples: i64) -> Vec<u8> {
    let mut out = vec![0x61];
    out.extend_from_slice(&(samples as u16).to_le_bytes());
    out
}

/// A loadable 1.61 VGM with a YM2151 clock and a command body.
fn raw_vgm_log(cmds: &[u8], total_samples: i64) -> Vec<u8> {
    let mut buf = vec![0u8; 0x40];
    buf[0..4].copy_from_slice(b"Vgm ");
    buf[8..12].copy_from_slice(&0x161u32.to_le_bytes());
    buf[0x18..0x1C].copy_from_slice(&(total_samples as u32).to_le_bytes());
    buf[0x30..0x34].copy_from_slice(&CLOCK.to_le_bytes());
    buf[0x34..0x38].copy_from_slice(&(0x40u32 - 0x34).to_le_bytes());
    let mut raw = buf;
    raw.extend_from_slice(cmds);
    raw.push(0x66);
    let len = (raw.len() - 4) as u32;
    raw[4..8].copy_from_slice(&len.to_le_bytes());
    raw
}

/// One algorithm-0 voice; only its last operator is keyed.
fn voice_setup(ch: u32, carrier_tl: u32) -> Vec<u8> {
    let mut out = Vec::new();
    for (reg, val) in [
        (0x20 | ch, 0xC0), // RL both, feedback 0, CON=0
        (0x60 | ch, 0x10), // op1 (un-keyed)
        (0x68 | ch, 0x12), // op2 (un-keyed)
        (0x70 | ch, 0x14), // op3 (un-keyed)
        (0x78 | ch, carrier_tl), // op4, the carrier
        (0x80 | ch, 0x1F),
        (0x88 | ch, 0x1F),
        (0x90 | ch, 0x1F),
        (0x98 | ch, 0x1F),
        (0xA0 | ch, 0x00),
        (0xA8 | ch, 0x00),
        (0xB0 | ch, 0x00),
        (0xB8 | ch, 0x00),
        (0xC0 | ch, 0x00),
        (0xC8 | ch, 0x00),
        (0xD0 | ch, 0x00),
        (0xD8 | ch, 0x00),
        (0xE0 | ch, 0x0F),
        (0xE8 | ch, 0x0F),
        (0xF0 | ch, 0x0F),
        (0xF8 | ch, 0x0F),
        (0x28 | ch, 0x40),
        (0x30 | ch, 0x00),
    ] {
        out.extend_from_slice(&ym(reg, val));
    }
    out
}

fn edge_case_log() -> Vec<u8> {
    let mut cmds = Vec::new();
    // A key-on heartbeat on the other channels gives the grid estimator a
    // regular pulse to work with.
    for ch in 1..8 {
        cmds.extend_from_slice(&ym(0x08, 0x78 | ch));
        cmds.extend_from_slice(&wait(2000));
        cmds.extend_from_slice(&ym(0x08, ch));
        cmds.extend_from_slice(&wait(2000));
    }
    // Eight held notes on channel 0, each keying only operator 4.
    for i in 0..8 {
        cmds.extend_from_slice(&voice_setup(0, 0x30 + i * 4));
        cmds.extend_from_slice(&ym(0x08, 0x40)); // mask bit for operator 4 only
        cmds.extend_from_slice(&wait(PULSE - 2000));
        cmds.extend_from_slice(&ym(0x08, 0));
        cmds.extend_from_slice(&wait(2000));
    }
    raw_vgm_log(&cmds, TOTAL_SAMPLES)
}

mod cbt_litmus_test {
    use super::*;

    #[test]
    fn test_carrier_tl_and_operator_flattening() {
        skip_without_furnace!();
        let td = tempfile::tempdir().unwrap();
        let src_path = td.path().join("cbt-edge.vgm");
        std::fs::write(&src_path, edge_case_log()).unwrap();
        let src = load_vgm(&src_path).expect("load_vgm");
        let keyons: Vec<(i64, u32)> = src
            .writes
            .iter()
            .filter(|w| {
                w.chip == "ym2151"
                    && w.reg == 0x08
                    && (w.val & 0x78) != 0
                    && (w.val & 7) == 0
            })
            .map(|w| (w.sample, w.val))
            .collect();
        assert!(keyons.len() >= 8);

        let fur = td.path().join("cbt-edge.fur");
        let vgm_path = td.path().join("cbt-edge-out.vgm");
        convert_vgm(&src_path, &fur, &ConvertOptions::default(), &RealDriver).expect("convert_vgm");
        export_fur(&fur, Some(&vgm_path)).expect("export_fur");
        let out = load_vgm(&vgm_path).expect("load exported vgm");
        let out_keys: Vec<i64> = out
            .writes
            .iter()
            .filter(|w| {
                w.chip == "ym2151"
                    && w.reg == 0x08
                    && (w.val & 0x78) != 0
                    && (w.val & 7) == 0
            })
            .map(|w| w.sample)
            .collect();
        let mut checked = 0;
        for &(sample, _val) in &keyons {
            if out_keys.is_empty() {
                break;
            }
            // Python `min` keeps the first element on ties.
            let near = *out_keys
                .iter()
                .min_by_key(|&&t| (t - sample).abs())
                .unwrap();
            if (near - sample).abs() > 2000 {
                continue;
            }
            checked += 1;
            for reg in [0x60u32, 0x68, 0x70] {
                assert_eq!(
                    last_tl(&out, near, reg),
                    Some(127),
                    "unkeyed operator {:#x} not muted at {:.3}s",
                    reg,
                    sample as f64 / 44100.0,
                );
            }
            assert_eq!(last_tl(&out, near, 0x78).is_some(), true);
        }
        let src_key_tls: HashSet<Option<u32>> = keyons
            .iter()
            .map(|&(sample, _v)| last_tl(&src, sample, 0x78))
            .collect();
        let out_tls: HashSet<u32> = out
            .writes
            .iter()
            .filter(|w| w.chip == "ym2151" && w.reg == 0x78)
            .map(|w| w.val)
            .collect();
        let missing: Vec<&Option<u32>> = src_key_tls
            .iter()
            .filter(|tl| !matches!(tl, Some(v) if out_tls.contains(v)))
            .collect();
        assert!(
            missing.is_empty(),
            "key-on carrier TL levels missing from the module: {:?}",
            missing
        );
        assert!(checked >= 8);
    }
}
