//! T6W28 (Neo Geo Pocket Color) and dual-chip VGM coverage.
//!
//! Port of `tests/test_vgm2151fur_t6w28.py`.

mod common;
use common::*;

use std::collections::HashSet;
use std::path::PathBuf;

use indexmap::IndexMap;

use vgm2fur::analyze::{analyze, AnalyzeOptions};
use vgm2fur::furio::{iter_blocks, parse_fur};
use vgm2fur::furwrite::{
    _build_patterns, write_fur, EmitHooks, WriteOptions, CHIP_K007232, CHIP_RF5C68,
    CHIP_SEGAPCM, CHIP_SMS, CHIP_T6W28, CHIP_YM2151, INS_T6W28, NOTE_OFF,
};
use vgm2fur::model::{NoteEvent, Song};
use vgm2fur::pitch::pitch_table;
use vgm2fur::sn76489::{collect_sn76489, t6w28_note, t6w28_noise_note};
use vgm2fur::vgm::{chips_in_head, load_vgm, ChipWrite, RomSlice, VgmFile};

const T6_CLOCK: u32 = 3_072_000;

fn tmp_dir() -> PathBuf {
    let p = std::env::temp_dir().join("vgm2151fur_t6w28");
    std::fs::create_dir_all(&p).expect("temp dir");
    p
}

fn raw_vgm_full(body: &[u8], clocks: &[(usize, u32)], version: u32, total: u32) -> Vec<u8> {
    let base = 0x100usize;
    let mut buf = vec![0u8; base];
    buf[0..4].copy_from_slice(b"Vgm ");
    buf[8..12].copy_from_slice(&version.to_le_bytes());
    buf[0x18..0x1C].copy_from_slice(&total.to_le_bytes());
    buf[0x34..0x38].copy_from_slice(&((base - 0x34) as u32).to_le_bytes());
    for &(off, value) in clocks {
        buf[off..off + 4].copy_from_slice(&(value & 0xFFFF_FFFF).to_le_bytes());
    }
    let mut out = buf;
    out.extend_from_slice(body);
    out.push(0x66);
    let len = (out.len() - 4) as u32;
    out[4..8].copy_from_slice(&len.to_le_bytes());
    out
}

fn raw_vgm(body: &[u8], clocks: &[(usize, u32)]) -> Vec<u8> {
    raw_vgm_full(body, clocks, 0x171, 100_000)
}

fn _vgm(writes: Vec<ChipWrite>) -> VgmFile {
    let mut v = VgmFile::default();
    v.path = PathBuf::from("synthetic.vgm");
    v.buf = Vec::new();
    v.version = 0x171;
    v.total_samples = 200_000;
    v.ym2151_clock = 0;
    v.k007232_clock = 0;
    v.roms = Vec::new();
    v.writes = writes;
    v
}

fn _left(t: i64, voice: u32, period: u32, atten: u32) -> Vec<ChipWrite> {
    vec![
        ChipWrite::new(t, "sn76489", 0, 0, 0x80 | (voice << 5) | (period & 0x0F)),
        ChipWrite::new(t, "sn76489", 0, 0, (period >> 4) & 0x3F),
        ChipWrite::new(t, "sn76489", 0, 0, 0x90 | (voice << 5) | atten),
    ]
}

fn _right(t: i64, voice: u32, atten: u32) -> Vec<ChipWrite> {
    vec![ChipWrite::new(t, "sn76489", 1, 0, 0x90 | (voice << 5) | atten)]
}

fn _close(t: i64, voice: u32) -> Vec<ChipWrite> {
    vec![
        ChipWrite::new(t, "sn76489", 0, 0, 0x90 | (voice << 5) | 15),
        ChipWrite::new(t, "sn76489", 1, 0, 0x90 | (voice << 5) | 15),
    ]
}

fn _instrument_types(data: &[u8]) -> Vec<i32> {
    let mut out = Vec::new();
    for block in iter_blocks(data).expect("fur blocks") {
        if &block.magic == b"INS2" && block.payload.len() >= 4 {
            out.push(
                u16::from_le_bytes([block.payload[2], block.payload[3]]) as i32,
            );
        }
    }
    out
}

fn contains_subslice(hay: &[u8], needle: &[u8]) -> bool {
    hay.windows(needle.len()).any(|w| w == needle)
}

mod TestT6W28Loader {
    use super::*;

    #[test]
    fn test_header_and_both_halves_parse() {
        let body = [
            0x50u8, 0x8F, 0x50, 0x01, 0x50, 0x96, 0x30, 0x9A, 0x30, 0xD0, 0x30, 0x02,
            0x30, 0xE7,
        ];
        let p = tmp_dir().join("t6_header.vgm");
        std::fs::write(&p, raw_vgm(&body, &[(0x0C, T6_CLOCK | 0xC0000000)])).expect("write");
        let vgm = load_vgm(&p).expect("load");
        assert!(vgm.sn76489_dual);
        assert!(vgm.sn76489_t6w28);
        assert_eq!(vgm.sn76489_clock, T6_CLOCK);
        let got: Vec<(u8, u32, u32)> = vgm
            .writes
            .iter()
            .map(|w| (w.chip_id, w.reg, w.val))
            .collect();
        assert_eq!(
            got,
            vec![
                (0, 0, 0x8F),
                (0, 0, 0x01),
                (0, 0, 0x96),
                (1, 0, 0x9A),
                (1, 0, 0xD0),
                (1, 0, 0x02),
                (1, 0, 0xE7),
            ]
        );
        let kinds = chips_in_head(&std::fs::read(&p).expect("read"));
        assert!(kinds.contains("t6w28"));
        assert!(!kinds.contains("sn76489"));
        assert!(!kinds.contains("sn76489_dual"));
    }

