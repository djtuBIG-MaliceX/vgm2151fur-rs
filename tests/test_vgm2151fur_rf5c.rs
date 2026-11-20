//! Ricoh RF5C68 / RF5C164: VGM parsing, register model and .fur emission.
//!
//! Both chips share one register file and one Furnace system (0x95); the
//! chipType flag picks the variant (0 RF5C68, 1 RF5C164). The tests cover the
//! $B0/$B1 register writes, $C1/$C2 memory writes, the 0xC0/0xC1 RAM blocks, the
//! sign-magnitude sample body, the "loop at the end is a hold" rule and the
//! one-shot note-off/retrigger split, then a synthetic FM Towns- and Sega
//! CD-style log through Furnace and back.
//!
//! Port of `tests/test_vgm2151fur_rf5c.py` at vgm2151fur a8edb6e.

mod common;
use common::*;

use std::collections::HashSet;
use std::path::PathBuf;

use indexmap::IndexMap;

use vgm2fur::analyze::{analyze, AnalyzeOptions};
use vgm2fur::furio::{chip_channels, iter_blocks, parse_fur};
use vgm2fur::furwrite::{
    write_fur, WriteOptions, CHIP_RF5C68, CHIP_YM2151, INS_RF5C68,
};
use vgm2fur::furnace::export_vgm;
use vgm2fur::model::Song;
use vgm2fur::rf5c::{
    build_rf5c_ram, collect_rf5c, rf5c_rate, RF5C_VOICES,
};
use vgm2fur::vgm::{chips_in_head, load_vgm, ChipWrite, RomSlice, VgmFile};

const LATTICE_CLOCK: u32 = 3_579_545;
const RF5C68_CLOCK: u32 = 8_000_000;
const RF5C164_CLOCK: u32 = 12_500_000;

fn tmp_dir() -> PathBuf {
    let p = std::env::temp_dir().join("vgm2151fur_rf5c");
    std::fs::create_dir_all(&p).expect("create temp dir");
    p
}

/// Local copy of `rf5c._sign_magnitude` (the module keeps it private).
fn sign_magnitude(val: i32) -> i32 {
    if val & 0x80 != 0 {
        val & 0x7F
    } else {
        -(val & 0x7F)
    }
}

fn _vgm(writes: Vec<ChipWrite>, roms: Vec<RomSlice>, rf5c68_clock: u32, rf5c164_clock: u32) -> VgmFile {
    VgmFile {
        path: PathBuf::from("synthetic.vgm"),
        buf: Vec::new(),
        version: 0x161,
        total_samples: 400_000,
        loop_samples: 0,
        loop_file_pos: None,
        loop_sample: None,
        ym2151_clock: LATTICE_CLOCK,
        k007232_clock: 0,
        gd3: IndexMap::new(),
        roms,
        writes,
        rf5c68_clock,
        rf5c164_clock,
        ..Default::default()
    }
}

/// `_raw_vgm(..., clock=RF5C68_CLOCK, clock_off=0x40)`.
fn _raw_vgm(cmds: &[u8], blocks: &[(u8, i64, Vec<u8>)]) -> Vec<u8> {
    _raw_vgm_full(cmds, blocks, RF5C68_CLOCK, 0x40)
}

