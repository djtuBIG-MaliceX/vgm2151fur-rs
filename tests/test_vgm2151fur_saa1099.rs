//! Philips SAA1099: parsing, collection, pitch inversion and .fur emission.
//!
//! Port of `tests/test_vgm2151fur_saa1099.py`.

#![allow(non_snake_case)]

mod common;
use common::*;

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use vgm2fur::analyze::{_loop_saa_pitch_macros, analyze, AnalyzeOptions};
use vgm2fur::furio::{chip_channels, iter_blocks, parse_fur};
use vgm2fur::furwrite::{write_fur, WriteOptions, CHIP_SAA1099, INS_SAA};
use vgm2fur::model::ToneIns;
use vgm2fur::pitch::pitch_table;
use vgm2fur::saa1099::{
    saa_module_clock, saa_period_note, saa_period_value, saa_rate, collect_saa1099, SAA_VOICES,
};
use vgm2fur::vgm::{chips_in_head, load_vgm, ChipWrite, VgmFile};

fn tmp_dir() -> PathBuf {
    let p = std::env::temp_dir().join("vgm2151fur_saa1099");
    std::fs::create_dir_all(&p).expect("temp dir");
    p
}

fn build_vgm(writes: Vec<ChipWrite>, total_samples: i64) -> VgmFile {
    let mut v = vgm(writes);
    v.version = 0x171;
    v.total_samples = total_samples;
    v
}

fn raw_vgm_saa(body: &[u8], clock: u32, dual: bool) -> Vec<u8> {
    let mut buf = vec![0u8; 0xD0];
    buf[0..4].copy_from_slice(b"Vgm ");
    buf[8..12].copy_from_slice(&0x171u32.to_le_bytes());
    buf[0x18..0x1C].copy_from_slice(&44100u32.to_le_bytes());
    buf[0x34..0x38].copy_from_slice(&(0xD0u32 - 0x34).to_le_bytes());
    let raw = clock | if dual { 0x4000_0000 } else { 0 };
    buf[0xC8..0xCC].copy_from_slice(&raw.to_le_bytes());
    let mut payload = buf;
    payload.extend_from_slice(body);
    payload.push(0x66);
    let len = (payload.len() - 4) as u32;
    payload[4..8].copy_from_slice(&len.to_le_bytes());
    payload
}

/// Furnace SAA platform conversion: pitch-table value -> register pair.
fn platform_pair(value: i32) -> (i32, i32) {
    let k = if value <= 511 {
        0
    } else {
        (32 - (value as u32).leading_zeros() as i32) - 9
    };
    ((0xFF - (value >> k)) & 0xFF, 7 - k)
}

fn repeat(values: &[i32], n: usize) -> Vec<i32> {
    let mut out = Vec::with_capacity(values.len() * n);
    for _ in 0..n {
        out.extend_from_slice(values);
    }
    out
}

fn value_for(note: i32, e5: i32) -> i32 {
    pitch_table(saa_module_clock(8_000_000.0), 2.0, 0xFFFF)
        .value(note, e5 - 0x80)
}

mod TestSaaParsing {
    use super::*;

    #[test]
    fn test_bd_command_and_header() {
        let body = [
            0xBDu8, 0x1C, 0x01, 0xBD, 0x00, 0x88, 0xBD, 0x14, 0x01, 0xBD, 0x08, 0x40, 0xBD,
            0x10, 0x07, 0xBD, 0x80, 0x88, 0xBD, 0x94, 0x01,
        ];
        let p = tmp_dir().join("saa_header.vgm");
        std::fs::write(&p, raw_vgm_saa(&body, 8_000_000, true)).expect("write");
        let vgm = load_vgm(&p).expect("load");
        assert_eq!(vgm.saa1099_clock, 8_000_000);
        assert!(vgm.saa1099_dual);
        let writes: Vec<(u8, u32, u32)> = vgm
            .writes
            .iter()
            .map(|w| (w.chip_id, w.reg, w.val))
            .collect();
        assert!(writes.contains(&(0, 0x1C, 1)));
        assert!(writes.contains(&(0, 0x00, 0x88)));
        assert!(writes.contains(&(1, 0x00, 0x88)));
        assert!(writes.contains(&(1, 0x14, 1)));
        assert!(chips_in_head(&std::fs::read(&p).expect("read")).contains("saa1099"));
    }

