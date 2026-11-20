//! Atari POKEY parsing, register model, collector and module emission.
//!
//! Port of `tests/test_vgm2151fur_pokey.py` at vgm2151fur a8edb6e.

mod common;
#[allow(unused_imports)]
use common::*;

use std::path::PathBuf;

use vgm2fur::analyze::{analyze, AnalyzeOptions};
use vgm2fur::furio::parse_fur;
use vgm2fur::furwrite::{
    write_fur, WriteOptions, CHIP_AY8910, CHIP_POKEY, INS_POKEY,
};
use vgm2fur::pitch::{module_clock, pitch_table};
use vgm2fur::pokey::{
    _furnace_wave, collect_pokey, furnace_written_register, pokey_note,
    pokey_value, POKEY_DEFAULT_CLOCK, POKEY_PITCH_MAX,
};
use vgm2fur::vgm::{chips_in_head, load_vgm, ChipWrite, VgmFile};

fn tmp_dir() -> PathBuf {
    let p = std::env::temp_dir().join("vgm2151fur_pokey");
    std::fs::create_dir_all(&p).expect("create temp dir");
    p
}

fn _vgm(writes: Vec<ChipWrite>) -> VgmFile {
    let mut v = vgm(writes);
    v.pokey_clock = POKEY_DEFAULT_CLOCK;
    v
}

fn _raw(cmds: &[u8], clock: u32) -> Vec<u8> {
    let mut buf = vec![0u8; 0xC0];
    buf[0..4].copy_from_slice(b"Vgm ");
    buf[8..12].copy_from_slice(&0x161u32.to_le_bytes());
    buf[0x18..0x1C].copy_from_slice(&44100u32.to_le_bytes());
    buf[0x34..0x38].copy_from_slice(&(0xC0u32 - 0x34).to_le_bytes());
    buf[0xB0..0xB4].copy_from_slice(&clock.to_le_bytes());
    buf.extend_from_slice(cmds);
    buf.push(0x66);
    let total = (buf.len() - 4) as u32;
    buf[4..8].copy_from_slice(&total.to_le_bytes());
    buf
}

fn _pokey(sample: i64, reg: u32, val: u32, chip_id: u8) -> ChipWrite {
    ChipWrite::new(sample, "pokey", chip_id, reg, val)
}

mod TestPokeyParsing {
    use super::*;

    #[test]
    fn test_clock_and_dual_register_select() {
        let raw = _raw(
            &[0xBB, 0x00, 0xAF, 0xBB, 0x85, 0x12],
            0x40000000 | POKEY_DEFAULT_CLOCK,
        );
        let path = tmp_dir().join("pokey_parse.vgm");
        std::fs::write(&path, &raw).expect("write temp");
        let v = load_vgm(&path).expect("load_vgm");
        std::fs::remove_file(&path).ok();
        assert_eq!(v.pokey_clock, POKEY_DEFAULT_CLOCK);
        assert!(v.pokey_dual);
        let writes: Vec<(u8, u32, u32)> = v
            .writes
            .iter()
            .filter(|w| w.chip == "pokey")
            .map(|w| (w.chip_id, w.reg, w.val))
            .collect();
        assert_eq!(writes, vec![(0, 0x00, 0xAF), (1, 0x05, 0x12)]);
    }

    #[test]
    fn test_header_census() {
        let raw = _raw(&[0x66], 0x40000000 | POKEY_DEFAULT_CLOCK);
        let kinds = chips_in_head(&raw);
        assert!(kinds.contains("pokey"));
        assert!(kinds.contains("pokey_dual"));
    }
}

mod TestPokeyPitch {
    use super::*;

    #[test]
    fn test_plain_high_and_15khz_round_trip() {
        let cases: [(i32, i32, i32, i32, Option<i32>); 4] = [
            (0x00, 0, 5, 100, None),
            (0x40, 0, 5, 100, None), // high clock voice 1
            (0x20, 2, 5, 100, None), // high clock voice 3
            (0x01, 2, 5, 200, None), // 15 kHz
        ];
        for (audctl, voice, wave, target, high) in cases {
            let t = pokey_value(target, audctl, voice, wave, false, high);
            assert_eq!(
                furnace_written_register(t, audctl, voice, wave, false),
                target,
                "audctl={audctl:?} voice={voice:?} wave={wave:?} target={target:?}"
            );
        }
    }

