//! OPL-family (YM3526/OPL, YM3812/OPL2, YMF262/OPL3) support.
//!
//! Covers the VGM commands and header clocks, Furnace's channel and slot tables
//! (an OPL3 permutes its eighteen columns and pairs them for four-operator
//! voices), the pitch model, rhythm mode with the frequency registers its kit
//! shares, the .fur emission of DIV_INS_OPL and DIV_INS_OPL_DRUMS instruments,
//! and - with the bundled console build - a round trip through Furnace that
//! reads back what it drove.

mod common;
#[allow(unused_imports)]
use common::*;

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use vgm2fur::analyze::{analyze, format_report, AnalyzeOptions};
use vgm2fur::diag::{
    export_fur, opl_attacks, opl_keyed_intervals, opl_pitch_stream, opl_voice_addrs, pitch_runs,
};
use vgm2fur::furio::{iter_blocks, parse_fur, FurRow};
use vgm2fur::furwrite::{
    write_fur, WriteOptions, CHIP_OPL, CHIP_OPL2, CHIP_OPL2_DRUMS, CHIP_OPL3,
    CHIP_OPL_DRUMS, INS_OPL, INS_OPL_DRUMS,
};
use vgm2fur::model::{NoteEvent, Song};
use vgm2fur::opl::{
    channel_count, drum_channel, is_drum_channel, melodic_count, opl_module_clock, opl_note_fnum,
    opl_note_for, opl_pitch_units, pitch_table, to_freq, FreqPitchTable, OPLState,  
    KIND_OPL, KIND_OPL2, KIND_OPL3, N as OPL_N, SLOTS_OPL2_DRUMS,
    SLOT_ADDR,
};
use vgm2fur::parity::{py_int, py_round};
use vgm2fur::timing::{
    _keyons_by_channel, estimate_grid, MIN_ROW_SAMPLES, TARGET_TICK_SAMPLES,
};
use vgm2fur::vgm::{chips_in_head, load_vgm, ChipWrite, VgmFile, VGM_RATE};

const CLOCK_OPL2: u32 = 3579545;
const CLOCK_OPL3: u32 = 14318180;
/// The channel the note-lattice heartbeat keys, kept clear of the channels a
/// test asserts on.
const PULSE: i32 = 5;

const OPLL_CLOCK: u32 = 3579545;

fn tmp() -> PathBuf {
    let path = std::env::temp_dir().join("vgm2151fur_opl");
    std::fs::create_dir_all(&path).expect("temp dir");
    path
}

// --- synthetic VGM construction ---------------------------------------------

#[derive(Clone, Copy)]
struct VgmOpts {
    kind: &'static str,
    total_samples: i64,
    opl_clock: Option<u32>,
    opl2_clock: Option<u32>,
    opl3_clock: Option<u32>,
    opl2_dual: bool,
    opl2_pan: bool,
    ym2413_clock: u32,
    ym2203_clock: u32,
}

impl Default for VgmOpts {
    fn default() -> Self {
        VgmOpts {
            kind: KIND_OPL2,
            total_samples: 400000,
            opl_clock: None,
            opl2_clock: None,
            opl3_clock: None,
            opl2_dual: false,
            opl2_pan: false,
            ym2413_clock: 0,
            ym2203_clock: 0,
        }
    }
}

fn _vgm(writes: Vec<ChipWrite>, opts: VgmOpts) -> VgmFile {
    let mut out = VgmFile::default();
    out.path = PathBuf::from("synthetic.vgm");
    out.buf = Vec::new();
    out.version = 0x151;
    out.total_samples = opts.total_samples;
    out.loop_samples = 0;
    out.loop_file_pos = None;
    out.loop_sample = None;
    out.ym2151_clock = 0;
    out.k007232_clock = 0;
    out.gd3 = Default::default();
    out.roms = Vec::new();
    match opts.kind {
        KIND_OPL => out.opl_clock = CLOCK_OPL2,
        KIND_OPL3 => out.opl3_clock = CLOCK_OPL3,
        _ => out.opl2_clock = CLOCK_OPL2,
    }
    if let Some(clock) = opts.opl_clock {
        out.opl_clock = clock;
    }
    if let Some(clock) = opts.opl2_clock {
        out.opl2_clock = clock;
    }
    if let Some(clock) = opts.opl3_clock {
        out.opl3_clock = clock;
    }
    out.opl2_dual = opts.opl2_dual;
    out.opl2_pan = opts.opl2_pan;
    out.ym2413_clock = opts.ym2413_clock;
    out.ym2203_clock = opts.ym2203_clock;
    out.writes = writes;
    out.writes.sort_by_key(|w| w.sample);
    out
}

/// A loadable VGM whose OPL clock sits in the given header field.
fn _raw_vgm(body: &[u8], clock_off: usize, clock: u32, dual: bool, pan: bool) -> Vec<u8> {
    let mut buf = vec![0u8; 0xC0];
    buf[0..4].copy_from_slice(b"Vgm ");
    buf[8..12].copy_from_slice(&0x151u32.to_le_bytes());
    buf[0x18..0x1C].copy_from_slice(&44100u32.to_le_bytes());
    buf[0x34..0x38].copy_from_slice(&((0xC0u32 - 0x34).to_le_bytes()));
    let mut raw = clock & 0x3FFFFFFF;
    if dual {
        raw |= 0x40000000;
    }
    if pan {
        raw |= 0x80000000;
    }
    buf[clock_off..clock_off + 4].copy_from_slice(&raw.to_le_bytes());
    buf.extend_from_slice(body);
    buf.push(0x66);
    let len = (buf.len() - 4) as u32;
    buf[4..8].copy_from_slice(&len.to_le_bytes());
    buf
}

// --- register helpers -------------------------------------------------------

/// The register as the log spells it: bank B carries bit 8 on an OPL3.
fn reg(kind: &str, addr: i32) -> u32 {
    if kind == KIND_OPL3 {
        addr as u32
    } else {
        (addr & 0xFF) as u32
    }
}

#[derive(Clone, Copy)]
struct OpSpec {
    am: u32,
    vib: u32,
    sus: u32,
    ksr: u32,
    mult: u32,
    tl: u32,
    ksl: u32,
    ar: u32,
    dr: u32,
    sl: u32,
    rr: u32,
    ws: u32,
}

impl Default for OpSpec {
    fn default() -> Self {
        OpSpec {
            am: 0,
            vib: 0,
            sus: 0,
            ksr: 0,
            mult: 1,
            tl: 20,
            ksl: 0,
            ar: 15,
            dr: 5,
            sl: 9,
            rr: 4,
            ws: 0,
        }
    }
}

/// All five registers of one operator, named as the datasheet names them.
fn op_writes(t: i64, kind: &'static str, addr: i32, s: OpSpec) -> Vec<ChipWrite> {
    vec![
        ChipWrite::new(
            t,
            kind,
            0,
            reg(kind, addr + 0x20),
            (s.am << 7) | (s.vib << 6) | (s.sus << 5) | (s.ksr << 4) | s.mult,
        ),
        ChipWrite::new(t + 1, kind, 0, reg(kind, addr + 0x40), (s.ksl << 6) | s.tl),
        ChipWrite::new(t + 2, kind, 0, reg(kind, addr + 0x60), (s.ar << 4) | s.dr),
        ChipWrite::new(t + 3, kind, 0, reg(kind, addr + 0x80), (s.sl << 4) | s.rr),
        ChipWrite::new(t + 4, kind, 0, reg(kind, addr + 0xE0), s.ws),
    ]
}

fn op_writes_cid(t: i64, kind: &'static str, addr: i32, s: OpSpec, cid: u8) -> Vec<ChipWrite> {
    op_writes(t, kind, addr, s)
        .into_iter()
        .map(|mut w| {
            w.chip_id = cid;
            w
        })
        .collect()
}

/// The operator addresses of the voice a module channel drives.
fn voice_slots(kind: &str, ch: i32, four: bool, drums: bool) -> Vec<u16> {
    let st = OPLState::new(kind, drums);
    let rows = if four { 4 } else { 2 };
    (0..rows)
        .filter_map(|r| {
            let slot = st.slots[r][ch as usize];
            (slot != OPL_N).then(|| SLOT_ADDR[slot as usize])
        })
        .collect()
}

#[derive(Clone, Copy)]
struct KeyOn {
    block: i32,
    fnum: i32,
    alg: i32,
    fb: i32,
    pan: i32,
    drums: bool,
    cid: u8,
    off_time: i64,
}

impl Default for KeyOn {
    fn default() -> Self {
        KeyOn {
            block: 4,
            fnum: 580,
            alg: 0,
            fb: 2,
            pan: 3,
            drums: false,
            cid: 0,
            off_time: 60000,
        }
    }
}

/// Frequency, connection and key bit of one voice, then its release.
fn key_on_full(t: i64, kind: &'static str, ch: i32, k: KeyOn) -> Vec<ChipWrite> {
    let st = OPLState::new(kind, k.drums);
    let cm = i32::from(st.chan_map[ch as usize]);
    let hi = (k.block << 2) | ((k.fnum >> 8) & 3);
    let mut out = vec![
        ChipWrite::new(t, kind, k.cid, reg(kind, cm + 0xA0), (k.fnum & 0xFF) as u32),
        ChipWrite::new(
            t,
            kind,
            k.cid,
            reg(kind, cm + 0xB0),
            (hi | 0x20) as u32,
        ),
        ChipWrite::new(
            t,
            kind,
            k.cid,
            reg(kind, cm + 0xC0),
            ((k.alg & 1) | ((k.fb & 7) << 1) | ((k.pan & 3) << 4)) as u32,
        ),
    ];
    if k.off_time != 0 {
        out.push(ChipWrite::new(
            k.off_time,
            kind,
            k.cid,
            reg(kind, cm + 0xB0),
            hi as u32,
        ));
    }
    out
}

/// The frequency-high write of a channel, at its hardware register.
///
/// A release has to name the hardware channel the engine drives for that
/// column, which an OPL3 permutes: clearing bit 5 of the wrong `0xBn` leaves
/// the voice keyed and swallows the next note.
fn hi_write(
    kind: &'static str,
    ch: i32,
    t: i64,
    block: i32,
    fnum: i32,
    cid: u8,
    kon: bool,
) -> ChipWrite {
    let st = OPLState::new(kind, false);
    let cm = i32::from(st.chan_map[ch as usize]);
    ChipWrite::new(
        t,
        kind,
        cid,
        reg(kind, cm + 0xB0),
        ((block << 2) | ((fnum >> 8) & 3) | if kon { 0x20 } else { 0 }) as u32,
    )
}

#[derive(Clone)]
struct Voice {
    four: bool,
    drums: bool,
    ops: Option<Vec<OpSpec>>,
    cid: u8,
    block: i32,
    fnum: i32,
    alg: i32,
    fb: i32,
    pan: i32,
    off_time: i64,
}

impl Default for Voice {
    fn default() -> Self {
        Voice {
            four: false,
            drums: false,
            ops: None,
            cid: 0,
            block: 4,
            fnum: 580,
            alg: 0,
            fb: 2,
            pan: 3,
            off_time: 60000,
        }
    }
}

/// Load a voice's operators and key it on.
fn voice(t: i64, kind: &'static str, ch: i32, v: Voice) -> Vec<ChipWrite> {
    let addrs = voice_slots(kind, ch, v.four, v.drums);
    let specs = v.ops.unwrap_or_else(|| vec![OpSpec::default(); addrs.len()]);
    let mut out: Vec<ChipWrite> = Vec::new();
    for (addr, spec) in addrs.iter().zip(specs.iter()) {
        out.extend(op_writes_cid(t, kind, i32::from(*addr), *spec, v.cid));
    }
    out.extend(key_on_full(
        t + 20,
        kind,
        ch,
        KeyOn {
            block: v.block,
            fnum: v.fnum,
            alg: v.alg,
            fb: v.fb,
            pan: v.pan,
            drums: v.drums,
            cid: v.cid,
            off_time: v.off_time,
        },
    ));
    out
}

/// Key-on ticks, so the grid estimator has a lattice to measure.
fn heart_full(
    kind: &'static str,
    ch: i32,
    start: i64,
    n: i64,
    spacing: i64,
) -> Vec<ChipWrite> {
    let st = OPLState::new(kind, false);
    let cm = i32::from(st.chan_map[ch as usize]);
    let mut out: Vec<ChipWrite> = Vec::new();
    for k in 0..n {
        let t = start + k * spacing;
        out.push(ChipWrite::new(t, kind, 0, reg(kind, cm + 0xA0), 0x2D));
        out.push(ChipWrite::new(
            t,
            kind,
            0,
            reg(kind, cm + 0xB0),
            ((4 << 2) | 1 | 0x20) as u32,
        ));
        out.push(ChipWrite::new(
            t + spacing / 2,
            kind,
            0,
            reg(kind, cm + 0xB0),
            ((4 << 2) | 1) as u32,
        ));
    }
    out
}

fn heart_default() -> Vec<ChipWrite> {
    heart_full(KIND_OPL2, PULSE, 100, 60, 5000)
}

fn heart(kind: &'static str) -> Vec<ChipWrite> {
    heart_full(kind, PULSE, 100, 60, 5000)
}

fn heart_ch(kind: &'static str, ch: i32) -> Vec<ChipWrite> {
    heart_full(kind, ch, 100, 60, 5000)
}

/// The rhythm latch: both depth bits, mode on, and the trigger bits given.
fn rhythm_full(t: i64, kind: &'static str, bits: u32, depth: u32, cid: u8) -> ChipWrite {
    ChipWrite::new(t, kind, cid, 0xBD, (depth << 6) | 0x20 | bits)
}

fn rhythm(t: i64) -> ChipWrite {
    rhythm_full(t, KIND_OPL2, 0, 3, 0)
}

fn rhythm_bits(t: i64, bits: u32) -> ChipWrite {
    rhythm_full(t, KIND_OPL2, bits, 3, 0)
}

fn rhythm_kind_bits(t: i64, kind: &'static str, bits: u32) -> ChipWrite {
    rhythm_full(t, kind, bits, 3, 0)
}

fn opll_wr(t: i64, addr: u32, val: u32) -> ChipWrite {
    ChipWrite::new(t, "opll", 0, addr, val & 0xFF)
}

fn opll_wr_cid(t: i64, addr: u32, val: u32, cid: u8) -> ChipWrite {
    ChipWrite::new(t, "opll", cid, addr, val & 0xFF)
}

#[derive(Clone, Copy)]
struct OpllNote {
    ch: i32,
    preset: u32,
    level: u32,
    block: i32,
    fnum: i32,
    off: Option<i64>,
    cid: u8,
}

impl Default for OpllNote {
    fn default() -> Self {
        OpllNote {
            ch: 3,
            preset: 13,
            level: 0,
            block: 4,
            fnum: 580,
            off: None,
            cid: 0,
        }
    }
}

/// A keyed YM2413 voice; the key bit is a level, so off clears it.
fn opll_note(t: i64, n: OpllNote) -> Vec<ChipWrite> {
    let hi = ((n.block & 7) << 1) | ((n.fnum >> 8) & 1);
    let mut out = vec![
        opll_wr_cid(t - 100, (0x30 + n.ch) as u32, (n.preset << 4) | n.level, n.cid),
        opll_wr_cid(t, (0x20 + n.ch) as u32, (hi | 0x10) as u32, n.cid),
        opll_wr_cid(t + 1, (0x10 + n.ch) as u32, (n.fnum & 0xFF) as u32, n.cid),
        opll_wr_cid(t + 2, (0x20 + n.ch) as u32, (hi | 0x10) as u32, n.cid),
    ];
    if let Some(off) = n.off {
        out.push(opll_wr_cid(off, (0x20 + n.ch) as u32, hi as u32, n.cid));
    }
    out
}

