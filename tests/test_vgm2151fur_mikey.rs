//! Atari Lynx Mikey VGM parsing and Furnace conversion.
//!
//! Port of `tests/test_vgm2151fur_mikey.py`.

#![allow(non_snake_case)]

mod common;
use common::*;

use std::path::PathBuf;

use vgm2fur::analyze::{analyze, AnalyzeOptions};
use vgm2fur::furio::parse_fur;
use vgm2fur::furwrite::{write_fur, WriteOptions};
use vgm2fur::mikey::{
    collect_mikey, mikey_duty, mikey_lfsr, mikey_period, MIKEY_DEFAULT_CLOCK,
};
use vgm2fur::vgm::{load_vgm, ChipWrite, VgmFile};

fn tmp_dir() -> PathBuf {
    let p = std::env::temp_dir().join("vgm2151fur_mikey");
    std::fs::create_dir_all(&p).expect("temp dir");
    p
}

fn _write(sample: i64, reg: u32, value: u32) -> ChipWrite {
    ChipWrite::new(sample, "mikey", 0, reg, value & 0xFF)
}

fn _vgm(writes: Vec<ChipWrite>, total: i64) -> VgmFile {
    VgmFile {
        path: PathBuf::from("synthetic.vgm"),
        buf: Vec::new(),
        version: 0x172,
        total_samples: total,
        loop_samples: 0,
        loop_file_pos: None,
        loop_sample: None,
        ym2151_clock: 0,
        k007232_clock: 0,
        gd3: Default::default(),
        writes,
        mikey_clock: MIKEY_DEFAULT_CLOCK,
        ..Default::default()
    }
}

fn _tone_writes(volume: i32, backup: i32, control: i32, feedback: i32, off: Option<i64>) -> Vec<ChipWrite> {
    let mut writes = vec![
        _write(0, 0x20, volume as u32),
        _write(0, 0x21, feedback as u32),
        _write(0, 0x23, 0),
        _write(0, 0x24, backup as u32),
        _write(0, 0x25, control as u32),
        _write(0, 0x27, 0),
    ];
    if let Some(off) = off {
        writes.push(_write(off, 0x20, 0));
    }
    writes
}

mod TestMikeyHelpers {
    use super::*;

    #[test]
    fn test_duty_mask() {
        assert_eq!(mikey_duty(0x48, 0x39), 0x288);
        assert_eq!(mikey_duty(0, 0), 0);
    }

    #[test]
    fn test_lfsr_seed() {
        assert_eq!(mikey_lfsr(0x34, 0xA0), 0xA34);
    }

    #[test]
    fn test_period() {
        assert_eq!(mikey_period(230, 0x39), 462);
    }
}

mod TestMikeyVgm {
    use super::*;

    #[test]
    fn test_header_clock_and_command() {
        let mut buf = vec![0u8; 0x105];
        buf[0..4].copy_from_slice(b"Vgm ");
        buf[0x04..0x08].copy_from_slice(&(0x100u32 + 4 + 5).to_le_bytes());
        buf[0x08..0x0C].copy_from_slice(&0x172u32.to_le_bytes());
        buf[0x18..0x1C].copy_from_slice(&44_100u32.to_le_bytes());
        buf[0x34..0x38].copy_from_slice(&0xCCu32.to_le_bytes());
        buf[0xE4..0xE8].copy_from_slice(&MIKEY_DEFAULT_CLOCK.to_le_bytes());
        buf[0x100..0x105].copy_from_slice(&[0x40, 0x20, 0x5A, 0x66, 0x00]);
        let path = temp_file(&tmp_dir(), "mikey.vgm", &buf);
        let vgm = load_vgm(&path).expect("load");
        assert_eq!(vgm.mikey_clock, MIKEY_DEFAULT_CLOCK);
        assert_eq!(vgm.writes[0].chip, "mikey");
        assert_eq!(vgm.writes[0].reg, 0x20);
        assert_eq!(vgm.writes[0].val, 0x5A);
    }
}

mod TestMikeyCollector {
    use super::*;

    #[test]
    fn test_lfsr_note() {
        let (hits, _offs, samples, warnings) =
            collect_mikey(&_vgm(_tone_writes(90, 230, 0x39, 0x48, Some(8820)), 44_100), false);
        assert_eq!(hits.len(), 1);
        assert_eq!(samples.len(), 0);
        assert_eq!(warnings, Vec::<String>::new());
        let hit = &hits[0];
        assert_eq!(hit.value, 462);
        assert_eq!(hit.vol, 90);
        assert_eq!(hit.duty, 0x288);
        assert_eq!(hit.pan, 0xFF);
        assert_eq!(hit.off_sample, 8820);
    }