    #[test]
    fn test_second_stream_without_the_dual_bit_is_reported() {
        let p = tmp_dir().join("t6_stray.vgm");
        std::fs::write(&p, raw_vgm(&[0x30, 0x9A], &[(0x0C, T6_CLOCK)])).expect("write");
        let vgm = load_vgm(&p).expect("load");
        assert!(!vgm.sn76489_dual);
        assert!(!vgm.sn76489_t6w28);
        let got: Vec<(u8, u32)> = vgm.writes.iter().map(|w| (w.chip_id, w.val)).collect();
        assert_eq!(got, vec![(1, 0x9A)]);
        assert!(vgm
            .warnings
            .iter()
            .any(|w| w.contains("without the dual-instance")));
    }

    #[test]
    fn test_t6w28_bit_without_dual_bit_is_mono_with_warning() {
        let p = tmp_dir().join("t6_half.vgm");
        std::fs::write(
            &p,
            raw_vgm(&[0x50, 0x9F], &[(0x0C, T6_CLOCK | 0x80000000)]),
        )
        .expect("write");
        let vgm = load_vgm(&p).expect("load");
        assert!(!vgm.sn76489_dual);
        assert!(!vgm.sn76489_t6w28);
        assert!(vgm.warnings.iter().any(|w| w.contains("T6W28 flag")));
    }
}

mod TestT6W28Collector {
    use super::*;

    #[test]
    fn test_independent_side_levels_become_volume_and_pan() {
        let mut writes = _left(0, 0, 31, 6);
        writes.extend(_right(0, 0, 10));
        writes.extend(_close(1000, 0));
        let mut v = _vgm(writes);
        v.sn76489_clock = T6_CLOCK;
        v.sn76489_t6w28 = true;
        v.sn76489_dual = true;
        let (hits, offs, warn) = collect_sn76489(&v, None);
        assert!(warn.is_empty());
        assert_eq!(hits.len(), 1);
        let hit = &hits[0];
        assert_eq!((hit.voice, hit.period, hit.vol), (0, 31, 9));
        // min attenuation 6: left full, right 11/15 (value 0xFB).
        assert_eq!(hit.pan, 0xFB);
        assert!(hit.t6w28);
        assert_eq!((offs[0].sample_time, offs[0].voice), (1000, 0));
    }

    #[test]
    fn test_side_volume_change_while_held_is_a_mix_point() {
        let mut writes = _left(0, 0, 31, 6);
        writes.extend(_right(0, 0, 10));
        writes.push(ChipWrite::new(500, "sn76489", 1, 0, 0x9F)); // right off
        writes.extend(_close(1000, 0));
        let mut v = _vgm(writes);
        v.sn76489_clock = T6_CLOCK;
        v.sn76489_t6w28 = true;
        v.sn76489_dual = true;
        let (hits, _offs, _warn) = collect_sn76489(&v, None);
        let hit = &hits[0];
        // The loud side is 9/15, so its split is 15; the now-silent right side
        // needs register attenuation 15, which is pan nibble 6 against that
        // 9-level column.
        assert_eq!(hit.mix_pts, vec![(500, 9, 0xF6)]);
    }

    #[test]
    fn test_noise_select_and_extra_period_come_from_the_right_half() {
        let mut writes = _left(0, 0, 31, 6); // tone engine 1
        writes.push(ChipWrite::new(0, "sn76489", 0, 0, 0xF6)); // left noise
        writes.push(ChipWrite::new(0, "sn76489", 1, 0, 0xFA)); // right noise
        writes.push(ChipWrite::new(0, "sn76489", 1, 0, 0xE0)); // fixed rate 0
        writes.push(ChipWrite::new(300, "sn76489", 1, 0, 0xC0)); // extra low
        writes.push(ChipWrite::new(300, "sn76489", 1, 0, 0x20)); // extra high
        writes.push(ChipWrite::new(300, "sn76489", 1, 0, 0xE7)); // select 3 white
        writes.push(ChipWrite::new(900, "sn76489", 0, 0, 0xFF));
        writes.push(ChipWrite::new(900, "sn76489", 1, 0, 0xFF));
        let mut v = _vgm(writes);
        v.sn76489_clock = T6_CLOCK;
        v.sn76489_t6w28 = true;
        v.sn76489_dual = true;
        let (hits, _offs, _warn) = collect_sn76489(&v, None);
        let noise = hits.iter().find(|h| h.noise).expect("noise hit");
        assert_eq!(noise.period, 0x100);
        assert_eq!(noise.noise_mode, 0);
        assert_eq!(noise.vol, 9);
        assert_eq!(noise.pan, 0xFB);
        assert!(noise.pitch_pts.contains(&(300, 0x200)));
        assert!(noise.mode_pts.contains(&(300, 4)));
    }