fn opll_vgm(writes: Vec<ChipWrite>, total_samples: i64) -> VgmFile {
    _vgm(
        writes,
        VgmOpts {
            ym2413_clock: OPLL_CLOCK,
            total_samples,
            ..Default::default()
        },
    )
}

fn ins_types(data: &[u8]) -> Vec<i32> {
    iter_blocks(data)
        .expect("blocks")
        .into_iter()
        .filter(|b| b.magic == *b"INS2")
        .map(|b| u16::from_le_bytes([b.payload[2], b.payload[3]]) as i32)
        .collect()
}

fn find_subslice(hay: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || hay.len() < needle.len() {
        return None;
    }
    hay.windows(needle.len()).position(|w| w == needle)
}

fn contains_bytes(hay: &[u8], needle: &[u8]) -> bool {
    find_subslice(hay, needle).is_some()
}

fn fm_feature(data: &[u8], index: usize) -> Vec<u8> {
    let mut seen: i64 = -1;
    for block in iter_blocks(data).expect("blocks") {
        if block.magic != *b"INS2" {
            continue;
        }
        seen += 1;
        if seen != index as i64 {
            continue;
        }
        let Some(pos) = find_subslice(block.payload, b"FM") else {
            return Vec::new();
        };
        let size =
            u16::from_le_bytes([block.payload[pos + 2], block.payload[pos + 3]]) as usize;
        return block.payload[pos + 4..pos + 4 + size].to_vec();
    }
    Vec::new()
}

/// [(chip_id, column, sample, unit)] at every key-on of one chip's stream.
///
/// The unit is the phase step `fnum << block`, which is what sets the
/// frequency: a rip may hold a register in another octave split and Furnace
/// re-derives the pair from its own table. Grouping by channel keeps a
/// comparison from drifting when one voice holds an extra note, and the kit's
/// shared frequency registers are named by their own column.
fn keyon_events(vgm: &VgmFile, kind: &str, drums: bool) -> Vec<(u8, i32, i64, i32)> {
    let st = OPLState::new(kind, drums);
    let mut chan_of: BTreeMap<(i32, i32), Vec<i32>> = BTreeMap::new();
    for (fch, &cm) in st.chan_map.iter().enumerate() {
        let port = if st.opl3 { i32::from((cm >> 8) & 1) } else { 0 };
        chan_of
            .entry((port, i32::from(cm & 0xFF)))
            .or_default()
            .push(fch as i32);
    }
    let mut image: BTreeMap<(i32, i32), u32> = BTreeMap::new();
    let mut keyed: BTreeMap<(i32, i32), bool> = BTreeMap::new();
    let mut out: Vec<(u8, i32, i64, i32)> = Vec::new();
    let mut trig: u32 = 0;

    let push = |out: &mut Vec<(u8, i32, i64, i32)>,
                image: &BTreeMap<(i32, i32), u32>,
                cid: u8,
                fch: i32,
                sample: i64,
                port: i32,
                off: i32| {
        let hi = image.get(&(port, 0xB0 + off)).copied().unwrap_or(0);
        let lo = image.get(&(port, 0xA0 + off)).copied().unwrap_or(0);
        out.push((cid, fch, sample, ((((hi & 3) << 8) | lo) << ((hi >> 2) & 7)) as i32));
    };

    for w in &vgm.writes {
        if w.chip != kind {
            continue;
        }
        let port = if st.opl3 {
            ((w.reg >> 8) & 1) as i32
        } else {
            0
        };
        let r = (w.reg & 0xFF) as i32;
        image.insert((port, r), w.val & 0xFF);
        if (0xB0..=0xB8).contains(&r) {
            let on = (w.val & 0x20) != 0;
            let was = keyed.get(&(port, r)).copied().unwrap_or(false);
            keyed.insert((port, r), on);
            if !(on && !was) {
                continue;
            }
            if let Some(chans) = chan_of.get(&(port, r - 0xB0)) {
                for &fch in chans {
                    if !drums || fch < st.melodic {
                        push(
                            &mut out,
                            &image,
                            w.chip_id,
                            fch,
                            w.sample,
                            port,
                            r - 0xB0,
                        );
                    }
                }
            }
        } else if r == 0xBD && !st.opl3 {
            let bits = if (w.val & 0x20) != 0 { w.val & 0x1F } else { 0 };
            for i in 0..5 {
                let bit = 1u32 << (4 - i);
                if !((bits & bit) != 0 && (trig & bit) == 0) {
                    continue;
                }
                let cm = i32::from(st.chan_map[st.melodic as usize + i]);
                push(
                    &mut out,
                    &image,
                    w.chip_id,
                    st.melodic + i as i32,
                    w.sample,
                    (cm >> 8) & 1,
                    cm & 0xFF,
                );
            }
            trig = bits;
        }
    }
    out
}

/// Every key-on phase step of a stream, in time order.
fn keyon_units(vgm: &VgmFile, kind: &str, drums: bool) -> Vec<i32> {
    keyon_events(vgm, kind, drums)
        .into_iter()
        .map(|(_cid, _fch, _t, unit)| unit)
        .collect()
}

// --- analysis helpers -------------------------------------------------------

fn analyze_default(src: &VgmFile) -> Song {
    analyze(src, AnalyzeOptions::default()).expect("analyze")
}

fn analyze_speed(src: &VgmFile, speed: i32) -> Song {
    analyze(
        src,
        AnalyzeOptions {
            speed: Some(speed),
            ..Default::default()
        },
    )
    .expect("analyze")
}

fn write_plain(song: &Song) -> Vec<u8> {
    write_fur(
        song,
        WriteOptions {
            include_pcm: false,
            ..Default::default()
        },
    )
}

fn patch(song: &Song, ins: i32) -> &vgm2fur::fm::FMPatch {
    &song.fm_patches[ins as usize]
}

#[allow(non_snake_case)]
mod TestOplLoader {
    use super::*;

    #[test]
    fn test_opl2_command_and_clock() {
        let body = [0x5Au8, 0x20, 0x01, 0x5A, 0xB0, 0x24];
        let path = tmp().join("opl2_raw.vgm");
        std::fs::write(&path, _raw_vgm(&body, 0x50, CLOCK_OPL2, false, false)).unwrap();
        let vgm = load_vgm(&path).unwrap();
        assert_eq!(vgm.opl2_clock, CLOCK_OPL2);
        let writes: Vec<(&str, u32, u32)> = vgm
            .writes
            .iter()
            .map(|w| (w.chip, w.reg, w.val))
            .collect();
        assert_eq!(writes, vec![("opl2", 0x20, 0x01), ("opl2", 0xB0, 0x24)]);
        let head = std::fs::read(&path).unwrap();
        assert!(chips_in_head(&head[..0xC0]).contains("opl2"));
    }

    #[test]
    fn test_opl1_command_and_clock() {
        let path = tmp().join("opl1_raw.vgm");
        std::fs::write(
            &path,
            _raw_vgm(&[0x5B, 0x20, 0x11], 0x54, CLOCK_OPL2, false, false),
        )
        .unwrap();
        let vgm = load_vgm(&path).unwrap();
        assert_eq!(vgm.opl_clock, CLOCK_OPL2);
        let kinds: BTreeSet<&str> = vgm.writes.iter().map(|w| w.chip).collect();
        assert_eq!(kinds, BTreeSet::from(["opl"]));
        let head = std::fs::read(&path).unwrap();
        assert!(chips_in_head(&head[..0xC0]).contains("opl"));
    }

    #[test]
    fn test_opl3_ports_share_one_chip() {
        let path = tmp().join("opl3_raw.vgm");
        std::fs::write(
            &path,
            _raw_vgm(
                &[0x5E, 0xB0, 0x24, 0x5F, 0xB0, 0x04, 0x5F, 0x04, 0x3D],
                0x5C,
                CLOCK_OPL3,
                false,
                false,
            ),
        )
        .unwrap();
        let vgm = load_vgm(&path).unwrap();
        assert_eq!(vgm.opl3_clock, CLOCK_OPL3);
        // Bank B keeps its number in bit 8 of reg, the way a YM2612 bank does.
        let writes: Vec<(&str, u32)> = vgm.writes.iter().map(|w| (w.chip, w.reg)).collect();
        assert_eq!(
            writes,
            vec![("opl3", 0x0B0), ("opl3", 0x1B0), ("opl3", 0x104)]
        );
        let ids: BTreeSet<u8> = vgm.writes.iter().map(|w| w.chip_id).collect();
        assert_eq!(ids, BTreeSet::from([0u8]));
        let head = std::fs::read(&path).unwrap();
        let kinds = chips_in_head(&head[..0xC0]);
        assert!(kinds.contains("opl3"));
        assert!(!kinds.contains("opl2"));
    }

    #[test]
    fn test_dual_opl2_uses_the_second_command() {
        let path = tmp().join("opl2_dual.vgm");
        std::fs::write(
            &path,
            _raw_vgm(
                &[0x5A, 0xB0, 0x24, 0xAA, 0xB1, 0x34],
                0x50,
                CLOCK_OPL2,
                true,
                true,
            ),
        )
        .unwrap();
        let vgm = load_vgm(&path).unwrap();
        assert!(vgm.opl2_dual); // bit 30: a chip answers on $AA
        assert!(vgm.opl2_pan); // bit 31: the pair is hard left/right
        let writes: Vec<(u8, u32)> = vgm.writes.iter().map(|w| (w.chip_id, w.reg)).collect();
        assert_eq!(writes, vec![(0, 0xB0), (1, 0xB1)]);
    }

    #[test]
    fn test_stray_second_chip_is_reported() {
        let path = tmp().join("opl2_stray.vgm");
        std::fs::write(
            &path,
            _raw_vgm(&[0xAA, 0xB0, 0x24], 0x50, CLOCK_OPL2, false, false),
        )
        .unwrap();
        let vgm = load_vgm(&path).unwrap();
        assert!(!vgm.opl2_dual);
        assert_eq!(vgm.writes, Vec::<ChipWrite>::new());
        assert!(
            vgm.warnings.iter().any(|w| w.contains("0xAA")),
            "{:?}",
            vgm.warnings
        );
    }

    #[test]
    fn test_missing_clock_warns_with_the_family_default() {
        let path = tmp().join("opl2_noclock.vgm");
        std::fs::write(
            &path,
            _raw_vgm(&[0x5A, 0xB0, 0x24], 0x50, 0, false, false),
        )
        .unwrap();
        let vgm = load_vgm(&path).unwrap();
        assert_eq!(vgm.opl2_clock, 0);
        assert!(vgm.warnings.iter().any(|w| w.contains("YM3812 clock")));
    }
}

#[allow(non_snake_case)]
mod TestOplPitchModel {
    use super::*;

    fn clock() -> f64 {
        opl_module_clock(CLOCK_OPL2 as f64, false)
    }

    #[test]
    fn test_reference_fnum_pairs() {
        // Measured through Furnace at its default clock: A-440 sits on f-num 290
        // in block 0, an octave up doubles the phase step instead of the block,
        // and a block only advances when f-num would overflow its ten bits.
        let c = clock();
        assert_eq!(opl_note_fnum(57, c, 0x80, false, 0, None), (0, 290));
        assert_eq!(opl_note_fnum(69, c, 0x80, false, 0, None), (0, 580));
        assert_eq!(opl_note_fnum(48, c, 0x80, false, 0, None), (0, 172));
        assert_eq!(opl_note_fnum(84, c, 0x80, false, 0, None), (2, 345));
        assert_eq!(opl_note_fnum(96, c, 0x80, false, 0, None), (3, 344));
    }

    #[test]
    fn test_an_octave_up_doubles_the_phase_step() {
        // From C3 up the f-num has resolution to spare, so an octave is exact to
        // a cent. The bottom octaves are covered by the register sweep below,
        // where one table step is legitimately a fifth of a tone wide.
        let c = clock();
        for note in 60..144 {
            let here = opl_note_fnum(note, c, 0x80, false, 0, None);
            let up = opl_note_fnum(note + 12, c, 0x80, false, 0, None);
            // Integer rounding in the table costs a fraction of a cent at the
            // bottom of the range, so the invariant is a pitch one.
            let err = (opl_pitch_units(up.0, up.1) - opl_pitch_units(here.0, here.1) - 768.0)
                .abs();
            assert!(err / 64.0 < 0.1, "{}", note);
        }
    }

    #[test]
    fn test_opl3_base_is_four_times_the_opl2_one() {
        // Four times the clock over four times the divider: the same phase step
        // at either chip's usual crystal, which is why a YMF262 board and an
        // AdLib card agree on pitch.
        let c = clock();
        let clock3 = opl_module_clock(CLOCK_OPL3 as f64, true);
        for note in [36, 57, 69, 84, 108] {
            let a = opl_note_fnum(note, c, 0x80, false, 0, None);
            let b = opl_note_fnum(note, clock3, 0x80, true, 0, None);
            assert_eq!(a.1 << a.0, b.1 << b.0, "{}", note);
        }
    }

    #[test]
    fn test_module_clock_only_moves_for_a_real_difference() {
        assert_eq!(
            opl_module_clock(CLOCK_OPL2 as f64, false),
            opl_module_clock(0.0, false)
        );
        assert_eq!(opl_module_clock(clock(), false), clock());
        assert_eq!(opl_module_clock(3679545.0, false), 3679545.0);
    }

    #[test]
    fn test_a_macro_compensates_an_octave_relabel_exactly() {
        let tbl = FreqPitchTable::new(clock(), 32768 * 72);
        for note in 12..168 {
            let here = to_freq(tbl.value(note, 0x80, 0));
            let moved = to_freq(tbl.value(note + 12, 0x80, -1536));
            assert_eq!(here.1 << here.0, moved.1 << moved.0, "{}", note);
        }
    }

    #[test]
    fn test_every_playable_register_round_trips() {
        for (opl3, c) in [
            (false, clock()),
            (true, opl_module_clock(14400000.0, true)),
        ] {
            let tbl = FreqPitchTable::new(c, if opl3 { 32768 * 288 } else { 32768 * 72 });
            for block in 0..8 {
                for fnum in 1..1024 {
                    let unit = fnum << block;
                    let est = 57.0
                        + 12.0
                            * ((unit as f64 * c) / (440.0 * tbl.base)).log2();
                    if !(0.0..=179.0).contains(&est) {
                        continue;
                    }
                    let (note, e5) = opl_note_for(block, fnum, c, opl3, None);
                    let got = to_freq(tbl.value(note, e5, 0));
                    let err = (opl_pitch_units(got.0, got.1)
                        - opl_pitch_units(block, fnum))
                    .abs();
                    // Below about C-3 one table step is wider than a half tone,
                    // so the closest cell cannot land on every register.
                    assert!(
                        err < 64.0,
                        "{} {} {} {} {} {:?}",
                        opl3,
                        block,
                        fnum,
                        note,
                        e5,
                        got
                    );
                }
            }
        }
    }

