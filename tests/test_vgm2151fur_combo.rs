//! Merged logs: several chip families inside one module.
//!
//! Port of `tests/test_vgm2151fur_combo.py` at vgm2151fur a8edb6e.

mod common;
#[allow(unused_imports)]
use common::*;

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::path::PathBuf;

use vgm2fur::analyze::{_family_sounds, analyze, format_report, AnalyzeOptions};
use vgm2fur::audit::compare_dac;
use vgm2fur::furio::{iter_blocks, parse_fur, CHIP_CHANNELS};
use vgm2fur::furwrite::{
    _append_system_names, _channel_vol_max, write_fur, WriteOptions, CHIP_AY8910, CHIP_MSM6295,
    CHIP_OPL2, CHIP_OPL3, CHIP_OPLL_DRUMS, CHIP_SMS, CHIP_YM2151, CHIP_YM2203, CHIP_YM2612,
};
use vgm2fur::model::Song;
use vgm2fur::opl::{OPLState, N as OPL_N, SLOT_ADDR};
use vgm2fur::parity::py_round;
use vgm2fur::sn76489::collect_sn76489;
use vgm2fur::vgm::{load_vgm, ChipWrite, DacStreamPlay, RomSlice, VgmFile, VGM_RATE};
use vgm2fur::ym2612dac::{collect_dac, CollectDacOptions, DAC_RELEASE_SAMPLES};

fn tmp() -> PathBuf {
    let path = std::env::temp_dir().join("vgm2151fur_combo");
    std::fs::create_dir_all(&path).expect("temp dir");
    path
}

/// A key-on spacing wide enough that the grid estimator puts one row per note,
/// so a test that counts notes on a channel means something.
const PULSE: i64 = 20000;

fn contains_bytes(hay: &[u8], needle: &[u8]) -> bool {
    hay.windows(needle.len()).any(|w| w == needle)
}

// ---------------------------------------------------------- synthetic logs

#[derive(Clone)]
struct VgmOpts {
    total_samples: i64,
    ym2151_clock: u32,
    ym2203_clock: u32,
    ym2612_clock: u32,
    ym2610_clock: u32,
    ym2608_clock: u32,
    opl_clock: u32,
    opl2_clock: u32,
    opl3_clock: u32,
    ym2413_clock: u32,
    sn76489_clock: u32,
    k007232_clock: u32,
    msm6295_clock: u32,
    ay8910_clock: u32,
    dac_data: Vec<u8>,
    dac_events: Vec<(i64, u8, i64)>,
    msm6295_pin7: bool,
    sn76489_stereo: bool,
}

impl Default for VgmOpts {
    fn default() -> Self {
        VgmOpts {
            total_samples: 400000,
            ym2151_clock: 0,
            ym2203_clock: 0,
            ym2612_clock: 0,
            ym2610_clock: 0,
            ym2608_clock: 0,
            opl_clock: 0,
            opl2_clock: 0,
            opl3_clock: 0,
            ym2413_clock: 0,
            sn76489_clock: 0,
            k007232_clock: 0,
            msm6295_clock: 0,
            ay8910_clock: 0,
            dac_data: Vec::new(),
            dac_events: Vec::new(),
            msm6295_pin7: false,
            sn76489_stereo: false,
        }
    }
}

/// A synthetic log. Unnamed chip clocks stay zero, as an absent chip's.
fn _vgm(mut writes: Vec<ChipWrite>, o: VgmOpts) -> VgmFile {
    writes.sort_by(|a, b| {
        (a.sample, a.chip, a.reg).cmp(&(b.sample, b.chip, b.reg))
    });
    VgmFile {
        path: PathBuf::from("merged.vgm"),
        buf: Vec::new(),
        version: 0x161,
        total_samples: o.total_samples,
        loop_samples: 0,
        loop_file_pos: None,
        loop_sample: None,
        gd3: Default::default(),
        roms: Vec::new(),
        writes,
        ym2151_clock: o.ym2151_clock,
        ym2203_clock: o.ym2203_clock,
        ym2612_clock: o.ym2612_clock,
        ym2610_clock: o.ym2610_clock,
        ym2608_clock: o.ym2608_clock,
        opl_clock: o.opl_clock,
        opl2_clock: o.opl2_clock,
        opl3_clock: o.opl3_clock,
        ym2413_clock: o.ym2413_clock,
        sn76489_clock: o.sn76489_clock,
        k007232_clock: o.k007232_clock,
        msm6295_clock: o.msm6295_clock,
        ay8910_clock: o.ay8910_clock,
        dac_data: o.dac_data,
        dac_events: o.dac_events,
        msm6295_pin7: o.msm6295_pin7,
        sn76489_stereo: o.sn76489_stereo,
        ..Default::default()
    }
}

fn _stream(sample: i64, offset: u32, length: u32) -> DacStreamPlay {
    _stream_mode(sample, offset, length, 1, 0x2A, 11025, 0, 1)
}

#[allow(clippy::too_many_arguments)]
fn _stream_mode(
    sample: i64,
    offset: u32,
    length: u32,
    mode: u8,
    reg: u8,
    rate: u32,
    chip_id: u8,
    step: u32,
) -> DacStreamPlay {
    DacStreamPlay {
        sample,
        chip_type: 0x02,
        chip_id,
        stream: 0,
        port: 0,
        reg,
        offset,
        length,
        mode,
        rate,
        loop_: mode & 0x80 != 0,
        reverse: mode & 0x10 != 0,
        step,
        ..Default::default()
    }
}

// ------------------------------------------------------------- note builders

/// One YM2612/YM2203 voice, keyed with the encoding its $28 uses.
fn opn_note(t: i64, ch: i32) -> Vec<ChipWrite> {
    opn_note_kind(t, ch, "ym2612")
}

fn opn_note_kind(t: i64, ch: i32, kind: &'static str) -> Vec<ChipWrite> {
    let port = ch / 3;
    let slot = (ch % 3) as u32;
    let base: u32 = 0x100 * port as u32;
    let mut out: Vec<ChipWrite> = Vec::new();
    for page in [0x30u32, 0x40, 0x50, 0x60, 0x70, 0x80, 0x90] {
        for op in 0..4u32 {
            let val = if page == 0x30 {
                0x01
            } else if page == 0x70 {
                0x1F
            } else {
                0
            };
            out.push(ChipWrite::new(
                t,
                kind,
                0,
                base | page | (op << 2) | slot,
                val,
            ));
        }
    }
    let key = if ch < 3 { ch as u32 } else { (ch + 1) as u32 };
    out.push(ChipWrite::new(t, kind, 0, base | 0xB0 | slot, 0x07));
    out.push(ChipWrite::new(t, kind, 0, base | 0xB4 | slot, 0xC0));
    let fnum: u32 = 0x280;
    let block: u32 = 4;
    out.push(ChipWrite::new(
        t + 1,
        kind,
        0,
        base | 0xA4 | slot,
        (block << 3) | ((fnum >> 8) & 7),
    ));
    out.push(ChipWrite::new(
        t + 1,
        kind,
        0,
        base | 0xA0 | slot,
        fnum & 0xFF,
    ));
    out.push(ChipWrite::new(t + 2, kind, 0, 0x28, (0xF << 4) | key));
    out.push(ChipWrite::new(9000, kind, 0, 0x28, key));
    out
}

/// One OPL voice. An OPL3 channel past eight rides its second bank.
fn opl_note(t: i64, ch: i32) -> Vec<ChipWrite> {
    opl_note_kind(t, ch, "opl2")
}