    #[test]
    fn test_note_round_trips_through_the_t6w28_pitch_tables() {
        let tone = pitch_table(T6_CLOCK as f64, 16.0, 0x3FF);
        for period in [8, 14, 31, 100, 511, 1023] {
            let (note, e5) = t6w28_note(period, T6_CLOCK as i64);
            assert_eq!(tone.value(note, e5 - 0x80), period);
        }
        // Periods below the table's shortest note clamp like the plain PSG.
        let (note, e5) = t6w28_note(2, T6_CLOCK as i64);
        assert_eq!(tone.value(note, e5 - 0x80), tone.value(179, 127));
        for period in [0x100, 0x200, 0x3FF] {
            let (note, e5) = t6w28_noise_note(period, T6_CLOCK as i64);
            let tbl = pitch_table(T6_CLOCK as f64, 15.0, 0x3FF);
            assert_eq!(tbl.value(note, e5 - 0x80), period);
        }
    }
}

mod TestT6W28Module {
    use super::*;

    fn _song() -> Song {
        let mut writes = _left(0, 1, 200, 4);
        writes.extend(_right(0, 1, 7));
        writes.extend(_left(1000, 2, 300, 2));
        writes.extend(_right(1000, 2, 2));
        writes.extend(_close(5000, 1));
        writes.extend(_close(5000, 2));
        let mut v = _vgm(writes);
        v.sn76489_clock = T6_CLOCK;
        v.sn76489_t6w28 = true;
        v.sn76489_dual = true;
        v.total_samples = 8000;
        analyze(&v, AnalyzeOptions::default()).expect("analyze")
    }

    #[test]
    fn test_one_t6w28_system_with_four_channels() {
        let song = _song();
        assert!(song.psg_t6w28);
        assert_eq!(song.psg_ids, vec![0]);
        let notes: Vec<&NoteEvent> = song
            .events
            .iter()
            .filter(|e| e.on && e.tone_chip == 3)
            .collect();
        assert_eq!(notes.len(), 2);
        assert_eq!(
            notes.iter().map(|e| e.tone_div).collect::<HashSet<_>>(),
            HashSet::from([16])
        );
        assert!(notes.iter().all(|e| e.tone_clock == T6_CLOCK));
        assert!(notes.iter().all(|e| e.pan != 0));
        let data = write_fur(
            &song,
            WriteOptions {
                include_pcm: false,
                ..Default::default()
            },
        );
        let m = parse_fur(&data).expect("fur");
        assert_eq!(m.chips, vec![CHIP_T6W28]);
        assert_eq!(m.n_ch, 4);
        assert_eq!(
            _instrument_types(&data).into_iter().collect::<HashSet<_>>(),
            HashSet::from([INS_T6W28])
        );
    }

    #[test]
    fn test_panning_reaches_a_pattern_cell() {
        let mut writes = _left(0, 0, 31, 0);
        writes.extend(_right(0, 0, 15));
        writes.extend(_close(5000, 0));
        let mut v = _vgm(writes);
        v.sn76489_clock = T6_CLOCK;
        v.sn76489_t6w28 = true;
        v.sn76489_dual = true;
        v.total_samples = 8000;
        let song = analyze(&v, AnalyzeOptions::default()).expect("analyze");
        let data = write_fur(
            &song,
            WriteOptions {
                include_pcm: false,
                ..Default::default()
            },
        );
        let m = parse_fur(&data).expect("fur");
        let rows = m.patterns.get(&(0u16, 0u16)).expect("pattern");
        let effects: Vec<(u8, u8)> = rows
            .iter()
            .flat_map(|row| row.fx.iter().copied())
            .collect();
        assert!(effects.contains(&(0x08, 0xF0)));
    }

    #[test]
    fn test_dual_sn76489_writes_two_sms_systems() {
        let mut writes = _left(0, 0, 31, 6);
        writes.push(ChipWrite::new(0, "sn76489", 1, 0, 0x80 | (200 & 0x0F)));
        writes.push(ChipWrite::new(0, "sn76489", 1, 0, (200 >> 4) & 0x3F));
        writes.push(ChipWrite::new(0, "sn76489", 1, 0, 0x90 | 6));
        writes.push(ChipWrite::new(5000, "sn76489", 0, 0, 0x9F));
        writes.push(ChipWrite::new(5000, "sn76489", 1, 0, 0x9F));
        let mut v = _vgm(writes);
        v.sn76489_clock = 3_579_545;
        v.sn76489_dual = true;
        v.total_samples = 8000;
        let song = analyze(&v, AnalyzeOptions::default()).expect("analyze");
        assert!(!song.psg_t6w28);
        assert_eq!(song.psg_ids, vec![0, 1]);
        assert_eq!(song.psg_count, 2);
        let chans: HashSet<i32> = song
            .events
            .iter()
            .filter(|e| e.on && e.tone_chip == 3)
            .map(|e| e.ch)
            .collect();
        assert_eq!(chans.len(), 2);
        assert_eq!(chans.iter().max().unwrap() - chans.iter().min().unwrap(), 4);
        let m = parse_fur(&write_fur(
            &song,
            WriteOptions {
                include_pcm: false,
                ..Default::default()
            },
        ))
        .expect("fur");
        assert_eq!(m.chips, vec![CHIP_SMS, CHIP_SMS]);
        assert_eq!(m.n_ch, 8);
    }