    #[test]
    fn test_exact_phase_step_from_the_second_octave_up() {
        for block in 2..7 {
            for fnum in [0x1FF, 0x2A6, 0x3C0, 514, 900] {
                let (note, e5) = opl_note_for(block, fnum, clock(), false, None);
                let got = to_freq(FreqPitchTable::new(clock(), 32768 * 72).value(note, e5, 0));
                assert_eq!(
                    got.1 << got.0,
                    fnum << block,
                    "{} {} {} {} {:?}",
                    block,
                    fnum,
                    note,
                    e5,
                    got
                );
            }
        }
    }

    #[test]
    fn test_unplayable_register_clamps_instead_of_raising() {
        let (note, e5) = opl_note_for(0, 2, clock(), false, None);
        assert!((0..=179).contains(&note));
        assert!((0x00..=0xFF).contains(&e5));
    }
}

#[allow(non_snake_case)]
mod TestOplChannelMap {
    use super::*;

    #[test]
    fn test_opl3_permutation_pairs_the_bank() {
        let st = OPLState::new(KIND_OPL3, false);
        // Furnace interleaves each pair so a four-operator voice owns two
        // adjacent columns: channel 1 is hardware channel 3, and bank B starts at
        // column 6. Its last three columns are bank A's top pair.
        assert_eq!(&st.chan_map[..8], &[0u16, 3, 1, 4, 2, 5, 0x100, 0x103]);
        assert_eq!(&st.chan_map[15..], &[6u16, 7, 8]);
        assert_eq!(channel_count(KIND_OPL3, false), 18);
    }

    #[test]
    fn test_four_op_bit_selects_the_pair() {
        let mut st = OPLState::new(KIND_OPL3, false);
        assert!(!st.is_four_op(0));
        st.write(1, 0x04, 0x0D); // bank A pairs 0 and 2, bank B pair 0
        assert!(st.is_four_op(0));
        assert!(!st.is_four_op(2));
        assert!(st.is_four_op(4));
        assert!(!st.is_four_op(5)); // an odd column never heads a voice
        assert!(st.is_four_op(6));
        // The four operators: two from each hardware channel of the pair.
        let addrs: Vec<u16> = (0..4)
            .map(|r| SLOT_ADDR[st.slots[r][0] as usize])
            .collect();
        assert_eq!(addrs, vec![0x00, 0x03, 0x08, 0x0B]);
    }

    #[test]
    fn test_kit_slots_and_shared_frequency_registers() {
        let st = OPLState::new(KIND_OPL2, true);
        assert_eq!(melodic_count(KIND_OPL2, true), 6);
        // The kick keeps channel 6's pair; snare, tom, cymbal and hi-hat take one
        // operator each. Snare and hi-hat share a frequency register, so do tom
        // and cymbal - which is why their columns are compared by pitch class.
        let addrs: Vec<u16> = (6..11)
            .map(|f| SLOT_ADDR[st.slots[0][f] as usize])
            .collect();
        assert_eq!(addrs, vec![0x10, 0x14, 0x12, 0x15, 0x11]);
        assert_eq!(&st.chan_map[6..], &[6u16, 7, 8, 8, 7]);
        assert_eq!(drum_channel(1 << 4, 6), 6); // kick
        assert_eq!(drum_channel(1 << 0, 6), 10); // hi-hat
        assert!(!is_drum_channel(KIND_OPL2, true, 6));
        assert!(is_drum_channel(KIND_OPL2, true, 7));
    }

    #[test]
    fn test_a_plain_module_has_no_kit_columns() {
        assert!(!is_drum_channel(KIND_OPL2, false, 7));
        assert_eq!(channel_count(KIND_OPL3, true), 20);
    }
}

#[allow(non_snake_case)]
mod TestOplAnalyze {
    use super::*;

    #[test]
    fn test_mid_note_multiplier_and_feedback_survive_and_reset() {
        // A tracker can raise the multiplier and feedback while a note holds.
        // Both are live timbre changes: keep them as 16xy/11xx row effects,
        // and restore the instrument's own values when the same instrument
        // starts the next note (Furnace does not reload an unchanged voice).
        let ops = vec![
            OpSpec {
                mult: 4,
                ..Default::default()
            },
            OpSpec::default(),
        ];
        let addrs = voice_slots(KIND_OPL3, 14, false, false);
        let cm = i32::from(OPLState::new(KIND_OPL3, false).chan_map[14]);
        let mut ws = voice(
            1000,
            KIND_OPL3,
            14,
            Voice {
                ops: Some(ops.clone()),
                fb: 2,
                off_time: 5000,
                ..Default::default()
            },
        );
        ws.extend([
            ChipWrite::new(
                3000,
                KIND_OPL3,
                0,
                reg(KIND_OPL3, i32::from(addrs[0]) + 0x20),
                0x01,
            ),
            ChipWrite::new(3000, KIND_OPL3, 0, reg(KIND_OPL3, cm + 0xC0), 0x3E),
        ]);
        ws.extend(voice(
            6000,
            KIND_OPL3,
            14,
            Voice {
                ops: Some(ops),
                fb: 2,
                off_time: 10000,
                ..Default::default()
            },
        ));
        let song = analyze_speed(
            &_vgm(
                ws,
                VgmOpts {
                    kind: KIND_OPL3,
                    total_samples: 20000,
                    ..Default::default()
                },
            ),
            1,
        );
        let m = parse_fur(&write_plain(&song)).unwrap();
        let mut fx: BTreeSet<(u8, u8)> = BTreeSet::new();
        for ch in 0..m.n_ch {
            for &oi in &m.orders[ch] {
                if let Some(rows) = m.patterns.get(&(ch as u16, oi)) {
                    for row in rows {
                        for &(cmd, value) in &row.fx {
                            fx.insert((cmd, value));
                        }
                    }
                }
            }
        }
        assert!(fx.contains(&(0x16, 0x11))); // live MULT 4 -> 1
        assert!(fx.contains(&(0x11, 0x07))); // live feedback 2 -> 7
        assert!(fx.contains(&(0x16, 0x14))); // next note restores MULT 4
        assert!(fx.contains(&(0x11, 0x02))); // next note restores feedback 2
    }

    #[test]
    fn test_two_operator_voice_and_instrument() {
        let mut ws = voice(
            100,
            KIND_OPL2,
            3,
            Voice {
                block: 4,
                fnum: 580,
                ..Default::default()
            },
        );
        ws.extend(heart_default());
        let song = analyze_default(&_vgm(ws, VgmOpts::default()));
        assert_eq!(song.fm_chip, KIND_OPL2);
        let ev = song.events.iter().find(|e| e.on && e.ch == 3).unwrap();
        assert_eq!(
            (ev.note, ev.e5),
            opl_note_for(4, 580, opl_module_clock(CLOCK_OPL2 as f64, false), false, None)
        );
        // The carrier's logged attenuation rides the volume column: the
        // instrument keeps the relative level, so the note opens 20 below top.
        assert_eq!(ev.vol, 63 - 20);
        let p = patch(&song, ev.ins);
        assert!(p.opl && !p.four && !p.drum);
        assert_eq!((p.alg, p.fb), (0, 2));
        assert_eq!(p.carriers(), vec![1]);
        assert_eq!(song.fm_vol_max, 63);
    }

    #[test]
    fn test_key_off_ends_the_note() {
        let mut ws = voice(
            100,
            KIND_OPL2,
            1,
            Voice {
                block: 3,
                fnum: 700,
                off_time: 40000,
                ..Default::default()
            },
        );
        ws.extend(heart_default());
        let song = analyze_default(&_vgm(ws, VgmOpts::default()));
        let ons: Vec<&NoteEvent> = song.events.iter().filter(|e| e.on && e.ch == 1).collect();
        let offs: Vec<&NoteEvent> = song.events.iter().filter(|e| !e.on && e.ch == 1).collect();
        assert_eq!(ons.len(), 1);
        assert!(
            !offs.is_empty() && (offs.last().unwrap().sample - 40000).abs() < 800
        );
    }

    #[test]
    fn test_additive_connection_marks_both_operators_as_carriers() {
        let mut ws = voice(
            100,
            KIND_OPL2,
            4,
            Voice {
                alg: 1,
                ..Default::default()
            },
        );
        ws.extend(heart_default());
        let song = analyze_default(&_vgm(ws, VgmOpts::default()));
        let ev = song.events.iter().find(|e| e.on && e.ch == 4).unwrap();
        let p = patch(&song, ev.ins);
        assert_eq!(p.alg, 1);
        assert_eq!(p.carriers(), vec![0, 1]);
    }

    #[test]
    fn test_am_and_vibrato_depth_become_chip_effects() {
        let mut ws = voice(100, KIND_OPL2, 0, Voice::default());
        ws.extend(heart_default());
        ws.extend([
            ChipWrite::new(5000, KIND_OPL2, 0, 0xBD, 0xF0),
            ChipWrite::new(9000, KIND_OPL2, 0, 0xBD, 0xB0),
        ]);
        let song = analyze_default(&_vgm(ws, VgmOpts::default()));
        let fx: Vec<(i32, i32)> = song
            .chip_fx
            .iter()
            .filter(|(_t, c, _v, _tick, _ch)| *c == 0x10 || *c == 0x17)
            .map(|(_t, c, v, _tick, _ch)| (*c, *v))
            .collect();
        assert!(fx.contains(&(0x10, 1))); // deep amplitude modulation
        assert!(fx.contains(&(0x17, 0))); // vibrato depth back to normal
    }

    #[test]
    fn test_rhythm_mode_picks_the_drums_system() {
        let mut ws = voice(
            100,
            KIND_OPL2,
            6,
            Voice {
                drums: true,
                block: 3,
                fnum: 900,
                ..Default::default()
            },
        );
        ws.extend(heart_default());
        // Kick and snare ride the latch bits; the kit owns columns 6 to 10.
        ws.extend([
            rhythm_bits(2000, 0),
            rhythm_bits(10000, 0x18),
            rhythm_bits(14000, 0),
            rhythm_bits(18000, 0x04),
            rhythm_bits(22000, 0),
        ]);
        let song = analyze_default(&_vgm(ws, VgmOpts::default()));
        assert!(song.opl_drums);
        let kit: Vec<&NoteEvent> = song.events.iter().filter(|e| e.on && e.drum).collect();
        let channels: BTreeSet<i32> = kit.iter().map(|e| e.ch).collect();
        assert_eq!(channels, BTreeSet::from([7, 8])); // snare and tom
        assert!(kit.iter().all(|e| patch(&song, e.ins).drum));
        let kick: Vec<&NoteEvent> = song
            .events
            .iter()
            .filter(|e| e.on && e.ch == 6)
            .collect();
        assert!(!kick.is_empty());
        assert!(!patch(&song, kick[0].ins).drum);
    }

    #[test]
    fn test_melodic_key_on_a_kit_channel_is_reported() {
        let mut ws = voice(
            100,
            KIND_OPL2,
            7,
            Voice {
                drums: true,
                block: 3,
                fnum: 500,
                ..Default::default()
            },
        );
        ws.extend(heart_default());
        ws.extend([rhythm_bits(2000, 0x10), rhythm_bits(6000, 0)]);
        let song = analyze_default(&_vgm(ws, VgmOpts::default()));
        assert!(song.opl_drums);
        assert!(!song.events.iter().any(|e| e.on && e.ch == 7));
        assert!(
            song.warnings.iter().any(|w| w.contains("kit")),
            "{:?}",
            song.warnings
        );
    }

    #[test]
    fn test_opl3_rhythm_latches_on_bank_zero() {
        // An OPL3's rhythm register is bank 0's $BD, not bank 1's $1BD.
        let mut ws = heart(KIND_OPL3);
        ws.extend([
            rhythm_kind_bits(2000, KIND_OPL3, 0),
            rhythm_kind_bits(10000, KIND_OPL3, 0x1F),
            rhythm_kind_bits(14000, KIND_OPL3, 0),
        ]);
        let song = analyze_default(&_vgm(
            ws,
            VgmOpts {
                kind: KIND_OPL3,
                ..Default::default()
            },
        ));
        assert!(song.opl_drums);
        let kit: BTreeSet<i32> = song
            .events
            .iter()
            .filter(|e| e.on && e.ch >= 16)
            .map(|e| e.ch)
            .collect();
        assert_eq!(kit, BTreeSet::from([16, 17, 18, 19]));
        assert!(song.events.iter().any(|e| e.on && e.ch == 15));
        assert!(song.drum_notes > 0);
    }

    #[test]
    fn test_opl3_bank_one_bd_is_not_a_rhythm_latch() {
        let mut ws = heart(KIND_OPL3);
        ws.push(ChipWrite::new(1000, KIND_OPL3, 0, 0x1BD, 0x3F));
        let song = analyze_default(&_vgm(
            ws,
            VgmOpts {
                kind: KIND_OPL3,
                ..Default::default()
            },
        ));
        assert!(!song.opl_drums);
        assert_eq!(song.drum_notes, 0);
    }

    #[test]
    fn test_opl3_four_op_note_lands_on_the_pair_head() {
        let mut ws = vec![ChipWrite::new(90, KIND_OPL3, 0, 0x104, 0x01)]; // bank A pair 0
        ws.extend(voice(
            95,
            KIND_OPL3,
            0,
            Voice {
                four: true,
                block: 4,
                fnum: 0x3D1,
                ops: Some(vec![
                    OpSpec::default(),
                    OpSpec {
                        mult: 5,
                        ..Default::default()
                    },
                    OpSpec {
                        mult: 3,
                        ..Default::default()
                    },
                    OpSpec {
                        mult: 7,
                        ..Default::default()
                    },
                ]),
                alg: 0,
                off_time: 60000,
                ..Default::default()
            },
        ));
        // The pair's second connection bit lives on the partner's C0 register.
        ws.insert(1, ChipWrite::new(96, KIND_OPL3, 0, 0xC3, 0x21));
        ws.extend(heart(KIND_OPL3));
        let song = analyze_default(&_vgm(
            ws,
            VgmOpts {
                kind: KIND_OPL3,
                ..Default::default()
            },
        ));
        let evs: Vec<&NoteEvent> = song.events.iter().filter(|e| e.on && e.ch == 0).collect();
        assert!(!evs.is_empty());
        let p = patch(&song, evs[0].ins);
        assert!(p.four);
        // Bit 1 of the algorithm came from hardware channel 3, and the operator
        // order is the pair's: ch0 op1, ch3 op1, ch0 op2, ch3 op2.
        assert_eq!(p.alg, 2);
        assert_eq!(
            p.ops.iter().map(|o| o.mult).collect::<Vec<_>>(),
            vec![1, 3, 5, 7]
        );
        assert_eq!(p.carriers(), vec![2, 3]);
    }