    #[test]
    fn test_single_chip_header() {
        let p = tmp_dir().join("saa_single.vgm");
        std::fs::write(&p, raw_vgm_saa(&[0xBD, 0x00, 0x0F], 8_000_000, false)).expect("write");
        let vgm = load_vgm(&p).expect("load");
        assert_eq!(vgm.saa1099_clock, 8_000_000);
        assert!(!vgm.saa1099_dual);
    }
}

mod TestSaaPitch {
    use super::*;

    #[test]
    fn test_every_register_pair_round_trips() {
        for octave in 0..8 {
            for offset in 0..256 {
                let value = saa_period_value(offset, octave);
                let (note, e5) = saa_period_note(8_000_000.0, value);
                let table_value = value_for(note, e5);
                // The platform's truncation must land back on the source pair.
                assert_eq!(platform_pair(table_value), (offset, octave));
            }
        }
    }

    #[test]
    fn test_rate_formula() {
        // offset 255, octave 7 at 8 MHz is the top half-cycle rate.
        assert_almost_eq!(saa_rate(8_000_000.0, 255, 7), 7812.5, 3);
        assert_almost_eq!(saa_rate(8_000_000.0, 0, 0), 15625.0 / 511.0, 3);
    }
}

mod TestSaaPitchLoop {
    use super::*;

    #[test]
    fn test_periodic_macro_loops_with_other_macros() {
        let pattern = [0i32, 4, 8, 12, 8, 4];
        let ins = ToneIns {
            pitch: repeat(&pattern, 3),
            pitch_loop: 255,
            panl: vec![1],
            panr: vec![2],
            mode: vec![3],
            env: vec![0x8A],
            ..Default::default()
        };
        let mut bucket = vec![ins];
        _loop_saa_pitch_macros(&mut bucket, true);
        assert_eq!(bucket[0].pitch, pattern.to_vec());
        assert_eq!(bucket[0].pitch_loop, 0);
        // The other macros are untouched.
        assert_eq!(bucket[0].panl, vec![1]);
        assert_eq!(bucket[0].mode, vec![3]);
        assert_eq!(bucket[0].env, vec![0x8A]);
    }

    #[test]
    fn test_periodic_tail_keeps_its_lead_in() {
        let lead = [9i32];
        let pattern = [0i32, 4, 8, 12, 8, 4];
        let mut pitch = lead.to_vec();
        pitch.extend(repeat(&pattern, 3));
        let ins = ToneIns {
            pitch,
            pitch_loop: 255,
            ..Default::default()
        };
        let mut bucket = vec![ins];
        _loop_saa_pitch_macros(&mut bucket, true);
        let mut expected = lead.to_vec();
        expected.extend_from_slice(&pattern);
        assert_eq!(bucket[0].pitch, expected);
        assert_eq!(bucket[0].pitch_loop, 1);
    }
}

fn collect_song(writes: Vec<ChipWrite>, dual: bool) -> VgmFile {
    let mut v = build_vgm(writes, 400_000);
    v.saa1099_clock = 8_000_000;
    v.saa1099_dual = dual;
    v
}

mod TestSaaCollect {
    use super::*;

    #[test]
    fn test_tone_note_and_pan_nibbles() {
        let writes = vec![
            ChipWrite::new(0, "saa1099", 0, 0x1C, 1),
            ChipWrite::new(0, "saa1099", 0, 0x14, 1),
            ChipWrite::new(0, "saa1099", 0, 0x00, 0xF0),
            ChipWrite::new(0, "saa1099", 0, 0x08, 0),
            ChipWrite::new(0, "saa1099", 0, 0x10, 7),
            ChipWrite::new(1000, "saa1099", 0, 0x00, 0),
        ];
        let (hits, _offs, _warn) = collect_saa1099(&collect_song(writes, false));
        assert_eq!(hits.len(), 1);
        let h = &hits[0];
        assert_eq!((h.left, h.right), (0, 15));
        assert!(h.tone);
        assert!(!h.noise);
    }

    #[test]
    fn test_same_sample_writes_use_the_final_value() {
        let writes = vec![
            ChipWrite::new(0, "saa1099", 0, 0x1C, 1),
            ChipWrite::new(0, "saa1099", 0, 0x14, 1),
            ChipWrite::new(0, "saa1099", 0, 0x00, 0x88),
            ChipWrite::new(0, "saa1099", 0, 0x00, 0x44),
            ChipWrite::new(1000, "saa1099", 0, 0x00, 0),
        ];
        let (hits, _offs, _warn) = collect_saa1099(&collect_song(writes, false));
        assert_eq!(hits.len(), 1);
        assert_eq!((hits[0].left, hits[0].right), (4, 4));
    }

