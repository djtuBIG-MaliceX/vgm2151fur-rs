//! YM2608 (OPNA) support: two address ports, the rhythm page, delta-T and layout.
//!
//! Port of `tests/test_vgm2151fur_ym2608.py` at vgm2151fur a8edb6e.
//!
//! The OPNA is where the YM2612's FM and the YM2610's sample engines meet, so
//! this module tests the joints rather than re-testing either: that `$56`/`$57`
//! land in one flat register number with the port in bit 8; that the pages
//! under `$30` on each port feed the rhythm and delta-T decoders instead of
//! each other; that a split channel 3 pushes the second block's columns along
//! instead of colliding with them; and that a delta-T body starts where the
//! chip's own address unit puts it, not at the head of the 256-byte block
//! around it.

mod common;
#[allow(unused_imports)]
use common::*;

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use vgm2fur::analyze::{analyze, format_report, AnalyzeOptions};
use vgm2fur::ay8910::ay_note;
use vgm2fur::diag::pitch_runs;
use vgm2fur::furio::{chip_channels, iter_blocks, parse_fur, parse_smp2_payload};
use vgm2fur::furwrite::{
    parse_fur_skeleton, write_fur, WriteOptions, CHIP_YM2608,
    CHIP_YM2608_EXT, INS_ADPCMA, INS_ADPCMB, INS_FM,
};
use vgm2fur::furnace::export_vgm;
use vgm2fur::model::Song;
use vgm2fur::vgm::{
    assemble_rom, chips_in_head, load_vgm, ChipWrite, RomSlice, VgmFile,
};
use vgm2fur::ym2608::{
    _deltat_slice, collect_ym2608_delta_t, collect_ym2608_rhythm, deltat_store_body,
    replay_deltat_memory, YM2608State, ADPCM_B_ADDR_SHIFT, RHYTHM_VOICES,
    YM2608_DEFAULT_CLOCK, YM2608_SSG_DIVIDER,
};
use vgm2fur::ym2612::{ym2612_note_for, YM2612_FREQ_BASE};

const CLOCK: u32 = YM2608_DEFAULT_CLOCK;

fn tmp_dir() -> PathBuf {
    let p = std::env::temp_dir().join("vgm2151fur_ym2608");
    std::fs::create_dir_all(&p).expect("create temp dir");
    p
}

/// A delta-T image with a recognisable body at 0x260.
fn rom() -> Vec<u8> {
    (0..0x800u32).map(|i| ((i * 5 + 1) & 0xFF) as u8).collect()
}

/// `_raw_vgm(body, *, clock=CLOCK, dual=False)`.
fn ym_raw_vgm(body: &[u8], clock: u32, dual: bool) -> Vec<u8> {
    let mut buf = vec![0u8; 0xC0];
    buf[0..4].copy_from_slice(b"Vgm ");
    buf[8..12].copy_from_slice(&0x161u32.to_le_bytes());
    buf[0x18..0x1C].copy_from_slice(&44100u32.to_le_bytes());
    buf[0x34..0x38].copy_from_slice(&(0xC0u32 - 0x34).to_le_bytes());
    let mut raw = clock & 0x3FFF_FFFF;
    if dual {
        raw |= 0x4000_0000;
    }
    buf[0x48..0x4C].copy_from_slice(&raw.to_le_bytes());
    let mut out = buf;
    out.extend_from_slice(body);
    out.push(0x66);
    let len = (out.len() as u32 - 4).to_le_bytes();
    out[4..8].copy_from_slice(&len);
    out
}

/// `_rom_block(data, *, typ=0x81, start=0)`.
fn rom_block(data: &[u8], typ: u8, start: u32) -> Vec<u8> {
    let size = 8 + data.len() as u32;
    let mut out = vec![0x67, 0x66, typ];
    out.extend_from_slice(&size.to_le_bytes());
    out.extend_from_slice(&(data.len() as u32).to_le_bytes());
    out.extend_from_slice(&start.to_le_bytes());
    out.extend_from_slice(data);
    out
}

/// `_vgm(writes, rom=None, *, total_samples=8000, dual=False)`.
fn synthetic(writes: Vec<ChipWrite>, rom: Option<&[u8]>) -> VgmFile {
    synthetic_opts(writes, rom, 8000, false)
}

fn synthetic_opts(
    mut writes: Vec<ChipWrite>,
    rom: Option<&[u8]>,
    total_samples: i64,
    dual: bool,
) -> VgmFile {
    let roms = match rom {
        Some(r) => vec![RomSlice {
            chip: 0,
            rom_size: r.len() as i64,
            start: 0,
            data: r.to_vec(),
            kind: "ym2608_deltat",
            time: 0,
        }],
        None => Vec::new(),
    };
    writes.sort_by_key(|w| w.sample);
    VgmFile {
        path: PathBuf::from("synthetic.vgm"),
        buf: Vec::new(),
        version: 0x161,
        total_samples,
        loop_samples: 0,
        loop_file_pos: None,
        loop_sample: None,
        ym2151_clock: 0,
        k007232_clock: 0,
        gd3: Default::default(),
        roms,
        writes,
        ym2608_clock: CLOCK,
        ym2608_dual: dual,
        ..Default::default()
    }
}

/// `_fm_note(ch, *, block=4, fnum=0x400, t=100, off=7000, cid=0)`.
fn fm_note(ch: i32, block: i32, fnum: i32, t: i64, off: i64, cid: u8) -> Vec<ChipWrite> {
    let port = ch / 3;
    let slot = (ch % 3) as u32;
    let base: u32 = if port != 0 { 0x100 } else { 0 };
    let mut out: Vec<ChipWrite> = Vec::new();
    for page in [0x30u32, 0x40, 0x50, 0x60, 0x70, 0x80, 0x90] {
        for op in 0..4u32 {
            let val = if page == 0x30 {
                0x01
            } else if page == 0x50 {
                0x1F
            } else {
                0
            };
            out.push(ChipWrite::new(
                t,
                "ym2608",
                cid,
                base | page | (op << 2) | slot,
                val,
            ));
        }
    }
    out.push(ChipWrite::new(t, "ym2608", cid, base | 0xB0 | slot, 0x07));
    out.push(ChipWrite::new(t, "ym2608", cid, base | 0xB4 | slot, 0xC0));
    out.push(ChipWrite::new(
        t,
        "ym2608",
        cid,
        base | 0xA4 | slot,
        ((block << 3) | ((fnum >> 8) & 7)) as u32,
    ));
    out.push(ChipWrite::new(t, "ym2608", cid, base | 0xA0 | slot, (fnum & 0xFF) as u32));
    let kon = if ch < 3 { ch } else { ch + 1 };
    out.push(ChipWrite::new(
        t,
        "ym2608",
        cid,
        0x28,
        ((0xF << 4) | kon) as u32,
    ));
    if off != 0 {
        out.push(ChipWrite::new(off, "ym2608", cid, 0x28, kon as u32));
    }
    out
}