fn opl_note_kind(t: i64, ch: i32, kind: &'static str) -> Vec<ChipWrite> {
    let st = OPLState::new(kind, false);
    let mut out: Vec<ChipWrite> = Vec::new();
    for row in 0..st.slots.len() {
        let slot = st.slots[row][ch as usize];
        if slot == OPL_N {
            continue;
        }
        let addr = SLOT_ADDR[slot as usize] as u32;
        out.push(ChipWrite::new(t, kind, 0, addr + 0x20, 0x01));
        out.push(ChipWrite::new(
            t,
            kind,
            0,
            addr + 0x40,
            if row == 0 { 0x30 } else { 0x10 },
        ));
        out.push(ChipWrite::new(t, kind, 0, addr + 0x60, 0xF5));
        out.push(ChipWrite::new(t, kind, 0, addr + 0x80, 0x94));
        out.push(ChipWrite::new(t, kind, 0, addr + 0xE0, row as u32));
    }
    let cm = st.chan_map[ch as usize] as u32;
    let fnum: u32 = 580;
    let block: u32 = 4;
    let hi = (block << 2) | ((fnum >> 8) & 3);
    out.push(ChipWrite::new(t + 1, kind, 0, cm + 0xA0, fnum & 0xFF));
    out.push(ChipWrite::new(t + 2, kind, 0, cm + 0xB0, hi | 0x20));
    out.push(ChipWrite::new(t + 2, kind, 0, cm + 0xC0, 0x30));
    out.push(ChipWrite::new(9000, kind, 0, cm + 0xB0, hi));
    out
}

/// One YM2151 voice: the operator pages, then $08 with its channel.
fn opm_note(t: i64, ch: i32) -> Vec<ChipWrite> {
    let ch = ch as u32;
    let mut out: Vec<ChipWrite> = vec![ChipWrite::new(t, "ym2151", 0, 0x20 | ch, 0xC7)];
    for op in 0..4u32 {
        let base = (op << 3) | ch;
        out.push(ChipWrite::new(t, "ym2151", 0, 0x40 | base, 0x11));
        out.push(ChipWrite::new(t, "ym2151", 0, 0x60 | base, 0x20));
        out.push(ChipWrite::new(t, "ym2151", 0, 0x80 | base, 0xF5));
        out.push(ChipWrite::new(t, "ym2151", 0, 0xA0 | base, 0xF4));
        out.push(ChipWrite::new(t, "ym2151", 0, 0xC0 | base, 0x75));
        out.push(ChipWrite::new(t, "ym2151", 0, 0xE0 | base, 0x04));
    }
    out.push(ChipWrite::new(t + 1, "ym2151", 0, 0x28 | ch, 0x4F));
    out.push(ChipWrite::new(t + 1, "ym2151", 0, 0x30 | ch, 0x40));
    out.push(ChipWrite::new(t + 2, "ym2151", 0, 0x08, 0x78 | ch));
    out.push(ChipWrite::new(9000, "ym2151", 0, 0x08, ch));
    out
}

/// One YM2413 voice on a ROM preset.
fn opll_note(t: i64, ch: i32) -> Vec<ChipWrite> {
    opll_note_off(t, ch, Some(9000))
}

fn opll_note_off(t: i64, ch: i32, off: Option<i64>) -> Vec<ChipWrite> {
    let ch = ch as u32;
    let fnum: u32 = 500;
    let block: u32 = 1;
    let preset: u32 = 6;
    let volume: u32 = 8;
    let mut out = vec![
        ChipWrite::new(t, "opll", 0, 0x10 + ch, fnum & 0xFF),
        ChipWrite::new(
            t + 1,
            "opll",
            0,
            0x20 + ch,
            (block << 1) | ((fnum >> 8) & 1) | 0x10,
        ),
        ChipWrite::new(t + 2, "opll", 0, 0x30 + ch, (preset << 4) | volume),
    ];
    if let Some(off) = off {
        out.push(ChipWrite::new(
            off,
            "opll",
            0,
            0x20 + ch,
            (block << 1) | ((fnum >> 8) & 1),
        ));
    }
    out
}

/// A Genesis PSG gate: latch the period, lift the attenuation, close it.
fn psg_tone(t: i64, voice: u32) -> Vec<ChipWrite> {
    let period: u32 = 300;
    let atten: u32 = 6;
    let hold: i64 = 8000;
    vec![
        ChipWrite::new(
            t,
            "sn76489",
            0,
            0,
            0x80 | (voice << 5) | ((period >> 8) & 0x0F),
        ),
        ChipWrite::new(t, "sn76489", 0, 0, period & 0xFF),
        ChipWrite::new(
            t + 1,
            "sn76489",
            0,
            0,
            0x80 | (voice << 5) | 0x10 | atten,
        ),
        ChipWrite::new(
            t + hold,
            "sn76489",
            0,
            0,
            0x80 | (voice << 5) | 0x10 | 15,
        ),
    ]
}

// ------------------------------------------------------------ stream parsing

/// A loadable YM2612 log with type-0x00 banks and a command stream.
fn _raw_pcm_log(body: &[u8], banks: &[&[u8]], clock: u32) -> Vec<u8> {
    let mut buf = vec![0u8; 0xC0];
    buf[0..4].copy_from_slice(b"Vgm ");
    buf[8..12].copy_from_slice(&0x161u32.to_le_bytes());
    buf[0x18..0x1C].copy_from_slice(&(VGM_RATE as u32).to_le_bytes());
    buf[0x2C..0x30].copy_from_slice(&(clock & 0x3FFF_FFFF).to_le_bytes());
    buf[0x34..0x38].copy_from_slice(&((0xC0u32 - 0x34).to_le_bytes()));
    let mut data: Vec<u8> = Vec::new();
    for b in banks {
        data.extend_from_slice(&[0x67, 0x66, 0x00]);
        data.extend_from_slice(&(b.len() as u32).to_le_bytes());
        data.extend_from_slice(b);
    }
    let mut out = buf;
    out.extend_from_slice(&data);
    out.extend_from_slice(body);
    out.push(0x66);
    let len = (out.len() as u32 - 4).to_le_bytes();
    out[4..8].copy_from_slice(&len);
    out
}

fn _load(raw: &[u8]) -> VgmFile {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("stream.vgm");
    std::fs::write(&path, raw).expect("write temp vgm");
    load_vgm(&path).expect("load vgm")
}

// --------------------------------------------------------- TestStreamCommands

#[allow(non_snake_case)]
mod TestStreamCommands {
    use super::*;

    const BANK: [u8; 512] = {
        let mut out = [0u8; 512];
        let mut i = 0;
        while i < 512 {
            out[i] = (((i * 31) % 251) + 4) as u8;
            i += 1;
        }
        out
    };

    /// Commands at real sample times, so a play can fall on the last one.
    fn _log(starts: &[(i64, u32, u32, u8)]) -> VgmFile {
        let head: &[u8] = &[
            0x90, 0x00, 0x02, 0x00, 0x2A, 0x91, 0x00, 0x00, 0x01, 0x00, 0x92, 0x00, 0x11, 0x2B,
            0x00, 0x00, 0x52, 0x2B, 0x80,
        ];
        let mut body: Vec<u8> = head.to_vec();
        let mut last: i64 = 0;
        for &(sample, offset, length, mode) in starts {
            if sample > last {
                body.push(0x61);
                body.extend_from_slice(&((sample - last) as u16).to_le_bytes());
                last = sample;
            }
            body.push(0x93);
            body.push(0x00);
            body.extend_from_slice(&offset.to_le_bytes());
            body.push(mode);
            body.extend_from_slice(&length.to_le_bytes());
        }
        _load(&_raw_pcm_log(&body, &[&BANK], 7670453))
    }

    #[test]
    fn test_a_stream_start_is_recorded() {
        let v = _log(&[(100, 0, 64, 1)]);
        assert_eq!(v.dac_stream_plays.len(), 1);
        let play = &v.dac_stream_plays[0];
        assert_eq!(
            (play.chip_type, play.reg, play.rate),
            (0x02, 0x2A, 11025)
        );
        assert_eq!(
            (play.sample, play.offset, play.length),
            (100, 0, 64)
        );
        assert_eq!(play.length_mode(), 1);
        assert!(!(play.loop_ || play.reverse));
    }

    #[test]
    fn test_the_okim6258_path_is_untouched() {
        let mut body: Vec<u8> = vec![0x90, 0x00, 0x17, 0x00, 0x01, 0x92, 0x00];
        body.extend_from_slice(&7813u32.to_le_bytes());
        body.extend_from_slice(&[0x93, 0x00]);
        body.extend_from_slice(&0u32.to_le_bytes());
        body.push(1);
        body.extend_from_slice(&8u32.to_le_bytes());
        let v = _load(&_raw_pcm_log(&body, &[&BANK], 7670453));
        assert_eq!(v.dac_stream_plays, vec![]);
        assert_eq!(v.msm6258_plays.len(), 1);
    }