    #[test]
    fn test_dual_sn76489_carries_each_chip_clock() {
        // A dual log can run the two chips from different crystals; the note
        // table for each hit must invert the clock that instance's system
        // gets, and both custom clocks must reach their FLAG blocks.
        let mut writes = _left(0, 0, 254, 0);
        writes.push(ChipWrite::new(0, "sn76489", 1, 0, 0x80 | (200 & 0x0F)));
        writes.push(ChipWrite::new(0, "sn76489", 1, 0, (200 >> 4) & 0x3F));
        writes.push(ChipWrite::new(0, "sn76489", 1, 0, 0x90 | 0));
        writes.push(ChipWrite::new(5000, "sn76489", 0, 0, 0x9F));
        writes.push(ChipWrite::new(5000, "sn76489", 1, 0, 0x9F));
        let mut v = _vgm(writes);
        v.sn76489_clock = 2_858_084;
        v.sn76489_clock_2 = 3_150_000;
        v.sn76489_dual = true;
        v.total_samples = 8000;
        let song = analyze(&v, AnalyzeOptions::default()).expect("analyze");
        let mut clocks: Vec<u32> = song
            .events
            .iter()
            .filter(|e| e.on && e.tone_chip == 3)
            .map(|e| e.tone_clock)
            .collect();
        clocks.sort();
        clocks.dedup();
        assert_eq!(clocks, vec![2_858_084u32, 3_150_000u32]);
        let data = write_fur(
            &song,
            WriteOptions {
                include_pcm: false,
                ..Default::default()
            },
        );
        assert!(contains_subslice(&data, b"customClock=2858084"));
        assert!(contains_subslice(&data, b"customClock=3150000"));
    }
}

mod TestDualCommands {
    use super::*;

    fn _load(body: &[u8], clock_off: usize, clock: u32) -> VgmFile {
        let p = tmp_dir().join(format!("dual_{clock_off:02x}.vgm"));
        std::fs::write(&p, raw_vgm(body, &[(clock_off, clock | 0x40000000)])).expect("write");
        load_vgm(&p).expect("load")
    }

    #[test]
    fn test_ym2151_second_command() {
        let vgm = _load(&[0xA4, 0x20, 0x07], 0x30, 3_579_545);
        assert!(vgm.ym2151_dual);
        let got: Vec<(u8, u32, u32)> =
            vgm.writes.iter().map(|w| (w.chip_id, w.reg, w.val)).collect();
        assert_eq!(got, vec![(1, 0x20, 0x07)]);
    }

    #[test]
    fn test_opl_family_second_commands() {
        let body = [
            0xABu8, 0x20, 0x01, // YM3526 chip 1
            0xAC, 0x21, 0x02, // Y8950 chip 1
            0xAE, 0x22, 0x03, // YMF262 chip 1 port 0
            0xAF, 0x23, 0x04, // YMF262 chip 1 port 1
        ];
        let p = tmp_dir().join("opl_dual.vgm");
        std::fs::write(
            &p,
            raw_vgm(
                &body,
                &[
                    (0x54, 3_579_545 | 0x40000000),
                    (0x58, 3_579_545 | 0x40000000),
                    (0x5C, 14_318_180 | 0x40000000),
                ],
            ),
        )
        .expect("write");
        let vgm = load_vgm(&p).expect("load");
        assert!(vgm.opl_dual);
        assert!(vgm.y8950_dual);
        assert!(vgm.opl3_dual);
        let got: Vec<(&str, u8, u32, u32)> = vgm
            .writes
            .iter()
            .map(|w| (w.chip, w.chip_id, w.reg, w.val))
            .collect();
        assert_eq!(
            got,
            vec![
                ("opl", 1, 0x20, 0x01),
                ("y8950", 1, 0x21, 0x02),
                ("opl3", 1, 0x22, 0x03),
                ("opl3", 1, 0x123, 0x04),
            ]
        );
    }

    #[test]
    fn test_c140_chip_select_is_address_bit_15() {
        let p = tmp_dir().join("c140_dual.vgm");
        std::fs::write(
            &p,
            raw_vgm(&[0xD4, 0x80, 0x12, 0x34], &[(0xA8, 12_288_000 | 0x40000000)]),
        )
        .expect("write");
        let vgm = load_vgm(&p).expect("load");
        assert!(vgm.c140_dual);
        let got: Vec<(u8, u32, u32)> =
            vgm.writes.iter().map(|w| (w.chip_id, w.reg, w.val)).collect();
        assert_eq!(got, vec![(1, 0x12, 0x34)]);
    }

    #[test]
    fn test_opl4_port_bit_7_selects_the_second_chip() {
        let p = tmp_dir().join("opl4_dual.vgm");
        std::fs::write(
            &p,
            raw_vgm(&[0xD0, 0x82, 0x20, 0x45], &[(0x60, 33_868_800 | 0x40000000)]),
        )
        .expect("write");
        let vgm = load_vgm(&p).expect("load");
        assert!(vgm.ymf278b_dual);
        let got: Vec<(u8, u32, u32)> =
            vgm.writes.iter().map(|w| (w.chip_id, w.reg, w.val)).collect();
        assert_eq!(got, vec![(1, 0x220, 0x45)]);
    }