fn fm_note_default(ch: i32) -> Vec<ChipWrite> {
    fm_note(ch, 4, 0x400, 100, 7000, 0)
}

/// `_ssg_voice(voice, *, period=0x1FF, level=15, t=300)`.
fn ssg_voice(voice: u32, period: u32, level: u32, t: i64) -> Vec<ChipWrite> {
    vec![
        ChipWrite::new(t, "ym2608_ssg", 0, voice * 2, period & 0xFF),
        ChipWrite::new(t, "ym2608_ssg", 0, voice * 2 + 1, (period >> 8) & 0x0F),
        ChipWrite::new(t, "ym2608_ssg", 0, 8 + voice, level),
        ChipWrite::new(t, "ym2608_ssg", 0, 7, 0x1E & !(1 << voice)),
    ]
}

fn ssg_voice_default() -> Vec<ChipWrite> {
    ssg_voice(0, 0x1FF, 15, 300)
}

/// `_rhythm_key(t, bits, *, cid=0)`.
fn rhythm_key(t: i64, bits: u32, cid: u8) -> ChipWrite {
    ChipWrite::new(t, "ym2608", cid, 0x10, bits)
}

/// `_rhythm_level(voice, level, pan=3, *, t=90)`.
fn rhythm_level(voice: u32, level: u32, pan: u32, t: i64) -> ChipWrite {
    ChipWrite::new(t, "ym2608", 0, 0x18 + voice, (pan << 6) | level)
}

fn rhythm_level_default(voice: u32, level: u32) -> ChipWrite {
    rhythm_level(voice, level, 3, 90)
}

/// `_delta_t_play(t, *, start, end, rate, vol=200, pan=3, loop=False)`.
fn delta_t_play(
    t: i64,
    start: i32,
    end: i32,
    rate: i32,
    vol: u32,
    pan: u32,
    loop_: bool,
) -> Vec<ChipWrite> {
    vec![
        ChipWrite::new(
            t,
            "ym2608",
            0,
            0x102,
            ((start >> ADPCM_B_ADDR_SHIFT) & 0xFF) as u32,
        ),
        ChipWrite::new(t, "ym2608", 0, 0x103, ((start >> 13) & 0xFF) as u32),
        ChipWrite::new(
            t,
            "ym2608",
            0,
            0x104,
            ((end >> ADPCM_B_ADDR_SHIFT) & 0xFF) as u32,
        ),
        ChipWrite::new(t, "ym2608", 0, 0x105, ((end >> 13) & 0xFF) as u32),
        ChipWrite::new(t, "ym2608", 0, 0x109, (rate & 0xFF) as u32),
        ChipWrite::new(t, "ym2608", 0, 0x10A, ((rate >> 8) & 0xFF) as u32),
        ChipWrite::new(t, "ym2608", 0, 0x10B, vol),
        ChipWrite::new(t, "ym2608", 0, 0x101, pan << 6),
        ChipWrite::new(
            t + 1,
            "ym2608",
            0,
            0x100,
            0x80 | (if loop_ { 0x10 } else { 0 }),
        ),
    ]
}

fn delta_t_play_default(t: i64, start: i32, end: i32, rate: i32) -> Vec<ChipWrite> {
    delta_t_play(t, start, end, rate, 200, 3, false)
}

fn load_vgm_from(raw: &[u8]) -> VgmFile {
    // Python's unittest is serial and reuses one probe file; Rust tests run in
    // parallel, so each call gets its own file.
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    let path = tmp_dir().join(format!("probe_{n}.vgm"));
    std::fs::write(&path, raw).expect("write vgm");
    load_vgm(&path).expect("load vgm")
}

/// `int.from_bytes(block.payload[2:4], "little")` for an INS2 block.
fn ins_types(data: &[u8]) -> Vec<i32> {
    let mut out = Vec::new();
    for block in iter_blocks(data).expect("iter_blocks") {
        if block.magic == *b"INS2" && block.payload.len() >= 4 {
            out.push(u16::from_le_bytes([block.payload[2], block.payload[3]]) as i32);
        }
    }
    out
}

fn has(data: &[u8], pat: &[u8]) -> bool {
    data.windows(pat.len()).any(|w| w == pat)
}

fn analyze_default(v: &VgmFile) -> Song {
    analyze(v, AnalyzeOptions::default()).expect("analyze")
}

fn write(song: &Song) -> Vec<u8> {
    write_fur(song, WriteOptions::default())
}

#[allow(non_snake_case)]
mod TestYm2608Parsing {
    use super::*;

    #[test]
    fn test_header_clock_and_chip_presence() {
        let body = [0x56u8, 0xB0, 0x07, 0x57, 0xA0, 0x40, 0x62];
        let vgm = load_vgm_from(&ym_raw_vgm(&body, CLOCK, false));
        assert_eq!(vgm.ym2608_clock, CLOCK);
        let raw = ym_raw_vgm(&body, CLOCK, false);
        let kinds = chips_in_head(&raw);
        assert!(kinds.contains("ym2608"));
    }

    #[test]
    fn test_dual_bit_marks_the_second_chip() {
        let body = [0xA6u8, 0xB0, 0x07];
        let vgm = load_vgm_from(&ym_raw_vgm(&body, CLOCK, true));
        assert!(vgm.ym2608_dual);
        let ids: BTreeSet<u8> = vgm.writes.iter().map(|w| w.chip_id).collect();
        assert_eq!(ids, BTreeSet::from([1]));
    }

    #[test]
    fn test_second_chip_without_the_bit_is_reported() {
        let vgm = load_vgm_from(&ym_raw_vgm(&[0xA6u8, 0xB0, 0x07], CLOCK, false));
        assert!(!vgm.ym2608_dual);
        assert!(
            vgm.warnings
                .iter()
                .any(|w| w.contains("dual") || w.contains("header")),
            "{:?}",
            vgm.warnings
        );
    }