fn _raw_vgm_full(
    cmds: &[u8],
    blocks: &[(u8, i64, Vec<u8>)],
    clock: u32,
    clock_off: usize,
) -> Vec<u8> {
    // A minimal 1.61 log with a chip clock, RAM blocks and a command body.
    let mut buf = vec![0u8; 0x100];
    buf[0..4].copy_from_slice(b"Vgm ");
    buf[8..12].copy_from_slice(&0x161u32.to_le_bytes());
    buf[0x18..0x1C].copy_from_slice(&44100u32.to_le_bytes());
    buf[0x30..0x34].copy_from_slice(&LATTICE_CLOCK.to_le_bytes());
    buf[0x34..0x38].copy_from_slice(&(0x100u32 - 0x34).to_le_bytes());
    buf[clock_off..clock_off + 4].copy_from_slice(&clock.to_le_bytes());
    let mut body: Vec<u8> = Vec::new();
    for (typ, start, data) in blocks {
        let mut payload: Vec<u8> = Vec::new();
        payload.extend_from_slice(&(*start as u16).to_le_bytes());
        payload.extend_from_slice(data);
        body.extend_from_slice(&[0x67, 0x66, *typ]);
        body.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        body.extend_from_slice(&payload);
    }
    body.extend_from_slice(cmds);
    body.push(0x66);
    let mut out = buf;
    out.extend_from_slice(&body);
    out
}

fn _lattice() -> Vec<ChipWrite> {
    lattice(60, 5_000)
}

fn _select(t: i64, ch: u32, chip: &'static str) -> ChipWrite {
    ChipWrite::new(t, chip, 0, 0x07, 0x40 | ch)
}

fn _reg(t: i64, reg: u32, val: u32, chip: &'static str) -> ChipWrite {
    ChipWrite::new(t, chip, 0, reg, val & 0xFF)
}

fn _enable(t: i64, bits: u32, chip: &'static str) -> ChipWrite {
    ChipWrite::new(t, chip, 0, 0x08, bits & 0xFF)
}

/// Keyword subset of the Python `_note(...)` helper.
#[derive(Clone)]
struct NoteKw {
    chip: &'static str,
    ch: u32,
    start: i32,
    loop_: i32,
    step: i32,
    env: u32,
    pan: u32,
    enable: Option<u32>,
}

impl Default for NoteKw {
    fn default() -> Self {
        NoteKw {
            chip: "rf5c68",
            ch: 0,
            start: 0x1000,
            loop_: 0,
            step: 0x1000,
            env: 0xCC,
            pan: 0x77,
            enable: None,
        }
    }
}

/// One driver-style burst: select, take down, start/loop/step/env, key on.
///
/// `start` and `loop` are byte offsets in sample RAM; the start register
/// holds the 256-byte page. A driver retriggers by disabling the voice, then
/// rewriting the page and enabling it again.
fn _note(t: i64, kw: &NoteKw) -> Vec<ChipWrite> {
    let chip = kw.chip;
    let on = match kw.enable {
        Some(v) => v,
        None => 0xFFu32 & !(1u32 << kw.ch),
    };
    vec![
        _select(t, kw.ch, chip),
        _enable(t, 0xFF, chip),
        _reg(t, 0x06, (kw.start >> 8) as u32, chip),
        _reg(t, 0x04, (kw.loop_ & 0xFF) as u32, chip),
        _reg(t, 0x05, ((kw.loop_ >> 8) & 0xFF) as u32, chip),
        _reg(t, 0x02, (kw.step & 0xFF) as u32, chip),
        _reg(t, 0x03, ((kw.step >> 8) & 0xFF) as u32, chip),
        _reg(t, 0x00, kw.env, chip),
        _reg(t, 0x01, kw.pan, chip),
        _enable(t, on, chip),
    ]
}

/// A body of sign-magnitude bytes with no 0xFF end marker inside it.
///
/// A magnitude of 127 would form 0xFF, the end marker, so the range stops at
/// 125 (0xFD).
fn _body(n: usize, seed: usize) -> Vec<u8> {
    (0..n)
        .map(|i| (0x80 | ((i * 3 + seed) % 126)) as u8)
        .collect()
}

/// Keyword subset of the Python `_synthetic(...)` helper.
struct SynthKw {
    start: i32,
    data: Option<Vec<u8>>,
    loop_: Option<i32>,
    step: i32,
    clock: Option<u32>,
}

impl Default for SynthKw {
    fn default() -> Self {
        SynthKw {
            start: 0x1000,
            data: None,
            loop_: None,
            step: 0x1000,
            clock: None,
        }
    }
}