    #[test]
    fn test_mode_zero_moves_the_cursor_only() {
        // A zero-length start parks the read cursor; the next play, whose own
        // offset says "ignore me", takes the song from there.
        let v = _log(&[(100, 256, 0, 0), (200, 0xFFFFFFFF, 32, 1)]);
        assert_eq!(v.dac_stream_plays.len(), 1);
        let play = &v.dac_stream_plays[0];
        assert_eq!(play.offset, 256);
        assert_eq!((play.length, play.length_mode()), (32, 1));
    }

    #[test]
    fn test_loop_and_reverse_flags() {
        let loop_play = &_log(&[(100, 0, 64, 0x81)]).dac_stream_plays[0];
        assert!(loop_play.loop_);
        assert!(!loop_play.reverse);
        assert_eq!(loop_play.length_mode(), 1);
        let back = &_log(&[(100, 0, 64, 0x11)]).dac_stream_plays[0];
        assert!(back.reverse);
        assert!(!back.loop_);
    }

    #[test]
    fn test_a_stream_without_its_bank_is_reported() {
        let mut body: Vec<u8> = vec![0x90, 0x00, 0x02, 0x00, 0x2A, 0x93, 0x00];
        body.extend_from_slice(&0u32.to_le_bytes());
        body.push(1);
        body.extend_from_slice(&8u32.to_le_bytes());
        let raw = _raw_pcm_log(&body, &[], 7670453);
        let v = _load(&raw);
        assert!(
            v.warnings.iter().any(|w| w.contains("no PCM data block")),
            "{:?}",
            v.warnings
        );
    }
}

// ------------------------------------------------------- TestStreamCollection

#[allow(non_snake_case)]
mod TestStreamCollection {
    use super::*;

    const BANK: [u8; 4096] = {
        let mut out = [0u8; 4096];
        let mut i = 0;
        while i < 4096 {
            out[i] = (((i * 7) % 253) + 1) as u8;
            i += 1;
        }
        out
    };

    /// The type-0x00 bank is unsigned; a module sample is signed.
    fn _signed(data: &[u8]) -> Vec<u8> {
        data.iter().map(|&x| x.wrapping_sub(0x80)).collect()
    }

    fn _file(plays: Vec<DacStreamPlay>, total: Option<i64>) -> VgmFile {
        let mut v = _vgm(
            vec![ChipWrite::new(0, "ym2612", 0, 0x2B, 0x80)],
            VgmOpts {
                ym2612_clock: 7670453,
                dac_data: BANK.to_vec(),
                total_samples: total.unwrap_or(20000),
                ..Default::default()
            },
        );
        v.dac_stream_plays = plays;
        v
    }

    #[test]
    fn test_slices_take_the_exact_byte_range() {
        let v = _file(vec![_stream(0, 0, 128), _stream(9000, 256, 64)], None);
        let (samples, plays, warn, stats) = collect_dac(
            &v,
            &CollectDacOptions {
                slices: true,
                ..Default::default()
            },
        );
        assert!(warn.is_empty());
        assert_eq!(
            samples.iter().map(|s| s.data.clone()).collect::<Vec<_>>(),
            vec![_signed(&BANK[..128]), _signed(&BANK[256..320])]
        );
        assert_eq!(
            plays.iter().map(|p| p.onset).collect::<Vec<_>>(),
            vec![0, 9000]
        );
        assert_eq!(stats.int("dac_stream_plays"), 2);
    }

    #[test]
    fn test_chunks_are_contiguous_and_capped() {
        let v = _file(vec![_stream(0, 0, BANK.len() as u32)], None);
        let (samples, _plays, _warn, stats) =
            collect_dac(&v, &CollectDacOptions::default());
        assert!(samples.len() > 1);
        assert!(samples.len() <= DAC_RELEASE_SAMPLES);
        let joined: Vec<u8> = samples.iter().flat_map(|s| s.data.clone()).collect();
        assert_eq!(joined, _signed(&BANK));
        assert_eq!(stats.int("dac_stream_chunks"), samples.len() as i64);
    }

    #[test]
    fn test_a_long_stream_spreads_the_budget_over_its_length() {
        // One play of a whole bank: every chunk starts where the bytes say, at
        // the stream's own rate, and none of them overlaps the next.
        let v = _file(vec![_stream(0, 0, BANK.len() as u32)], None);
        let (samples, plays, _warn, _stats) =
            collect_dac(&v, &CollectDacOptions::default());
        let mut done: i64 = 0;
        for (play, smp) in plays.iter().zip(samples.iter()) {
            let want = done as f64 * VGM_RATE as f64 / smp.c4_rate as f64;
            assert!(
                (play.onset as f64 - want).abs() <= 1.0,
                "{} != {}",
                play.onset,
                want
            );
            done += smp.data.len() as i64;
        }
        assert_eq!(done, BANK.len() as i64);
    }

    #[test]
    fn test_every_chunk_keeps_the_stream_rate() {
        let v = _file(vec![_stream_mode(0, 0, BANK.len() as u32, 1, 0x2A, 22050, 0, 1)], None);
        let (samples, _plays, _warn, _stats) =
            collect_dac(&v, &CollectDacOptions::default());
        assert!(samples.iter().all(|s| s.c4_rate == 22050));
    }

    #[test]
    fn test_the_end_of_file_flush_is_dropped() {
        let v = _file(
            vec![_stream(0, 0, 400), _stream(20000, 400, 400)],
            Some(20000),
        );
        let (_samples, plays, _warn, stats) =
            collect_dac(&v, &CollectDacOptions::default());
        assert_eq!(stats.int("dac_stream_tail_plays"), 1);
        assert!(plays.iter().all(|p| p.onset < 20000));
    }

    #[test]
    fn test_a_register_that_is_not_the_dac_is_refused() {
        let v = _file(vec![_stream_mode(0, 0, 400, 1, 0x28, 11025, 0, 1)], None);
        let (samples, plays, _warn, stats) =
            collect_dac(&v, &CollectDacOptions::default());
        assert!(samples.is_empty() && plays.is_empty());
        assert_eq!(stats.int("dac_stream_other_reg"), 1);
    }

    #[test]
    fn test_a_second_chip_is_refused() {
        let v = _file(vec![_stream_mode(0, 0, 400, 1, 0x2A, 11025, 1, 1)], None);
        let (_samples, _plays, _warn, stats) =
            collect_dac(&v, &CollectDacOptions::default());
        assert_eq!(stats.int("dac_stream_second_chip"), 1);
    }

    #[test]
    fn test_unsigned_bank_becomes_signed_pcm() {
        // Type 0x00 stores 0x80-centred bytes; a module sample is signed.
        let bank: [u8; 5] = [0x00, 0x40, 0x80, 0xC0, 0xFF];
        let mut v = _vgm(
            vec![ChipWrite::new(0, "ym2612", 0, 0x2B, 0x80)],
            VgmOpts {
                ym2612_clock: 7670453,
                dac_data: bank.to_vec(),
                total_samples: 20000,
                ..Default::default()
            },
        );
        v.dac_stream_plays = vec![_stream(0, 0, bank.len() as u32)];
        let (samples, _plays, _warn, _stats) = collect_dac(
            &v,
            &CollectDacOptions {
                slices: true,
                ..Default::default()
            },
        );
        assert_eq!(samples[0].data, vec![0x80, 0xC0, 0x00, 0x40, 0x7F]);
    }

    #[test]
    fn test_milliseconds_count_the_rate() {
        let v = _file(vec![_stream_mode(0, 0, 100, 2, 0x2A, 11025, 0, 1)], None);
        let (samples, _plays, _warn, _stats) = collect_dac(
            &v,
            &CollectDacOptions {
                slices: true,
                ..Default::default()
            },
        );
        assert_eq!(
            samples[0].data.len(),
            py_round(100.0 * 11025.0 / 1000.0) as usize
        );
    }

    #[test]
    fn test_a_step_reads_every_nth_byte() {
        let v = _file(vec![_stream_mode(0, 0, 64, 1, 0x2A, 11025, 0, 2)], None);
        let (samples, _plays, _warn, _stats) = collect_dac(
            &v,
            &CollectDacOptions {
                slices: true,
                ..Default::default()
            },
        );
        let expected = _signed(&BANK[..64].iter().copied().step_by(2).collect::<Vec<_>>());
        assert_eq!(samples[0].data, expected);
    }