    #[test]
    fn test_port_bit_and_ssg_split() {
        let body = [
            0x56u8, 0x08, 0x0F, // port 0, below $10: the SSG
            0x56, 0x18, 0xC8, // port 0, rhythm level: stays with the chip
            0x56, 0xB0, 0x07, // port 0, FM block 1
            0x57, 0xA0, 0x40, // port 1 FM (a slot write on the second block)
            0x57, 0x01, 0xC0, // port 1 below $11: delta-T pan
            0x62, // wait
        ];
        let vgm = load_vgm_from(&ym_raw_vgm(&body, CLOCK, false));
        let by_reg: BTreeMap<(&'static str, u32), u32> =
            vgm.writes.iter().map(|w| ((w.chip, w.reg), w.val)).collect();
        assert_eq!(by_reg[&("ym2608_ssg", 0x08)], 0x0F);
        assert_eq!(by_reg[&("ym2608", 0x18)], 0xC8);
        assert_eq!(by_reg[&("ym2608", 0xB0)], 0x07);
        assert_eq!(by_reg[&("ym2608", 0x1A0)], 0x40);
        assert_eq!(by_reg[&("ym2608", 0x101)], 0xC0);
    }

    #[test]
    fn test_deltat_block_assembles_at_its_offset() {
        let data: Vec<u8> = (0..256u32).map(|i| ((i * 7 + 3) & 0xFF) as u8).collect();
        let body = rom_block(&data, 0x81, 0x40);
        let vgm = load_vgm_from(&ym_raw_vgm(&body, CLOCK, false));
        assert_eq!(vgm.ym2608_clock, CLOCK);
        let got = assemble_rom(&vgm, "ym2608_deltat", 0, 0, false, true);
        assert!(got.len() >= 0x140);
        assert_eq!(&got[0x40..0x50], &data[..16]);
    }
}

#[allow(non_snake_case)]
mod TestYm2608State {
    use super::*;

    #[test]
    fn test_rhythm_page_lands_on_the_adpcm_a_decoder() {
        let mut st = YM2608State::new();
        st.write(0x11, 49); // global level
        st.write(0x18, (3 << 6) | 26); // BD: pan both, level 26
        assert_eq!(st.a_master, 49);
        assert_eq!(st.a_vol[0], 26);
        assert_eq!(st.a_pan[0], 3);
    }

    #[test]
    fn test_delta_t_page_lands_on_the_adpcm_b_decoder() {
        let mut st = YM2608State::new();
        for (reg, val) in [
            (0x101, 0xC0),
            (0x102, 0x60),
            (0x103, 0x01),
            (0x104, 0xBF),
            (0x105, 0x01),
            (0x109, 0x64),
            (0x10A, 0x62),
            (0x10B, 139),
        ] {
            st.write(reg, val);
        }
        assert_eq!(st.b_pan, 3);
        assert_eq!(st.b_start, 0x160);
        assert_eq!(st.b_end, 0x1BF);
        assert_eq!(st.b_freq, 0x6264);
        assert_eq!(st.b_vol, 139);
    }

    #[test]
    fn test_irq_registers_reach_no_decoder() {
        let mut st = YM2608State::new();
        let before = (st.b_control, st.b_pan, st.b_freq, st.a_master, st.dac_enabled);
        st.write(0x110, 0xFF); // port 1 IRQ flag control
        st.write(0x29, 0xFF); // port 0 IRQ mask
        st.write(0x2A, 0x80); // YM2612 DAC data; nothing on the OPNA
        assert_eq!(
            (st.b_control, st.b_pan, st.b_freq, st.a_master, st.dac_enabled),
            before
        );
    }

    #[test]
    fn test_key_on_maps_the_second_block_past_a_reserved_slot() {
        let mut st = YM2608State::new();
        for (raw, want) in [(0, 0usize), (1, 1), (2, 2), (4, 3), (5, 4), (6, 5)] {
            st.op_mask = vec![0; 6];
            st.write(0x28, (0xF << 4) | raw);
            assert_eq!(st.op_mask[want], 0xF, "$28 value {raw}");
        }
    }

    #[test]
    fn test_no_dac_on_the_opna() {
        let mut st = YM2608State::new();
        assert!(!st.dac_enabled);
        st.write(0x2B, 0x80); // YM2612 DAC enable; OPNA IRQ counter byte
        assert!(!st.dac_enabled);
    }

    #[test]
    fn test_special_mode_gives_channel_three_four_frequencies() {
        let mut st = YM2608State::new();
        st.write(0x27, 0x40); // channel 3 special
        assert!(st.multi_freq());
        st.write(0xAC, (4 << 3) | 1);
        st.write(0xA8, 0x22); // group 0 -> the column Furnace names O3
        assert_eq!(st.frequency_op(2, 0), (4, 0x122));
    }

    #[test]
    fn test_pan_byte_alias_matches_the_ym2610_name() {
        let mut st = YM2608State::new();
        st.write(0xB4, (1 << 7) | (1 << 6));
        assert_eq!(st.pan_byte(0), st.pan_byte_ch(0));
    }

    #[test]
    fn test_rhythm_level_keeps_the_global_separate() {
        let mut st = YM2608State::new();
        st.write(0x18, (3 << 6) | 31);
        st.write(0x11, 0x3F);
        assert_eq!(st.rhythm_level(0), 31);
        st.write(0x11, 49);
        assert_eq!(st.rhythm_level(0), 31);
        assert!(st.adpcm_a_level(0) < 31);
    }
}

#[allow(non_snake_case)]
mod TestDeltaTCutting {
    use super::*;

    #[test]
    fn test_body_starts_at_the_chip_address_not_its_block_head() {
        let mut rom = vec![0u8; 0x600];
        for i in 0x200..0x400usize {
            rom[i] = ((i & 0x7F) | 0x80) as u8;
        }
        let start = 0x260usize;
        let body = _deltat_slice(&rom, start as i32, 0x3FF);
        assert_eq!(&body[..8], &rom[start..start + 8]);
        assert_eq!(body.len() % 32, 0); // whole address units
    }