    #[test]
    fn test_rf5c_register_bit_7_selects_the_second_chip() {
        let p = tmp_dir().join("rf5c_dual.vgm");
        std::fs::write(
            &p,
            raw_vgm(&[0xB0, 0x88, 0x01], &[(0x40, 8_000_000 | 0x40000000)]),
        )
        .expect("write");
        let vgm = load_vgm(&p).expect("load");
        assert!(vgm.rf5c68_dual);
        let got: Vec<(u8, u32, u32)> =
            vgm.writes.iter().map(|w| (w.chip_id, w.reg, w.val)).collect();
        assert_eq!(got, vec![(1, 0x08, 0x01)]);
        assert!(!vgm.warnings.iter().any(|w| w.contains("only handles one RF5C")));
    }

    #[test]
    fn test_segapcm_address_bit_15_selects_the_second_chip() {
        let p = tmp_dir().join("segapcm_dual.vgm");
        std::fs::write(
            &p,
            raw_vgm(&[0xC0, 0x00, 0x80, 0x11], &[(0x38, 4_000_000 | 0x40000000)]),
        )
        .expect("write");
        let vgm = load_vgm(&p).expect("load");
        assert!(vgm.segapcm_dual);
        let got: Vec<(u8, u32, u32)> =
            vgm.writes.iter().map(|w| (w.chip_id, w.reg, w.val)).collect();
        assert_eq!(got, vec![(1, 0x00, 0x11)]);
        assert!(!vgm.warnings.iter().any(|w| w.contains("only handles one SegaPCM")));
    }

    #[test]
    fn test_second_game_gear_mask_command() {
        let p = tmp_dir().join("gg_dual.vgm");
        std::fs::write(
            &p,
            raw_vgm(&[0x3F, 0xF0], &[(0x0C, 3_579_545 | 0x40000000)]),
        )
        .expect("write");
        let vgm = load_vgm(&p).expect("load");
        assert!(vgm.sn76489_dual);
        let got: Vec<(u8, u32, u32)> =
            vgm.writes.iter().map(|w| (w.chip_id, w.reg, w.val)).collect();
        assert_eq!(got, vec![(1, 1, 0xF0)]);
    }
}

fn _opm_note(cid: u8, t: i64, ch: u32, off: i64) -> Vec<ChipWrite> {
    let mut out: Vec<ChipWrite> = Vec::new();
    for (page, vals) in [
        (0x20u32, (0xC7u32, 0x00u32)),
        (0x28, (0x4A, 0x00)),
        (0x30, (0x00, 0x00)),
        (0x40, (0x01, 0x00)),
        (0x50, (0x1F, 0x00)),
        (0x60, (0x00, 0x00)),
        (0x70, (0x00, 0x00)),
        (0x80, (0x0F, 0x00)),
        (0x90, (0x00, 0x00)),
    ] {
        for (i, val) in [vals.0, vals.1].into_iter().enumerate() {
            out.push(ChipWrite::new(
                t,
                "ym2151",
                cid,
                page + ch + i as u32 * 8,
                val,
            ));
        }
    }
    out.push(ChipWrite::new(t, "ym2151", cid, 0x08, 0x78 | ch));
    out.push(ChipWrite::new(off, "ym2151", cid, 0x08, ch));
    out
}

mod TestT6W28CutAndVolumeRows {
    use super::*;

    #[test]
    fn test_note_off_does_not_pair_ec_with_ed() {
        // The note and its off round onto one row. ECxx on the same line as
        // EDxx can cut the following note, so the writer must not pair them.
        let mut writes = _left(100, 0, 200, 10);
        writes.extend(_right(100, 0, 10));
        writes.extend(_close(300, 0));
        let mut v = _vgm(writes);
        v.sn76489_clock = T6_CLOCK;
        v.sn76489_t6w28 = true;
        v.sn76489_dual = true;
        v.total_samples = 8000;
        let song = analyze(&v, AnalyzeOptions::default()).expect("analyze");
        let m = parse_fur(&write_fur(&song, WriteOptions::default())).expect("fur");
        for ((ch, _idx), rows) in m.patterns.iter() {
            if *ch >= 4 {
                continue;
            }
            for row in rows {
                let cmds: HashSet<i32> = row.fx.iter().map(|&(c, _v)| c as i32).collect();
                assert!(!(cmds.contains(&0xEC) && cmds.contains(&0xED)), "{row:?}");
            }
        }
    }

    #[test]
    fn test_louder_same_row_step_keeps_the_note_attack() {
        let mut writes = _left(0, 0, 200, 10);
        writes.extend(_right(0, 0, 10));
        writes.extend(_left(50, 0, 200, 2));
        writes.extend(_right(50, 0, 2));
        writes.extend(_close(4000, 0));
        let mut v = _vgm(writes);
        v.sn76489_clock = T6_CLOCK;
        v.sn76489_t6w28 = true;
        v.sn76489_dual = true;
        v.total_samples = 8000;
        let song = analyze(&v, AnalyzeOptions::default()).expect("analyze");
        let m = parse_fur(&write_fur(&song, WriteOptions::default())).expect("fur");
        let rows = m
            .patterns
            .get(&(0u16, m.orders[0][0]))
            .expect("pattern");
        let note_rows: Vec<(usize, &vgm2fur::furio::FurRow)> = rows
            .iter()
            .enumerate()
            .filter(|(_, r)| r.note != -1 && r.note != 180)
            .collect();
        assert_eq!(note_rows[0].1.vol, 5);
        let later = (note_rows[0].0 + 1..rows.len()).any(|i| {
            rows[i].note == -1 && rows[i].vol == 13
        });
        assert!(later);
    }
}