fn _synthetic(chip: &'static str, kw: SynthKw) -> VgmFile {
    let data = kw.data.unwrap_or_else(|| _body(200, 1));
    let (rf5c68_clock, rf5c164_clock) = if chip == "rf5c68" {
        (kw.clock.unwrap_or(RF5C68_CLOCK), 0)
    } else {
        (0, kw.clock.unwrap_or(RF5C164_CLOCK))
    };
    let mut rom_data = data;
    rom_data.push(0xFF);
    let rom = RomSlice {
        chip: 0,
        rom_size: 0,
        start: kw.start as i64,
        data: rom_data,
        kind: chip,
        time: 0,
    };
    let mut writes = _lattice();
    writes.extend(_note(
        1000,
        &NoteKw {
            chip,
            start: kw.start,
            loop_: kw.loop_.unwrap_or(0),
            step: kw.step,
            ..Default::default()
        },
    ));
    _vgm(writes, vec![rom], rf5c68_clock, rf5c164_clock)
}

fn _flag_texts(data: &[u8]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for b in iter_blocks(data).expect("blocks") {
        if b.magic == *b"FLAG" {
            let end = b.payload.iter().position(|&x| x == 0).unwrap_or(b.payload.len());
            out.push(String::from_utf8_lossy(&b.payload[..end]).into_owned());
        }
    }
    out
}

fn _iter_ins(data: &[u8]) -> Vec<(usize, [u8; 4], u32, Vec<u8>)> {
    iter_blocks(data)
        .expect("blocks")
        .into_iter()
        .filter(|b| b.magic == *b"INS2")
        .map(|b| (b.off, b.magic, b.size, b.payload.to_vec()))
        .collect()
}

mod TestRf5cParsing {
    use super::*;

    #[test]
    fn test_register_and_memory_writes() {
        // Bank 1 (0x1000) then a memory byte at offset 0x0020.
        let cmds: &[u8] = &[
            0xB0, 0x07, 0x01, // control: bank 1, chip enable off
            0xB0, 0x07, 0xC2, // select channel 2, chip enable on
            0xB0, 0x06, 0x34, // start
            0xC1, 0x20, 0x00, 0xAA, // memory write at 0x1000 | 0x0020
            0xB1, 0x07, 0x42, // RF5C164 select channel 2
            0xC2, 0x10, 0x00, 0xBB, // RF5C164 memory write at 0x0010
        ];
        let raw = _raw_vgm(cmds, &[]);
        let p = tmp_dir().join("parse.vgm");
        std::fs::write(&p, &raw).expect("write");
        let v = load_vgm(&p).expect("load");
        assert_eq!(v.rf5c68_clock, RF5C68_CLOCK);
        let regs: Vec<(&str, u32, u32)> = v
            .writes
            .iter()
            .filter(|w| w.chip == "rf5c68")
            .map(|w| (w.chip, w.reg, w.val))
            .collect();
        assert_eq!(
            regs,
            vec![
                ("rf5c68", 0x07, 0x01),
                ("rf5c68", 0x07, 0xC2),
                ("rf5c68", 0x06, 0x34)
            ]
        );
        let mem: Vec<(&str, u32, u32)> = v
            .writes
            .iter()
            .filter(|w| w.chip.ends_with("_mem"))
            .map(|w| (w.chip, w.reg, w.val))
            .collect();
        assert_eq!(
            mem,
            vec![
                ("rf5c68_mem", 0x1020, 0xAA),
                ("rf5c164_mem", 0x0010, 0xBB)
            ]
        );
        assert!(chips_in_head(&raw[..0x100]).contains("rf5c68"));
    }