    #[test]
    fn test_store_body_pads_to_the_length_furnace_plays() {
        assert_eq!(deltat_store_body(&vec![1u8; 832]).len(), 1024);
        let padded = deltat_store_body(&vec![1u8; 832]);
        assert_eq!(&padded[832..], &vec![0x80u8; 192][..]);
        assert_eq!(deltat_store_body(&vec![1u8; 256]), vec![1u8; 256]);
    }
}

#[allow(non_snake_case)]
mod TestRhythmCollector {
    use super::*;

    #[test]
    fn test_key_bits_open_one_hit_per_voice() {
        let writes = vec![
            rhythm_level_default(0, 26), // BD level
            rhythm_level_default(3, 20), // HH level
            rhythm_key(100, 0x09, 0),    // BD + HH together
        ];
        let (hits, warn) = collect_ym2608_rhythm(&synthetic(writes, None));
        assert!(warn.is_empty());
        assert_eq!(
            hits.iter().map(|h| h.voice).collect::<Vec<_>>(),
            vec![0, 3]
        );
        assert_eq!(hits[0].vol, 26);
        assert_eq!(hits[1].vol, 20);
    }

    #[test]
    fn test_release_byte_closes_the_gate() {
        let writes = vec![
            rhythm_level_default(1, 24),
            rhythm_key(100, 0x02, 0), // SD on
            rhythm_key(900, 0x82, 0), // SD off
        ];
        let (hits, _w) = collect_ym2608_rhythm(&synthetic(writes, None));
        assert_eq!(hits.len(), 1);
        assert_eq!((hits[0].sample_time, hits[0].end_time), (100, 900));
    }

    #[test]
    fn test_retrigger_ends_the_previous_hit() {
        let writes = vec![
            rhythm_level_default(1, 24),
            rhythm_key(100, 0x02, 0),
            rhythm_key(700, 0x02, 0), // the same voice again
        ];
        let (hits, _w) = collect_ym2608_rhythm(&synthetic(writes, None));
        assert_eq!(
            hits.iter()
                .map(|h| (h.sample_time, h.end_time))
                .collect::<Vec<_>>(),
            vec![(100, 700), (700, 8000)]
        );
    }

    #[test]
    fn test_level_inside_the_gate_moves_the_note_not_a_step() {
        let writes = vec![
            rhythm_key(100, 0x02, 0), // SD keys at the old level
            rhythm_level(1, 30, 3, 120), // ... and is set microseconds on
        ];
        let (hits, _w) = collect_ym2608_rhythm(&synthetic(writes, None));
        assert_eq!(hits[0].vol, 30);
        assert!(hits[0].vol_pts.is_empty());
    }

    #[test]
    fn test_level_after_the_gate_is_a_step() {
        let writes = vec![
            rhythm_level_default(1, 30),
            rhythm_key(100, 0x02, 0),
            rhythm_level(1, 8, 3, 800), // still inside the voice's body
        ];
        let (hits, _w) = collect_ym2608_rhythm(&synthetic(writes, None));
        assert_eq!(hits[0].vol, 30);
        assert_eq!(hits[0].vol_pts, vec![(800, 8)]);
    }

    #[test]
    fn test_level_just_after_the_gate_belongs_to_the_hit() {
        let writes = vec![
            rhythm_level_default(1, 0),
            rhythm_key(100, 0x02, 0),
            rhythm_level(1, 24, 3, 410),
        ];
        let (hits, _w) = collect_ym2608_rhythm(&synthetic(writes, None));
        assert_eq!(hits[0].vol, 24);
        assert!(hits[0].vol_pts.is_empty());
    }

    #[test]
    fn test_global_level_is_not_folded_into_the_hits() {
        let writes = vec![
            rhythm_level_default(0, 31),
            rhythm_level_default(3, 31),
            ChipWrite::new(90, "ym2608", 0, 0x11, 0x3F),
            rhythm_key(100, 0x09, 0),
            ChipWrite::new(400, "ym2608", 0, 0x11, 0x30),
        ];
        let (hits, _w) = collect_ym2608_rhythm(&synthetic(writes, None));
        assert_eq!(hits.len(), 2);
        for h in &hits {
            assert_eq!(h.vol, 31);
            assert!(h.vol_pts.is_empty());
        }
    }

    #[test]
    fn test_pan_comes_from_the_level_byte() {
        let writes = vec![
            rhythm_level(4, 20, 1, 90), // TM left
            rhythm_key(100, 0x10, 0),
        ];
        let (hits, _w) = collect_ym2608_rhythm(&synthetic(writes, None));
        assert_eq!(hits[0].pan, 0x0F);
    }
}

#[allow(non_snake_case)]
mod TestDeltaTCollector {
    use super::*;

    fn collector_rom() -> Vec<u8> {
        (0..0x1000u32).map(|i| ((i * 7 + 3) & 0xFF) as u8).collect()
    }

    fn play(extra: Vec<ChipWrite>, t: i64, start: i32, end: i32, rate: i32, loop_: bool) -> VgmFile {
        let mut writes = delta_t_play(t, start, end, rate, 200, 3, loop_);
        writes.extend(extra);
        synthetic(writes, Some(&collector_rom()))
    }

    #[test]
    fn test_a_start_bit_opens_one_gate_per_play() {
        let vgm = play(Vec::new(), 100, 0x200, 0x3FF, 0x4000, false);
        let (smp, warn) = collect_ym2608_delta_t(&vgm, &collector_rom());
        assert!(warn.is_empty());
        assert_eq!(smp.len(), 1);
        let h = &smp[0].hits[0];
        assert_eq!((h.start, h.end), (0x200, 0x3FF));
        assert_eq!(h.rate, 0x4000);
    }

    #[test]
    fn test_reset_and_cleared_start_close_the_gate() {
        let vgm = play(
            vec![ChipWrite::new(900, "ym2608", 0, 0x100, 0x01)], // reset
            100,
            0x200,
            0x3FF,
            0x4000,
            false,
        );
        let (smp, _w) = collect_ym2608_delta_t(&vgm, &collector_rom());
        assert_eq!(smp[0].hits[0].end_time, 900);
    }