    #[test]
    fn test_joined_binary_register() {
        // Kalinka's joined 1+2 voice: AUDF1=0xA7, AUDF2=0x06, AUDCTL=0x50.
        let t = pokey_value(0xA7, 0x50, 0, 5, true, Some(0x06));
        assert_eq!(furnace_written_register(t, 0x50, 0, 5, true), 0x06A7);
        let (note, _e5) = pokey_note(POKEY_DEFAULT_CLOCK, t);
        assert!((0..=179).contains(&note));
    }

    #[test]
    fn test_waveform_snapping_round_trips() {
        for wave in [6, 7] {
            for (audctl, voice, joined) in [(0x00, 0, false), (0x40, 0, false)] {
                let t0 = pokey_value(80, audctl, voice, 5, joined, None);
                let target = furnace_written_register(t0, audctl, voice, wave, joined);
                let t = pokey_value(target & 0xFF, audctl, voice, wave, joined, None);
                assert_eq!(
                    furnace_written_register(t, audctl, voice, wave, joined),
                    target,
                    "wave={wave:?} audctl={audctl:?} voice={voice:?} target={target:?}"
                );
            }
        }
    }
}

mod TestPokeyCollector {
    use super::*;

    #[test]
    fn test_gate_and_volume_envelope() {
        let mut writes = vec![_pokey(0, 8, 0x00, 0), _pokey(0, 0, 100, 0), _pokey(0, 1, 0xAF, 0)];
        for k in 1..15i64 {
            writes.push(_pokey(735 * k, 1, 0xA0u32 | (15 - k) as u32, 0));
        }
        writes.push(_pokey(735 * 15, 1, 0x00, 0));
        let (hits, offs, audctl, waves, _warn) = collect_pokey(&_vgm(writes));
        assert_eq!(hits.len(), 1);
        let hit = &hits[0];
        assert_eq!((hit.voice, hit.vol), (0, 15));
        assert_eq!(hit.vol_pts.len(), 14);
        assert_eq!(hit.vol_pts[hit.vol_pts.len() - 1], (735 * 14, 1));
        assert_eq!(
            offs.iter().map(|o| o.sample_time).collect::<Vec<_>>(),
            vec![735 * 15]
        );
        assert_eq!(
            furnace_written_register(hit.value, 0, 0, 5, false),
            100
        );
        assert!(audctl.is_empty());
        assert_eq!(waves[0], (0, 0, 0, 5));
    }

    #[test]
    fn test_distortion_6_uses_long_poly4_wave() {
        // Distortion 6 must use Furnace wave 7: both write AUDC 6, but wave
        // 6's short snap table cannot represent AUDF 51 (it skips 51/52),
        // which changes the poly4 sampling phase.  The original modules use
        // wave 7.
        assert_eq!(_furnace_wave(6), 7);
        let writes = vec![
            _pokey(0, 8, 0x00, 0),
            _pokey(0, 2, 51, 0),
            _pokey(0, 3, 0xCF, 0),
            _pokey(735 * 4, 3, 0x00, 0),
        ];
        let (hits, _offs, _audctl, _waves, _warn) = collect_pokey(&_vgm(writes));
        assert_eq!(hits.len(), 1);
        let hit = &hits[0];
        assert_eq!(hit.wave, 7);
        assert_eq!(furnace_written_register(hit.value, 0, 1, 7, false), 51);
    }

    #[test]
    fn test_filter_control_keeps_silent_note() {
        // AUDCTL bit 2 filters channel 1 with channel 3.  Voice 3 is silent
        // (AUDC volume 0) but its AUDF still drives the filter, so it needs a
        // volume-0 note in the module.
        let writes = vec![
            _pokey(0, 4, 190, 0),
            _pokey(0, 5, 0xA0, 0),
            _pokey(0, 0, 192, 0),
            _pokey(0, 1, 0xAF, 0),
            _pokey(0, 8, 0x04, 0),
            _pokey(735 * 4, 8, 0x00, 0),
        ];
        let (hits, _offs, _audctl, _waves, _warn) = collect_pokey(&_vgm(writes));
        let control: Vec<_> = hits.iter().filter(|h| h.voice == 2).collect();
        assert_eq!(control.len(), 1);
        assert_eq!(control[0].vol, 0);
        assert_eq!(
            furnace_written_register(control[0].value, 0x04, 2, 5, false),
            190
        );
    }