    #[test]
    fn test_a_pair_switches_between_two_and_four_operators() {
        // One column heads a four-operator voice and then a plain pair, because
        // the log turns register 0x104 off between them.
        let mut ws = vec![ChipWrite::new(90, KIND_OPL3, 0, 0x104, 0x02)];
        ws.extend(voice(
            95,
            KIND_OPL3,
            2,
            Voice {
                four: true,
                block: 4,
                fnum: 0x3D1,
                ops: Some(vec![
                    OpSpec {
                        tl: 9,
                        ..Default::default()
                    },
                    OpSpec {
                        tl: 20,
                        ..Default::default()
                    },
                    OpSpec {
                        tl: 31,
                        ..Default::default()
                    },
                    OpSpec {
                        tl: 42,
                        ..Default::default()
                    },
                ]),
                off_time: 0,
                ..Default::default()
            },
        ));
        ws.extend([
            ChipWrite::new(30000, KIND_OPL3, 0, 0x104, 0x00),
            hi_write(KIND_OPL3, 2, 30100, 4, 0x3D1, 0, false),
        ]);
        ws.extend(voice(
            31000,
            KIND_OPL3,
            2,
            Voice {
                block: 4,
                fnum: 700,
                ops: Some(vec![
                    OpSpec {
                        tl: 12,
                        ..Default::default()
                    },
                    OpSpec {
                        tl: 33,
                        ..Default::default()
                    },
                ]),
                off_time: 60000,
                ..Default::default()
            },
        ));
        ws.extend(heart_ch(KIND_OPL3, 0));
        let song = analyze_default(&_vgm(
            ws,
            VgmOpts {
                kind: KIND_OPL3,
                ..Default::default()
            },
        ));
        let kinds: BTreeSet<bool> = song
            .events
            .iter()
            .filter(|e| e.on && e.ch == 2)
            .map(|e| patch(&song, e.ins).four)
            .collect();
        assert_eq!(kinds, BTreeSet::from([false, true]));
    }

    #[test]
    fn test_dual_opl2_instances_get_their_own_channels() {
        let mut ws = voice(100, KIND_OPL2, 0, Voice::default());
        ws.extend(heart_default());
        ws.extend(voice(
            140,
            KIND_OPL2,
            1,
            Voice {
                cid: 1,
                block: 5,
                fnum: 400,
                ..Default::default()
            },
        ));
        let song = analyze_default(&_vgm(ws, VgmOpts::default()));
        assert_eq!(song.opl_ids, vec![0, 1]);
        let chans: BTreeSet<i32> = song
            .events
            .iter()
            .filter(|e| e.on)
            .map(|e| e.ch)
            .collect();
        assert!(chans.contains(&0));
        assert!(chans.contains(&10)); // chip 1's channel 1 is column 10
    }

    #[test]
    fn test_pan_bits_reach_the_column() {
        let mut ws = heart(KIND_OPL3);
        ws.extend(voice(
            100,
            KIND_OPL3,
            6,
            Voice {
                block: 3,
                fnum: 700,
                pan: 1,
                ..Default::default()
            },
        ));
        let song = analyze_default(&_vgm(
            ws,
            VgmOpts {
                kind: KIND_OPL3,
                ..Default::default()
            },
        ));
        let ev = song.events.iter().find(|e| e.on && e.ch == 6).unwrap();
        assert_eq!(ev.pan, 0xF0); // left only
    }

    #[test]
    fn test_centre_after_a_hard_pan_emits_an_explicit_byte() {
        // A hard-panned channel that returns to centre must get 0xFF, not an
        // omitted effect: otherwise the old pan stays latched for every later
        // row. Both the note byte and a live mid-note change carry it.
        let mut st = OPLState::new(KIND_OPL3, false);
        st.write(0, 0xC0, 0x10); // left only
        assert_eq!(st.pan_byte(0), 0xF0);
        st.write(0, 0xC0, 0x30); // both sides
        assert_eq!(st.pan_byte(0), 0xFF);

        let cm = i32::from(st.chan_map[0]);
        let mut ws = heart(KIND_OPL3);
        ws.extend(voice(
            100,
            KIND_OPL3,
            0,
            Voice {
                pan: 1,
                ..Default::default()
            },
        ));
        ws.push(ChipWrite::new(20000, KIND_OPL3, 0, (cm + 0xC0) as u32, 0x30));
        let song = analyze_default(&_vgm(
            ws,
            VgmOpts {
                kind: KIND_OPL3,
                ..Default::default()
            },
        ));
        let ev = song.events.iter().find(|e| e.on && e.ch == 0).unwrap();
        assert_eq!(ev.pan, 0xF0);
        let live: Vec<i32> = song
            .channel_fx
            .iter()
            .filter(|(t, ch, cmd, _v)| *ch == 0 && *cmd == 0x08 && *t >= 20000)
            .map(|(_t, _ch, _cmd, v)| *v)
            .collect();
        assert_eq!(live, vec![0xFF]);
    }

    #[test]
    fn test_an_opl2_column_gets_no_pan_effect() {
        let mut ws = voice(100, KIND_OPL2, 0, Voice::default());
        ws.extend(heart_default());
        let song = analyze_default(&_vgm(ws, VgmOpts::default()));
        let ev = song.events.iter().find(|e| e.on && e.ch == 0).unwrap();
        assert_eq!(ev.pan, 0);
    }

    #[test]
    fn test_a_rhythm_poke_without_a_latch_keeps_nine_channels() {
        // The bug this guards: a driver that pokes $BD with bit 5 set during chip
        // init and clears it in the same sample never sounds a drum, yet taking
        // that write as "this is a kit track" moved the module to the drums system
        // and left every melody note on channels 7 to 9 with no column at all.
        let mut ws = voice(
            100,
            KIND_OPL2,
            6,
            Voice {
                block: 3,
                fnum: 900,
                ..Default::default()
            },
        );
        ws.extend(heart_default());
        ws.extend(voice(
            20000,
            KIND_OPL2,
            7,
            Voice {
                block: 3,
                fnum: 500,
                ..Default::default()
            },
        ));
        ws.extend(voice(
            40000,
            KIND_OPL2,
            8,
            Voice {
                block: 4,
                fnum: 300,
                ..Default::default()
            },
        ));
        // Mode on and off inside one sample, no trigger bit ever set.
        ws.extend([
            ChipWrite::new(150, KIND_OPL2, 0, 0xBD, 0x20),
            ChipWrite::new(150, KIND_OPL2, 0, 0xBD, 0xC0),
        ]);
        let song = analyze_default(&_vgm(ws, VgmOpts::default()));
        assert!(!song.opl_drums, "a poke became a kit");
        let data = write_plain(&song);
        let m = parse_fur(&data).unwrap();
        assert_eq!(m.chips, vec![CHIP_OPL2]);
        assert_eq!(m.n_ch, 9);
        let chans: BTreeSet<i32> = song
            .events
            .iter()
            .filter(|e| e.on)
            .map(|e| e.ch)
            .collect();
        // The heartbeat keys its own column too; what matters is that the three
        // channels rhythm mode would have taken still carry their melody.
        assert!(
            BTreeSet::from([6, 7, 8]).is_subset(&chans),
            "{:?}",
            chans
        );
        assert!(
            !song.warnings.iter().any(|w| w.contains("kit")),
            "{:?}",
            song.warnings
        );
    }

    #[test]
    fn test_a_latch_beside_the_poke_still_selects_the_kit() {
        // The other half: once a percussion bit is really latched while the mode
        // is held, that is a kit track and its columns exist to be played on.
        let mut ws = voice(
            100,
            KIND_OPL2,
            6,
            Voice {
                drums: true,
                block: 3,
                fnum: 900,
                ..Default::default()
            },
        );
        ws.extend(heart_default());
        ws.extend([
            rhythm(2000),
            rhythm_bits(10000, 0x18),
            rhythm_bits(14000, 0),
        ]);
        let song = analyze_default(&_vgm(ws, VgmOpts::default()));
        assert!(song.opl_drums);
        assert_eq!(
            parse_fur(&write_plain(&song)).unwrap().chips,
            vec![CHIP_OPL2_DRUMS]
        );
    }

    #[test]
    fn test_hand_written_vibrato_keeps_its_return_legs() {
        // A slow-attack pad on channel 8 moves its pitch by alternating one
        // f-num register between two values every ~150 ms (modaozi Dungeon Theme
        // intro). Mid-note pitch used to be compared against the value the note
        // *started* on, so every write returning to that pitch was discarded: half
        // the oscillation survived and the pitch never came back down. Track the
        // register's current value instead, and both legs appear.
        let mut ws = voice(
            100,
            KIND_OPL2,
            7,
            Voice {
                block: 4,
                fnum: 580,
                ..Default::default()
            },
        );
        ws.extend(heart_default());
        for k in 0..6 {
            ws.push(ChipWrite::new(
                20000 + k * 6600,
                KIND_OPL2,
                0,
                0xA7,
                ((580 + if k % 2 != 0 { 96 } else { 0 }) & 0xFF) as u32,
            ));
        }
        ws.push(ChipWrite::new(120000, KIND_OPL2, 0, 0xB7, 0x0F));
        let song = analyze_default(&_vgm(ws, VgmOpts::default()));
        let ev = song.events.iter().find(|e| e.on && e.ch == 7).unwrap();
        let pts = &ev.fm_pitch_pts;
        // six writes, the first already at the note's own pitch: one anchor plus
        // five real transitions.
        assert_eq!(pts.len(), 6, "a leg of the oscillation was dropped");
        // up, back to the start, up again - strictly alternating.
        assert_eq!(pts[0].2, pts[2].2, "the pitch did not return");
        assert_ne!(pts[1].2, pts[0].2);
        assert_eq!(
            pts.iter().map(|p| p.2).collect::<Vec<_>>(),
            vec![580, 676, 580, 676, 580, 676]
        );
        // The time steps must alternate too: five returns/up-moves, not one.
        let times: BTreeSet<i64> = pts.iter().map(|p| p.0).collect();
        assert_eq!(times.len(), pts.len());
    }

    #[test]
    fn test_only_the_moving_channel_records_pitch() {
        // The alternation above must not smear onto the rest of the module: a
        // channel whose frequency registers nobody touched keeps no pitch points.
        let mut ws = voice(
            100,
            KIND_OPL2,
            7,
            Voice {
                block: 4,
                fnum: 580,
                ..Default::default()
            },
        );
        ws.extend(heart_default());
        ws.extend(voice(
            120,
            KIND_OPL2,
            2,
            Voice {
                block: 3,
                fnum: 400,
                ..Default::default()
            },
        ));
        for k in 0..6 {
            ws.push(ChipWrite::new(
                20000 + k * 6600,
                KIND_OPL2,
                0,
                0xA7,
                ((580 + if k % 2 != 0 { 96 } else { 0 }) & 0xFF) as u32,
            ));
        }
        let song = analyze_default(&_vgm(ws, VgmOpts::default()));
        let moving: BTreeMap<i32, usize> = song
            .events
            .iter()
            .filter(|e| e.on)
            .map(|e| (e.ch, e.fm_pitch_pts.len()))
            .collect();
        assert_eq!(moving.get(&7), Some(&6), "{:?}", moving);
        assert!(
            moving.get(&2).copied().unwrap_or(0) == 0,
            "a still channel picked up motion"
        );

        let mut ws = voice(
            100,
            KIND_OPL2,
            3,
            Voice {
                ..Default::default()
            },
        );
        ws.extend(heart_default());
        let src = _vgm(ws, VgmOpts::default());
        assert!(estimate_grid(
            &src,
            0.75,
            TARGET_TICK_SAMPLES,
            MIN_ROW_SAMPLES,
            None,
            "key-on lattice",
            None
        )
        .is_some());
        let keys = _keyons_by_channel(&src, None);
        assert!(
            keys.keys().any(|k| *k >= 4000),
            "{:?}",
            keys.keys().collect::<Vec<_>>()
        );
    }
}

#[allow(non_snake_case)]
mod TestOplWriter {
    use super::*;

    #[test]
    fn test_chip_ids_and_channel_counts() {
        for (kind, ch) in [(KIND_OPL2, 0), (KIND_OPL3, 8)] {
            let mut ws = voice(100, kind, ch, Voice::default());
            ws.extend(heart(kind));
            let data = write_plain(&analyze_default(&_vgm(
                ws,
                VgmOpts {
                    kind,
                    ..Default::default()
                },
            )));
            let m = parse_fur(&data).unwrap();
            let want = if kind == KIND_OPL2 {
                CHIP_OPL2
            } else {
                CHIP_OPL3
            };
            assert_eq!(m.chips, vec![want]);
            assert_eq!(m.n_ch, channel_count(kind, false) as usize);
        }
    }

    #[test]
    fn test_rhythm_module_uses_the_drums_system() {
        let mut ws = voice(
            100,
            KIND_OPL2,
            6,
            Voice {
                drums: true,
                ..Default::default()
            },
        );
        ws.extend(heart_default());
        ws.extend([rhythm(1000), rhythm_bits(10000, 8), rhythm_bits(14000, 0)]);
        let data = write_plain(&analyze_default(&_vgm(ws, VgmOpts::default())));
        let m = parse_fur(&data).unwrap();
        assert_eq!(m.chips, vec![CHIP_OPL2_DRUMS]);
        assert_eq!(m.n_ch, 11);
        assert!(contains_bytes(&data, b"BD"));
        assert!(contains_bytes(&data, b"HiHat"));
        assert!(ins_types(&data).contains(&INS_OPL_DRUMS));
    }

    fn zero_attack_notes(first_rr: u32) -> Vec<FurRow> {
        // The source leaves the previous operator at RR=0 (no release) and
        // writes the next patch's registers during the silent gap. A module
        // loads that patch only at the key-on, where a zero-attack operator
        // would keep the stale level instead of starting from silence. The
        // writer arms Furnace's hard envelope reset (0x30) for that note.
        let mut ws = voice(
            1000,
            KIND_OPL3,
            0,
            Voice {
                ops: Some(vec![
                    OpSpec {
                        ar: 15,
                        dr: 2,
                        sl: 6,
                        rr: first_rr,
                        ..Default::default()
                    },
                    OpSpec {
                        ar: 15,
                        dr: 2,
                        sl: 7,
                        rr: 2,
                        ..Default::default()
                    },
                ]),
                block: 4,
                fnum: 580,
                off_time: 60000,
                ..Default::default()
            },
        );
        ws.extend(voice(
            100000,
            KIND_OPL3,
            0,
            Voice {
                ops: Some(vec![
                    OpSpec {
                        ar: 0,
                        dr: 0,
                        sl: 15,
                        rr: 15,
                        ..Default::default()
                    },
                    OpSpec {
                        ar: 15,
                        dr: 0,
                        sl: 15,
                        rr: 6,
                        ..Default::default()
                    },
                ]),
                block: 4,
                fnum: 700,
                off_time: 160000,
                ..Default::default()
            },
        ));
        ws.extend(heart(KIND_OPL3));
        let song = analyze_default(&_vgm(
            ws,
            VgmOpts {
                kind: KIND_OPL3,
                ..Default::default()
            },
        ));
        let m = parse_fur(&write_plain(&song)).unwrap();
        let mut rows: Vec<FurRow> = Vec::new();
        for &oi in &m.orders[0] {
            if let Some(prows) = m.patterns.get(&(0, oi)) {
                for row in prows {
                    if (0..180).contains(&row.note) {
                        rows.push(row.clone());
                    }
                }
            }
        }
        assert!(rows.len() >= 2);
        rows
    }

    #[test]
    fn test_zero_attack_after_an_unreleased_operator_gets_hard_reset() {
        let rows = zero_attack_notes(0);
        assert!(
            !rows[0].fx.iter().any(|(c, _v)| *c == 0x30),
            "{:?}",
            rows[0].fx
        );
        assert!(
            rows[1].fx.iter().any(|&(c, v)| c == 0x30 && v == 1),
            "{:?}",
            rows[1].fx
        );

        // A released modulator already reaches silence, so the reset is not
        // needed and the note keys normally.
        let rows = zero_attack_notes(4);
        assert!(
            !rows[1].fx.iter().any(|(c, _v)| *c == 0x30),
            "{:?}",
            rows[1].fx
        );
    }