    #[test]
    fn test_loop_flag_survives() {
        let vgm = play(Vec::new(), 100, 0x200, 0x3FF, 0x4000, true);
        let (smp, _w) = collect_ym2608_delta_t(&vgm, &collector_rom());
        assert!(smp[0].hits[0].r#loop);
    }

    #[test]
    fn test_body_is_the_exact_source_range() {
        let vgm = play(Vec::new(), 100, 0x260, 0x3FF, 0x4000, false);
        let (smp, _w) = collect_ym2608_delta_t(&vgm, &collector_rom());
        assert_eq!(smp[0].data, collector_rom()[0x260..0x400]);
        assert_eq!(deltat_store_body(&smp[0].data).len(), 512);
    }

    #[test]
    fn test_mid_play_rate_and_level_become_steps() {
        let vgm = play(
            vec![
                ChipWrite::new(600, "ym2608", 0, 0x10B, 90),
                ChipWrite::new(700, "ym2608", 0, 0x109, 0x00),
                ChipWrite::new(701, "ym2608", 0, 0x10A, 0x80),
            ],
            100,
            0x200,
            0xFFF,
            0x4000,
            false,
        );
        let (smp, _w) = collect_ym2608_delta_t(&vgm, &collector_rom());
        let h = &smp[0].hits[0];
        assert!(h.vol_pts.contains(&(600, 90)));
        assert!(h.rate_pts.contains(&(701, 0x8000)));
    }

    #[test]
    fn test_no_data_block_is_reported() {
        let vgm = play(Vec::new(), 100, 0x200, 0x3FF, 0x4000, false);
        let (_smp, warn) = collect_ym2608_delta_t(&vgm, b"");
        assert!(
            warn.iter().any(|w| w.contains("delta-T") || w.contains("0x81")),
            "{warn:?}"
        );
    }
}

#[allow(non_snake_case)]
mod TestYm2608Analyze {
    use super::*;

    fn base() -> VgmFile {
        let mut writes = fm_note_default(0);
        writes.extend(ssg_voice_default());
        writes.push(ChipWrite::new(90, "ym2608", 0, 0x11, 0x3F));
        writes.push(rhythm_level_default(0, 26));
        writes.push(rhythm_key(400, 0x01, 0));
        writes.extend(delta_t_play_default(500, 0x200, 0x3FF, 0x4000));
        synthetic(writes, Some(&rom()))
    }

    #[test]
    fn test_chip_selection_and_flags() {
        let song = analyze_default(&base());
        assert_eq!(song.fm_chip, "ym2608");
        assert!(song.ym2610_ids.is_empty());
        assert!(!song.ym2608_ext);
        assert_eq!(song.ym2608_ids, vec![0]);
    }

    #[test]
    fn test_layout_matches_the_ym2610b_span() {
        let song = analyze_default(&base());
        let chs = |pred: &dyn Fn(&vgm2fur::model::NoteEvent) -> bool| -> Vec<i32> {
            let mut s = BTreeSet::new();
            for e in &song.events {
                if e.on && pred(e) {
                    s.insert(e.ch);
                }
            }
            s.into_iter().collect()
        };
        assert_eq!(chs(&|e| !e.pcm && e.ch < 6), vec![0]);
        assert_eq!(chs(&|e| (6..9).contains(&e.ch)), vec![6]);
        assert_eq!(chs(&|e| (9..15).contains(&e.ch)), vec![9]); // BD
        assert_eq!(chs(&|e| e.ch >= 15), vec![15]); // delta-T
        assert_eq!(chip_channels(CHIP_YM2608), 16);
    }

    #[test]
    fn test_rhythm_note_carries_the_voice_level() {
        let song = analyze_default(&base());
        let hit = song
            .events
            .iter()
            .find(|e| e.on && e.ch == 9)
            .expect("rhythm event");
        assert_eq!(hit.vol, 26);
        assert_eq!(hit.pan, 0xFF);
        assert!(hit.vol <= 31);
    }

    #[test]
    fn test_global_level_is_emitted_as_an_1f_effect() {
        let mut writes = fm_note_default(0);
        writes.push(ChipWrite::new(80, "ym2608", 0, 0x11, 32));
        writes.push(rhythm_level_default(0, 26));
        writes.push(rhythm_key(400, 0x01, 0));
        let song = analyze_default(&synthetic(writes, None));
        let hit = song
            .events
            .iter()
            .find(|e| e.on && e.ch == 9)
            .expect("rhythm event");
        assert_eq!(hit.vol, 26);
        assert!(song.channel_fx.contains(&(80, 9, 0x1F, 32)));
    }

    #[test]
    fn test_second_block_is_the_next_three_channels() {
        let mut writes = fm_note_default(4);
        writes.extend(fm_note_default(0));
        let song = analyze_default(&synthetic(writes, None));
        let notes: Vec<i32> = {
            let mut s = BTreeSet::new();
            for e in &song.events {
                if e.on {
                    s.insert(e.ch);
                }
            }
            s.into_iter().collect()
        };
        assert_eq!(notes, vec![0, 4]);
    }

    #[test]
    fn test_deltat_note_and_instrument() {
        let song = analyze_default(&base());
        let play = song
            .events
            .iter()
            .find(|e| e.on && e.ch == 15)
            .expect("delta-t event");
        assert_eq!(play.note, 60);
        assert!(play.ins >= song.fm_patches.len() as i32, "ins={} fm_patches={}", play.ins, song.fm_patches.len());
        assert_eq!(song.ym2608_b_samples.len(), 1);
    }

    #[test]
    fn test_linked_ssg_entry_leaves_the_default() {
        // The VGM extra header's linked SSG volume is ignored. Furnace keeps
        // its own default (128), and Chip Manager lets the user adjust ssgVol.
        let mut vgm = synthetic(fm_note_default(0), None);
        vgm.extra_volumes.insert(0x87, (0, 0x80A0));
        let data = write(&analyze_default(&vgm));
        let mut flags: Vec<String> = Vec::new();
        for blk in iter_blocks(&data).expect("iter_blocks") {
            if blk.magic == *b"FLAG" {
                flags.push(String::from_utf8_lossy(blk.payload).into_owned());
            }
        }
        assert!(!flags.join("\n").contains("ssgVol="));
    }

    #[test]
    fn test_same_sample_global_levels_collapse_to_the_key_value() {
        let writes = vec![
            ChipWrite::new(400, "ym2608", 0, 0x11, 0x30),
            ChipWrite::new(400, "ym2608", 0, 0x11, 0x3C),
            ChipWrite::new(400, "ym2608", 0, 0x18, 0xCF),
            ChipWrite::new(400, "ym2608", 0, 0x10, 0x01),
        ];
        let song = analyze_default(&synthetic(writes, None));
        let mod_ = parse_fur(&write(&song)).expect("parse_fur");
        let global_row = song.row_of(400);
        let pat_len = mod_.pat_len as i32;
        let pat = global_row / pat_len;
        let row = global_row % pat_len;
        let cell = &mod_.patterns[&(9u16, pat as u16)][row as usize];
        assert!(cell.fx.contains(&(0x1F, 0x3C)), "{:?}", cell.fx);
    }