    #[test]
    fn test_pitch_plateau_hits_the_exact_register() {
        // The exact note/E5 must land inside the transform's plateau, not on
        // its lower edge (which would write target-1).
        for (target, audctl, voice, wave) in [
            (192, 0x04, 0, 5),
            (51, 0x00, 1, 7),
            (102, 0x00, 1, 7),
            (190, 0x04, 2, 5),
        ] {
            let t = pokey_value(target, audctl, voice, wave, false, None);
            let (note, e5) = pokey_note(POKEY_DEFAULT_CLOCK, t);
            let value = pitch_table(
                module_clock(POKEY_DEFAULT_CLOCK as f64),
                1.0,
                POKEY_PITCH_MAX,
            )
            .value(note, e5 - 0x80);
            assert_eq!(
                furnace_written_register(value, audctl, voice, wave, false),
                target,
                "target={target:?} audctl={audctl:?} voice={voice:?} wave={wave:?}"
            );
        }
    }

    #[test]
    fn test_volume_rise_retriggers() {
        let writes = vec![
            _pokey(0, 0, 100, 0),
            _pokey(0, 1, 0xAF, 0),
            _pokey(2 * 735, 1, 0xA5, 0), // decayed? no, still loud: 15 -> 5 drop
            _pokey(3 * 735, 1, 0xAF, 0), // rise back to 15: new hit
            _pokey(4 * 735, 1, 0x00, 0),
        ];
        let (hits, offs, _audctl, _waves, _warn) = collect_pokey(&_vgm(writes));
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].sample_time, 0);
        assert_eq!(hits[1].sample_time, 3 * 735);
        assert_eq!(
            offs.iter().map(|o| o.sample_time).collect::<Vec<_>>(),
            vec![4 * 735]
        );
    }

    #[test]
    fn test_joined_pair_uses_high_audc() {
        let writes = vec![
            _pokey(0, 8, 0x50, 0),
            _pokey(0, 0, 0xA7, 0),
            _pokey(0, 2, 0x06, 0),
            _pokey(0, 3, 0xA5, 0), // AUDC2: vol 5, wave 5
            _pokey(600, 2, 0x07, 0), // high byte moves
            _pokey(1200, 3, 0x00, 0), // volume off
        ];
        let (hits, offs, _audctl, _waves, _warn) = collect_pokey(&_vgm(writes));
        assert_eq!(hits.len(), 1);
        let hit = &hits[0];
        assert_eq!(hit.voice, 0);
        assert_eq!(hit.vol, 5);
        assert_eq!(
            furnace_written_register(hit.value, 0x50, 0, 5, true),
            0x06A7
        );
        assert_eq!(hit.freq_pts.len(), 1);
        assert_eq!(hit.freq_pts[0].0, 600);
        assert_eq!(
            furnace_written_register(hit.freq_pts[0].1, 0x50, 0, 5, true),
            0x07A7
        );
        assert_eq!(
            offs.iter().map(|o| o.sample_time).collect::<Vec<_>>(),
            vec![1200]
        );
    }

    #[test]
    fn test_join_off_reopens_independent_voice() {
        let writes = vec![
            _pokey(0, 8, 0x50, 0),
            _pokey(0, 0, 0xA7, 0),
            _pokey(0, 2, 0x06, 0),
            _pokey(0, 1, 0x00, 0),
            _pokey(0, 3, 0xA5, 0),
            _pokey(1000, 8, 0x00, 0), // join off; AUDC1=0, AUDC2=5
            _pokey(2000, 3, 0x00, 0),
        ];
        let (hits, offs, _audctl, _waves, _warn) = collect_pokey(&_vgm(writes));
        assert!(hits.iter().any(|h| h.voice == 1));
        let conv: Vec<_> = hits.iter().filter(|h| h.voice == 1).collect();
        assert_eq!(conv.len(), 1);
        assert_eq!(conv[0].sample_time, 1000);
        assert_eq!(
            offs.iter().map(|o| o.sample_time).collect::<Vec<_>>(),
            vec![2000]
        );
    }

    #[test]
    fn test_dual_chip_ids() {
        let writes = vec![
            _pokey(0, 0, 100, 0),
            _pokey(0, 1, 0xAF, 0),
            _pokey(0, 0, 200, 1),
            _pokey(0, 1, 0xAF, 1),
            _pokey(735 * 10, 1, 0, 0),
            _pokey(735 * 10, 1, 0, 1),
        ];
        let (hits, _offs, _audctl, _waves, _warn) = collect_pokey(&_vgm(writes));
        let mut ids: Vec<i32> = hits.iter().map(|h| h.chip_id).collect();
        ids.sort();
        assert_eq!(ids, vec![0, 1]);
    }
}

mod TestPokeyWriter {
    use super::*;