    #[test]
    fn test_clock_fields_and_defaults() {
        let raw = _raw_vgm_full(b"", &[], RF5C164_CLOCK, 0x6C);
        let p = tmp_dir().join("clock.vgm");
        std::fs::write(&p, &raw).expect("write");
        let v = load_vgm(&p).expect("load");
        assert_eq!(v.rf5c164_clock, RF5C164_CLOCK);
        assert!(chips_in_head(&raw[..0x100]).contains("rf5c164"));
    }

    #[test]
    fn test_ram_block_is_a_sixteen_bit_write() {
        let raw = _raw_vgm(
            b"",
            &[(0xC0, 0x0120, (0..16u8).collect())],
        );
        let p = tmp_dir().join("block.vgm");
        std::fs::write(&p, &raw).expect("write");
        let v = load_vgm(&p).expect("load");
        let roms: Vec<(&str, i64, usize)> =
            v.roms.iter().map(|s| (s.kind, s.start, s.data.len())).collect();
        assert_eq!(roms, vec![("rf5c68", 0x0120, 16)]);
    }

    #[test]
    fn test_no_unconverted_warning() {
        let raw = _raw_vgm(&[0xB0, 0x08, 0xFF], &[]);
        let p = tmp_dir().join("warn.vgm");
        std::fs::write(&p, &raw).expect("write");
        let v = load_vgm(&p).expect("load");
        assert!(!v.warnings.iter().any(|w| w.contains("does not convert")));
    }
}

mod TestRf5cCollector {
    use super::*;

    #[test]
    fn test_one_burst_is_one_hit() {
        let (_ram, samples, offs, warn) =
            collect_rf5c(&_synthetic("rf5c68", SynthKw::default()), "rf5c68", Some(RF5C68_CLOCK), 0);
        assert!(warn.is_empty());
        assert_eq!(samples.len(), 1);
        assert_eq!(samples[0].hits.len(), 1);
        let hit = &samples[0].hits[0];
        assert_eq!((hit.voice, hit.start, hit.step), (0, 0x1000, 0x1000));
        assert_eq!((hit.env, hit.pan), (0xCC, 0x77));
        assert_eq!(offs, vec![]);
    }

    #[test]
    fn test_sample_body_is_sign_magnitude() {
        let mut data = vec![0x80u8, 0x81, 0x81, 0x01, 0x7F, 0x7E];
        data.extend(_body(10, 1));
        // 0x80 -> 0, 0x81 -> +1, 0x01 -> -1, 0x7F -> -127, 0x7E -> -126
        let body: Vec<u8> = data
            .iter()
            .map(|&b| (sign_magnitude(b as i32) & 0xFF) as u8)
            .collect();
        assert_eq!(body[..6], [0, 1, 1, 255, 129, 130]);
        let (_ram, samples, _offs, _warn) = collect_rf5c(
            &_synthetic(
                "rf5c68",
                SynthKw {
                    data: Some(data),
                    ..Default::default()
                },
            ),
            "rf5c68",
            Some(RF5C68_CLOCK),
            0,
        );
        assert_eq!(samples[0].data, body);
    }

    #[test]
    fn test_end_marker_ends_the_body() {
        let (_ram, samples, _offs, _warn) = collect_rf5c(
            &_synthetic(
                "rf5c68",
                SynthKw {
                    data: Some(_body(64, 1)),
                    ..Default::default()
                },
            ),
            "rf5c68",
            Some(RF5C68_CLOCK),
            0,
        );
        assert_eq!(samples[0].length(), 64);
    }

    #[test]
    fn test_a_mid_body_loop_is_kept() {
        let (_ram, samples, _offs, _warn) = collect_rf5c(
            &_synthetic(
                "rf5c68",
                SynthKw {
                    data: Some(_body(100, 1)),
                    loop_: Some(0x1000 + 20),
                    ..Default::default()
                },
            ),
            "rf5c68",
            Some(RF5C68_CLOCK),
            0,
        );
        let smp = &samples[0];
        assert_eq!(smp.loop_start, Some(20));
        assert_eq!(smp.loop_end, Some(100));
    }