    #[test]
    fn test_deltat_register_upload_rebuilds_the_memory() {
        let data: Vec<u8> = (0..31u32).map(|i| ((i * 11 + 5) & 0xFF) as u8).collect();
        let mut writes = vec![
            ChipWrite::new(0, "ym2608", 0, 0x100, 0x01), // reset
            ChipWrite::new(0, "ym2608", 0, 0x101, 0x02), // DRAM x8, shift 5
            ChipWrite::new(0, "ym2608", 0, 0x102, 0x04), // start byte 0x80
            ChipWrite::new(0, "ym2608", 0, 0x103, 0x00),
            ChipWrite::new(0, "ym2608", 0, 0x104, 0x04), // stop byte 0x9F
            ChipWrite::new(0, "ym2608", 0, 0x105, 0x00),
            ChipWrite::new(0, "ym2608", 0, 0x100, 0x60), // external write mode
        ];
        for &b in &data {
            writes.push(ChipWrite::new(0, "ym2608", 0, 0x108, b as u32));
        }
        writes.extend([
            ChipWrite::new(100, "ym2608", 0, 0x109, 0x00),
            ChipWrite::new(100, "ym2608", 0, 0x10A, 0x40),
            ChipWrite::new(100, "ym2608", 0, 0x10B, 200),
            ChipWrite::new(100, "ym2608", 0, 0x101, 0xC0),
            ChipWrite::new(101, "ym2608", 0, 0x100, 0xA0), // play external memory
        ]);
        let mut vgm = synthetic(writes, None);
        vgm.roms.push(RomSlice {
            chip: 0,
            rom_size: 0x200,
            start: 0,
            data: Vec::new(),
            kind: "ym2608_deltat",
            time: 0,
        });
        let mem = replay_deltat_memory(&vgm, &assemble_rom(&vgm, "ym2608_deltat", 0, 0, false, true));
        assert_eq!(&mem[0x80..0x80 + data.len()], &data[..]);
        let song = analyze_default(&vgm);
        assert_eq!(song.ym2608_b_samples.len(), 1);
        assert_eq!(&song.ym2608_b_samples[0].data[..data.len()], &data[..]);
    }

    #[test]
    fn test_mixed_log_warns_and_keeps_the_opna() {
        let mut writes = fm_note_default(0);
        writes.push(ChipWrite::new(100, "ym2612", 0, 0x28, 0xF0));
        writes.push(ChipWrite::new(150, "ym2612", 0, 0x2A, 0x40));
        let song = analyze_default(&synthetic(writes, None));
        assert_eq!(song.fm_chip, "ym2608");
        assert!(
            song.warnings.iter().any(|w| w.contains("YM2612")),
            "{:?}",
            song.warnings
        );
    }

    #[test]
    fn test_no_fm_keys_is_reported() {
        let writes = vec![ChipWrite::new(100, "ym2608", 0, 0x10, 0x01)];
        let song = analyze_default(&synthetic(writes, None));
        assert!(
            song.warnings.iter().any(|w| w.contains("key-on")),
            "{:?}",
            song.warnings
        );
    }

    #[test]
    fn test_ssg_pitch_uses_the_opna_divider() {
        let period = 0x1FF;
        let song = analyze_default(&base());
        let ev = song
            .events
            .iter()
            .find(|e| e.on && (6..9).contains(&e.ch))
            .expect("ssg event");
        let want = ay_note(CLOCK, period, YM2608_SSG_DIVIDER as f64);
        assert_eq!((ev.note, ev.e5), want);
        assert_ne!(
            ay_note(CLOCK, period, YM2608_SSG_DIVIDER as f64),
            ay_note(CLOCK, period, 16.0),
            "the test would not notice a YM2203 divider slipping in"
        );
    }

    #[test]
    fn test_rhythm_channels_get_their_releases() {
        let song = analyze_default(&base());
        let offs: BTreeSet<i32> = song
            .events
            .iter()
            .filter(|e| !e.on && e.pcm)
            .map(|e| e.ch)
            .collect();
        assert!(offs.contains(&9));
    }
}

#[allow(non_snake_case)]
mod TestYm2608ExtChannel3 {
    use super::*;

    fn writes() -> Vec<ChipWrite> {
        let mut w = vec![ChipWrite::new(90, "ym2608", 0, 0x27, 0x40)]; // channel 3 special
        for op in 0..4u32 {
            // four operators audible
            w.push(ChipWrite::new(95, "ym2608", 0, 0x30 | (op << 2) | 2, 0x01));
        }
        for group in 0..3u32 {
            // the split frequencies
            w.push(ChipWrite::new(100, "ym2608", 0, 0xAC | group, (4 << 3) | 1));
            w.push(ChipWrite::new(100, "ym2608", 0, 0xA8 | group, 0x10 + group));
        }
        w.extend([
            ChipWrite::new(100, "ym2608", 0, 0xA4 | 2, (5 << 3) | 2),
            ChipWrite::new(100, "ym2608", 0, 0xA0 | 2, 0x30),
            ChipWrite::new(100, "ym2608", 0, 0xB0 | 2, 0x07),
            ChipWrite::new(100, "ym2608", 0, 0xB4 | 2, 0xC0),
            ChipWrite::new(110, "ym2608", 0, 0x28, (0xF << 4) | 2),
        ]);
        w.extend(fm_note_default(3)); // a block-B voice
        w
    }

    #[test]
    fn test_ext_is_detected() {
        let song = analyze_default(&synthetic(writes(), None));
        assert!(song.ym2608_ext);
    }

    #[test]
    fn test_columns_and_second_block() {
        let song = analyze_default(&synthetic(writes(), None));
        let mut split: Vec<i32> = song
            .events
            .iter()
            .filter(|e| e.on && (2..6).contains(&e.ch))
            .map(|e| e.ch)
            .collect();
        split.sort();
        assert_eq!(split.len(), 4, "channel 3 must open four columns");
        let mut b_notes: Vec<i32> = song
            .events
            .iter()
            .filter(|e| e.on && e.ch >= 6)
            .map(|e| e.ch)
            .collect();
        b_notes.sort();
        assert_eq!(&b_notes[..1], &[6], "FM4 belongs to column 6, not 3");
    }