mod TestPsgShortGateCut {
    use super::*;

    fn gate_vgm() -> VgmFile {
        let mut v = VgmFile::default();
        v.path = PathBuf::from("synthetic.vgm");
        v.buf = Vec::new();
        v.version = 0x161;
        v.total_samples = 100_000;
        v.loop_samples = 0;
        v.loop_file_pos = None;
        v.loop_sample = None;
        v.ym2151_clock = 0;
        v.k007232_clock = 0;
        v.roms = Vec::new();
        v.writes = Vec::new();
        v.sn76489_clock = 3_579_540;
        v
    }

    /// A base `Song` built through the public analyzer, then adjusted to the
    /// values the Python tests assign directly (the Rust `Song` has no
    /// all-default constructor).
    fn gate_song(events: Vec<NoteEvent>) -> Song {
        let mut song = analyze(
            &note_song(),
            AnalyzeOptions {
                pcm: false,
                ..Default::default()
            },
        )
        .expect("base song analyzes");
        song.vgm = gate_vgm();
        song.t0 = 0;
        song.loop_sample = None;
        song.speed = 12;
        song.hz = 480.0;
        song.row_subdiv = 4.0;
        song.fm_patches = Vec::new();
        song.pcm_samples = Vec::new();
        song.fm_systems = Vec::new();
        song.events = events;
        song
    }

    fn note(sample: i64, on: bool, note: i32, ins: i32, vol: i32, end: i64) -> NoteEvent {
        let mut ev = NoteEvent::new(sample, 11, on, note, ins, vol, 0x80, 0);
        ev.end = end;
        ev.tone_chip = 3;
        ev.tone_clock = 3_579_540;
        ev.tone_div = 16;
        ev
    }

    fn build(song: &Song) -> Vec<(i32, i32, Vec<Option<vgm2fur::furwrite::Row>>)> {
        let mut stats: IndexMap<String, i64> = IndexMap::new();
        let mut warnings: Vec<String> = Vec::new();
        let mut hooks = EmitHooks::default();
        let (_orders, patterns, _loop, _len) = _build_patterns(
            song,
            4,
            256,
            3,
            -8,
            Some(&mut stats),
            0,
            &mut warnings,
            &mut hooks,
        );
        patterns
    }

    fn find_rows(
        patterns: &[(i32, i32, Vec<Option<vgm2fur::furwrite::Row>>)],
    ) -> &Vec<Option<vgm2fur::furwrite::Row>> {
        patterns
            .iter()
            .find(|(ch, _pat, rows)| {
                *ch == 3
                    && rows.iter().any(|r| {
                        r.as_ref()
                            .is_some_and(|r| r.note != -1 && r.note != NOTE_OFF)
                    })
            })
            .map(|(_, _, rows)| rows)
            .expect("pattern with a note")
    }

    fn at(rows: &[Option<vgm2fur::furwrite::Row>]) -> usize {
        rows.iter()
            .position(|r| r.as_ref().is_some_and(|r| r.note != -1 && r.note != NOTE_OFF))
            .expect("note row")
    }

    fn fx_value(fx: &[(i32, i32)], cmd: i32) -> Option<i32> {
        // `dict(fx)` keeps the last value for a repeated command.
        fx.iter().rev().find(|(c, _)| *c == cmd).map(|&(_, v)| v)
    }

    #[test]
    fn test_short_noise_gate_keeps_a_safe_cut() {
        // A one-frame noise hit whose off rounds onto its own row used to
        // lose both its ED delay and the off, so the gate ran until the
        // channel's next key-on much later. Keep the cut on the note row, but
        // only when ECxx lands after the delayed note starts (ECxx > EDxx).
        // ECxx <= EDxx makes Furnace cut before the note sounds, the T6W28
        // premature-cut bug. 1102.5 samples/row is the measured Fantasy Zone
        // shape: the hit starts two ticks into row 56 and ends ten ticks in,
        // so both round onto row 56.
        let song = gate_song(vec![
            note(61920, true, 62, 0, 12, 62655),
            note(62655, false, 180, -1, -1, 0),
        ]);
        let patterns = build(&song);
        let rows = find_rows(&patterns);
        let at = at(rows);
        let cell = rows[at].as_ref().expect("row");
        let fx = cell.fx.as_deref().unwrap_or(&[]);
        assert!(fx_value(fx, 0xED).is_some());
        assert!(fx_value(fx, 0xEC).is_some());
        assert!(fx_value(fx, 0xEC).unwrap() > fx_value(fx, 0xED).unwrap());
    }