    #[test]
    fn test_reverse_walks_the_range_backwards() {
        let v = _file(vec![_stream_mode(0, 0, 64, 0x11, 0x2A, 11025, 0, 1)], None);
        let (samples, _plays, _warn, _stats) = collect_dac(
            &v,
            &CollectDacOptions {
                slices: true,
                ..Default::default()
            },
        );
        let mut range = BANK[..64].to_vec();
        range.reverse();
        assert_eq!(samples[0].data, _signed(&range));
    }
}

// ------------------------------------------------------------ TestFamilySound

#[allow(non_snake_case)]
mod TestFamilySound {
    use super::*;

    #[test]
    fn test_init_only_setup_is_silent() {
        let mut writes: Vec<ChipWrite> = (0x10..0x20)
            .map(|reg| ChipWrite::new(0, "ym2608", 0, reg, 0x00))
            .collect();
        writes.push(ChipWrite::new(0, "ym2612", 0, 0x27, 0x00));
        let v = _vgm(writes, VgmOpts::default());
        assert!(!_family_sounds(
            &v,
            &HashSet::from(["ym2608", "ym2608_ssg"])
        ));
        assert!(!_family_sounds(&v, &HashSet::from(["ym2612"])));
    }

    #[test]
    fn test_a_rhythm_latch_sounds() {
        let v = _vgm(
            vec![ChipWrite::new(100, "ym2608", 0, 0x10, 0xA0 | 0x20)],
            VgmOpts::default(),
        );
        assert!(_family_sounds(&v, &HashSet::from(["ym2608"])));
    }

    #[test]
    fn test_a_tuned_ssg_voice_sounds() {
        let v = _vgm(
            vec![ChipWrite::new(100, "ym2608_ssg", 0, 1, 0x01)],
            VgmOpts::default(),
        );
        assert!(_family_sounds(
            &v,
            &HashSet::from(["ym2608", "ym2608_ssg"])
        ));
    }

    #[test]
    fn test_a_key_on_sounds() {
        assert!(_family_sounds(
            &_vgm(opn_note(100, 0), VgmOpts::default()),
            &HashSet::from(["ym2612"])
        ));
        assert!(_family_sounds(
            &_vgm(opl_note(100, 0), VgmOpts::default()),
            &HashSet::from(["opl2"])
        ));
        assert!(_family_sounds(
            &_vgm(opm_note(100, 0), VgmOpts::default()),
            &HashSet::from(["ym2151"])
        ));
        assert!(_family_sounds(
            &_vgm(opll_note(100, 0), VgmOpts::default()),
            &HashSet::from(["opll"])
        ));
    }

    #[test]
    fn test_a_dac_write_is_not_an_fm_note() {
        // The stream path decides whether a DAC-only chip earns a system, so
        // the register scan stays silent about it.
        let v = _vgm(
            vec![ChipWrite::new(0, "ym2612", 0, 0x2B, 0x80)],
            VgmOpts {
                dac_events: vec![(10, 1, 0)],
                ..Default::default()
            },
        );
        assert!(!_family_sounds(&v, &HashSet::from(["ym2612"])));
    }

    #[test]
    fn test_stray_opna_setup_does_not_outrank_the_ym2203() {
        // A malformed log can carry a YM2608 init block (and its SSG period
        // left at 1) without an OPNA clock. It must not win the system over a
        // YM2203 that actually keys notes.
        let writes = vec![
            ChipWrite::new(0, "ym2608", 0, 0x10, 0x18),
            ChipWrite::new(0, "ym2608_ssg", 0, 1, 2),
            ChipWrite::new(0, "ym2608_ssg", 0, 0, 1),
            ChipWrite::new(100, "ym2203", 0, 0x28, 0xF0),
            ChipWrite::new(200, "ym2203", 0, 0x28, 0x00),
        ];
        let v = _vgm(
            writes,
            VgmOpts {
                ym2203_clock: 3993600,
                ym2608_clock: 0,
                ..Default::default()
            },
        );
        assert!(_family_sounds(
            &v,
            &HashSet::from(["ym2608", "ym2608_ssg"])
        ));
        let song = analyze(&v, AnalyzeOptions::default()).expect("analyze");
        assert_eq!(song.fm_chip, "ym2203");
    }
}

// ------------------------------------------------------- TestStandaloneSmsPsg

#[allow(non_snake_case)]
mod TestStandaloneSmsPsg {
    use super::*;

    #[test]
    fn test_standalone_sn76489_gets_an_sms_system() {
        let v = _vgm(
            psg_tone(100, 0),
            VgmOpts {
                sn76489_clock: 3579545,
                ..Default::default()
            },
        );
        let song = analyze(&v, AnalyzeOptions::default()).expect("analyze");
        assert_eq!(song.psg_ids, vec![0]);
        assert_eq!(song.psg_count, 1);
        let data = write_fur(&song, WriteOptions::default());
        let m = parse_fur(&data).expect("parse fur");
        assert_eq!(m.chips, vec![CHIP_SMS]);
        assert_eq!(m.n_ch, 4);
        // The plain SMS core is mono; no Game Gear chipType.
        assert!(!contains_bytes(&data, b"chipType=3"));
    }

    #[test]
    fn test_sn_volume_attack_is_a_note() {
        // A clear attenuation jump back up is a new attack on the pattern,
        // not a volume step inside one note.
        let writes = vec![
            ChipWrite::new(0, "sn76489", 0, 0, 0x80 | ((0x200 >> 8) & 0x0F)),
            ChipWrite::new(0, "sn76489", 0, 0, 0x200 & 0xFF),
            ChipWrite::new(1, "sn76489", 0, 0, 0x80 | 0x10 | 10),
            ChipWrite::new(3000, "sn76489", 0, 0, 0x80 | 0x10 | 4),
            ChipWrite::new(9000, "sn76489", 0, 0, 0x80 | 0x10 | 15),
        ];
        let v = _vgm(
            writes,
            VgmOpts {
                sn76489_clock: 3579545,
                ..Default::default()
            },
        );
        let (hits, _offs, _warn) = collect_sn76489(&v, None);
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[1].sample_time, 3000);
        assert_eq!(hits[1].vol, 11);
    }

    #[test]
    fn test_sn_volume_returning_to_start_level_is_an_attack() {
        // The attack is compared with the level the voice is on now, not the
        // note's starting level. A swell that comes back to its start level
        // must still start a new note.
        let writes = vec![
            ChipWrite::new(0, "sn76489", 0, 0, 0x80 | ((0x200 >> 8) & 0x0F)),
            ChipWrite::new(0, "sn76489", 0, 0, 0x200 & 0xFF),
            ChipWrite::new(1, "sn76489", 0, 0, 0x80 | 0x10 | 5),
            ChipWrite::new(2000, "sn76489", 0, 0, 0x80 | 0x10 | 9),
            ChipWrite::new(3000, "sn76489", 0, 0, 0x80 | 0x10 | 5),
        ];
        let v = _vgm(
            writes,
            VgmOpts {
                sn76489_clock: 3579545,
                ..Default::default()
            },
        );
        let (hits, _offs, _warn) = collect_sn76489(&v, None);
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[1].sample_time, 3000);
        assert_eq!(hits[1].vol, 10);
    }

    #[test]
    fn test_game_gear_stereo_mask_pans_the_voice() {
        // 0xF0 is the all-left mask; voice 2 is left-only, right off.
        let mut writes = vec![ChipWrite::new(0, "sn76489", 0, 1, 0xF0)];
        writes.extend(psg_tone(100, 2));
        let v = _vgm(
            writes,
            VgmOpts {
                sn76489_clock: 3579545,
                sn76489_stereo: true,
                ..Default::default()
            },
        );
        let song = analyze(&v, AnalyzeOptions::default()).expect("analyze");
        assert!(song.psg_stereo);
        let note = song
            .events
            .iter()
            .find(|e| e.on && e.tone_chip == 3)
            .expect("voice 2 note");
        assert_eq!(note.pan, 0xF0);
        let data = write_fur(&song, WriteOptions::default());
        assert!(contains_bytes(&data, b"chipType=3"));
    }

    #[test]
    fn test_game_gear_stereo_change_while_held_becomes_a_pan_point() {
        let mut writes = vec![ChipWrite::new(0, "sn76489", 0, 1, 0xF0)];
        writes.extend(psg_tone(100, 0));
        writes.push(ChipWrite::new(2000, "sn76489", 0, 1, 0x0F));
        let v = _vgm(
            writes,
            VgmOpts {
                sn76489_clock: 3579545,
                sn76489_stereo: true,
                ..Default::default()
            },
        );
        let song = analyze(&v, AnalyzeOptions::default()).expect("analyze");
        assert!(song
            .channel_fx
            .iter()
            .any(|&(_t, _ch, cmd, val)| cmd == 0x08 && val == 0x0F));
    }
}