    #[test]
    fn test_noise_generator_and_mode() {
        let writes = vec![
            ChipWrite::new(0, "saa1099", 0, 0x1C, 1),
            ChipWrite::new(0, "saa1099", 0, 0x15, 0x04),
            ChipWrite::new(0, "saa1099", 0, 0x16, 0x02),
            ChipWrite::new(0, "saa1099", 0, 0x02, 0x0F),
            ChipWrite::new(0, "saa1099", 0, 0x0A, 0x40),
            ChipWrite::new(0, "saa1099", 0, 0x11, 0x07),
            ChipWrite::new(1000, "saa1099", 0, 0x02, 0),
        ];
        let (hits, _offs, _warn) = collect_saa1099(&collect_song(writes, false));
        assert_eq!(hits.len(), 1);
        let h = &hits[0];
        assert!(!h.tone);
        assert!(h.noise);
        assert_eq!(h.noise_freq, 2); // low nibble of 0x02 (group 0)
    }

    #[test]
    fn test_amplitude_retrigger() {
        let writes = vec![
            ChipWrite::new(0, "saa1099", 0, 0x1C, 1),
            ChipWrite::new(0, "saa1099", 0, 0x14, 1),
            ChipWrite::new(0, "saa1099", 0, 0x00, 0x99),
            ChipWrite::new(1000, "saa1099", 0, 0x00, 0),
            ChipWrite::new(2000, "saa1099", 0, 0x00, 0x99),
            ChipWrite::new(3000, "saa1099", 0, 0x00, 0),
        ];
        let (hits, _offs, _warn) = collect_saa1099(&collect_song(writes, false));
        assert_eq!(hits.len(), 2);
    }

    #[test]
    fn test_dual_chip_state_is_independent() {
        let writes = vec![
            ChipWrite::new(0, "saa1099", 0, 0x1C, 1),
            ChipWrite::new(0, "saa1099", 1, 0x1C, 1),
            ChipWrite::new(0, "saa1099", 0, 0x14, 1),
            ChipWrite::new(0, "saa1099", 1, 0x14, 1),
            ChipWrite::new(0, "saa1099", 0, 0x00, 0x0F),
            ChipWrite::new(0, "saa1099", 1, 0x00, 0xF0),
            ChipWrite::new(1000, "saa1099", 0, 0x00, 0),
            ChipWrite::new(1000, "saa1099", 1, 0x00, 0),
        ];
        let (hits, _offs, _warn) = collect_saa1099(&collect_song(writes, true));
        let ids: BTreeSet<u8> = hits.iter().map(|h| h.chip_id).collect();
        assert_eq!(ids, BTreeSet::from([0u8, 1]));
        let states: BTreeSet<(u8, i32, i32)> =
            hits.iter().map(|h| (h.chip_id, h.left, h.right)).collect();
        assert_eq!(states, BTreeSet::from([(0u8, 15, 0), (1, 0, 15)]));
    }

    #[test]
    fn test_envelope_source_gets_a_silent_note() {
        let writes = vec![
            ChipWrite::new(0, "saa1099", 0, 0x1C, 1),
            // Group 0 source voice is 1; leave its amplitude at zero but give
            // it a frequency and enable the internal envelope.
            ChipWrite::new(0, "saa1099", 0, 0x09, 0x40),
            ChipWrite::new(0, "saa1099", 0, 0x10, 0x07),
            ChipWrite::new(0, "saa1099", 0, 0x18, 0x8A),
            // Output voice 2 is audible.
            ChipWrite::new(0, "saa1099", 0, 0x14, 0x04),
            ChipWrite::new(0, "saa1099", 0, 0x02, 0xFF),
            ChipWrite::new(0, "saa1099", 0, 0x0A, 0x40),
            ChipWrite::new(0, "saa1099", 0, 0x11, 0x07),
            ChipWrite::new(1000, "saa1099", 0, 0x02, 0),
        ];
        let (hits, _offs, _warn) = collect_saa1099(&collect_song(writes, false));
        let hidden: Vec<_> = hits.iter().filter(|h| h.hidden).collect();
        let normal: Vec<_> = hits.iter().filter(|h| !h.hidden).collect();
        assert_eq!(hidden.len(), 1);
        assert_eq!(hidden[0].voice, 1);
        assert_eq!(normal.len(), 1);
        assert_eq!(normal[0].voice, 2);
        assert_eq!(normal[0].env, 0x8A);
    }
}