    #[test]
    fn test_pitch_matches_the_shared_opn_formula() {
        let song = analyze_default(&synthetic(writes(), None));
        let ev = song
            .events
            .iter()
            .find(|e| e.on && e.ch == 6)
            .expect("block-B event");
        let want = ym2612_note_for(4, 0x400, CLOCK, YM2612_FREQ_BASE);
        assert_eq!((ev.note, ev.e5), want);
    }
}

#[allow(non_snake_case)]
mod TestYm2608Timing {
    use super::*;
    use vgm2fur::timing::_keyons_by_channel;

    fn chips() -> HashSet<&'static str> {
        ["ym2608"].into_iter().collect()
    }

    #[test]
    fn test_key_on_per_block_channel() {
        let mut writes = fm_note_default(0);
        writes.extend(fm_note_default(4));
        let vgm = synthetic(writes, None);
        let got = _keyons_by_channel(&vgm, Some(&chips()));
        let mut voices: Vec<i32> = got.keys().map(|k| k - 2000).collect();
        voices.sort();
        assert_eq!(voices, vec![0, 4], "one onset lane per FM voice");
    }

    #[test]
    fn test_rhythm_keys_are_not_fm_onsets() {
        let mut writes = fm_note_default(0);
        writes.push(ChipWrite::new(500, "ym2608", 0, 0x10, 0x3F));
        let got = _keyons_by_channel(&synthetic(writes, None), Some(&chips()));
        assert_eq!(got.values().map(|v| v.len()).sum::<usize>(), 1);
    }
}

#[allow(non_snake_case)]
mod TestYm2608Writer {
    use super::*;

    pub(super) fn notes(ext: bool) -> Vec<ChipWrite> {
        let mut writes = Vec::new();
        if ext {
            writes.push(ChipWrite::new(90, "ym2608", 0, 0x27, 0x40));
            for op in 0..4u32 {
                writes.push(ChipWrite::new(95, "ym2608", 0, 0x30 | (op << 2) | 2, 0x01));
            }
            for group in 0..3u32 {
                writes.push(ChipWrite::new(100, "ym2608", 0, 0xAC | group, (4 << 3) | 1));
                writes.push(ChipWrite::new(100, "ym2608", 0, 0xA8 | group, 0x10 + group));
            }
            writes.extend([
                ChipWrite::new(100, "ym2608", 0, 0xA4 | 2, (5 << 3) | 2),
                ChipWrite::new(100, "ym2608", 0, 0xA0 | 2, 0x30),
                ChipWrite::new(100, "ym2608", 0, 0xB0 | 2, 0x07),
                ChipWrite::new(100, "ym2608", 0, 0xB4 | 2, 0xC0),
                ChipWrite::new(110, "ym2608", 0, 0x28, (0xF << 4) | 2),
            ]);
        }
        writes.extend(fm_note(if ext { 1 } else { 0 }, 4, 0x400, 100, 7900, 0));
        writes.extend(ssg_voice_default());
        writes.extend([
            ChipWrite::new(90, "ym2608", 0, 0x11, 0x3F),
            rhythm_level_default(0, 26),
            rhythm_key(400, 0x11, 0),
            // Hi-hat again later, so two voices and two rows exist.
            rhythm_level_default(3, 12),
            rhythm_key(900, 0x08, 0),
        ]);
        writes.extend(delta_t_play_default(1200, 0x260, 0x3FF, 0x4000));
        writes
    }

    fn song(ext: bool) -> Song {
        analyze_default(&synthetic(notes(ext), Some(&rom())))
    }

    #[test]
    fn test_chip_id_and_span() {
        let sk = parse_fur_skeleton(&write(&song(false)));
        assert_eq!(sk.chips, vec![CHIP_YM2608]);
    }

    #[test]
    fn test_ext_chip_id_and_span() {
        let sk = parse_fur_skeleton(&write(&song(true)));
        assert_eq!(sk.chips, vec![CHIP_YM2608_EXT]);
    }

    #[test]
    fn test_clock_travels_as_furnaces_own_selection() {
        let data = write(&song(false));
        assert!(has(&data, b"clockSel=1"));
        let zero = vec![0u8; 0x800];
        let mut vgm = synthetic(notes(false), Some(&zero));
        vgm.ym2608_clock = 8_000_000;
        let mut odd = synthetic(notes(false), Some(&rom()));
        odd.ym2608_clock = 7_920_000;
        assert!(has(&write(&analyze_default(&odd)), b"customClock=7920000"));
        assert!(!has(&write(&analyze_default(&vgm)), b"customClock"));
    }

    #[test]
    fn test_instrument_types_and_the_empty_kit_map() {
        let data = write(&song(false));
        let types = ins_types(&data);
        assert!(types.contains(&INS_FM), "{types:?}");
        assert!(types.contains(&INS_ADPCMB), "{types:?}");
        assert_eq!(types.iter().filter(|&&t| t == INS_ADPCMA).count(), 1);
    }

    #[test]
    fn test_deltat_sample_lands_in_the_opnas_one_bank() {
        let data = write(&song(false));
        let mut blocks = Vec::new();
        for blk in iter_blocks(&data).expect("iter_blocks") {
            if blk.magic == *b"SMP2" {
                blocks.push(parse_smp2_payload(blk.payload).expect("smp2"));
            }
        }
        assert!(!blocks.is_empty(), "no delta-T sample written");
        for smp in &blocks {
            assert_eq!(smp.depth, 6); // DIV_SAMPLE_DEPTH_ADPCM_B
            let masks: Vec<u32> = (0..4usize)
                .map(|i| {
                    u32::from_le_bytes([
                        smp.presence[i * 4],
                        smp.presence[i * 4 + 1],
                        smp.presence[i * 4 + 2],
                        smp.presence[i * 4 + 3],
                    ])
                })
                .collect();
            assert_ne!(masks[0] & 1, 0, "{masks:?}"); // bank 0, system 0
            assert!(masks[1..].iter().all(|&m| m == 0), "{masks:?}");
        }
    }