// ------------------------------------------------------------ TestComboLayout

#[allow(non_snake_case)]
mod TestComboLayout {
    use super::*;

    fn system_kinds(song: &Song) -> Vec<String> {
        song.fm_systems.iter().map(|s| s.kind.clone()).collect()
    }

    fn on_channels(song: &Song) -> Vec<i32> {
        let mut set: BTreeSet<i32> = BTreeSet::new();
        for e in &song.events {
            if e.on && !e.pcm {
                set.insert(e.ch);
            }
        }
        set.into_iter().collect()
    }

    #[test]
    fn test_dac_only_opn2_keeps_both_systems() {
        let bank: Vec<u8> = (0..4096).map(|i| (((i * 13) % 251) + 2) as u8).collect();
        let mut writes: Vec<ChipWrite> = vec![ChipWrite::new(0, "ym2612", 0, 0x2B, 0x80)];
        for ch in 0..3i64 {
            writes.extend(opl_note(1000 + ch * PULSE, ch as i32));
            writes.extend(opl_note(9000 + ch * PULSE, ch as i32));
        }
        let mut v = _vgm(
            writes,
            VgmOpts {
                ym2612_clock: 7600489,
                opl2_clock: 3579545,
                dac_data: bank.clone(),
                total_samples: 80000,
                ..Default::default()
            },
        );
        v.dac_stream_plays = vec![_stream(500, 0, bank.len() as u32)];
        let song = analyze(&v, AnalyzeOptions::default()).expect("analyze");
        assert_eq!(system_kinds(&song), vec!["ym2612", "opl2"]);
        let ons: Vec<_> = song.events.iter().filter(|e| e.on && !e.pcm).collect();
        assert!(
            ons.iter().any(|e| e.dac),
            "the DAC note is missing"
        );
        // The OPL melody starts after the OPN2's six columns.
        let mut non_dac: BTreeSet<i32> = BTreeSet::new();
        for e in &ons {
            if !e.dac {
                non_dac.insert(e.ch);
            }
        }
        assert_eq!(non_dac.into_iter().collect::<Vec<_>>(), vec![6, 7, 8]);
        assert!(
            song.warnings
                .iter()
                .any(|w| w.contains("both systems were converted")),
            "{:?}",
            song.warnings
        );
    }

    #[test]
    fn test_opm_beside_an_opl_keeps_its_own_eight_columns() {
        let mut writes: Vec<ChipWrite> = Vec::new();
        for ch in 0..8i64 {
            writes.extend(opm_note(1000 + ch * PULSE, ch as i32));
        }
        for ch in 0..9i64 {
            writes.extend(opl_note(1500 + ch * PULSE, ch as i32));
        }
        let v = _vgm(
            writes,
            VgmOpts {
                ym2151_clock: 4000000,
                opl2_clock: 3579545,
                total_samples: 200000,
                ..Default::default()
            },
        );
        let song = analyze(&v, AnalyzeOptions::default()).expect("analyze");
        assert_eq!(
            song.fm_systems
                .iter()
                .map(|s| (s.kind.clone(), s.fm_channels))
                .collect::<Vec<_>>(),
            vec![
                ("ym2151".to_string(), 8),
                ("opl2".to_string(), 9)
            ]
        );
        assert_eq!(song.fm_chip, "ym2151");
        let opm: Vec<i32> = {
            let mut set: BTreeSet<i32> = BTreeSet::new();
            for e in &song.events {
                if e.on && !e.pcm && e.ch < 8 {
                    set.insert(e.ch);
                }
            }
            set.into_iter().collect()
        };
        let opl: Vec<i32> = {
            let mut set: BTreeSet<i32> = BTreeSet::new();
            for e in &song.events {
                if e.on && !e.pcm && e.ch >= 8 {
                    set.insert(e.ch - 8);
                }
            }
            set.into_iter().collect()
        };
        assert_eq!(opm, (0..8).collect::<Vec<_>>());
        assert_eq!(opl, (0..9).collect::<Vec<_>>());
    }

    #[test]
    fn test_opn2_with_notes_beside_an_opl3() {
        let bank: Vec<u8> = (0..2048).map(|i| (((i * 29) % 241) + 3) as u8).collect();
        let mut writes: Vec<ChipWrite> = vec![ChipWrite::new(0, "ym2612", 0, 0x2B, 0x80)];
        for ch in 0..6i64 {
            writes.extend(opn_note(1000 + ch * PULSE, ch as i32));
        }
        for ch in 0..4i64 {
            writes.extend(opl_note_kind(1300 + ch * PULSE, ch as i32, "opl3"));
        }
        let mut v = _vgm(
            writes,
            VgmOpts {
                ym2612_clock: 7670453,
                opl3_clock: 14318180,
                dac_data: bank.clone(),
                total_samples: 200000,
                ..Default::default()
            },
        );
        v.dac_stream_plays = vec![_stream(500, 0, bank.len() as u32)];
        let song = analyze(&v, AnalyzeOptions::default()).expect("analyze");
        assert_eq!(
            song.fm_systems
                .iter()
                .map(|s| (s.kind.clone(), s.fm_channels))
                .collect::<Vec<_>>(),
            vec![
                ("ym2612".to_string(), 6),
                ("opl3".to_string(), 18)
            ]
        );
        let chans = on_channels(&song);
        // The OPN2 keeps 0-5, its DAC column inside them; the OPL3 follows.
        assert_eq!(
            chans.iter().copied().filter(|&c| c < 6).collect::<Vec<_>>(),
            vec![0, 1, 2, 3, 4, 5]
        );
        assert!(chans.iter().any(|&c| c >= 6), "{:?}", chans);
        assert!(chans.iter().max().copied().unwrap() < 24);
        let dac: BTreeSet<i32> = song
            .events
            .iter()
            .filter(|e| e.dac && e.on)
            .map(|e| e.ch)
            .collect();
        assert_eq!(dac, BTreeSet::from([5]));
    }

    #[test]
    fn test_oki_sample_bodies_do_not_shift_the_ay_channels() {
        // The tone base must count MSM6295 *chips* (4 voices), not sample
        // bodies. A bank with two phrases used to push the AY block 8 columns
        // too far, past the end of the module, so its notes disappeared.
        let mut rom = vec![0u8; 0x20800];
        rom[0x20000..0x20008]
            .copy_from_slice(&[0x00, 0x05, 0x00, 0x00, 0x05, 0x07, 0x00, 0x00]);
        rom[0x20008..0x20010]
            .copy_from_slice(&[0x00, 0x06, 0x00, 0x00, 0x07, 0x8F, 0x00, 0x00]);
        for b in &mut rom[0x20500..0x20508] {
            *b = 0x11;
        }
        for b in &mut rom[0x20600..0x20790] {
            *b = 0x22;
        }
        let writes = vec![
            ChipWrite::new(0, "msm6295", 0, 0x0E, 0x81),
            ChipWrite::new(10, "msm6295", 0, 0x10, 0x02),
            ChipWrite::new(100, "msm6295", 0, 0x00, 0x80),
            ChipWrite::new(110, "msm6295", 0, 0x00, 0x10),
            ChipWrite::new(200, "msm6295", 0, 0x00, 0x81),
            ChipWrite::new(210, "msm6295", 0, 0x00, 0x80),
            ChipWrite::new(2000, "msm6295", 0, 0x00, 0x40),
            ChipWrite::new(100, "ay8910", 0, 7, 0x38),
            ChipWrite::new(100, "ay8910", 0, 0, 0x00),
            ChipWrite::new(100, "ay8910", 0, 1, 0x01),
            ChipWrite::new(100, "ay8910", 0, 8, 0x0F),
        ];
        let mut v = _vgm(
            writes,
            VgmOpts {
                msm6295_clock: 2_000_000,
                ay8910_clock: 1789772,
                total_samples: 3000,
                ..Default::default()
            },
        );
        v.roms = vec![RomSlice {
            chip: 0,
            rom_size: rom.len() as i64,
            start: 0,
            data: rom.clone(),
            kind: "msm6295",
            time: 0,
        }];
        v.msm6295_pin7 = true;
        let song = analyze(&v, AnalyzeOptions::default()).expect("analyze");
        assert_eq!(song.oki_samples.len(), 2);
        let ay = song
            .events
            .iter()
            .find(|e| e.on && e.tone_chip == 2)
            .expect("AY note");
        // 8 placeholder columns + 4 MSM6295 voices, then the AY block.
        assert_eq!(ay.ch, 12);
        let mod_ = parse_fur(&write_fur(&song, WriteOptions::default())).expect("parse fur");
        assert_eq!(mod_.chips, vec![CHIP_MSM6295, CHIP_AY8910]);
        assert_eq!(mod_.n_ch, 7);
        let mut notes: Vec<i32> = Vec::new();
        for oi in &mod_.orders[4] {
            if let Some(rows) = mod_.patterns.get(&(4, *oi)) {
                for row in rows {
                    if (0..180).contains(&row.note) {
                        notes.push(row.note);
                    }
                }
            }
        }
        assert!(!notes.is_empty(), "the AY channel lost its notes");
    }