mod TestSaaFurEmission {
    use super::*;

    fn emission_song(dual: bool) -> vgm2fur::model::Song {
        let mut writes = vec![
            ChipWrite::new(0, "saa1099", 0, 0x1C, 1),
            ChipWrite::new(0, "saa1099", 0, 0x14, 1),
            ChipWrite::new(0, "saa1099", 0, 0x00, 0x88),
            ChipWrite::new(0, "saa1099", 0, 0x08, 0x40),
            ChipWrite::new(0, "saa1099", 0, 0x10, 0x07),
            ChipWrite::new(735, "saa1099", 0, 0x00, 0x44),
            ChipWrite::new(1470, "saa1099", 0, 0x00, 0),
        ];
        if dual {
            writes.extend([
                ChipWrite::new(0, "saa1099", 1, 0x1C, 1),
                ChipWrite::new(0, "saa1099", 1, 0x14, 1),
                ChipWrite::new(0, "saa1099", 1, 0x00, 0x0F),
                ChipWrite::new(0, "saa1099", 1, 0x08, 0x40),
                ChipWrite::new(0, "saa1099", 1, 0x10, 0x07),
                ChipWrite::new(1470, "saa1099", 1, 0x00, 0),
            ]);
        }
        let mut v = build_vgm(writes, 2205);
        v.saa1099_clock = 8_000_000;
        v.saa1099_dual = dual;
        analyze(&v, AnalyzeOptions::default()).expect("analyze")
    }

    fn write(song: &vgm2fur::model::Song) -> Vec<u8> {
        write_fur(
            song,
            WriteOptions {
                include_fm: false,
                include_pcm: false,
                ..Default::default()
            },
        )
    }

    #[test]
    fn test_single_chip_module() {
        let song = emission_song(false);
        assert_eq!(song.saa_ids, vec![0]);
        assert_eq!(song.saa_count, 1);
        assert_eq!(song.speed, 1);
        assert_almost_eq!(song.hz, 60.0);
        let data = write(&song);
        let mod_ = parse_fur(&data).expect("parse");
        assert_eq!(mod_.chips, vec![CHIP_SAA1099]);
        assert_eq!(chip_channels(CHIP_SAA1099), SAA_VOICES);
        assert_eq!(mod_.n_ch, SAA_VOICES);
        let ins_types: Vec<i32> = iter_blocks(&data)
            .expect("blocks")
            .into_iter()
            .filter(|b| b.magic == *b"INS2")
            .map(|b| u16::from_le_bytes([b.payload[2], b.payload[3]]) as i32)
            .collect();
        assert!(ins_types.contains(&INS_SAA));
        // The note's volume column is 15; the raw amplitude is in the pan
        // macros.
        let notes: Vec<_> = song.events.iter().filter(|e| e.on && e.ch >= 8).collect();
        assert_eq!(notes.len(), 1);
        // The level is a normalized volume macro and the note column carries
        // the segment peak; the balance is the note's 08xy state.
        assert_eq!(notes[0].vol, 8);
        assert_eq!(notes[0].pan, 0xFF);
        assert_eq!(song.saa_ins[notes[0].ins as usize].vol, vec![15, 8]);
    }

    #[test]
    fn test_dual_chip_module() {
        let song = emission_song(true);
        let data = write(&song);
        let mod_ = parse_fur(&data).expect("parse");
        assert_eq!(mod_.chips, vec![CHIP_SAA1099, CHIP_SAA1099]);
        assert_eq!(mod_.n_ch, 12);
    }

    #[test]
    fn test_furnace_round_trip() {
        skip_without_furnace!();
        let song = emission_song(true);
        let fur = tmp_dir().join("saa_roundtrip.fur");
        let out = tmp_dir().join("saa_roundtrip.vgm");
        std::fs::write(&fur, write(&song)).expect("write fur");
        vgm2fur::furnace::export_vgm(&fur, &out).expect("export");
        let vgm = load_vgm(&out).expect("load");
        assert_eq!(vgm.saa1099_clock, 8_000_000);
        assert!(vgm.saa1099_dual);
        let mut state: BTreeMap<(u8, u32), u32> = BTreeMap::new();
        for w in &vgm.writes {
            if w.sample <= 0 {
                state.insert((w.chip_id, w.reg), w.val);
            }
        }
        assert_eq!(state.get(&(0, 0x00)), Some(&0x88));
        assert_eq!(state.get(&(1, 0x00)), Some(&0x0F));
    }
}