    #[test]
    fn test_a_loop_at_the_end_is_a_hold() {
        // Drivers that want a one-shot point the loop at the last byte before
        // the end marker; that is not a musical loop.
        let (_ram, samples, _offs, _warn) = collect_rf5c(
            &_synthetic(
                "rf5c68",
                SynthKw {
                    data: Some(_body(100, 1)),
                    loop_: Some(0x1000 + 99),
                    ..Default::default()
                },
            ),
            "rf5c68",
            Some(RF5C68_CLOCK),
            0,
        );
        assert!(samples[0].loop_start.is_none());
    }

    #[test]
    fn test_retrigger_does_not_emit_an_off() {
        let mut writes = _lattice();
        writes.extend(_note(1000, &NoteKw::default()));
        writes.extend(_note(1100, &NoteKw::default()));
        // Second note disables then re-enables channel 0 in the same burst.
        let mut rom_data = _body(200, 1);
        rom_data.push(0xFF);
        let rom = RomSlice {
            chip: 0,
            rom_size: 0,
            start: 0x1000,
            data: rom_data,
            kind: "rf5c68",
            time: 0,
        };
        let (_ram, samples, offs, _warn) =
            collect_rf5c(&_vgm(writes, vec![rom], RF5C68_CLOCK, 0), "rf5c68", Some(RF5C68_CLOCK), 0);
        assert_eq!(samples[0].hits.len(), 2);
        assert_eq!(offs, vec![]);
    }

    #[test]
    fn test_a_real_off_is_emitted() {
        let mut writes = _lattice();
        writes.extend(_note(1000, &NoteKw::default()));
        writes.push(_enable(60000, 0xFF, "rf5c68")); // disable everything
        let mut rom_data = _body(200, 1);
        rom_data.push(0xFF);
        let rom = RomSlice {
            chip: 0,
            rom_size: 0,
            start: 0x1000,
            data: rom_data,
            kind: "rf5c68",
            time: 0,
        };
        let (_ram, _samples, offs, _warn) =
            collect_rf5c(&_vgm(writes, vec![rom], RF5C68_CLOCK, 0), "rf5c68", Some(RF5C68_CLOCK), 0);
        let got: Vec<(i64, i32)> = offs.iter().map(|o| (o.sample_time, o.voice)).collect();
        assert_eq!(got, vec![(60000, 0)]);
    }

    #[test]
    fn test_rate_is_clock_times_step_over_786432() {
        assert_almost_eq!(rf5c_rate(8_000_000, 0x1000), 8_000_000f64 * 4096.0 / 786_432.0);
        assert_almost_eq!(rf5c_rate(12_500_000, 0x1000), 12_500_000f64 * 4096.0 / 786_432.0);
    }

    #[test]
    fn test_live_step_writes_are_recorded_on_the_hit() {
        // A microsample can hold one key while the driver writes the melody
        // into $02/$03. The key-on step is the note and the later writes are
        // its pitch trajectory.
        let mut writes = _lattice();
        writes.extend(_note(1000, &NoteKw::default()));
        writes.push(_select(5000, 0, "rf5c68"));
        writes.push(_reg(5000, 0x02, 0x00, "rf5c68"));
        writes.push(_reg(5000, 0x03, 0x20, "rf5c68")); // step 0x2000, one octave up
        let mut rom_data = _body(200, 1);
        rom_data.push(0xFF);
        let rom = RomSlice {
            chip: 0,
            rom_size: 0,
            start: 0x1000,
            data: rom_data,
            kind: "rf5c68",
            time: 0,
        };
        let vgm = _vgm(writes, vec![rom], RF5C68_CLOCK, 0);
        let (_ram, samples, _offs, _warn) =
            collect_rf5c(&vgm, "rf5c68", Some(RF5C68_CLOCK), 0);
        let hit = &samples[0].hits[0];
        assert_eq!(hit.step, 0x1000);
        assert_eq!(hit.step_pts, vec![(5000, 0x2000)]);
        let song = analyze(&vgm, AnalyzeOptions::default()).expect("analyze");
        let ev = song
            .events
            .iter()
            .find(|e| e.on && e.pcm)
            .expect("pcm event");
        assert_eq!(ev.sweep, vec![(5000, 768)]);
    }