    #[test]
    fn test_ym2203_and_its_ssg_beside_an_opl2() {
        // A merged Hoot/Sound Canvas log can carry an OPL2 melody and a
        // YM2203 (FM + SSG). Both are Furnace systems, so neither side is
        // dropped for having fewer key-ons.
        let mut writes: Vec<ChipWrite> = Vec::new();
        for ch in 0..3i64 {
            writes.extend(opn_note_kind(1000 + ch * PULSE, ch as i32, "ym2203"));
        }
        writes.extend([
            ChipWrite::new(1300, "ym2203_ssg", 0, 7, 0x38),
            ChipWrite::new(1300, "ym2203_ssg", 0, 0, 0x00),
            ChipWrite::new(1300, "ym2203_ssg", 0, 1, 0x01),
            ChipWrite::new(1300, "ym2203_ssg", 0, 8, 0x0F),
        ]);
        for ch in 0..3i64 {
            writes.extend(opl_note(1500 + ch * PULSE, ch as i32));
        }
        let v = _vgm(
            writes,
            VgmOpts {
                ym2203_clock: 3993600,
                opl2_clock: 3579545,
                total_samples: 200000,
                ..Default::default()
            },
        );
        let song = analyze(&v, AnalyzeOptions::default()).expect("analyze");
        assert_eq!(system_kinds(&song), vec!["ym2203", "opl2"]);
        assert_eq!(song.fm_chip, "ym2203");
        let chans = on_channels(&song);
        assert_eq!(
            chans.iter().copied().filter(|&c| c < 6).collect::<Vec<_>>(),
            vec![0, 1, 2, 3]
        );
        assert!(chans.iter().any(|&c| c >= 6), "{:?}", chans);
        let data = write_fur(&song, WriteOptions::default());
        let mod_ = parse_fur(&data).expect("parse fur");
        assert_eq!(mod_.chips, vec![CHIP_YM2203, CHIP_OPL2]);
        assert_eq!(mod_.n_ch, 15);
    }

    #[test]
    fn test_three_systems_stack_in_order() {
        let mut writes: Vec<ChipWrite> = Vec::new();
        for ch in 0..3i64 {
            writes.extend(opn_note(1000 + ch * PULSE, ch as i32));
        }
        for ch in 0..3i64 {
            writes.extend(opll_note(1200 + ch * PULSE, ch as i32));
        }
        writes.push(ChipWrite::new(1300, "opll", 0, 0x0E, 0x20 | 0x1F));
        for ch in 0..3i64 {
            writes.extend(psg_tone(1400 + ch * PULSE, ch as u32));
        }
        let v = _vgm(
            writes,
            VgmOpts {
                ym2612_clock: 7670453,
                ym2413_clock: 3579545,
                sn76489_clock: 3579545,
                total_samples: 200000,
                ..Default::default()
            },
        );
        let song = analyze(&v, AnalyzeOptions::default()).expect("analyze");
        assert_eq!(system_kinds(&song), vec!["ym2612", "opll"]);
        let chans = on_channels(&song);
        assert!(chans.iter().all(|&c| (0..9 + 9 + 4).contains(&c)), "{:?}", chans);
        // Every system sits in its own block: OPN2 0-5, OPLL 6-14, PSG 15-18.
        assert_eq!(
            chans.iter().copied().filter(|&c| c < 6).collect::<Vec<_>>(),
            vec![0, 1, 2]
        );
        assert!(chans.iter().any(|&c| (6..15).contains(&c)), "{:?}", chans);
        assert!(chans.iter().any(|&c| c >= 15), "{:?}", chans);
    }

    #[test]
    fn test_init_only_arcade_chip_loses_its_system() {
        let mut writes: Vec<ChipWrite> = (0x10..0x20)
            .map(|reg| ChipWrite::new(0, "ym2608", 0, reg, 0x00))
            .collect();
        writes.extend(opn_note(1000, 0));
        writes.extend(psg_tone(2000, 0));
        let v = _vgm(
            writes,
            VgmOpts {
                ym2608_clock: 7987200,
                ym2612_clock: 7670453,
                sn76489_clock: 3579545,
                total_samples: 200000,
                ..Default::default()
            },
        );
        let song = analyze(&v, AnalyzeOptions::default()).expect("analyze");
        assert_eq!(system_kinds(&song), vec!["ym2612"]);
        assert!(
            song.warnings.iter().any(|w| w.contains("init block")),
            "{:?}",
            song.warnings
        );
    }

    #[test]
    fn test_a_silent_psg_is_dropped_with_a_warning() {
        let mut writes = opl_note(1000, 0);
        for v in 0..4u32 {
            writes.push(ChipWrite::new(
                1000,
                "sn76489",
                0,
                0,
                0x80 | (v << 5) | 0x10 | 15,
            ));
        }
        let v = _vgm(
            writes,
            VgmOpts {
                opl2_clock: 3579545,
                sn76489_clock: 3579545,
                total_samples: 200000,
                ..Default::default()
            },
        );
        let song = analyze(&v, AnalyzeOptions::default()).expect("analyze");
        assert_eq!(song.psg_ids, Vec::<i32>::new());
    }
}

// ------------------------------------------------------------- TestComboWriter

#[allow(non_snake_case)]
mod TestComboWriter {
    use super::*;

    fn combo_song() -> Song {
        let mut writes: Vec<ChipWrite> = Vec::new();
        for ch in 0..8i64 {
            writes.extend(opm_note(1000 + ch * PULSE, ch as i32));
        }
        for ch in 0..9i64 {
            writes.extend(opl_note(1500 + ch * PULSE, ch as i32));
        }
        analyze(
            &_vgm(
                writes,
                VgmOpts {
                    ym2151_clock: 4000000,
                    opl2_clock: 3579545,
                    total_samples: 200000,
                    ..Default::default()
                },
            ),
            AnalyzeOptions::default(),
        )
        .expect("analyze")
    }

    fn _flag_texts(data: &[u8]) -> Vec<String> {
        let mut out = Vec::new();
        for block in iter_blocks(data).expect("blocks") {
            if block.magic == *b"FLAG" {
                let end = block
                    .payload
                    .iter()
                    .position(|&b| b == 0)
                    .unwrap_or(block.payload.len());
                out.push(String::from_utf8_lossy(&block.payload[..end]).into_owned());
            }
        }
        out
    }

    #[test]
    fn test_chip_list_and_channel_count() {
        let song = combo_song();
        let data = write_fur(&song, WriteOptions::default());
        let m = parse_fur(&data).expect("parse fur");
        assert_eq!(m.chips[..2], [CHIP_YM2151, CHIP_OPL2]);
        assert_eq!(m.n_ch, 8 + 9);
        let total: usize = m
            .chips
            .iter()
            .map(|c| {
                CHIP_CHANNELS
                    .iter()
                    .find(|(k, _)| k == c)
                    .map(|(_, n)| *n)
                    .expect("chip channels")
            })
            .sum();
        assert_eq!(total, m.n_ch);
    }