    #[test]
    fn test_instrument_type_and_operator_bytes() {
        let mut ws = voice(
            100,
            KIND_OPL2,
            0,
            Voice {
                alg: 0,
                fb: 5,
                ops: Some(vec![
                    OpSpec {
                        am: 1,
                        vib: 1,
                        ksr: 1,
                        mult: 7,
                        tl: 8,
                        ar: 15,
                        dr: 6,
                        sl: 3,
                        rr: 5,
                        ws: 1,
                        ..Default::default()
                    },
                    OpSpec {
                        sus: 1,
                        mult: 4,
                        tl: 20,
                        ksl: 2,
                        ar: 14,
                        dr: 9,
                        sl: 12,
                        rr: 3,
                        ws: 2,
                        ..Default::default()
                    },
                ]),
                ..Default::default()
            },
        );
        ws.extend(heart_default());
        let song = analyze_default(&_vgm(ws, VgmOpts::default()));
        let ev = song.events.iter().find(|e| e.on && e.ch == 0).unwrap();
        let data = write_plain(&song);
        assert_eq!(ins_types(&data)[ev.ins as usize], INS_OPL);
        let feat = fm_feature(&data, ev.ins as usize);
        assert_eq!(feat[0], 4 | 0x10 | 0x20); // four bytes, two enabled
        assert_eq!(feat[1], (0 << 4) | 5); // algorithm and feedback
        assert_eq!(feat[3] & 32, 0); // not a four-op voice
        // Furnace stores an OPL operator as eight bytes, one per register plus
        // the two it derives: key-scale rate rides the multiplier byte, sustain
        // the level, vibrato the attack, amplitude modulation and key-scale level
        // the decay, then volume/depth, sustain level with release, a reserved
        // byte and the waveform.
        let md = &feat[4..12];
        let car = &feat[12..20];
        assert_eq!(md[0], 0x80 | 7); // ksr and multiplier
        assert_eq!(md[1] & 63, 8); // its logged level
        assert_eq!(md[1] >> 7, 0); // sustain clear
        assert_eq!(md[2], (1 << 5) | 15); // vibrato and attack rate
        assert_eq!(md[3], (1 << 7) | 6); // am and decay rate
        assert_eq!(md[5], (3 << 4) | 5); // sustain level, release
        assert_eq!(md[6], 0); // no SSG envelope on OPL
        assert_eq!(md[7], 1); // waveform select
        assert_eq!(car[0] & 15, 4); // multiplier only
        assert!(car[1] >> 7 != 0); // sustain set
        assert_eq!(car[3], (2 << 5) | 9); // key-scale level, decay
        assert_eq!(car[4] >> 5, 2); // the carrier follows volume
        assert_eq!(car[7], 2);
    }

    #[test]
    fn test_a_kit_instrument_carries_four_operators() {
        let mut ws = voice(
            100,
            KIND_OPL2,
            6,
            Voice {
                drums: true,
                ..Default::default()
            },
        );
        ws.extend(heart_default());
        let addrs: Vec<u16> = (7..11)
            .map(|f| SLOT_ADDR[SLOTS_OPL2_DRUMS[0][f] as usize])
            .collect();
        ws.extend(op_writes(
            95,
            KIND_OPL2,
            i32::from(addrs[0]),
            OpSpec {
                mult: 6,
                tl: 11,
                ..Default::default()
            },
        )); // snare
        ws.extend(op_writes(
            95,
            KIND_OPL2,
            i32::from(addrs[3]),
            OpSpec {
                mult: 9,
                tl: 17,
                ..Default::default()
            },
        )); // hi-hat
        ws.extend([rhythm(2000), rhythm_bits(10000, 8), rhythm_bits(14000, 0)]);
        let data = write_plain(&analyze_default(&_vgm(ws, VgmOpts::default())));
        assert!(ins_types(&data).contains(&INS_OPL_DRUMS));
        let feat = fm_feature(
            &data,
            ins_types(&data)
                .iter()
                .position(|&t| t == INS_OPL_DRUMS)
                .unwrap(),
        );
        assert_eq!(feat[0], 4 | 0xF0); // all four read
        assert_eq!(feat[3] & 32, 0); // not a four-op voice
        assert_eq!(feat[4..12][0] & 15, 6); // snare multiplier
        assert_eq!(feat[4 + 24..4 + 32][1] & 63, 17); // hi-hat level
    }

    #[test]
    fn test_four_op_instrument_sets_the_ops_flag() {
        let mut ws = vec![ChipWrite::new(90, KIND_OPL3, 0, 0x104, 0x08)]; // bank B pair 0
        ws.extend(voice(
            95,
            KIND_OPL3,
            6,
            Voice {
                four: true,
                block: 4,
                fnum: 0x3D1,
                ..Default::default()
            },
        ));
        ws.extend(heart(KIND_OPL3));
        let song = analyze_default(&_vgm(
            ws,
            VgmOpts {
                kind: KIND_OPL3,
                ..Default::default()
            },
        ));
        let data = write_plain(&song);
        let want: Vec<usize> = song
            .fm_patches
            .iter()
            .enumerate()
            .filter(|(_i, p)| p.four)
            .map(|(i, _p)| i)
            .collect();
        assert!(!want.is_empty());
        let feat = fm_feature(&data, want[0]);
        assert_eq!(feat[3] & 32, 32); // the ops==4 flag
        assert_eq!(feat[0], 4 | 0xF0); // four operators enabled
        assert_eq!(parse_fur(&data).unwrap().chips, vec![CHIP_OPL3]);
    }

    #[test]
    fn test_clock_flag_only_when_it_differs() {
        let mut ws = voice(100, KIND_OPL2, 0, Voice::default());
        ws.extend(heart_default());
        let plain = write_plain(&analyze_default(&_vgm(ws, VgmOpts::default())));
        assert!(!contains_bytes(&plain, b"customClock"));

        let mut ws = voice(100, KIND_OPL2, 0, Voice::default());
        ws.extend(heart_default());
        let odd = write_plain(&analyze_default(&_vgm(
            ws,
            VgmOpts {
                opl2_clock: Some(3679545),
                ..Default::default()
            },
        )));
        assert!(contains_bytes(&odd, b"customClock=3679545"));

        let mut ws = voice(100, KIND_OPL3, 8, Voice::default());
        ws.extend(heart(KIND_OPL3));
        let o3 = write_plain(&analyze_default(&_vgm(
            ws,
            VgmOpts {
                kind: KIND_OPL3,
                ..Default::default()
            },
        )));
        assert!(contains_bytes(&o3, b"customClock=14318180"));

        let mut ws = voice(100, KIND_OPL3, 8, Voice::default());
        ws.extend(heart(KIND_OPL3));
        let exact = write_plain(&analyze_default(&_vgm(
            ws,
            VgmOpts {
                opl3_clock: Some(py_int(opl_module_clock(14318182.0, true)) as u32),
                kind: KIND_OPL3,
                ..Default::default()
            },
        )));
        assert!(!contains_bytes(&exact, b"customClock"));

        let mut ws = voice(100, KIND_OPL3, 8, Voice::default());
        ws.extend(heart(KIND_OPL3));
        let o3b = write_plain(&analyze_default(&_vgm(
            ws,
            VgmOpts {
                opl3_clock: Some(14400000),
                kind: KIND_OPL3,
                ..Default::default()
            },
        )));
        assert!(contains_bytes(&o3b, b"customClock=14400000"));
    }

    #[test]
    fn test_dual_chips_get_two_systems_and_hard_pan_triples() {
        let mut ws = voice(100, KIND_OPL2, 0, Voice::default());
        ws.extend(heart_default());
        ws.extend(voice(
            140,
            KIND_OPL2,
            1,
            Voice {
                cid: 1,
                block: 5,
                fnum: 400,
                ..Default::default()
            },
        ));
        let src = _vgm(
            ws,
            VgmOpts {
                opl2_dual: true,
                opl2_pan: true,
                ..Default::default()
            },
        );
        let data = write_plain(&analyze_default(&src));
        let m = parse_fur(&data).unwrap();
        assert_eq!(m.chips, vec![CHIP_OPL2, CHIP_OPL2]);
        assert_eq!(m.n_ch, 18);
        assert!(contains_bytes(&data, b"FM 2 (2)"));
    }
}

#[allow(non_snake_case)]
mod TestOplFurnaceRoundTrip {
    use super::*;

    fn export(song: &Song, name: &str) -> VgmFile {
        let fur = tmp().join(format!("{}.fur", name));
        let out = tmp().join(format!("{}.vgm", name));
        std::fs::write(&fur, write_plain(song)).unwrap();
        vgm2fur::furnace::export_vgm(&fur, &out).expect("export");
        load_vgm(&out).expect("load export")
    }

    /// Values seen at each register address, bank B keeping bit 8.
    fn regs(vgm: &VgmFile, kind: &str) -> BTreeMap<u32, BTreeSet<u32>> {
        let mut regs: BTreeMap<u32, BTreeSet<u32>> = BTreeMap::new();
        for w in &vgm.writes {
            if w.chip == kind {
                regs.entry(w.reg).or_default().insert(w.val);
            }
        }
        regs
    }

    #[test]
    fn test_note_lands_on_the_logged_phase_step() {
        skip_without_furnace!();
        // Registers the table reaches exactly: one f-num step at these blocks is
        // well under a cent, so the chip must get the rip's own phase step.
        for (block, fnum) in [(2, 690), (4, 580), (1, 313), (0, 290)] {
            let mut ws = voice(
                100,
                KIND_OPL2,
                3,
                Voice {
                    block,
                    fnum,
                    ..Default::default()
                },
            );
            ws.extend(heart_default());
            let song = analyze_default(&_vgm(ws, VgmOpts::default()));
            let vgm = export(&song, &format!("opl2_{}_{}", block, fnum));
            let units = keyon_units(&vgm, KIND_OPL2, false);
            assert!(
                units.contains(&(fnum << block)),
                "{} {} {:?}",
                block,
                fnum,
                &units[..units.len().min(6)]
            );
        }
    }

    #[test]
    fn test_a_high_register_lands_within_one_f_num_step() {
        skip_without_furnace!();
        // Block 6 leaves the f-num just eleven steps between a cell and the next,
        // so the closest note sits two cents away. The pitch is what matters here.
        let mut ws = voice(
            100,
            KIND_OPL2,
            3,
            Voice {
                block: 6,
                fnum: 977,
                ..Default::default()
            },
        );
        ws.extend(heart_default());
        let song = analyze_default(&_vgm(ws, VgmOpts::default()));
        let vgm = export(&song, "opl2_high");
        let units: Vec<i32> = keyon_events(&vgm, KIND_OPL2, false)
            .into_iter()
            .filter(|(cid, fch, _t, _u)| *cid == 0 && *fch == 3)
            .map(|(_cid, _fch, _t, u)| u)
            .collect();
        // The module loops. The exporter writes the first note once at the
        // top and once again when the loop comes round. Both must agree.
        assert!(!units.is_empty());
        let distinct: BTreeSet<i32> = units.iter().copied().collect();
        assert_eq!(distinct.len(), 1, "{:?}", units);
        let cents = (1200.0 * (units[0] as f64 / f64::from(977 << 6)).log2()).abs();
        assert!(cents < 4.0, "{:?}", units);
    }

    #[test]
    fn test_operator_bytes_reach_the_chip() {
        skip_without_furnace!();
        let mut ws = voice(
            100,
            KIND_OPL2,
            0,
            Voice {
                fb: 5,
                ops: Some(vec![
                    OpSpec {
                        am: 1,
                        vib: 1,
                        sus: 1,
                        ksr: 1,
                        mult: 7,
                        tl: 8,
                        ar: 15,
                        dr: 6,
                        sl: 3,
                        rr: 5,
                        ws: 2,
                        ..Default::default()
                    },
                    OpSpec {
                        mult: 4,
                        tl: 20,
                        ksl: 1,
                        ..Default::default()
                    },
                ]),
                ..Default::default()
            },
        );
        ws.extend(heart_default());
        let vgm = export(&analyze_default(&_vgm(ws, VgmOpts::default())), "opl2_operators");
        let regs = regs(&vgm, KIND_OPL2);
        // Modulator 0x2n: am, vib, sustain, key-scale rate and multiplier.
        assert!(regs[&0x20].contains(&(0x80 | 0x40 | 0x20 | 0x10 | 7)));
        assert!(
            regs[&0x40].iter().any(|v| v & 63 == 8),
            "{:?}",
            regs[&0x40]
        );
        assert!(regs[&0x60].contains(&((15 << 4) | 6)));
        assert!(regs[&0x80].contains(&((3 << 4) | 5)));
        assert!(regs[&0xE0].contains(&2)); // waveform select
        // Carrier of the same voice: its own level and key-scale level.
        assert!(regs[&0x43].iter().any(|v| v & 63 == 20));
        assert!(regs[&0x43].iter().any(|v| v & 0xC0 == 1 << 6));
        assert!(regs[&0xC0].contains(&((5 << 1) | 0x30))); // feedback and both outputs
    }

    #[test]
    fn test_a_carrier_level_rides_the_volume_column() {
        skip_without_furnace!();
        // The instrument keeps one level and the note carries the difference, so
        // two attenuations of the same patch share an instrument and not two.
        let mut ws = voice(
            100,
            KIND_OPL2,
            4,
            Voice {
                ops: Some(vec![
                    OpSpec::default(),
                    OpSpec {
                        tl: 6,
                        ..Default::default()
                    },
                ]),
                off_time: 30000,
                ..Default::default()
            },
        );
        ws.extend(voice(
            40000,
            KIND_OPL2,
            4,
            Voice {
                ops: Some(vec![
                    OpSpec::default(),
                    OpSpec {
                        tl: 30,
                        ..Default::default()
                    },
                ]),
                off_time: 90000,
                ..Default::default()
            },
        ));
        ws.extend(heart_default());
        let song = analyze_default(&_vgm(ws, VgmOpts::default()));
        let evs: Vec<&NoteEvent> = song.events.iter().filter(|e| e.on && e.ch == 4).collect();
        assert_eq!(evs.len(), 2);
        assert_eq!(
            evs.iter().map(|e| e.ins).collect::<Vec<_>>(),
            vec![evs[0].ins; 2],
            "the level split the instrument"
        );
        let vols: BTreeSet<i32> = evs.iter().map(|e| e.vol).collect();
        assert_eq!(vols, BTreeSet::from([63 - 6, 63 - 30]));
    }