    #[test]
    fn test_chip_instrument_and_register_round_trip() {
        let writes = vec![
            _pokey(0, 8, 0x00, 0),
            _pokey(0, 0, 100, 0),
            _pokey(0, 1, 0xAF, 0),
            _pokey(735 * 10, 1, 0x00, 0),
        ];
        let song = analyze(&_vgm(writes), AnalyzeOptions::default()).expect("analyze");
        assert_eq!(song.pokey_ids, vec![0]);
        assert_eq!(song.pokey_count, 1);
        let mod_ = parse_fur(&write_fur(
            &song,
            WriteOptions {
                include_fm: false,
                ..Default::default()
            },
        ))
        .expect("parse_fur");
        assert_eq!(mod_.chips, vec![CHIP_POKEY]);
        assert_eq!(mod_.n_ch, 4);
        let rows = mod_.patterns.get(&(0u16, 0u16)).expect("pattern 0/0");
        let note_row = rows.iter().find(|r| r.note >= 0).expect("note row");
        assert_eq!(note_row.note, 111);
        assert!(note_row.fx.iter().any(|(cmd, _val)| *cmd == 0x10));
        let e5 = note_row
            .fx
            .iter()
            .find(|(cmd, _val)| *cmd == 0xE5)
            .map(|(_cmd, val)| *val)
            .unwrap_or(0x80);
        assert_eq!(
            furnace_written_register(
                pitch_table(
                    module_clock(POKEY_DEFAULT_CLOCK as f64),
                    1.0,
                    POKEY_PITCH_MAX,
                )
                .value(note_row.note, e5 as i32 - 0x80),
                0,
                0,
                5,
                false,
            ),
            100
        );
        assert_eq!(rows.iter().find(|r| r.note == 180).expect("off row").note, 180);
    }

    #[test]
    fn test_dual_header_keeps_silent_second_chip() {
        let writes = vec![
            _pokey(0, 0, 100, 0),
            _pokey(0, 1, 0xAF, 0),
            _pokey(735 * 4, 1, 0x00, 0),
        ];
        let mut v = _vgm(writes);
        v.pokey_dual = true;
        let song = analyze(&v, AnalyzeOptions::default()).expect("analyze");
        assert_eq!(song.pokey_ids, vec![0, 1]);
        let mod_ = parse_fur(&write_fur(
            &song,
            WriteOptions {
                include_fm: false,
                ..Default::default()
            },
        ))
        .expect("parse_fur");
        assert_eq!(mod_.chips, vec![CHIP_POKEY, CHIP_POKEY]);
        assert_eq!(mod_.n_ch, 8);
    }

    #[test]
    fn test_dual_chip_module_layout() {
        let writes = vec![
            _pokey(0, 0, 100, 0),
            _pokey(0, 1, 0xAF, 0),
            _pokey(0, 0, 200, 1),
            _pokey(0, 1, 0xAF, 1),
            _pokey(735 * 4, 1, 0, 0),
            _pokey(735 * 4, 1, 0, 1),
        ];
        let song = analyze(&_vgm(writes), AnalyzeOptions::default()).expect("analyze");
        assert_eq!(song.pokey_ids, vec![0, 1]);
        let mod_ = parse_fur(&write_fur(
            &song,
            WriteOptions {
                include_fm: false,
                ..Default::default()
            },
        ))
        .expect("parse_fur");
        assert_eq!(mod_.chips, vec![CHIP_POKEY, CHIP_POKEY]);
        assert_eq!(mod_.n_ch, 8);
    }

    #[test]
    fn test_pokey_instrument_type() {
        assert_eq!(INS_POKEY, 20);
    }
}

mod TestGeneralDualHeader {
    use super::*;

    #[test]
    fn test_ay_dual_header_keeps_silent_second_chip() {
        let writes = vec![
            ChipWrite::new(0, "ay8910", 0, 7, 0x38),
            ChipWrite::new(0, "ay8910", 0, 0, 0xB0),
            ChipWrite::new(0, "ay8910", 0, 1, 0x00),
            ChipWrite::new(0, "ay8910", 0, 8, 0x0F),
            ChipWrite::new(735 * 4, "ay8910", 0, 8, 0x00),
        ];
        let mut v = _vgm(writes);
        v.ay8910_clock = POKEY_DEFAULT_CLOCK;
        v.ay8910_dual = true;
        let song = analyze(&v, AnalyzeOptions::default()).expect("analyze");
        assert_eq!(song.ay_ids, vec![0, 1]);
        let mod_ = parse_fur(&write_fur(
            &song,
            WriteOptions {
                include_fm: false,
                ..Default::default()
            },
        ))
        .expect("parse_fur");
        assert_eq!(mod_.chips, vec![CHIP_AY8910, CHIP_AY8910]);
        assert_eq!(mod_.n_ch, 6);
    }
}