    #[test]
    fn test_banked_memory_write_lands_in_the_window() {
        let rom = RomSlice {
            chip: 0,
            rom_size: 0,
            start: 0x200,
            data: {
                let mut d = vec![0x80u8; 8];
                d.push(0xFF);
                d
            },
            kind: "rf5c68",
            time: 0,
        };
        let mut writes = _lattice();
        writes.push(ChipWrite::new(0, "rf5c68", 0, 0x07, 0x01)); // bank 1
        writes.push(ChipWrite::new(0, "rf5c68_mem", 0, 0x1000 | 0x200, 0x90));
        let ram = build_rf5c_ram(&_vgm(writes, vec![rom], RF5C68_CLOCK, 0), "rf5c68", 0);
        assert_eq!(ram[0x200], 0x80);
        assert_eq!(ram[0x1200], 0x90);
    }
}

mod TestRf5cWriter {
    use super::*;

    #[test]
    fn test_rf5c68_is_one_system_with_eight_channels() {
        let song = analyze(&_synthetic("rf5c68", SynthKw::default()), AnalyzeOptions::default())
            .expect("analyze");
        let data = write_fur(&song, WriteOptions::default());
        let m = parse_fur(&data).expect("parse");
        // The YM2151 lattice writes a heartbeat for the grid; the RF5C68 is
        // the second system.
        assert_eq!(m.chips, vec![CHIP_YM2151, CHIP_RF5C68]);
        assert_eq!(m.n_ch, 8 + RF5C_VOICES);
        let total: usize = m.chips.iter().map(|&c| chip_channels(c)).sum();
        assert_eq!(total, m.n_ch);
        let flags = _flag_texts(&data);
        assert!(
            flags.iter().any(|f| f.contains("chipType=0")),
            "{:?}",
            flags
        );
        assert!(
            flags.iter().any(|f| f.contains("clockSel=0")),
            "{:?}",
            flags
        );
    }

    #[test]
    fn test_rf5c164_sets_the_chip_type_flag() {
        let song = analyze(&_synthetic("rf5c164", SynthKw::default()), AnalyzeOptions::default())
            .expect("analyze");
        let data = write_fur(&song, WriteOptions::default());
        let m = parse_fur(&data).expect("parse");
        assert_eq!(m.chips, vec![CHIP_YM2151, CHIP_RF5C68]);
        let flags = _flag_texts(&data);
        assert!(
            flags.iter().any(|f| f.contains("chipType=1")),
            "{:?}",
            flags
        );
        assert!(
            flags.iter().any(|f| f.contains("clockSel=2")),
            "{:?}",
            flags
        );
    }

    #[test]
    fn test_instrument_type_is_rf5c68() {
        let song = analyze(&_synthetic("rf5c68", SynthKw::default()), AnalyzeOptions::default())
            .expect("analyze");
        let data = write_fur(&song, WriteOptions::default());
        let types: Vec<i32> = _iter_ins(&data)
            .iter()
            .map(|(_pos, _magic, _size, payload)| {
                (payload[2] as i32) | ((payload[3] as i32) << 8)
            })
            .collect();
        assert!(types.contains(&INS_RF5C68), "{:?}", types);
    }
}

mod TestRf5cRoundTrip {
    use super::*;