    #[test]
    fn test_four_op_pair_drives_both_hardware_channels() {
        skip_without_furnace!();
        let mut ws = vec![ChipWrite::new(90, KIND_OPL3, 0, 0x104, 0x01)];
        // The partner's connection bit is loaded first: a key-on takes the patch
        // it finds, exactly as the chip does.
        ws.push(ChipWrite::new(92, KIND_OPL3, 0, 0xC3, 0x01 | 0x20));
        ws.extend(voice(
            95,
            KIND_OPL3,
            0,
            Voice {
                four: true,
                block: 4,
                fnum: 977,
                ops: Some(vec![
                    OpSpec::default(),
                    OpSpec {
                        mult: 5,
                        ..Default::default()
                    },
                    OpSpec {
                        mult: 3,
                        ..Default::default()
                    },
                    OpSpec {
                        mult: 7,
                        ..Default::default()
                    },
                ]),
                alg: 0,
                pan: 2,
                ..Default::default()
            },
        ));
        ws.extend(heart(KIND_OPL3));
        let vgm = export(
            &analyze_default(&_vgm(
                ws,
                VgmOpts {
                    kind: KIND_OPL3,
                    ..Default::default()
                },
            )),
            "opl3_4op",
        );
        let regs = regs(&vgm, KIND_OPL3);
        // Four operators: two from hardware channel 0 and two from channel 3.
        assert!(regs[&0x20].iter().any(|v| v & 15 == 1));
        assert!(regs[&0x23].iter().any(|v| v & 15 == 5));
        assert!(regs[&0x28].iter().any(|v| v & 15 == 3));
        assert!(regs[&0x2B].iter().any(|v| v & 15 == 7));
        assert!(
            regs.get(&0x104)
                .is_some_and(|s| s.iter().any(|v| v & 0x01 != 0)),
            "the pair bit never reached bank B"
        );
        assert!(
            vgm.writes
                .iter()
                .any(|w| (w.val & 0x20) != 0 && w.reg == 0xB0),
            "the voice never keyed"
        );
        let units: Vec<i32> = keyon_events(&vgm, KIND_OPL3, false)
            .into_iter()
            .filter(|(cid, fch, _t, _u)| *cid == 0 && *fch == 0)
            .map(|(_cid, _fch, _t, u)| u)
            .collect();
        // The module loops. The exporter writes the first note once at the
        // top and once again when the loop comes round. Both must agree.
        assert!(!units.is_empty());
        let distinct: BTreeSet<i32> = units.iter().copied().collect();
        assert_eq!(distinct.len(), 1, "{:?}", units);
        // One f-num step at block 4 is a cent, and the table cannot promise more.
        assert!(
            (1200.0 * (units[0] as f64 / f64::from(977 << 4)).log2()).abs() < 4.0,
            "{:?}",
            units
        );
    }

    #[test]
    fn test_kit_bytes_land_on_the_rhythm_slots() {
        skip_without_furnace!();
        let mut ws = voice(
            100,
            KIND_OPL2,
            6,
            Voice {
                drums: true,
                block: 3,
                fnum: 900,
                ..Default::default()
            },
        );
        ws.extend(heart_default());
        let addrs: Vec<u16> = (7..11)
            .map(|f| SLOT_ADDR[SLOTS_OPL2_DRUMS[0][f] as usize])
            .collect();
        ws.extend(op_writes(
            95,
            KIND_OPL2,
            i32::from(addrs[0]),
            OpSpec {
                mult: 6,
                tl: 11,
                ..Default::default()
            },
        )); // snare
        ws.extend(op_writes(
            95,
            KIND_OPL2,
            i32::from(addrs[2]),
            OpSpec {
                mult: 9,
                tl: 17,
                ..Default::default()
            },
        )); // top cymbal
        ws.extend([
            rhythm(2000),
            rhythm_bits(12000, 0x08 | 0x02),
            rhythm_bits(16000, 0),
        ]);
        let vgm = export(&analyze_default(&_vgm(ws, VgmOpts::default())), "opl2_kit");
        let regs = regs(&vgm, KIND_OPL2);
        assert!(regs[&0x34].iter().any(|v| v & 15 == 6)); // snare multiplier
        assert!(regs[&0x54].iter().any(|v| v & 63 == 11)); // snare level
        assert!(regs[&0x35].iter().any(|v| v & 15 == 9)); // cymbal multiplier
        assert!(regs[&0x55].iter().any(|v| v & 63 == 17)); // cymbal level
        assert!(
            regs.get(&0xBD)
                .is_some_and(|s| s.iter().any(|v| v & 0x20 != 0)),
            "rhythm mode never came back on"
        );
        assert!(
            regs.get(&0xBD)
                .is_some_and(|s| s.iter().any(|v| v & 0x08 != 0)),
            "the snare was never latched"
        );
    }

    #[test]
    fn test_second_chip_is_written_on_its_own_command() {
        skip_without_furnace!();
        let mut ws = voice(100, KIND_OPL2, 0, Voice::default());
        ws.extend(heart_default());
        ws.extend(voice(
            140,
            KIND_OPL2,
            1,
            Voice {
                cid: 1,
                block: 5,
                fnum: 400,
                ..Default::default()
            },
        ));
        let vgm = export(&analyze_default(&_vgm(ws, VgmOpts::default())), "opl2_dual");
        let kinds: BTreeSet<&str> = vgm.writes.iter().map(|w| w.chip).collect();
        assert_eq!(kinds, BTreeSet::from(["opl2"]));
        // Both instances are keyed, each on its own chip.
        let by_id: BTreeSet<(u8, i32)> = keyon_events(&vgm, KIND_OPL2, false)
            .into_iter()
            .map(|(cid, _fch, _t, unit)| (cid, unit))
            .collect();
        assert!(by_id.contains(&(0, 580 << 4)));
        assert!(by_id.contains(&(1, 400 << 5)));
    }
}

fn preset_walk() -> VgmFile {
    let mut ws = opll_note(
        1000,
        OpllNote {
            ch: 3,
            preset: 13,
            level: 0,
            off: Some(8000),
            ..Default::default()
        },
    );
    ws.extend([
        opll_wr(1800, 0x33, (5 << 4) | 3),
        opll_wr(2600, 0x33, (4 << 4) | 6),
        opll_wr(3400, 0x33, (4 << 4) | 9),
    ]);
    opll_vgm(ws, 14000)
}

fn bend() -> VgmFile {
    let mut ws = opll_note(
        1000,
        OpllNote {
            ch: 4,
            preset: 1,
            level: 0,
            off: Some(12000),
            ..Default::default()
        },
    );
    for (i, fnum) in [640, 700, 760, 820].into_iter().enumerate() {
        let t = 1800 + i as i64 * 500;
        ws.extend([
            opll_wr(t, 0x14, (fnum & 0xFF) as u32),
            opll_wr(
                t + 2,
                0x24,
                (((4 & 7) << 1) | ((fnum >> 8) & 1) | 0x10) as u32,
            ),
        ]);
    }
    opll_vgm(ws, 14000)
}

fn retriggers() -> VgmFile {
    let mut ws: Vec<ChipWrite> = Vec::new();
    for base in [1000, 8000] {
        ws.extend(opll_note(
            base,
            OpllNote {
                ch: 3,
                preset: 3,
                level: 15,
                off: Some(base + 4000),
                ..Default::default()
            },
        ));
        ws.push(opll_wr(base + 1470, 0x33, (3 << 4) | 0));
    }
    opll_vgm(ws, 14000)
}

fn clap() -> VgmFile {
    let mut ws = opll_note(
        1000,
        OpllNote {
            ch: 2,
            preset: 3,
            level: 0,
            off: Some(2080),
            ..Default::default()
        },
    );
    let levels = [0, 15, 0, 15, 0, 8, 4, 15];
    let times = [2480, 2880, 3280, 3680, 4080, 4480, 4880, 5410];
    for (&t, &level) in times.iter().zip(levels.iter()) {
        ws.push(opll_wr(t, 0x32, (3 << 4) | level));
    }
    for (i, fnum) in [620, 660, 700, 740, 780, 820, 860, 900, 940, 980]
        .into_iter()
        .enumerate()
    {
        let t = 2500 + i as i64 * 400;
        ws.extend([
            opll_wr(t, 0x12, (fnum & 0xFF) as u32),
            opll_wr(t + 2, 0x22, (((4 & 7) << 1) | ((fnum >> 8) & 1)) as u32),
        ]);
    }
    opll_vgm(ws, 14000)
}

#[allow(non_snake_case)]
mod TestOpllTickMacros {
    use super::*;

    #[test]
    fn test_fast_preset_and_volume_changes_become_macros() {
        let song = analyze_speed(&preset_walk(), 8);
        let ev = song.events.iter().find(|e| e.on && e.ch == 3).unwrap();
        let p = patch(&song, ev.ins);
        // The first key keeps its own $3x page (preset 13), while the fast
        // walk afterwards becomes a wave macro instead of one row value.
        assert_eq!(p.preset, 13);
        assert!(p.wave_macro.contains(&5));
        assert!(p.wave_macro.contains(&4));
        assert!(p.vol_macro.len() > 1);
        assert_eq!(ev.vol, 15);
        assert!(song.fm_patches.len() < 250);
        assert_eq!(song.warnings, Vec::<String>::new());
    }

    #[test]
    fn test_fast_drum_bends_stay_pitch_macros() {
        // A sub-row f-num walk cannot ride row effects (all its writes land on
        // one row, and OPLL row steps do not reach the registers on export),
        // so it must remain a pitch macro.
        let song = analyze_speed(&bend(), 8);
        let ev = song.events.iter().find(|e| e.on && e.ch == 4).unwrap();
        let p = patch(&song, ev.ins);
        assert!(p.pitch_macro.len() > 1);
        assert!(ev.pitch_steps.is_empty());
        assert!(ev.sweep.is_empty());
    }

    #[test]
    fn test_next_note_f_num_prep_does_not_retune_the_previous_note() {
        // The driver writes the next note's low f-num (often 1, a placeholder)
        // shortly before its key-on. The previous note's key-off is earlier,
        // but the collector still holds the voice, so that write lands in the
        // previous note's pitch points. Retuning to it dropped an ordinary
        // note two or more octaves into a bass note.
        let hi1 = ((1 & 7) << 1) | ((216 >> 8) & 1);
        let hi2 = ((1 & 7) << 1) | ((257 >> 8) & 1);
        let ws = vec![
            opll_wr(900, 0x35, (1 << 4) | 0),
            opll_wr(1000, 0x25, (hi1 | 0x10) as u32),
            opll_wr(1001, 0x15, 216 & 0xFF),
            opll_wr(1002, 0x25, (hi1 | 0x10) as u32),
            opll_wr(5000, 0x25, hi1 as u32),
            // next note's preparation: low f-num 1, then key-on 50 samples on
            opll_wr(5400, 0x15, 1),
            opll_wr(5402, 0x25, ((1 & 7) << 1) as u32),
            opll_wr(5480, 0x35, (1 << 4) | 0),
            opll_wr(5500, 0x25, (hi2 | 0x10) as u32),
            opll_wr(5501, 0x15, 257 & 0xFF),
            opll_wr(5502, 0x25, (hi2 | 0x10) as u32),
            opll_wr(9000, 0x25, hi2 as u32),
        ];
        let song = analyze_speed(&opll_vgm(ws, 12000), 1);
        let notes: Vec<&NoteEvent> = song
            .events
            .iter()
            .filter(|e| e.on && e.ch == 5)
            .collect();
        assert_eq!(notes[0].note, 76);
        assert_eq!(notes[1].note, 79);
        assert!(notes[0].pitch_steps.is_empty());
        assert!(notes[0].sweep.is_empty());
        assert!(patch(&song, notes[0].ins).pitch_macro.is_empty());
    }

    #[test]
    fn test_slow_row_pitch_motion_uses_row_steps() {
        // Writes spread over separate rows ride E5/E8/E9 exactly. Keeping them
        // as instrument copies is what exhausted the 250-instrument budget on
        // OPLL-heavy logs and starved the AY/PSG families.
        let mut ws = opll_note(
            1000,
            OpllNote {
                ch: 4,
                preset: 1,
                level: 0,
                off: Some(40000),
                ..Default::default()
            },
        );
        for (i, fnum) in [640, 700, 760, 820].into_iter().enumerate() {
            let t = 6000 + i as i64 * 6000;
            ws.extend([
                opll_wr(t, 0x14, (fnum & 0xFF) as u32),
                opll_wr(
                    t + 2,
                    0x24,
                    (((4 & 7) << 1) | ((fnum >> 8) & 1) | 0x10) as u32,
                ),
            ]);
        }
        let song = analyze_speed(&opll_vgm(ws, 50000), 1);
        let ev = song.events.iter().find(|e| e.on && e.ch == 4).unwrap();
        assert!(!ev.pitch_steps.is_empty());
        assert!(patch(&song, ev.ins).pitch_macro.is_empty());
    }

    #[test]
    fn test_opening_volume_and_retriggers_keep_the_loud_level() {
        // Both keys open at $3x level 15 (silent in the column), then a loud
        // $3x write lands two ticks later. The macro must carry it, and the
        // second key must not have its opening write eaten by the released
        // first note still in `held`.
        let song = analyze_speed(&retriggers(), 8);
        let ons: Vec<&NoteEvent> = song.events.iter().filter(|e| e.on && e.ch == 3).collect();
        assert_eq!(ons.len(), 2);
        for ev in ons {
            let p = patch(&song, ev.ins);
            assert_eq!(ev.vol, 15);
            assert_eq!(p.vol_speed, 1);
            assert_eq!(&p.vol_macro[..3], &[0, 0, 15]);
        }
    }

    #[test]
    fn test_clap_keeps_level_automation_after_key_off() {
        // The key bit clears at +1080, but the driver keeps gating the tail
        // with $3x. The note must still release there while its volume macro
        // runs to the last write, and the f-num walk must reach as far.
        let song = analyze_speed(&clap(), 1);
        let ev = song.events.iter().find(|e| e.on && e.ch == 2).unwrap();
        let p = patch(&song, ev.ins);
        assert_eq!(ev.end - ev.sample, 1080);
        assert!(p.vol_macro.len() >= 5);
        assert_eq!(*p.vol_macro.last().unwrap(), 0);
        assert!(p.pitch_macro.len() >= p.vol_macro.len());
        let off = song
            .events
            .iter()
            .find(|e| !e.on && e.ch == 2 && e.sample - ev.sample == 1080)
            .unwrap();
        assert_eq!(off.note, 180);
    }

    #[test]
    fn test_export_keeps_the_sub_row_preset_walk() {
        skip_without_furnace!();
        let song = analyze_speed(&preset_walk(), 8);
        let start = 1000;
        let stem = tmp().join("opll_tick_macros");
        let fur = stem.with_extension("fur");
        std::fs::write(&fur, write_plain(&song)).unwrap();
        let out = stem.with_extension("vgm");
        let path = export_fur(&fur, Some(&out)).unwrap();
        let back = load_vgm(&path).unwrap();
        let limit = start + song.samples_per_row() as i64;
        let writes: Vec<(i64, u32)> = back
            .writes
            .iter()
            .filter(|w| {
                w.chip == "opll"
                    && (w.reg & 0xFF) == 0x33
                    && start <= w.sample
                    && w.sample < limit
            })
            .map(|w| (w.sample - start, (w.val >> 4) & 15))
            .collect();
        assert!(writes.len() >= 3);
        let presets: Vec<u32> = writes.iter().map(|(_t, p)| *p).collect();
        assert!(presets.contains(&5));
        assert!(presets.contains(&4));
    }

    #[test]
    fn test_export_keeps_level_and_pitch_through_the_release() {
        skip_without_furnace!();
        let song = analyze_speed(&clap(), 1);
        let start = 1000;
        let off = 2080;
        let stem = tmp().join("opll_clap_release");
        let fur = stem.with_extension("fur");
        std::fs::write(&fur, write_plain(&song)).unwrap();
        let out = stem.with_extension("vgm");
        let path = export_fur(&fur, Some(&out)).unwrap();
        let back = load_vgm(&path).unwrap();
        let after: Vec<u32> = back
            .writes
            .iter()
            .filter(|w| {
                w.chip == "opll"
                    && (w.reg & 0xFF) == 0x32
                    && off < w.sample
                    && w.sample <= start + 20000
            })
            .map(|w| w.val & 15)
            .collect();
        // The tail's level steps still reach the chip after the key-off.
        assert!(after.len() >= 3);
        assert!(after.contains(&0));
        assert!(after.contains(&15));
        let pitch_after: Vec<i64> = back
            .writes
            .iter()
            .filter(|w| {
                w.chip == "opll"
                    && matches!(w.reg & 0xFF, 0x12 | 0x22)
                    && off < w.sample
                    && w.sample <= start + 20000
            })
            .map(|w| w.sample - start)
            .collect();
        assert!(pitch_after.len() >= 5);
        assert!(*pitch_after.iter().max().unwrap() >= 3000);
    }
}