    #[test]
    fn test_clock_flags_travel_with_each_chip() {
        let song = combo_song();
        let data = write_fur(&song, WriteOptions::default());
        let flags = _flag_texts(&data);
        // The OPM clock is not Furnace's default, so it has to be named; the
        // OPL2 runs at COLOR_NTSC and needs no flag of its own.
        assert_eq!(flags, vec!["customClock=4000000"]);
    }

    #[test]
    fn test_channel_names_follow_the_system_order() {
        let song = combo_song();
        let mut longs: Vec<String> = Vec::new();
        let mut shorts: Vec<String> = Vec::new();
        _append_system_names(&song, &song.fm_systems, &mut longs, &mut shorts);
        assert_eq!(longs.len(), 17);
        assert_eq!(longs[..8], (1..=8).map(|i| format!("FM {}", i)).collect::<Vec<_>>());
        assert_eq!(longs[8..], (1..=9).map(|i| format!("FM {}", i)).collect::<Vec<_>>());
        // The OPL block is numbered by module channel, so no tag repeats.
        assert_eq!(shorts[..8], (1..=8).map(|i| format!("F{}", i)).collect::<Vec<_>>());
        assert_eq!(shorts[8..], (9..=17).map(|i| format!("F{}", i)).collect::<Vec<_>>());
        assert_eq!(
            shorts.iter().collect::<HashSet<_>>().len(),
            shorts.len()
        );
    }

    #[test]
    fn test_each_column_uses_its_own_chip_volume_ceiling() {
        let song = combo_song();
        let table = _channel_vol_max(&song, 17, 63, 0);
        assert_eq!(table[..8], [127i32; 8]);
        assert_eq!(table[8..17], [63i32; 9]);
    }

    #[test]
    fn test_a_single_family_keeps_one_ceiling() {
        let mut writes = opn_note(1000, 0);
        writes.extend(opn_note(1000 + PULSE, 1));
        let song = analyze(
            &_vgm(
                writes,
                VgmOpts {
                    ym2612_clock: 7670453,
                    total_samples: 60000,
                    ..Default::default()
                },
            ),
            AnalyzeOptions::default(),
        )
        .expect("analyze");
        assert_eq!(_channel_vol_max(&song, 6, 127, 0), vec![127i32; 6]);
    }

    #[test]
    fn test_report_names_every_system() {
        let song = combo_song();
        let text = format_report(&song);
        assert!(text.contains("YM2151"));
        assert!(text.contains("OPL2"));
    }
}

// -------------------------------------------------- TestComboInstrumentPriority

#[allow(non_snake_case)]
mod TestComboInstrumentPriority {
    use super::*;

    #[test]
    fn test_ay_keeps_its_decay_beside_an_opll_macro_copy() {
        // The OPLL level walk creates a macro copy that grows the patch list
        // into the AY event's pre-bind instrument index. The AY note must not
        // be taken for an OPLL note: that used to pin its volume column to 15
        // and lose the decay, leaving a randomly loud square.
        skip_without_furnace!();
        let mut writes = opll_note_off(1000, 0, Some(30000));
        writes.extend([
            ChipWrite::new(5000, "opll", 0, 0x30, (6 << 4) | 4),
            ChipWrite::new(5100, "opll", 0, 0x30, (6 << 4) | 2),
        ]);
        writes.extend([
            ChipWrite::new(0, "ay8910", 0, 7, 0xFE),
            ChipWrite::new(0, "ay8910", 0, 0, 254),
            ChipWrite::new(0, "ay8910", 0, 1, 0),
            ChipWrite::new(3000, "ay8910", 0, 8, 9),
            ChipWrite::new(8000, "ay8910", 0, 8, 8),
            ChipWrite::new(13000, "ay8910", 0, 8, 7),
            ChipWrite::new(25000, "ay8910", 0, 8, 0),
        ]);
        let song = analyze(
            &_vgm(
                writes,
                VgmOpts {
                    ym2413_clock: 3579545,
                    ay8910_clock: 1789773,
                    total_samples: 60000,
                    ..Default::default()
                },
            ),
            AnalyzeOptions::default(),
        )
        .expect("analyze");
        let ay = song
            .events
            .iter()
            .find(|e| e.on && e.tone_chip == 2)
            .expect("AY note");
        let idx = ay.ins - song.fm_patches.len() as i32;
        assert!(
            0 <= idx && idx < song.ay_ins.len() as i32,
            "{:?}",
            (ay.ins, song.fm_patches.len(), song.ay_ins.len())
        );
        assert_eq!(song.ay_ins[idx as usize].vol[0], 9);
        assert_eq!(ay.vol, 15); // the macro carries the level
    }
}

// ------------------------------------------------------------ TestComboRoundTrip

#[allow(non_snake_case)]
mod TestComboRoundTrip {
    use super::*;

    fn export(song: &Song, name: &str) -> VgmFile {
        let fur = tmp().join(format!("{}.fur", name));
        let out = tmp().join(format!("{}.vgm", name));
        std::fs::write(&fur, write_fur(
            song,
            WriteOptions {
                compress: true,
                ..Default::default()
            },
        ))
        .expect("write fur");
        vgm2fur::furnace::export_vgm(&fur, &out).expect("export");
        load_vgm(&out).expect("load export")
    }

    #[test]
    fn test_opm_and_opl_play_their_own_channels() {
        skip_without_furnace!();
        let mut writes: Vec<ChipWrite> = Vec::new();
        for ch in [0i64, 3, 7] {
            writes.extend(opm_note(1000 + ch * PULSE / 3, ch as i32));
        }
        for ch in [0i64, 4] {
            writes.extend(opl_note(1500 + ch * PULSE / 3, ch as i32));
        }
        let v = _vgm(
            writes,
            VgmOpts {
                ym2151_clock: 4000000,
                opl2_clock: 3579545,
                total_samples: 200000,
                ..Default::default()
            },
        );
        let song = analyze(&v, AnalyzeOptions::default()).expect("analyze");
        assert_eq!(
            song.fm_systems
                .iter()
                .map(|s| (s.kind.clone(), s.fm_channels))
                .collect::<Vec<_>>(),
            vec![
                ("ym2151".to_string(), 8),
                ("opl2".to_string(), 9)
            ]
        );
        let exp = export(&song, "opm_opl");
        assert_eq!(exp.ym2151_clock, 4000000);
        assert_eq!(exp.opl2_clock, 3579545);
        let opm: BTreeSet<u32> = exp
            .writes
            .iter()
            .filter(|w| w.chip == "ym2151" && w.reg == 0x08 && (w.val & 0x78) != 0)
            .map(|w| w.val & 7)
            .collect();
        let opl: BTreeSet<u32> = exp
            .writes
            .iter()
            .filter(|w| {
                w.chip == "opl2"
                    && (0xB0..=0xB8).contains(&(w.reg & 0xFF))
                    && (w.val & 0x20) != 0
            })
            .map(|w| (w.reg & 0xFF) - 0xB0)
            .collect();
        // Each family drives its own registers: the OPM voices it keyed, and
        // the two OPL channels the melody was written on.
        assert_eq!(opm, BTreeSet::from([0, 3, 7]));
        assert_eq!(opl, BTreeSet::from([0, 4]));
    }