    #[test]
    fn test_deltat_body_is_padded_to_the_stored_quantum() {
        let data = write(&song(false));
        let smp = iter_blocks(&data)
            .expect("iter_blocks")
            .into_iter()
            .find(|blk| blk.magic == *b"SMP2")
            .map(|blk| parse_smp2_payload(blk.payload).expect("smp2"))
            .expect("SMP2 block");
        assert_eq!(
            ((smp.n_frames as usize + 1) / 2) % 0x100,
            0,
            "a short body makes Furnace read the next sample"
        );
    }

    #[test]
    fn test_channel_names_follow_furnace() {
        let data = write(&song(false));
        for label in [b"Kick".as_slice(), b"Snare", b"Top", b"HiHat", b"Tom", b"Rim"] {
            assert!(has(&data, label), "{label:?} missing");
        }
        let ext = write(&song(true));
        assert!(has(&ext, b"FM 3 OP1"));
    }

    #[test]
    fn test_report_names_the_chip_and_its_parts() {
        let text = format_report(&song(true));
        assert!(text.contains("YM2608 Ext x1 @ 7.9872 MHz"), "{text}");
        assert!(text.contains("SSG notes"), "{text}");
    }
}

#[allow(non_snake_case)]
mod TestYm2608RoundTrip {
    use super::*;

    fn round(name: &str, ext: bool) -> (VgmFile, VgmFile) {
        let writes = TestYm2608Writer::notes(ext);
        let vgm = synthetic(writes, Some(&rom()));
        // Pin the grid: at the measured rate a synthetic log this short gets
        // one or two rows, and two kit hits would share a cell legitimately.
        let song = analyze(
            &vgm,
            AnalyzeOptions {
                speed: Some(1),
                ..Default::default()
            },
        )
        .expect("analyze");
        let dir = tmp_dir();
        let fur = dir.join(format!("{name}.fur"));
        std::fs::write(
            &fur,
            write_fur(
                &song,
                WriteOptions {
                    compress: false,
                    ..Default::default()
                },
            ),
        )
        .expect("write fur");
        let out = dir.join(format!("{name}.vgm"));
        export_vgm(&fur, &out).expect("export_vgm");
        (vgm, load_vgm(&out).expect("load exported vgm"))
    }

    fn level_per_voice(vgm: &VgmFile) -> BTreeMap<String, BTreeSet<i32>> {
        let mut levels: BTreeMap<String, BTreeSet<i32>> = BTreeMap::new();
        let mut held = [0x1F; 6]; // ymfm_adpcm.cpp:48-51
        for w in &vgm.writes {
            if w.chip != "ym2608" || (w.reg & 0x100) != 0 {
                continue;
            }
            let r = (w.reg & 0xFF) as usize;
            if (0x18..=0x1D).contains(&r) {
                held[r - 0x18] = (w.val & 0x1F) as i32;
            }
            if r == 0x10 && (w.val & 0x80) == 0 {
                for i in 0..6usize {
                    if w.val & (1 << i) != 0 {
                        levels
                            .entry(RHYTHM_VOICES[i].0.to_string())
                            .or_default()
                            .insert(held[i]);
                    }
                }
            }
        }
        levels
    }

    fn plays(vgm: &VgmFile) -> Vec<(i64, i64)> {
        let mut reg: BTreeMap<u8, i64> = BTreeMap::new();
        let mut out = Vec::new();
        for w in &vgm.writes {
            if w.chip != "ym2608" || (w.reg & 0x100) == 0 {
                continue;
            }
            let r = (w.reg & 0xFF) as u8;
            if r > 0x0B {
                continue;
            }
            reg.insert(r, w.val as i64);
            if r != 0x00 || (w.val & 0x80) == 0 || (w.val & 1) != 0 {
                continue;
            }
            let start = (reg.get(&0x03).copied().unwrap_or(0) << 8
                | reg.get(&0x02).copied().unwrap_or(0))
                << 5;
            let end = ((reg.get(&0x05).copied().unwrap_or(0) << 8
                | reg.get(&0x04).copied().unwrap_or(0))
                << 5)
                | 31;
            out.push((
                end - start + 1,
                reg.get(&0x0A).copied().unwrap_or(0) << 8
                    | reg.get(&0x09).copied().unwrap_or(0),
            ));
        }
        out
    }

    #[test]
    fn test_rhythm_voices_keep_their_own_levels() {
        skip_without_furnace!();
        let (src, exp) = round("rt_rhythm", false);
        let a = level_per_voice(&src);
        let b = level_per_voice(&exp);
        let keys: BTreeSet<&str> = a.keys().map(|k| k.as_str()).collect();
        assert_eq!(keys, BTreeSet::from(["BD", "HH", "TM"]));
        assert!(!b.is_empty(), "Furnace wrote no rhythm keys");
        for voice in a.keys() {
            assert_eq!(a.get(voice), b.get(voice), "{voice}");
        }
    }

    #[test]
    fn test_deltat_registers_reach_the_same_span_and_step() {
        skip_without_furnace!();
        let (src, exp) = round("rt_delta", false);
        let a = plays(&src);
        let b = plays(&exp);
        assert!(!b.is_empty(), "Furnace wrote no delta-T starts");
        let want = deltat_store_body(&rom()[0x260..0x400]).len() as i64;
        assert_eq!(b[0].0, want);
        assert!((b[0].1 - a[0].1).abs() <= 8);
    }

    #[test]
    fn test_audit_fields_are_clean() {
        skip_without_furnace!();
        let (src, exp) = round("rt_audit", true);
        let rep = vgm2fur::audit::compare_ym2608_samples(&src, &exp);
        assert!(!rep.is_empty(), "the audit saw no OPNA in the log");
        for kind in ["rhythm", "delta_t"] {
            let d = rep.get(kind).expect(kind);
            assert_eq!(d["missed"].as_i64(), Some(0), "{kind} {d}");
            for field in ["data", "length", "vol", "pan", "rate"] {
                assert_eq!(d[field].as_i64(), Some(0), "{kind} {field} {d}");
            }
        }
    }

    #[test]
    fn test_fm_pitch_survives_the_split() {
        skip_without_furnace!();
        let (src, exp) = round("rt_pitch", true);
        for ch in 0..6 {
            let runs = pitch_runs(&src, &exp, ch, 0.25, 60.0);
            assert!(runs.is_empty(), "ch{ch}: {:?}", &runs[..runs.len().min(2)]);
        }
    }
}