type AttackRow = (i32, i32, i64, indexmap::IndexMap<(i32, i32), u32>);

fn check_round_trip(src: &VgmFile, kind: &str, min_attacks: usize, name: &str) {
    // At every attack the chip is handed the same registers it was before.
    //
    // The comparison is sample-atomic (`diag.opl_attacks`): a driver that
    // re-attacks quickly writes the new voice's operator bytes and its key bit
    // in one sample, interleaved with the release before it, and a tracker row
    // holds one value - so what matters is the register set once that sample is
    // finished, which is also what the analyzer snapshots. Kit columns are
    // compared for their operators but not their frequency: two percussion
    // voices share one register there, so the byte belongs to whichever was
    // written last. A note the analyzer bent is compared at its attack only if
    // it still holds the source's f-num, because a macro, row step or slide
    // moves it on purpose afterwards.
    let song = analyze_default(src);
    assert_eq!(song.fm_chip, kind);
    let drums = song.opl_drums;
    let slug: String = name
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { '_' })
        .take(40)
        .collect();
    let stem = tmp().join(format!("round_{}_{}", kind, slug));
    let fur = stem.with_extension("fur");
    std::fs::write(&fur, write_plain(&song)).unwrap();
    let out = stem.with_extension("vgm");
    let path = export_fur(&fur, Some(&out)).unwrap();
    let exp = load_vgm(&path).unwrap();

    let mut want: BTreeMap<(i32, i32), Vec<AttackRow>> = BTreeMap::new();
    let mut got: BTreeMap<(i32, i32), Vec<AttackRow>> = BTreeMap::new();
    for row in opl_attacks(src, Some(kind), Some(drums)) {
        want.entry((row.0, row.1)).or_default().push(row);
    }
    for row in opl_attacks(&exp, Some(kind), Some(drums)) {
        got.entry((row.0, row.1)).or_default().push(row);
    }
    assert_eq!(
        want.keys().collect::<Vec<_>>(),
        got.keys().collect::<Vec<_>>(),
        "{}: channels",
        name
    );

    let chunk = channel_count(kind, drums);
    let st = OPLState::new(kind, drums);
    let mut mine: BTreeMap<(i32, i32), Vec<NoteEvent>> = BTreeMap::new();
    for e in &song.events {
        if !e.on {
            continue;
        }
        let (cid, col) = if song.opl_ids.len() > 1 {
            (e.ch / chunk, e.ch % chunk)
        } else {
            (song.opl_ids.first().copied().unwrap_or(0), e.ch)
        };
        mine.entry((cid, col)).or_default().push(e.clone());
    }

    let total: usize = want.values().map(|v| v.len()).sum();
    let mut bytes_cmp: usize = 0;
    let mut bad: Vec<String> = Vec::new();
    // A module loops back to its first row, so an exported attack after a
    // channel's last source attack has nothing to compare with. Drop that
    // seam replay; everything inside the song still matches.
    for (key, brows) in got.iter_mut() {
        let rows = want.get(key).cloned().unwrap_or_default();
        let edge = if let Some(last) = rows.last() {
            last.2 + 600
        } else {
            src.total_samples + 600
        };
        let kept: Vec<AttackRow> = brows.iter().filter(|r| r.2 <= edge).cloned().collect();
        if kept.len() != brows.len() {
            assert_eq!(
                kept,
                brows[..kept.len()].to_vec(),
                "{} ch{:?}: attack order at the seam",
                name,
                key
            );
            *brows = kept;
        }
    }
    for (ch, rows) in &want {
        let brows = got.get(ch).cloned().unwrap_or_default();
        assert_eq!(
            brows.len(),
            rows.len(),
            "{} ch{:?}: attack count",
            name,
            ch
        );
        let evs = mine.get(ch).cloned().unwrap_or_default();
        let (_cid, col) = *ch;
        let cm = i32::from(st.chan_map[col as usize]);
        let port = if st.opl3 {
            ((cm >> 8) & 1) as i32
        } else {
            0
        };
        let off = cm & 0xFF;
        let kit = is_drum_channel(kind, drums, col);
        for (i, (_cid2, _col2, t, simg)) in rows.iter().enumerate() {
            let (_c3, _c4, _t2, eimg) = &brows[i];
            let ev = evs.get(i);
            let bent = ev.is_some_and(|e| {
                !e.fm_pitch_pts.is_empty() || !e.pitch_steps.is_empty() || !e.sweep.is_empty()
            });
            if !kit && !bent {
                let hi = simg.get(&(port, 0xB0 + off)).copied().unwrap_or(0);
                let want_u = ((((hi & 3) << 8)
                    | simg.get(&(port, 0xA0 + off)).copied().unwrap_or(0))
                    << ((hi >> 2) & 7)) as i32;
                let ge = eimg.get(&(port, 0xB0 + off)).copied().unwrap_or(0);
                let got_u = ((((ge & 3) << 8)
                    | eimg.get(&(port, 0xA0 + off)).copied().unwrap_or(0))
                    << ((ge >> 2) & 7)) as i32;
                if want_u != got_u {
                    bad.push(format!(
                        "ch{:?} t={:.3} unit want {} got {}",
                        ch,
                        *t as f64 / VGM_RATE as f64,
                        want_u,
                        got_u
                    ));
                }
            }
            for addr in opl_voice_addrs(kind, drums, col, simg) {
                let a_port = if st.opl3 {
                    i32::from((addr >> 8) & 1)
                } else {
                    0
                };
                let a_off = i32::from(addr & 0xFF);
                for page in [0x20, 0x40, 0x60, 0x80, 0xE0] {
                    let w = simg.get(&(a_port, a_off + page)).copied().unwrap_or(0);
                    let Some(g) = eimg.get(&(a_port, a_off + page)).copied() else {
                        continue;
                    };
                    bytes_cmp += 1;
                    if w != g {
                        bad.push(format!(
                            "ch{:?} t={:.3} op {}/{:02X} want {:02X} got {:02X}",
                            ch,
                            *t as f64 / VGM_RATE as f64,
                            a_port,
                            a_off + page,
                            w,
                            g
                        ));
                    }
                }
            }
        }
    }
    // Both comparisons must actually have run: every attack of the fixture,
    // and several operator registers at each one.
    assert!(total > min_attacks, "{}", name);
    assert!(bytes_cmp > total * 4, "{}", name);
    assert_eq!(
        &bad[..bad.len().min(6)],
        &[] as &[String],
        "{}: {} diffs",
        name,
        bad.len()
    );
}

fn four_op_opl3() -> VgmFile {
    let mut ws = vec![ChipWrite::new(90, KIND_OPL3, 0, 0x104, 0x01)];
    for i in 0..14 {
        let base = 1000 + i * 9000;
        ws.extend(voice(
            base,
            KIND_OPL3,
            0,
            Voice {
                four: true,
                block: 4,
                fnum: 580,
                ops: Some(vec![
                    OpSpec::default(),
                    OpSpec {
                        mult: 5,
                        ..Default::default()
                    },
                    OpSpec {
                        mult: 3,
                        ..Default::default()
                    },
                    OpSpec {
                        mult: 7,
                        ..Default::default()
                    },
                ]),
                alg: 0,
                pan: 2,
                off_time: base + 6000,
                ..Default::default()
            },
        ));
        ws.extend(voice(
            base + 100,
            KIND_OPL3,
            2,
            Voice {
                block: 3,
                fnum: 514,
                ops: Some(vec![
                    OpSpec {
                        tl: 9,
                        ..Default::default()
                    },
                    OpSpec {
                        tl: 20,
                        ..Default::default()
                    },
                ]),
                alg: 1,
                off_time: base + 5000,
                ..Default::default()
            },
        ));
    }
    _vgm(
        ws,
        VgmOpts {
            kind: KIND_OPL3,
            total_samples: 125000,
            ..Default::default()
        },
    )
}

fn opl2_rhythm() -> VgmFile {
    let mut ws: Vec<ChipWrite> = Vec::new();
    for i in 0..4 {
        let base = 1000 + i * 16000;
        ws.extend(voice(
            base,
            KIND_OPL2,
            6,
            Voice {
                drums: true,
                block: 3,
                fnum: 900,
                off_time: base + 12000,
                ..Default::default()
            },
        ));
        let addrs: Vec<u16> = (7..11)
            .map(|f| SLOT_ADDR[SLOTS_OPL2_DRUMS[0][f] as usize])
            .collect();
        ws.extend(op_writes(
            base + 100,
            KIND_OPL2,
            i32::from(addrs[0]),
            OpSpec {
                mult: 6,
                tl: 11,
                ..Default::default()
            },
        ));
        ws.extend(op_writes(
            base + 100,
            KIND_OPL2,
            i32::from(addrs[2]),
            OpSpec {
                mult: 9,
                tl: 17,
                ..Default::default()
            },
        ));
        ws.extend([
            rhythm_bits(base + 500, 0x10),
            rhythm_bits(base + 2000, 0x1E),
            rhythm_bits(base + 6000, 0x02),
            rhythm_bits(base + 10000, 0),
        ]);
    }
    _vgm(
        ws,
        VgmOpts {
            total_samples: 65000,
            ..Default::default()
        },
    )
}

fn opl1_rhythm() -> VgmFile {
    let mut ws: Vec<ChipWrite> = Vec::new();
    for i in 0..4 {
        let base = 1000 + i * 16000;
        ws.extend(voice(
            base,
            KIND_OPL,
            6,
            Voice {
                drums: true,
                block: 3,
                fnum: 900,
                off_time: base + 12000,
                ..Default::default()
            },
        ));
        let addrs: Vec<u16> = (7..11)
            .map(|f| SLOT_ADDR[SLOTS_OPL2_DRUMS[0][f] as usize])
            .collect();
        ws.extend(op_writes(
            base + 100,
            KIND_OPL,
            i32::from(addrs[0]),
            OpSpec {
                mult: 6,
                tl: 11,
                ..Default::default()
            },
        ));
        ws.extend(op_writes(
            base + 100,
            KIND_OPL,
            i32::from(addrs[2]),
            OpSpec {
                mult: 9,
                tl: 17,
                ..Default::default()
            },
        ));
        ws.extend([
            rhythm_kind_bits(base + 500, KIND_OPL, 0x10),
            rhythm_kind_bits(base + 2000, KIND_OPL, 0x1E),
            rhythm_kind_bits(base + 6000, KIND_OPL, 0x02),
            rhythm_kind_bits(base + 10000, KIND_OPL, 0),
        ]);
    }
    _vgm(
        ws,
        VgmOpts {
            kind: KIND_OPL,
            total_samples: 65000,
            ..Default::default()
        },
    )
}

fn dual_opl2() -> VgmFile {
    let mut ws: Vec<ChipWrite> = Vec::new();
    for i in 0..8 {
        let base = 1000 + i * 9000;
        ws.extend(voice(
            base,
            KIND_OPL2,
            0,
            Voice {
                block: 4,
                fnum: 580,
                off_time: base + 5000,
                ..Default::default()
            },
        ));
        ws.extend(voice(
            base + 50,
            KIND_OPL2,
            1,
            Voice {
                cid: 1,
                block: 5,
                fnum: 400,
                off_time: base + 5000,
                ..Default::default()
            },
        ));
    }
    _vgm(
        ws,
        VgmOpts {
            opl2_dual: true,
            opl2_pan: true,
            total_samples: 80000,
            ..Default::default()
        },
    )
}

#[allow(non_snake_case)]
mod TestOplRegisterRoundTrip {
    use super::*;

    #[test]
    fn test_four_operator_pair_keeps_every_register() {
        skip_without_furnace!();
        let src = four_op_opl3();
        let song = analyze_default(&src);
        assert!(
            song.fm_patches.iter().any(|p| p.four),
            "no 4-op voice"
        );
        check_round_trip(&src, KIND_OPL3, 20, "four_op");
    }

    #[test]
    fn test_opl2_rhythm_kit_keeps_every_register() {
        skip_without_furnace!();
        let src = opl2_rhythm();
        let song = analyze_default(&src);
        assert!(song.opl_drums);
        assert_eq!(
            parse_fur(&write_plain(&song)).unwrap().chips,
            vec![CHIP_OPL2_DRUMS]
        );
        check_round_trip(&src, KIND_OPL2, 10, "opl2_kit");
    }

    #[test]
    fn test_opl1_rhythm_keeps_every_register() {
        skip_without_furnace!();
        // The YM3526 has rhythm mode exactly like a YM3812: same $BD register,
        // same kit. Its absent waveform page is checked by TestOplChip1.
        let src = opl1_rhythm();
        let song = analyze_default(&src);
        assert_eq!(song.fm_chip, KIND_OPL);
        assert!(song.opl_drums, "the YM3526 was kept melodic");
        let m = parse_fur(&write_plain(&song)).unwrap();
        assert_eq!(m.chips, vec![CHIP_OPL_DRUMS]);
        assert_eq!(m.n_ch, 11);
        check_round_trip(&src, KIND_OPL, 10, "opl1_kit");
    }

    #[test]
    fn test_dual_opl2_chips_keep_every_register() {
        skip_without_furnace!();
        let src = dual_opl2();
        assert!(src.opl2_dual && src.opl2_pan);
        let song = analyze_default(&src);
        assert_eq!(song.opl_ids, vec![0, 1]);
        let m = parse_fur(&write_plain(&song)).unwrap();
        assert_eq!(m.chips, vec![CHIP_OPL2, CHIP_OPL2]);
        assert_eq!(m.n_ch, 18);
        check_round_trip(&src, KIND_OPL2, 10, "dual_opl2");
    }
}

fn mixed(opl_notes: i32, ym_notes: i32) -> VgmFile {
    let mut writes: Vec<ChipWrite> = Vec::new();
    for i in 0..opl_notes {
        writes.extend(voice(
            1000 + i as i64 * 5000,
            KIND_OPL2,
            i % 9,
            Voice {
                block: 4,
                fnum: 500 + i,
                off_time: 0,
                ..Default::default()
            },
        ));
    }
    for i in 0..ym_notes {
        let t = 2000 + i as i64 * 6000;
        let ch = i % 3;
        writes.push(ChipWrite::new(t, "ym2203", 0, 0x28, (0xF0 | ch) as u32));
        writes.push(ChipWrite::new(t + 100, "ym2203", 0, 0x28, ch as u32));
    }
    writes.sort_by_key(|w| w.sample);
    _vgm(
        writes,
        VgmOpts {
            total_samples: 200000,
            ym2203_clock: 3579545,
            ..Default::default()
        },
    )
}

#[allow(non_snake_case)]
mod TestMixedFamilySelection {
    use super::*;