    #[test]
    fn test_dac_stream_and_melody_both_survive() {
        skip_without_furnace!();
        let bank: Vec<u8> = (0..8192).map(|i| (((i * 5) % 251) + 4) as u8).collect();
        let mut writes: Vec<ChipWrite> = vec![ChipWrite::new(0, "ym2612", 0, 0x2B, 0x80)];
        for ch in 0..3i64 {
            writes.extend(opl_note(1000 + ch * PULSE, ch as i32));
            writes.extend(opl_note(9000 + ch * PULSE, ch as i32));
        }
        let mut v = _vgm(
            writes,
            VgmOpts {
                ym2612_clock: 7600489,
                opl2_clock: 3579545,
                dac_data: bank.clone(),
                total_samples: 80000,
                ..Default::default()
            },
        );
        v.dac_stream_plays = vec![_stream(200, 0, bank.len() as u32)];
        let song = analyze(&v, AnalyzeOptions::default()).expect("analyze");
        assert!(song.dac_count > 0);
        let fur = tmp().join("dac_opl.fur");
        std::fs::write(
            &fur,
            write_fur(
                &song,
                WriteOptions {
                    compress: true,
                    ..Default::default()
                },
            ),
        )
        .expect("write fur");
        let m = parse_fur(&std::fs::read(&fur).expect("read fur")).expect("parse fur");
        assert_eq!(m.chips[..2], [CHIP_YM2612, CHIP_OPL2]);
        let out = tmp().join("dac_opl.vgm");
        vgm2fur::furnace::export_vgm(&fur, &out).expect("export");
        let exp = load_vgm(&out).expect("load export");
        let mut starts: Vec<i64> = exp.dac_block_plays.iter().map(|(t, _b, _r)| *t).collect();
        starts.extend(exp.dac_stream_plays.iter().map(|p| p.sample));
        assert!(!starts.is_empty(), "no DAC start survived the round trip");
        let keys: Vec<&ChipWrite> = exp
            .writes
            .iter()
            .filter(|w| {
                w.chip == "opl2"
                    && (0xB0..=0xB8).contains(&(w.reg & 0xFF))
                    && (w.val & 0x20) != 0
            })
            .collect();
        assert!(!keys.is_empty(), "the OPL melody vanished");
    }

    #[test]
    fn test_opm_and_opl3_keep_their_own_channels() {
        skip_without_furnace!();
        let mut writes: Vec<ChipWrite> = Vec::new();
        for ch in 0..8i64 {
            writes.extend(opm_note(1000 + ch * PULSE, ch as i32));
        }
        for ch in 0..4i64 {
            writes.extend(opl_note_kind(1300 + ch * PULSE, ch as i32, "opl3"));
        }
        let v = _vgm(
            writes,
            VgmOpts {
                ym2151_clock: 4000000,
                opl3_clock: 14318180,
                total_samples: 200000,
                ..Default::default()
            },
        );
        let song = analyze(&v, AnalyzeOptions::default()).expect("analyze");
        assert_eq!(
            song.fm_systems
                .iter()
                .map(|s| (s.kind.clone(), s.chip_id))
                .collect::<Vec<_>>(),
            vec![
                ("ym2151".to_string(), 0),
                ("opl3".to_string(), 0)
            ]
        );
        assert_eq!(
            parse_fur(&write_fur(&song, WriteOptions::default()))
                .expect("parse fur")
                .chips[..2],
            [CHIP_YM2151, CHIP_OPL3]
        );
        let exp = export(&song, "opm_opl3");
        assert_eq!(exp.ym2151_clock, 4000000);
        assert_eq!(exp.opl3_clock, 14318180);
        // Neither family leaks into the other's registers, and every voice
        // that was keyed survives the module.
        assert_eq!(opm_voices(&exp), opm_voices(&v));
        assert!(!opm_voices(&exp).is_empty());
        assert_eq!(opl_voices(&exp), opl_voices(&v));
        assert!(!opl_voices(&exp).is_empty());
        let text = format_report(&song);
        assert!(text.contains("YM2151"));
        assert!(text.contains("YMF262"));
    }

    fn opm_voices(v: &VgmFile) -> BTreeSet<u32> {
        v.writes
            .iter()
            .filter(|w| w.chip == "ym2151" && (w.reg & 0xFF) == 0x08 && (w.val & 0x78) != 0)
            .map(|w| w.val & 7)
            .collect()
    }

    fn opl_voices(v: &VgmFile) -> BTreeSet<u32> {
        v.writes
            .iter()
            .filter(|w| {
                w.chip == "opl3"
                    && (0xB0..=0xB8).contains(&(w.reg & 0xFF))
                    && (w.val & 0x20) != 0
            })
            .map(|w| (w.reg & 0xFF) - 0xB0)
            .collect()
    }

    #[test]
    fn test_dac_stream_audit_covers_the_whole_bank() {
        skip_without_furnace!();
        let bank: Vec<u8> = (0..8192).map(|i| (((i * 5) % 251) + 4) as u8).collect();
        let mut writes: Vec<ChipWrite> = vec![ChipWrite::new(0, "ym2612", 0, 0x2B, 0x80)];
        for ch in 0..3i64 {
            writes.extend(opl_note(1000 + ch * PULSE, ch as i32));
            writes.extend(opl_note(9000 + ch * PULSE, ch as i32));
        }
        let mut v = _vgm(
            writes,
            VgmOpts {
                ym2612_clock: 7600489,
                opl2_clock: 3579545,
                dac_data: bank.clone(),
                total_samples: 80000,
                ..Default::default()
            },
        );
        v.dac_stream_plays = vec![_stream(200, 0, bank.len() as u32)];
        let song = analyze(&v, AnalyzeOptions::default()).expect("analyze");
        assert_eq!(
            song.fm_systems
                .iter()
                .map(|s| s.kind.clone())
                .collect::<Vec<_>>(),
            vec!["ym2612", "opl2"]
        );
        assert!(song.dac_count > 0);
        let fur = tmp().join("dac_audit.fur");
        std::fs::write(
            &fur,
            write_fur(
                &song,
                WriteOptions {
                    compress: true,
                    ..Default::default()
                },
            ),
        )
        .expect("write fur");
        let out = tmp().join("dac_audit.vgm");
        vgm2fur::furnace::export_vgm(&fur, &out).expect("export");
        let exp = load_vgm(&out).expect("load export");
        let audit = compare_dac(&v, &exp);
        let val = |key: &str| audit.get(key).and_then(|x| x.as_i64()).unwrap_or(0);
        assert_eq!(val("missed"), 0, "{:?}", audit);
        assert!(val("matched") > 0);
        assert_eq!(val("rate_bad"), 0, "{:?}", audit);
        // Every PCM byte of the source stream is inside the module's banks.
        assert!(val("bytes_exp") > val("bytes_src") * 99 / 100);
        let keys: Vec<&ChipWrite> = exp
            .writes
            .iter()
            .filter(|w| {
                w.chip == "opl2"
                    && (0xB0..=0xB8).contains(&(w.reg & 0xFF))
                    && (w.val & 0x20) != 0
            })
            .collect();
        assert!(!keys.is_empty(), "the OPL melody vanished");
    }

    #[test]
    fn test_opn2_opll_and_psg_each_survive() {
        skip_without_furnace!();
        let mut writes: Vec<ChipWrite> = Vec::new();
        for ch in 0..3i64 {
            writes.extend(opn_note(1000 + ch * PULSE, ch as i32));
        }
        for ch in 0..3i64 {
            writes.extend(opll_note(1200 + ch * PULSE, ch as i32));
        }
        writes.push(ChipWrite::new(1300, "opll", 0, 0x0E, 0x20 | 0x1F));
        for ch in 0..3i64 {
            writes.extend(psg_tone(1400 + ch * PULSE, ch as u32));
        }
        let v = _vgm(
            writes,
            VgmOpts {
                ym2612_clock: 7670453,
                ym2413_clock: 3579545,
                sn76489_clock: 3579545,
                total_samples: 200000,
                ..Default::default()
            },
        );
        let song = analyze(&v, AnalyzeOptions::default()).expect("analyze");
        assert_eq!(
            song.fm_systems
                .iter()
                .map(|s| s.kind.clone())
                .collect::<Vec<_>>(),
            vec!["ym2612", "opll"]
        );
        assert_eq!(
            parse_fur(&write_fur(&song, WriteOptions::default()))
                .expect("parse fur")
                .chips[..3],
            [CHIP_YM2612, CHIP_OPLL_DRUMS, CHIP_SMS]
        );
        let exp = export(&song, "three_engines");
        assert_eq!(exp.ym2612_clock, 7670453);
        assert_eq!(exp.ym2413_clock, 3579545);
        assert_eq!(exp.sn76489_clock, 3579545);
        let mut keys: BTreeMap<&str, usize> = BTreeMap::new();
        for w in &exp.writes {
            *keys.entry(w.chip).or_insert(0) += 1;
        }
        // All three engines still write: the OPN2 melody, the YM2413 kit and
        // the PSG's own gates.
        for chip in ["ym2612", "opll", "sn76489"] {
            assert!(keys.get(chip).copied().unwrap_or(0) > 0, "{:?}", keys);
        }
    }
}