    fn _round_trip(vgm: &VgmFile, tag: &str) -> (Song, VgmFile, PathBuf) {
        let song = analyze(vgm, AnalyzeOptions::default()).expect("analyze");
        // Each case gets its own file names: the suite runs tests in parallel
        // and the reference's shared temp path would race here.
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let fur = tmp_dir().join(format!("rf5c_round_{tag}_{stamp}.fur"));
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
        let out = tmp_dir().join(format!("rf5c_round_{tag}_{stamp}.export.vgm"));
        export_vgm(&fur, &out).expect("export");
        (song, load_vgm(&out).expect("load export"), fur)
    }

    fn _keyons(v: &VgmFile) -> Vec<(i64, i32)> {
        let mut out: Vec<(i64, i32)> = Vec::new();
        let mut prev: u32 = 0xFF;
        for w in &v.writes {
            if (w.chip == "rf5c68" || w.chip == "rf5c164") && (w.reg & 0xFF) == 0x08 {
                for i in 0..8 {
                    if (prev >> i) & 1 == 1 && (w.val >> i) & 1 == 0 {
                        out.push((w.sample, i as i32));
                    }
                }
                prev = w.val;
            }
        }
        out
    }

    fn _assert_matched(a: &[(i64, i32)], b: &[(i64, i32)]) {
        let mut used: HashSet<usize> = HashSet::new();
        let mut missed = 0;
        for &(t, ch) in a {
            let mut best: Option<(i64, usize)> = None;
            for (j, &(t2, ch2)) in b.iter().enumerate() {
                if used.contains(&j) || ch2 != ch {
                    continue;
                }
                let d = (t2 - t).abs();
                if d <= 2000 && (best.is_none() || d < best.unwrap().0) {
                    best = Some((d, j));
                }
            }
            match best {
                None => missed += 1,
                Some((_d, j)) => {
                    used.insert(j);
                }
            }
        }
        assert_eq!(missed, 0);
    }

    #[test]
    fn test_rf5c68_round_trips_its_keyons() {
        skip_without_furnace!();
        let src = _synthetic("rf5c68", SynthKw::default());
        let (song, exp, fur) = _round_trip(&src, "68");
        assert!(!song.rf5c68_samples.is_empty());
        assert!(song.rf5c68_samples.iter().all(|s| !s.data.is_empty()));
        let m = parse_fur(&std::fs::read(&fur).expect("read fur")).expect("parse");
        assert_eq!(m.chips, vec![CHIP_YM2151, CHIP_RF5C68]);
        assert_eq!(exp.rf5c68_clock, RF5C68_CLOCK);
        let src_keys = _keyons(&src);
        let exp_keys: Vec<(i64, i32)> = _keyons(&exp)
            .into_iter()
            .filter(|k| k.0 < src.total_samples)
            .collect();
        assert_eq!(src_keys.len(), exp_keys.len());
        _assert_matched(&src_keys, &exp_keys);
    }

    #[test]
    fn test_rf5c164_round_trips_its_keyons() {
        skip_without_furnace!();
        let src = _synthetic(
            "rf5c164",
            SynthKw {
                start: 0x2000,
                data: Some(_body(200, 5)),
                ..Default::default()
            },
        );
        let (song, exp, fur) = _round_trip(&src, "68");
        assert!(!song.rf5c164_samples.is_empty());
        assert!(song.rf5c164_samples.iter().all(|s| !s.data.is_empty()));
        let m = parse_fur(&std::fs::read(&fur).expect("read fur")).expect("parse");
        assert_eq!(m.chips, vec![CHIP_YM2151, CHIP_RF5C68]);
        assert_eq!(exp.rf5c164_clock, RF5C164_CLOCK);
        let src_keys = _keyons(&src);
        let exp_keys: Vec<(i64, i32)> = _keyons(&exp)
            .into_iter()
            .filter(|k| k.0 < src.total_samples)
            .collect();
        assert_eq!(src_keys.len(), exp_keys.len());
        _assert_matched(&src_keys, &exp_keys);
    }
}