    #[test]
    fn test_opn_and_opl_are_both_kept() {
        // Furnace hosts a YM2203 beside an OPL family as two systems. The
        // earlier key-on dominance test dropped the smaller side, which lost
        // the YM2203's FM and SSG parts entirely on merged rips.
        let song = analyze_default(&mixed(8, 2));
        assert_eq!(song.fm_chip, "ym2203");
        assert_eq!(song.ym2203_ids, vec![0]);
        assert_eq!(song.opl_ids, vec![0]);
        assert_eq!(
            song.fm_systems
                .iter()
                .map(|s| s.kind.as_str())
                .collect::<Vec<_>>(),
            vec!["ym2203", "opl2"]
        );
        assert!(
            song.warnings
                .iter()
                .any(|w| w.contains("each keeps its own system")),
            "{:?}",
            song.warnings
        );
    }

    #[test]
    fn test_opn_and_opl_keep_both_systems_when_opn_is_denser() {
        let song = analyze_default(&mixed(2, 8));
        assert_eq!(song.fm_chip, "ym2203");
        assert_eq!(song.ym2203_ids, vec![0]);
        assert_eq!(song.opl_ids, vec![0]);
        assert_eq!(
            song.fm_systems
                .iter()
                .map(|s| s.kind.as_str())
                .collect::<Vec<_>>(),
            vec!["ym2203", "opl2"]
        );
    }

    #[test]
    fn test_two_opl_kinds_pick_the_denser_side() {
        let mut writes: Vec<ChipWrite> = Vec::new();
        for i in 0..2 {
            writes.extend(voice(
                1000 + i as i64 * 5000,
                KIND_OPL2,
                i,
                Voice {
                    block: 4,
                    fnum: 500,
                    off_time: 0,
                    ..Default::default()
                },
            ));
        }
        for i in 0..5 {
            writes.extend(voice(
                1500 + i as i64 * 5000,
                KIND_OPL3,
                i,
                Voice {
                    block: 4,
                    fnum: 600,
                    off_time: 0,
                    ..Default::default()
                },
            ));
        }
        let mut src = _vgm(
            writes,
            VgmOpts {
                kind: KIND_OPL2,
                total_samples: 200000,
                ..Default::default()
            },
        );
        src.opl3_clock = 14400000;
        let song = analyze_default(&src);
        assert_eq!(song.fm_chip, KIND_OPL3);
        assert!(
            song.warnings
                .iter()
                .any(|w| w.contains("more than one OPL family")),
            "{:?}",
            song.warnings
        );
    }
}

#[allow(non_snake_case)]
mod TestOplVerify {
    use super::*;

    #[test]
    fn test_pitch_stream_follows_the_channel_registers() {
        let mut ws = voice(
            100,
            KIND_OPL2,
            3,
            Voice {
                block: 4,
                fnum: 580,
                ..Default::default()
            },
        );
        ws.extend(heart_default());
        let src = _vgm(ws, VgmOpts::default());
        let stream = opl_pitch_stream(&src, 3);
        assert!(!stream.is_empty());
        let want = py_int(py_round(opl_pitch_units(4, 580))) as i32;
        assert!(stream.iter().any(|(_t, u)| *u == want));
    }

    #[test]
    fn test_intervals_name_the_keyed_span() {
        let mut ws = voice(
            100,
            KIND_OPL2,
            1,
            Voice {
                block: 3,
                fnum: 700,
                off_time: 40000,
                ..Default::default()
            },
        );
        ws.extend(heart_default());
        let src = _vgm(ws, VgmOpts::default());
        let spans = opl_keyed_intervals(&src, 1);
        assert_eq!(spans.len(), 1, "{:?}", spans);
        let (start, end) = spans[0];
        assert!((100..=200).contains(&start), "{}", start);
        assert!((end - 40000).abs() <= 800, "{}", end);
    }

    #[test]
    fn test_a_kit_column_is_not_measured() {
        // Snare and hi-hat share one frequency register, so neither stream says
        // anything about the voice; the measure stays silent rather than wrong.
        let mut ws = voice(
            100,
            KIND_OPL2,
            6,
            Voice {
                drums: true,
                ..Default::default()
            },
        );
        ws.extend(heart_default());
        ws.extend([rhythm(2000), rhythm_bits(10000, 0x09), rhythm_bits(14000, 0)]);
        let src = _vgm(ws, VgmOpts::default());
        assert_eq!(opl_pitch_stream(&src, 7), Vec::new());
        assert_eq!(opl_keyed_intervals(&src, 10), Vec::new());
    }

    #[test]
    fn test_the_second_chip_is_measured_apart() {
        let mut ws = voice(100, KIND_OPL2, 0, Voice::default());
        ws.extend(heart_default());
        ws.extend(voice(
            140,
            KIND_OPL2,
            1,
            Voice {
                cid: 1,
                block: 5,
                fnum: 400,
                ..Default::default()
            },
        ));
        let src = _vgm(ws, VgmOpts::default());
        let first = opl_pitch_stream(&src, 0);
        let second = opl_pitch_stream(&src, 9 + 1);
        assert!(first
            .iter()
            .any(|(_t, u)| *u == py_int(py_round(opl_pitch_units(4, 580))) as i32));
        assert!(second
            .iter()
            .any(|(_t, u)| *u == py_int(py_round(opl_pitch_units(5, 400))) as i32));
    }

    #[test]
    fn test_a_wrong_octave_shows_up_as_one_run() {
        // The same eight notes written an octave apart: the measure must report the
        // spans, so a real error cannot hide behind the trim.
        fn stream(block: i32) -> VgmFile {
            let mut ws = Vec::new();
            for k in 0..8 {
                let t = 210 + k * 5000;
                ws.extend(voice(
                    t,
                    KIND_OPL2,
                    3,
                    Voice {
                        block,
                        fnum: 781,
                        off_time: t + 4600,
                        ..Default::default()
                    },
                ));
            }
            ws.extend(heart_default());
            _vgm(
                ws,
                VgmOpts {
                    total_samples: 45000,
                    ..Default::default()
                },
            )
        }

        let runs = pitch_runs(&stream(5), &stream(4), 3, 0.25, 1.0);
        assert!(!runs.is_empty(), "an octave went unnoticed");
        assert!(runs[0].worst_st.abs() > 11.0, "{}", runs[0].worst_st);
        assert_eq!(
            pitch_runs(&stream(4), &stream(4), 3, 0.25, 1.0),
            Vec::new()
        );
    }
}

#[allow(non_snake_case)]
mod TestOplReport {
    use super::*;

    #[test]
    fn test_the_report_names_the_chip_and_its_columns() {
        let mut ws = voice(100, KIND_OPL3, 8, Voice::default());
        ws.extend(heart(KIND_OPL3));
        let song = analyze_default(&_vgm(
            ws,
            VgmOpts {
                kind: KIND_OPL3,
                ..Default::default()
            },
        ));
        let text = format_report(&song);
        assert!(
            text.contains("FM chip     YMF262 x1 @ 14.3182 MHz"),
            "{}",
            text
        );
        assert!(
            text.contains("FM voices   18 columns, 0 four-operator instrument(s)"),
            "{}",
            text
        );
    }

    #[test]
    fn test_a_glide_never_blames_an_opn_chip() {
        // The pitch-sweep warning is shared with the OPN paths; on an OPL log it
        // has to name the chip that was actually read.
        let mut ws = voice(
            100,
            KIND_OPL3,
            2,
            Voice {
                block: 4,
                fnum: 580,
                ..Default::default()
            },
        );
        ws.extend(heart(KIND_OPL3));
        ws.extend([
            ChipWrite::new(2000, KIND_OPL3, 0, 0xA1, 0x80),
            ChipWrite::new(2000, KIND_OPL3, 0, 0xB1, (4 << 2) | 3),
        ]);
        ws.extend(voice(
            20000,
            KIND_OPL3,
            2,
            Voice {
                block: 4,
                fnum: 700,
                off_time: 60000,
                ..Default::default()
            },
        ));
        let song = analyze_default(&_vgm(
            ws,
            VgmOpts {
                kind: KIND_OPL3,
                ..Default::default()
            },
        ));
        for w in &song.warnings {
            for other in ["YM2151", "YM2203", "YM2612"] {
                assert!(!w.contains(other), "{}", w);
            }
        }
    }

    #[test]
    fn test_a_kit_module_counts_its_hits_apart() {
        let mut ws = voice(
            100,
            KIND_OPL2,
            6,
            Voice {
                drums: true,
                ..Default::default()
            },
        );
        ws.extend(heart_default());
        ws.extend([rhythm(2000), rhythm_bits(10000, 0x0C), rhythm_bits(14000, 0)]);
        let song = analyze_default(&_vgm(ws, VgmOpts::default()));
        let text = format_report(&song);
        assert!(
            text.contains("FM chip     YM3812 + rhythm x1 @ 3.57954 MHz"),
            "{}",
            text
        );
        let kit = text
            .lines()
            .find(|line| line.contains("kit hits"))
            .expect("kit line");
        assert!(
            kit.contains(&format!("+{} kit hits", song.drum_notes)),
            "{}",
            kit
        );
        let mut ws = voice(100, KIND_OPL2, 3, Voice::default());
        ws.extend(heart_default());
        assert!(!format_report(&analyze_default(&_vgm(ws, VgmOpts::default())))
            .contains("kit hits"));
    }
}

#[allow(non_snake_case)]
mod TestOplVerifyRoundTrip {
    use super::*;

    #[test]
    fn test_a_converted_song_diverges_nowhere() {
        skip_without_furnace!();
        // The pipeline's own measure on a song the converter built: no channel may
        // report a held wrong pitch, and one that does would show the row grid,
        // the register model or the key map to be wrong.
        let mut ws = voice(
            100,
            KIND_OPL3,
            2,
            Voice {
                block: 4,
                fnum: 580,
                off_time: 30000,
                ..Default::default()
            },
        );
        ws.extend(voice(
            40000,
            KIND_OPL3,
            2,
            Voice {
                block: 5,
                fnum: 700,
                off_time: 80000,
                ..Default::default()
            },
        ));
        ws.extend(voice(
            120,
            KIND_OPL3,
            8,
            Voice {
                block: 3,
                fnum: 977,
                off_time: 50000,
                ..Default::default()
            },
        ));
        ws.push(ChipWrite::new(90, KIND_OPL3, 0, 0x104, 0x08));
        ws.extend(voice(
            95,
            KIND_OPL3,
            6,
            Voice {
                four: true,
                block: 4,
                fnum: 977,
                ops: Some(vec![
                    OpSpec::default(),
                    OpSpec {
                        mult: 5,
                        ..Default::default()
                    },
                    OpSpec {
                        mult: 3,
                        ..Default::default()
                    },
                    OpSpec {
                        mult: 7,
                        ..Default::default()
                    },
                ]),
                ..Default::default()
            },
        ));
        ws.extend(heart_ch(KIND_OPL3, 15));
        ws.extend(heart_ch(KIND_OPL3, 0));
        let song = analyze_default(&_vgm(
            ws,
            VgmOpts {
                kind: KIND_OPL3,
                ..Default::default()
            },
        ));
        let fur = tmp().join("verify_4op.fur");
        std::fs::write(&fur, write_plain(&song)).unwrap();
        let out = tmp().join("verify_4op.vgm");
        let path = export_fur(&fur, Some(&out)).unwrap();
        let exp = load_vgm(&path).unwrap();
        let min_run_ms = f64::max(
            4.0,
            song.samples_per_row() * 1000.0 / VGM_RATE as f64 * 0.75,
        );
        for ch in 0..channel_count(KIND_OPL3, false) {
            let runs = pitch_runs(&song.vgm, &exp, ch, 0.25, min_run_ms);
            assert!(
                runs.iter()
                    .map(|r| (r.t0, r.worst_st))
                    .collect::<Vec<_>>()
                    .is_empty(),
                "ch{}",
                ch
            );
        }
    }
}

#[allow(non_snake_case)]
mod TestOplChip1 {
    use super::*;

    #[test]
    fn test_the_pitch_table_matches_the_opl2() {
        // Same base and same shift: a YM3526 at COLOR_NTSC is in tune with a
        // YM3812, and A-440 sits on the published f-num 290.
        let clock = opl_module_clock(CLOCK_OPL2 as f64, false);
        let tbl = pitch_table(clock, false, None);
        assert_eq!(tbl.base, 32768.0 * 72.0);
        assert_eq!(opl_note_fnum(57, clock, 0x80, false, 0, None), (0, 290));
        assert_eq!(opl_note_fnum(69, clock, 0x80, false, 0, None), (0, 580));
    }

    #[test]
    fn test_the_module_is_an_opl_system_of_nine_channels() {
        let mut ws = voice(100, KIND_OPL, 2, Voice::default());
        ws.extend(heart(KIND_OPL));
        let song = analyze_default(&_vgm(
            ws,
            VgmOpts {
                kind: KIND_OPL,
                ..Default::default()
            },
        ));
        assert_eq!(song.fm_chip, KIND_OPL);
        let m = parse_fur(&write_plain(&song)).unwrap();
        assert_eq!(m.chips, vec![CHIP_OPL]);
        assert_eq!(m.n_ch, 9);
    }

    #[test]
    fn test_rhythm_mode_gives_the_opl_drums_system() {
        let mut ws = voice(
            100,
            KIND_OPL,
            6,
            Voice {
                drums: true,
                ..Default::default()
            },
        );
        ws.extend(heart(KIND_OPL));
        ws.extend([
            rhythm_kind_bits(1000, KIND_OPL, 0),
            rhythm_kind_bits(9000, KIND_OPL, 0x18),
            rhythm_kind_bits(13000, KIND_OPL, 0),
        ]);
        let song = analyze_default(&_vgm(
            ws,
            VgmOpts {
                kind: KIND_OPL,
                ..Default::default()
            },
        ));
        assert!(song.opl_drums);
        let m = parse_fur(&write_plain(&song)).unwrap();
        assert_eq!(m.chips, vec![CHIP_OPL_DRUMS]);
        assert_eq!(m.n_ch, 11);
    }
}

#[allow(non_snake_case)]
mod TestOplChip1RoundTrip {
    use super::*;

    #[test]
    fn test_no_waveform_register_and_the_logged_frequency() {
        skip_without_furnace!();
        // The chip has no 0xE0 page, so nothing may reach it - and the note still
        // lands on the rip's own phase step.
        let mut ws = voice(
            100,
            KIND_OPL,
            2,
            Voice {
                block: 4,
                fnum: 580,
                ops: Some(vec![
                    OpSpec {
                        ws: 1,
                        ..Default::default()
                    },
                    OpSpec {
                        ws: 3,
                        ..Default::default()
                    },
                ]),
                ..Default::default()
            },
        );
        ws.extend(heart(KIND_OPL));
        let song = analyze_default(&_vgm(
            ws,
            VgmOpts {
                kind: KIND_OPL,
                ..Default::default()
            },
        ));
        let fur = tmp().join("opl1.fur");
        std::fs::write(&fur, write_plain(&song)).unwrap();
        let out = tmp().join("opl1.vgm");
        let path = export_fur(&fur, Some(&out)).unwrap();
        let back = load_vgm(&path).unwrap();
        let pages: BTreeSet<u32> = back
            .writes
            .iter()
            .filter(|w| w.chip == KIND_OPL)
            .map(|w| w.reg & 0xF0)
            .collect();
        assert!(
            !pages.contains(&0xE0),
            "{:?}",
            pages.iter().map(|p| format!("{:#x}", p)).collect::<Vec<_>>()
        );
        assert!(keyon_units(&back, KIND_OPL, false).contains(&(580 << 4)));
    }
}