    #[test]
    fn test_cut_at_the_next_key_on_sample_is_not_written() {
        // A one-sample blip (the PCE wave case) has its key-off on the same
        // sample as the next key-on. The next key-on already ends it; an ECxx
        // here would land on the new note's tick.
        let song = gate_song(vec![
            note(61920, true, 62, 0, 12, 62655),
            note(62655, false, 180, -1, -1, 0),
            note(62655, true, 60, 0, 12, 64000),
        ]);
        let patterns = build(&song);
        let rows = find_rows(&patterns);
        let at = at(rows);
        let cell = rows[at].as_ref().expect("row");
        let fx = cell.fx.as_deref().unwrap_or(&[]);
        assert!(fx_value(fx, 0xEC).is_none());
        assert!(fx_value(fx, 0xED).is_some());
    }

    #[test]
    fn test_cut_reaching_the_next_key_on_is_not_written() {
        // Neo South Island Act 2: the cut (ED03 EC05) lands on the same tick
        // as the next key-on. Writing ECxx there cuts the new note instead of
        // the old one, so the next key-on must be allowed to end the note.
        let song = gate_song(vec![
            note(61920, true, 62, 0, 12, 62655),
            note(62655, false, 180, -1, -1, 0),
            // Same row tick as the would-be ED+EC cut.
            note(62843, true, 62, 0, 12, 64000),
        ]);
        let patterns = build(&song);
        let rows = find_rows(&patterns);
        let at = at(rows);
        let cell = rows[at].as_ref().expect("row");
        let fx = cell.fx.as_deref().unwrap_or(&[]);
        assert!(fx_value(fx, 0xEC).is_none());
        assert!(fx_value(fx, 0xED).is_some());
        assert!(rows
            .get(at + 1)
            .and_then(|r| r.as_ref())
            .is_some_and(|r| r.note != -1 && r.note != NOTE_OFF));
    }
}

mod TestDualYm2151 {
    use super::*;

    #[test]
    fn test_both_instances_get_their_own_columns() {
        let mut writes = _opm_note(0, 100, 0, 5000);
        writes.extend(_opm_note(1, 100, 0, 5000));
        let mut v = _vgm(writes);
        v.ym2151_clock = 3_579_545;
        v.ym2151_dual = true;
        v.total_samples = 8000;
        let song = analyze(&v, AnalyzeOptions::default()).expect("analyze");
        assert_eq!(song.ym2151_ids, vec![0, 1]);
        let systems: Vec<(&str, u8, i32)> = song
            .fm_systems
            .iter()
            .map(|s| (s.kind.as_str(), s.chip_id, s.fm_channels))
            .collect();
        assert_eq!(systems, vec![("ym2151", 0, 8), ("ym2151", 1, 8)]);
        let chans: HashSet<i32> = song
            .events
            .iter()
            .filter(|e| e.on && !e.pcm)
            .map(|e| e.ch)
            .collect();
        assert_eq!(chans, HashSet::from([0, 8]));
        let m = parse_fur(&write_fur(
            &song,
            WriteOptions {
                include_pcm: false,
                ..Default::default()
            },
        ))
        .expect("fur");
        assert_eq!(m.chips, vec![CHIP_YM2151, CHIP_YM2151]);
        assert_eq!(m.n_ch, 16);
    }
}

mod TestDualSegaPcm {
    use super::*;

    fn _key(cid: u8, t: i64) -> Vec<ChipWrite> {
        vec![
            ChipWrite::new(t, "segapcm", cid, 0x04, 0x00),
            ChipWrite::new(t, "segapcm", cid, 0x05, 0x04),
            ChipWrite::new(t, "segapcm", cid, 0x84, 0x00),
            ChipWrite::new(t, "segapcm", cid, 0x85, 0x04),
            ChipWrite::new(t + 2, "segapcm", cid, 0x06, 0x04),
            ChipWrite::new(t + 2, "segapcm", cid, 0x07, 0x84),
            ChipWrite::new(t + 2, "segapcm", cid, 0x02, 0x40),
            ChipWrite::new(t + 2, "segapcm", cid, 0x03, 0x40),
            ChipWrite::new(t + 2, "segapcm", cid, 0x86, 0x02),
        ]
    }

    #[test]
    fn test_second_rom_and_voice_are_converted() {
        let mut rom = vec![0u8; 0x1000];
        for byte in rom.iter_mut().take(0x500).skip(0x400) {
            *byte = 0xC0;
        }
        let roms = vec![
            RomSlice {
                chip: 0,
                rom_size: 0x1000,
                start: 0,
                data: rom.clone(),
                kind: "segapcm",
                time: 0,
            },
            RomSlice {
                chip: 1,
                rom_size: 0x1000,
                start: 0,
                data: rom,
                kind: "segapcm",
                time: 0,
            },
        ];
        let mut writes = _key(0, 100);
        writes.extend(_key(1, 200));
        let mut v = _vgm(writes);
        v.roms = roms;
        v.segapcm_clock = 4_000_000;
        v.segapcm_dual = true;
        v.total_samples = 8000;
        let song = analyze(&v, AnalyzeOptions::default()).expect("analyze");
        assert_eq!(song.segapcm_ids, vec![0, 1]);
        assert_eq!(song.segapcm_samples.len(), 2);
        let mut chans: Vec<i32> = song
            .events
            .iter()
            .filter(|e| e.on && e.pcm)
            .map(|e| e.ch)
            .collect();
        chans.sort();
        assert_eq!(chans.len(), 2);
        assert_eq!(chans[1] - chans[0], 16);
        let m = parse_fur(&write_fur(&song, WriteOptions::default())).expect("fur");
        assert_eq!(m.chips, vec![CHIP_YM2151, CHIP_SEGAPCM, CHIP_SEGAPCM]);
        assert_eq!(m.n_ch, 8 + 32);
    }
}