    #[test]
    fn test_volume_and_pitch_motion() {
        let mut writes = _tone_writes(90, 230, 0x39, 0x48, None);
        writes.push(_write(882, 0x20, 78));
        writes.push(_write(1764, 0x24, 162));
        let (hits, _offs, _samples, _warnings) =
            collect_mikey(&_vgm(writes, 44_100), false);
        let hit = &hits[0];
        assert_eq!(hit.vol_pts[0], (882, 78));
        assert_eq!(hit.freq_pts[0], (1764, 326));
    }

    #[test]
    fn test_full_setup_before_volume_splits_note() {
        let writes = vec![
            _write(0, 0x21, 0x84),
            _write(0, 0x24, 230),
            _write(0, 0x25, 0x38),
            _write(5, 0x20, 40),
            _write(5000, 0x21, 7),
            _write(5000, 0x24, 244),
            _write(5000, 0x25, 0x19),
            _write(5005, 0x20, 48),
            _write(10000, 0x20, 0),
        ];
        let (hits, _offs, _samples, _warnings) =
            collect_mikey(&_vgm(writes, 12_000), false);
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].off_sample, 5005);
        assert_eq!(hits[1].sample_time, 5005);
        assert_eq!(hits[0].duty, 0x304);
        assert_eq!(hits[1].duty, 7);
    }

    #[test]
    fn test_attack_settle_updates_initial_duty() {
        let writes = vec![
            _write(0, 0x20, 40),
            _write(5, 0x21, 7),
            _write(5, 0x25, 0x19),
            _write(9000, 0x20, 0),
        ];
        let (hits, _offs, _samples, _warnings) =
            collect_mikey(&_vgm(writes, 10_000), false);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].duty, 7);
        assert_eq!(hits[0].duty_pts, Vec::<(i64, i32)>::new());
    }

    #[test]
    fn test_pan_register() {
        let mut writes = _tone_writes(90, 230, 0x39, 0x48, None);
        writes.push(_write(0, 0x40, 0x3C)); // left 3, right 12
        writes.push(_write(0, 0x44, 0x00)); // pan bits clear: full scale
        writes.push(_write(0, 0x50, 0x00));
        let (hits, _offs, _samples, _warnings) =
            collect_mikey(&_vgm(writes, 44_100), false);
        assert_eq!(hits[0].pan, 0xFF);
    }

    #[test]
    fn test_dac_stream() {
        let mut writes = Vec::new();
        for i in 0..1900i32 {
            let value = (64.0 + 60.0 * (i as f64 / 20.0).sin()) as i64;
            writes.push(_write(i as i64 * 6, 0x22, (value & 0xFF) as u32));
        }
        let (_hits, _offs, samples, warnings) =
            collect_mikey(&_vgm(writes, 44_100), false);
        assert!(!samples.is_empty());
        assert_eq!(warnings, Vec::<String>::new());
        assert_eq!(samples[0].rate, 7350);
        assert_eq!(samples[0].data.len(), 1837);
    }
}

mod TestMikeyModule {
    use super::*;

    #[test]
    fn test_tone_module() {
        let song = analyze(
            &_vgm(_tone_writes(90, 230, 0x39, 0x48, Some(8820)), 44_100),
            AnalyzeOptions::default(),
        )
        .expect("analyze");
        assert_eq!(song.mikey_ids, vec![0]);
        assert_eq!(song.mikey_count, 1);
        assert!(!song.mikey_ins.is_empty());
        let ins = &song.mikey_ins[0];
        assert_eq!(ins.duty, vec![0x288]);
        let mod_ = parse_fur(&write_fur(
            &song,
            WriteOptions { include_fm: false, ..Default::default() },
        ))
        .expect("parse fur");
        assert_eq!(mod_.chips, vec![0xA8]);
        assert_eq!(mod_.n_ch, 4);
        let mut notes = Vec::new();
        for ((ch, _pat), rows) in &mod_.patterns {
            if *ch == 0 {
                for row in rows {
                    if row.note >= 0 {
                        notes.push(row.note);
                    }
                }
            }
        }
        assert_eq!(notes[0], 109);
    }

    #[test]
    fn test_dac_module() {
        let writes: Vec<ChipWrite> = (0..240i64)
            .map(|i| _write(i * 6, 0x22, ((i * 17) & 0x7F) as u32))
            .collect();
        let song = analyze(&_vgm(writes, 8820), AnalyzeOptions::default()).expect("analyze");
        assert!(!song.mikey_samples.is_empty());
        let mod_ = parse_fur(&write_fur(
            &song,
            WriteOptions { include_fm: false, ..Default::default() },
        ))
        .expect("parse fur");
        assert_eq!(mod_.chips, vec![0xA8]);
        assert_eq!(mod_.n_ch, 4);
        assert!(!mod_.samples.is_empty());
        assert_eq!(mod_.samples[0].depth, 8);
        assert_eq!(mod_.samples[0].c4_rate, 7350);
    }
}