mod TestDualRf5c {
    use super::*;

    fn _key(cid: u8, t: i64) -> Vec<ChipWrite> {
        vec![
            ChipWrite::new(t, "rf5c68", cid, 0x07, 0x40),
            ChipWrite::new(t, "rf5c68", cid, 0x00, 0x7F),
            ChipWrite::new(t, "rf5c68", cid, 0x01, 0xFF),
            ChipWrite::new(t, "rf5c68", cid, 0x02, 0x00),
            ChipWrite::new(t, "rf5c68", cid, 0x03, 0x10),
            ChipWrite::new(t, "rf5c68", cid, 0x04, 0x00),
            ChipWrite::new(t, "rf5c68", cid, 0x05, 0x00),
            ChipWrite::new(t, "rf5c68", cid, 0x06, 0x01),
            ChipWrite::new(t, "rf5c68", cid, 0x08, 0xFE),
        ]
    }

    #[test]
    fn test_both_rf5c_instances_are_converted() {
        let mut body: Vec<u8> = (0x40u8..0x80).collect();
        body.push(0xFF);
        let roms = vec![
            RomSlice {
                chip: 0,
                rom_size: 0x1000,
                start: 0x100,
                data: body.clone(),
                kind: "rf5c68",
                time: 0,
            },
            RomSlice {
                chip: 1,
                rom_size: 0x1000,
                start: 0x100,
                data: body,
                kind: "rf5c68",
                time: 0,
            },
        ];
        let mut writes = _key(0, 100);
        writes.extend(_key(1, 200));
        let mut v = _vgm(writes);
        v.roms = roms;
        v.rf5c68_clock = 8_000_000;
        v.rf5c68_dual = true;
        v.total_samples = 8000;
        let song = analyze(&v, AnalyzeOptions::default()).expect("analyze");
        assert_eq!(song.rf5c68_samples.len(), 2);
        let mut chans: Vec<i32> = song
            .events
            .iter()
            .filter(|e| e.on && e.pcm)
            .map(|e| e.ch)
            .collect();
        chans.sort();
        assert_eq!(chans.len(), 2);
        assert_eq!(chans[1] - chans[0], 8);
        let m = parse_fur(&write_fur(&song, WriteOptions::default())).expect("fur");
        assert_eq!(m.chips, vec![CHIP_YM2151, CHIP_RF5C68, CHIP_RF5C68]);
        assert_eq!(m.n_ch, 8 + 16);
    }
}

mod TestDualK007232 {
    use super::*;

    fn _key(cid: u8, t: i64) -> Vec<ChipWrite> {
        vec![
            ChipWrite::new(t, "k007232", cid, 0x00, 0xF0),
            ChipWrite::new(t, "k007232", cid, 0x01, 0x0F),
            ChipWrite::new(t, "k007232", cid, 0x02, 0x00),
            ChipWrite::new(t, "k007232", cid, 0x03, 0x01),
            ChipWrite::new(t, "k007232", cid, 0x04, 0x00),
            ChipWrite::new(t, "k007232", cid, 0x14, 0x00),
            ChipWrite::new(t, "k007232", cid, 0x10, 0x88),
            ChipWrite::new(t, "k007232", cid, 0x11, 0x88),
            ChipWrite::new(t, "k007232", cid, 0x05, 0x00),
        ]
    }

    #[test]
    fn test_second_bank_and_voice_are_converted() {
        let mut body: Vec<u8> = (0x40u8..0x80).collect();
        body.push(0x80);
        let roms = vec![
            RomSlice {
                chip: 0,
                rom_size: 0x200,
                start: 0,
                data: body.clone(),
                kind: "k007232",
                time: 0,
            },
            RomSlice {
                chip: 1,
                rom_size: 0x200,
                start: 0,
                data: body,
                kind: "k007232",
                time: 0,
            },
        ];
        let mut writes = _key(0, 100);
        writes.extend(_key(1, 200));
        let mut v = _vgm(writes);
        v.roms = roms;
        v.k007232_clock = 3_579_545;
        v.k007232_dual = true;
        v.total_samples = 8000;
        let song = analyze(&v, AnalyzeOptions::default()).expect("analyze");
        assert_eq!(song.pcm_samples.len(), 2);
        let mut chans: Vec<i32> = song
            .events
            .iter()
            .filter(|e| e.on && e.pcm)
            .map(|e| e.ch)
            .collect();
        chans.sort();
        assert_eq!(chans.len(), 2);
        assert_eq!(chans[1] - chans[0], 2);
        // Each short body reaches its 0x80 end marker before the song ends.
        assert_eq!(
            song.events.iter().filter(|e| e.pcm && !e.on).count(),
            2
        );
        let m = parse_fur(&write_fur(&song, WriteOptions::default())).expect("fur");
        assert_eq!(m.chips, vec![CHIP_YM2151, CHIP_K007232, CHIP_K007232]);
        assert_eq!(m.n_ch, 8 + 4);
    }
}
