//! YM2203 (OPN) support: parsing, pitch mapping, analysis and .fur emission.

mod common;
use common::*;

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use vgm2fur::analyze::{analyze, AnalyzeOptions};
use vgm2fur::ay8910::ay_rate;
use vgm2fur::diag::{opn_pitch_stream, pitch_runs};
use vgm2fur::furio::{chip_channels, iter_blocks, parse_fur};
use vgm2fur::furwrite::{
    write_fur, WriteOptions, CHIP_YM2203, CHIP_YM2203_EXT, INS_AY, INS_FM,
};
use vgm2fur::furnace::export_vgm;
use vgm2fur::model::{furnace_note_name, NoteEvent, Song};
use vgm2fur::parity::py_round;
use vgm2fur::vgm::{chips_in_head, load_vgm, ChipWrite, VgmFile};
use vgm2fur::ym2203::{opn_note_for, opn_note_fnum, opn_pitch_units, OPN_FREQ_BASE};

const CLOCK: u32 = 3_072_000;

fn tmp_dir() -> PathBuf {
    let p = std::env::temp_dir().join("vgm2151fur_ym2203");
    std::fs::create_dir_all(&p).expect("create temp dir");
    p
}

/// The synthetic `VgmFile` the Python `_vgm` builds.
fn synthetic(writes: Vec<ChipWrite>, ym2203_clock: u32) -> VgmFile {
    let mut v = VgmFile::default();
    v.path = PathBuf::from("synthetic.vgm");
    v.buf = Vec::new();
    v.version = 0x161;
    v.total_samples = 400_000;
    v.loop_samples = 0;
    v.loop_file_pos = None;
    v.loop_sample = None;
    v.ym2151_clock = 0;
    v.k007232_clock = 0;
    v.gd3 = Default::default();
    v.roms = Vec::new();
    v.ym2203_clock = ym2203_clock;
    let mut writes = writes;
    // a real VGM is in stream order; sorting also keeps same-sample register
    // write order stable
    writes.sort_by_key(|w| w.sample);
    v.writes = writes;
    v
}

/// `_raw_vgm` in the reference: the YM2203 clock lives at header 0x44.
fn raw_vgm(body: &[u8], clock: u32, dual: bool) -> Vec<u8> {
    let mut buf = vec![0u8; 0xC0];
    buf[0..4].copy_from_slice(b"Vgm ");
    buf[8..12].copy_from_slice(&0x161u32.to_le_bytes());
    buf[0x18..0x1C].copy_from_slice(&44100u32.to_le_bytes());
    buf[0x34..0x38].copy_from_slice(&(0xC0u32 - 0x34).to_le_bytes());
    let raw = (clock & 0x3FFF_FFFF) | if dual { 0x4000_0000 } else { 0 };
    buf[0x44..0x48].copy_from_slice(&raw.to_le_bytes());
    let mut out = buf;
    out.extend_from_slice(body);
    out.push(0x66);
    let len = (out.len() - 4) as u32;
    out[4..8].copy_from_slice(&len.to_le_bytes());
    out
}

fn make_patch(
    t: i64,
    cid: u8,
    sr: [u32; 4],
    ssg: [u32; 4],
) -> Vec<ChipWrite> {
    let mut out: Vec<ChipWrite> = Vec::new();
    for page in [0x30u32, 0x40, 0x50, 0x60, 0x70, 0x80, 0x90] {
        for op in 0..4usize {
            let val = match page {
                0x30 => 0x01,
                0x50 => 0x1F,
                0x70 => sr[op],
                0x90 => ssg[op],
                _ => 0x00,
            };
            out.push(ChipWrite::new(t, "ym2203", cid, page + op as u32 * 4, val));
        }
    }
    out.push(ChipWrite::new(t, "ym2203", cid, 0xB0, 0x07)); // alg 7: all carriers
    out
}

#[allow(clippy::too_many_arguments)]
fn fm_note_full(
    block: u32,
    fnum: u32,
    t: i64,
    off: i64,
    cid: u8,
    ch: u32,
    patch_t: Option<i64>,
    sr: [u32; 4],
    ssg: [u32; 4],
) -> Vec<ChipWrite> {
    let mut out = make_patch(patch_t.unwrap_or(t), cid, sr, ssg);
    out.push(ChipWrite::new(
        t,
        "ym2203",
        cid,
        0xA4 + ch,
        (block << 3) | ((fnum >> 8) & 7),
    ));
    out.push(ChipWrite::new(t, "ym2203", cid, 0xA0 + ch, fnum & 0xFF));
    out.push(ChipWrite::new(t, "ym2203", cid, 0x28, 0xF0 | ch));
    if off != 0 {
        out.push(ChipWrite::new(off, "ym2203", cid, 0x28, ch));
    }
    out
}

/// `_fm_note(block, fnum)` with the Python defaults.
fn fm_note(block: u32, fnum: u32) -> Vec<ChipWrite> {
    fm_note_full(block, fnum, 100, 60_000, 0, 0, None, [0; 4], [0; 4])
}

fn ssg_note(t: i64, off: i64, cid: u8) -> Vec<ChipWrite> {
    vec![
        ChipWrite::new(t, "ym2203_ssg", cid, 0, 0x1C),
        ChipWrite::new(t, "ym2203_ssg", cid, 1, 0x01), // period 284
        ChipWrite::new(t, "ym2203_ssg", cid, 7, 0xFE), // tone A only
        ChipWrite::new(t, "ym2203_ssg", cid, 8, 0x0F), // volume A max
        ChipWrite::new(off, "ym2203_ssg", cid, 8, 0x00),
    ]
}

fn ssg_note_default() -> Vec<ChipWrite> {
    ssg_note(100, 60_000, 0)
}

fn ins_types(data: &[u8]) -> Vec<i32> {
    let mut out = Vec::new();
    for block in iter_blocks(data).unwrap() {
        if block.magic == *b"INS2" && block.payload.len() >= 4 {
            out.push(u16::from_le_bytes([block.payload[2], block.payload[3]]) as i32);
        }
    }
    out
}

fn fm_feature(data: &[u8]) -> Vec<u8> {
    for block in iter_blocks(data).unwrap() {
        if block.magic != *b"INS2" {
            continue;
        }
        let payload = block.payload;
        if let Some(pos) = payload.windows(2).position(|w| w == b"FM") {
            if pos + 4 <= payload.len() {
                let size =
                    u16::from_le_bytes([payload[pos + 2], payload[pos + 3]]) as usize;
                if pos + 4 + size <= payload.len() {
                    return payload[pos + 4..pos + 4 + size].to_vec();
                }
            }
        }
    }
    Vec::new()
}

fn has(data: &[u8], pat: &[u8]) -> bool {
    data.windows(pat.len()).any(|w| w == pat)
}

fn count_pat(data: &[u8], pat: &[u8]) -> usize {
    data.windows(pat.len()).filter(|w| *w == pat).count()
}

/// Python's `opn_pitch_units` also takes a float f-num; the Rust port's
/// signature is `i32`. This mirrors the Python body exactly for the round-trip
/// comparison.
fn opn_pitch_units_f(block: i32, fnum: f64) -> f64 {
    let fnum = if fnum <= 0.0 { 1.0 } else { fnum };
    768.0 * (fnum.log2() + block as f64)
}

fn analyze_pcm_off(v: &VgmFile) -> Song {
    analyze(
        v,
        AnalyzeOptions {
            pcm: false,
            ..Default::default()
        },
    )
    .expect("analyze")
}

fn write_no_pcm(song: &Song) -> Vec<u8> {
    write_fur(
        song,
        WriteOptions {
            include_pcm: false,
            ..Default::default()
        },
    )
}

#[allow(non_snake_case)]
mod TestYm2203Parsing {
    use super::*;

    #[test]
    fn test_header_and_register_split() {
        let body = [
            0x55, 0x30, 0x01, // FM ch0 op1
            0x55, 0x08, 0x0F, // SSG channel A volume
            0xA5, 0x40, 0x02, // second chip FM
            0xA5, 0x09, 0x0F, // second chip SSG
        ];
        let p = tmp_dir().join("ym2203_dual.vgm");
        std::fs::write(&p, raw_vgm(&body, CLOCK, true)).unwrap();
        let vgm = load_vgm(&p).unwrap();
        assert_eq!(vgm.ym2203_clock, CLOCK);
        assert!(vgm.ym2203_dual);
        let got: Vec<(&str, u8, u32, u32)> = vgm
            .writes
            .iter()
            .map(|w| (w.chip, w.chip_id, w.reg, w.val))
            .collect();
        assert_eq!(
            got,
            vec![
                ("ym2203", 0, 0x30, 0x01),
                ("ym2203_ssg", 0, 0x08, 0x0F),
                ("ym2203", 1, 0x40, 0x02),
                ("ym2203_ssg", 1, 0x09, 0x0F),
            ]
        );
    }

    #[test]
    fn test_raw_clock_is_masked() {
        let p = tmp_dir().join("ym2203_clock.vgm");
        std::fs::write(&p, raw_vgm(b"\x55\x30\x01", CLOCK, true)).unwrap();
        assert_eq!(load_vgm(&p).unwrap().ym2203_clock, CLOCK);
    }

    #[test]
    fn test_chips_in_head() {
        assert!(chips_in_head(&raw_vgm(b"\x66", CLOCK, false)).contains("ym2203"));
        let kinds = chips_in_head(&raw_vgm(b"\x66", CLOCK, true));
        assert!(kinds.contains("ym2203"));
        assert!(kinds.contains("ym2203_dual"));
    }

    #[test]
    fn test_stray_a5_is_first_chip_with_warning() {
        let p = tmp_dir().join("ym2203_stray.vgm");
        std::fs::write(&p, raw_vgm(&[0xA5, 0x40, 0x02], CLOCK, false)).unwrap();
        let vgm = load_vgm(&p).unwrap();
        assert!(!vgm.ym2203_dual);
        let got: Vec<(&str, u8)> = vgm.writes.iter().map(|w| (w.chip, w.chip_id)).collect();
        assert_eq!(got, vec![("ym2203", 0)]);
        assert!(vgm
            .warnings
            .iter()
            .any(|w| w.contains("without the dual-instance")));
    }

    #[test]
    fn test_missing_clock_warning() {
        let p = tmp_dir().join("ym2203_noclock.vgm");
        std::fs::write(&p, raw_vgm(b"\x55\x30\x01", 0, false)).unwrap();
        let vgm = load_vgm(&p).unwrap();
        assert_eq!(vgm.ym2203_clock, 0);
        assert!(vgm.warnings.iter().any(|w| w.contains("YM2203 clock")));
    }
}

#[allow(non_snake_case)]
mod TestYm2203Pitch {
    use super::*;

    #[test]
    fn test_note_fnum_is_the_forward_mapping() {
        // Furnace's linear-mode note conversion (calcBaseFreqFNumBlock).
        for note in [60, 72, 84, 96, 108, 115] {
            let (block, fnum) = opn_note_fnum(note, CLOCK);
            let nbase = note << 7;
            let fbase = 440.0 * 2.0f64.powf((nbase as f64 - 7296.0) / 1536.0);
            let bf = py_round(fbase * (OPN_FREQ_BASE / CLOCK as f64)) as i64;
            let want_block = ((note - 60) / 12).clamp(0, 7);
            assert_eq!((block, fnum), (want_block, (bf >> want_block) as i32));
        }
    }

    #[test]
    fn test_e5_is_one_128th_semitone() {
        // fnum 1232 is note 115 (f-num 1204) plus 0.398 semitone; that is
        // 51 units of E5 in this module's default linear pitch mode.
        assert_eq!(opn_note_for(4, 1232, CLOCK), (115, 0x80 + 51));
        assert_eq!(opn_note_for(4, 1204, CLOCK), (115, 0x80));
    }

    #[test]
    fn test_round_trip_through_e5() {
        for (block, fnum) in [(4, 1100), (4, 1232), (3, 923), (3, 1466), (2, 2016)] {
            let (note, e5) = opn_note_for(block, fnum, CLOCK);
            let (b, base) = opn_note_fnum(note, CLOCK);
            let got = base as f64 * 2.0f64.powf((e5 - 0x80) as f64 / 1536.0);
            // the chosen note may be in another octave; compare pitch, not the
            // raw f-num
            assert!(
                (opn_pitch_units_f(b, got) - opn_pitch_units(block, fnum)).abs() < 0.2
            );
        }
    }

    #[test]
    fn test_pitch_units_ignore_block_equivalents() {
        assert_almost_eq!(opn_pitch_units(3, 1000), opn_pitch_units(4, 500), 9);
    }
}

#[allow(non_snake_case)]
mod TestYm2203ExtColumns {
    use super::*;

    const T_ON: i64 = 200_000;
    const T_MOVE: i64 = 230_000;
    const T_OFF: i64 = 400_000 - 5_000;

    fn writes() -> Vec<ChipWrite> {
        let mut w = Vec::new();
        for op in 0..4u32 {
            // detune/mult, rates
            w.push(ChipWrite::new(100, "ym2203", 0, 0x30 + op * 4, 0x01));
            w.push(ChipWrite::new(101, "ym2203", 0, 0x50 + op * 4, 0x1F));
            w.push(ChipWrite::new(102, "ym2203", 0, 0x60 + op * 4, 0x00));
            w.push(ChipWrite::new(103, "ym2203", 0, 0x70 + op * 4, 0x1F));
            w.push(ChipWrite::new(104, "ym2203", 0, 0x80 + op * 4, 0x1F));
        }
        // one distinct level per operator, in register order S1,S3,S2,S4
        for (reg, tl) in [(0x42u32, 0x10u32), (0x46, 0x20), (0x4A, 0x30), (0x4E, 0x40)] {
            w.push(ChipWrite::new(105, "ym2203", 0, reg, tl));
        }
        w.push(ChipWrite::new(106, "ym2203", 0, 0xB2, 0x07)); // ch3 alg 7
        w.push(ChipWrite::new(107, "ym2203", 0, 0xAC, 0x15)); // per-slot f-num:
        w.push(ChipWrite::new(108, "ym2203", 0, 0xA8, 0x00)); // this makes it Ext
        w.push(ChipWrite::new(T_ON, "ym2203", 0, 0x28, 0xF0 | 2));
        // the driver opens up slot S2 while the note sounds: $4A, a louder level
        // than any operator stored, so it is unreachable by volume alone.
        w.push(ChipWrite::new(T_MOVE, "ym2203", 0, 0x4A, 0x18));
        w.push(ChipWrite::new(T_OFF, "ym2203", 0, 0x28, 0x02));
        w
    }

    fn setup() -> Song {
        analyze_pcm_off(&synthetic(writes(), 0))
    }

    fn cols(song: &Song) -> Vec<&NoteEvent> {
        let mut ons: Vec<&NoteEvent> = song
            .events
            .iter()
            .filter(|e| e.on && !e.pcm && e.sample == T_ON)
            .collect();
        ons.sort_by_key(|e| e.ch);
        assert_eq!(ons.len(), 4, "the four operator columns each sound");
        ons
    }

    #[test]
    fn test_ext_layout_is_detected() {
        let song = setup();
        assert!(song.ym2203_ext);
    }

    #[test]
    fn test_a_partial_key_on_before_the_first_full_chord_is_kept() {
        // Grounseed "Betrayal" opens channel 3 with a two-operator mask before
        // any full 0xF chord. Those are real notes, not operator setup.
        let writes = vec![
            ChipWrite::new(100, "ym2203", 0, 0xAC, 0x15),
            ChipWrite::new(100, "ym2203", 0, 0xA8, 0x00),
            ChipWrite::new(T_ON, "ym2203", 0, 0x28, 0x30 | 2),
        ];
        let song = analyze_pcm_off(&synthetic(writes, 0));
        let mut ons: Vec<i32> = song
            .events
            .iter()
            .filter(|e| e.on && e.sample == T_ON)
            .map(|e| e.ch)
            .collect();
        ons.sort();
        assert_eq!(ons, vec![2, 3]); // operators 1 and 2
    }

    /// A four-operator channel-3 chord with a simple patch.
    fn ext_chord() -> Vec<ChipWrite> {
        let mut w = Vec::new();
        for op in 0..4u32 {
            w.push(ChipWrite::new(100, "ym2203", 0, 0x30 + op * 4, 0x01));
            w.push(ChipWrite::new(101, "ym2203", 0, 0x50 + op * 4, 0x1F));
            w.push(ChipWrite::new(102, "ym2203", 0, 0x60 + op * 4, 0x00));
            w.push(ChipWrite::new(103, "ym2203", 0, 0x70 + op * 4, 0x1F));
            w.push(ChipWrite::new(104, "ym2203", 0, 0x80 + op * 4, 0x1F));
        }
        w.push(ChipWrite::new(106, "ym2203", 0, 0xB2, 0x07));
        w.push(ChipWrite::new(106, "ym2203", 0, 0x27, 0x40)); // 3-slot mode
        w.push(ChipWrite::new(107, "ym2203", 0, 0xA6, (4 << 3) | 2));
        w.push(ChipWrite::new(107, "ym2203", 0, 0xA2, 0x34));
        w.push(ChipWrite::new(107, "ym2203", 0, 0xAC, (4 << 3) | 1));
        w.push(ChipWrite::new(107, "ym2203", 0, 0xA8, 0x18));
        w.push(ChipWrite::new(T_ON, "ym2203", 0, 0x28, 0xF2));
        w
    }

    #[test]
    fn test_held_operator_is_not_retriggered_by_a_frequency_change() {
        // Betrayal releases operators by clearing its $28 mask bits while
        // loading the next note's f-num into the same registers. The set bits
        // are still-held operators, not new notes.
        let t = 230_000i64;
        let mut ws = ext_chord();
        ws.extend([
            ChipWrite::new(t, "ym2203", 0, 0xA6, (4 << 3) | 2),
            ChipWrite::new(t, "ym2203", 0, 0xA2, 0x78),
            ChipWrite::new(t, "ym2203", 0, 0xAC, (4 << 3) | 1),
            ChipWrite::new(t, "ym2203", 0, 0xA8, 0x78),
            ChipWrite::new(t, "ym2203", 0, 0x28, 0xD2), // clear operator 2
            ChipWrite::new(t, "ym2203", 0, 0x28, 0x92), // clear operator 3
            ChipWrite::new(t, "ym2203", 0, 0x28, 0x12), // clear operator 4
        ]);
        let song = analyze_pcm_off(&synthetic(ws, 0));
        let retriggers: Vec<i32> = song
            .events
            .iter()
            .filter(|e| e.on && e.sample == t)
            .map(|e| e.ch)
            .collect();
        assert_eq!(retriggers, Vec::<i32>::new(), "a release mask re-keyed a held column");
        let ends: BTreeMap<i32, i64> = song
            .events
            .iter()
            .filter(|e| e.on && e.sample == T_ON)
            .map(|e| (e.ch, e.end))
            .collect();
        assert_eq!(ends[&3], t); // column 1 released
        assert_eq!(ends[&4], t); // column 2 released
        assert_eq!(ends[&5], t); // column 3 released
        assert_eq!(ends.get(&2).copied().unwrap_or(0), 0, "column 0 is still held");
    }

    #[test]
    fn test_split_fnum_write_does_not_make_a_pitch_step() {
        // The block/high byte and the low byte of one f-num are often written
        // a millisecond apart. The half-updated value is not a pitch step.
        let t = 230_000i64;
        let mut ws = ext_chord();
        ws.extend([
            ChipWrite::new(t, "ym2203", 0, 0xAC, (4 << 3) | 4), // high byte
            ChipWrite::new(t + 44, "ym2203", 0, 0xA8, 0x78),    // low byte
        ]);
        let song = analyze_pcm_off(&synthetic(ws, 0));
        let ev = song.events.iter().find(|e| e.on && e.ch == 4).expect("ch4 event");
        assert!(
            !ev.fm_pitch_pts.iter().any(|p| p.0 == t),
            "{:?}",
            ev.fm_pitch_pts
        );
        assert!(
            ev.fm_pitch_pts.iter().any(|p| *p == (t + 44, 4, 0x478)),
            "{:?}",
            ev.fm_pitch_pts
        );
    }

    #[test]
    fn test_each_column_keeps_its_own_operator_level() {
        let song = setup();
        let col = cols(&song);
        let patch = song.fm_patches[col[1].ins as usize].clone();
        let got: Vec<u8> = patch.ops.iter().map(|p| p.tl & 0x7F).collect();
        // $42,$46,$4A,$4E are ops[0..3] in register order; only $4A moved.
        assert_eq!(got[0], 0x10);
        assert_eq!(got[1], 0x20);
        assert_eq!(got[3], 0x40);
    }

    #[test]
    fn test_a_louder_move_opens_the_instrument_on_the_right_operator() {
        // $4A is column 1's operator (ops[2]), and it wanted 0x18.
        let song = setup();
        let col = cols(&song);
        let patch = song.fm_patches[col[1].ins as usize].clone();
        assert_eq!(
            patch.ops[2].tl & 0x7F,
            0x18,
            "the floor must land on the operator that moved"
        );
    }

    #[test]
    fn test_column_volume_states_the_note_level() {
        // That column started at 0x30 with the instrument now at 0x18, so it must
        // attenuate by the difference; the other columns started at their floor.
        let song = setup();
        let col = cols(&song);
        assert_eq!(col[1].vol, 127 - (0x30 - 0x18));
        assert_eq!(col[1].vol_tl_base, Some(0x18));
        for c in [0usize, 2, 3] {
            assert_eq!(col[c].vol, 127, "column {c} never moved off its floor");
        }
    }

    #[test]
    fn test_mid_note_move_survives_to_the_writer() {
        let song = setup();
        let col = cols(&song);
        let pts: Vec<i32> = col[1].tl_pts.iter().map(|&(_t, tl)| tl).collect();
        assert!(pts.contains(&0x18), "the climb was dropped again");
    }

    #[test]
    fn test_no_column_asks_for_an_unreachable_level() {
        let song = setup();
        let col = cols(&song);
        let patch = song.fm_patches[col[1].ins as usize].clone();
        for (c, ev) in col.iter().enumerate() {
            let floor = patch.ops[[0usize, 2, 1, 3][c]].tl & 0x7F;
            let loud: Vec<i32> = ev
                .tl_pts
                .iter()
                .filter(|&&(_t, tl)| (tl as u8) < floor)
                .map(|&(_t, tl)| tl)
                .collect();
            assert!(loud.is_empty(), "column {c} cannot reach {loud:?}");
        }
    }
}

#[allow(non_snake_case)]
mod TestYm2203ExtExport {
    use super::*;

    const T_ON: i64 = 200_000;
    const T_MOVE: i64 = 230_000;
    const T_OFF: i64 = 400_000 - 5_000;

    fn writes() -> Vec<ChipWrite> {
        let mut w = Vec::new();
        for op in 0..4u32 {
            // detune/mult, rates
            w.push(ChipWrite::new(100, "ym2203", 0, 0x30 + op * 4, 0x01));
            w.push(ChipWrite::new(101, "ym2203", 0, 0x50 + op * 4, 0x1F));
            w.push(ChipWrite::new(102, "ym2203", 0, 0x60 + op * 4, 0x00));
            w.push(ChipWrite::new(103, "ym2203", 0, 0x70 + op * 4, 0x1F));
            w.push(ChipWrite::new(104, "ym2203", 0, 0x80 + op * 4, 0x1F));
        }
        // One distinct level per operator, in register order S1,S3,S2,S4.
        for (reg, tl) in [(0x42u32, 0x10u32), (0x46, 0x20), (0x4A, 0x30), (0x4E, 0x40)] {
            w.push(ChipWrite::new(105, "ym2203", 0, reg, tl));
        }
        w.extend([
            ChipWrite::new(106, "ym2203", 0, 0xB2, 0x07), // ch3 alg 7
            ChipWrite::new(107, "ym2203", 0, 0xAC, 0x15), // per-slot f-num:
            ChipWrite::new(108, "ym2203", 0, 0xA8, 0x00), // this makes it Ext
            ChipWrite::new(T_ON, "ym2203", 0, 0x28, 0xF2),
            // The driver walks two columns down while the chord holds.
            ChipWrite::new(T_MOVE, "ym2203", 0, 0x4A, 0x18),
            ChipWrite::new(T_MOVE, "ym2203", 0, 0x46, 0x18),
            ChipWrite::new(T_OFF, "ym2203", 0, 0x28, 0x02),
        ]);
        w
    }

    /// YM2203 operators step by four within a channel, so ch3's TL
    /// registers are $42,$46,$4A,$4E (S1,S3,S2,S4).
    fn levels(stream: &VgmFile) -> BTreeMap<u32, BTreeSet<u32>> {
        let mut out: BTreeMap<u32, BTreeSet<u32>> = BTreeMap::new();
        for w in &stream.writes {
            let r = w.reg & 0xFF;
            if (0x40..=0x4E).contains(&r) && (r & 3) == 2 {
                out.entry(r).or_default().insert(w.val & 0x7F);
            }
        }
        out
    }

    #[test]
    fn test_moved_slot_levels_reach_the_chip() {
        skip_without_furnace!();
        let src = synthetic(writes(), CLOCK);
        let song = analyze_pcm_off(&src);
        assert!(song.ym2203_ext);
        let fur = tmp_dir().join("ym2203_ext_export.fur");
        std::fs::write(&fur, write_no_pcm(&song)).unwrap();
        let out = tmp_dir().join("ym2203_ext_export.vgm");
        export_vgm(&fur, &out).unwrap();
        let back = load_vgm(&out).unwrap();
        let source = levels(&src);
        let got = levels(&back);
        for reg in [0x46u32, 0x4A] {
            assert!(source[&reg].contains(&0x18));
            assert!(
                got.get(&reg).is_some_and(|s| s.contains(&0x18)),
                "${reg:02X} did not reach the export"
            );
            assert!(source[&reg].is_subset(got.get(&reg).unwrap()), "{reg}");
        }
        // The columns the driver never moved keep their source levels.
        for reg in [0x42u32, 0x4E] {
            assert_eq!(got.get(&reg), source.get(&reg), "{reg}");
        }
    }
}

#[allow(non_snake_case)]
mod TestYm2203LongVibrato {
    use super::*;

    fn long_vibrato() -> VgmFile {
        let mut ws = fm_note_full(4, 1204, 100, 2_000_000, 0, 0, None, [0; 4], [0; 4]);
        for i in 0..1900 {
            let t = 100 + i as i64 * 1000;
            let fnum = [1210u32, 1232, 1250, 1232][i % 4];
            ws.push(ChipWrite::new(
                t,
                "ym2203",
                0,
                0xA4,
                (4 << 3) | ((fnum >> 8) & 7),
            ));
            ws.push(ChipWrite::new(t, "ym2203", 0, 0xA0, fnum & 0xFF));
        }
        let mut vgm = synthetic(ws, CLOCK);
        vgm.total_samples = 2_000_000;
        vgm
    }

    #[test]
    fn test_vibrato_stays_on_row_steps() {
        let song = analyze(&long_vibrato(), AnalyzeOptions::default()).unwrap();
        let moving: Vec<&NoteEvent> = song
            .events
            .iter()
            .filter(|e| e.on && !e.fm_pitch_pts.is_empty())
            .collect();
        assert_eq!(moving.len(), 1);
        assert!(!moving[0].pitch_steps.is_empty());
        assert!(moving[0].pitch_steps.len() >= 1000);
        // A macro that fits the vibrato into 255 entries would turn the
        // 60 Hz wobble into a staircase; any macro left must run at speed 1.
        for patch in &song.fm_patches {
            let unique: BTreeSet<i32> = patch.pitch_macro.iter().copied().collect();
            if unique.len() > 1 {
                assert_eq!(patch.pitch_speed, 1, "{}", patch.name);
            }
        }
    }

    #[test]
    fn test_exported_pitch_follows_the_source() {
        skip_without_furnace!();
        let src = long_vibrato();
        let song = analyze(&src, AnalyzeOptions::default()).unwrap();
        let fur = tmp_dir().join("ym2203_vibrato.fur");
        std::fs::write(&fur, write_no_pcm(&song)).unwrap();
        let out = tmp_dir().join("ym2203_vibrato.vgm");
        export_vgm(&fur, &out).unwrap();
        let back = load_vgm(&out).unwrap();
        for ch in 0..9 {
            assert_eq!(
                pitch_runs(&src, &back, ch, 0.25, 60.0),
                Vec::new(),
                "ch{ch}: pitch diverged"
            );
        }
    }
}

#[allow(non_snake_case)]
mod TestYm2203Analyze {
    use super::*;

    #[test]
    fn test_fm_note_and_patch() {
        let song = analyze(&synthetic(fm_note(4, 1232), CLOCK), AnalyzeOptions::default())
            .unwrap();
        assert_eq!(song.fm_chip, "ym2203");
        assert_eq!(song.ym2203_ids, vec![0]);
        assert_eq!(song.fm_patches.len(), 1);
        let p = &song.fm_patches[0];
        assert!(p.opn);
        assert_eq!(p.alg, 7);
        assert_eq!(p.ops[0].dt_mul, 0x01);
        let ev = song.events.iter().find(|e| e.on).expect("event");
        assert_eq!((ev.ch, ev.note, ev.e5), (0, 115, 0x80 + 51));
        assert_eq!(ev.ins, 0);
        assert_eq!(ev.vol, -1);
    }

    #[test]
    fn test_key_off_event_and_end() {
        let song = analyze(
            &synthetic(fm_note_full(4, 1232, 100, 60_000, 0, 0, None, [0; 4], [0; 4]), CLOCK),
            AnalyzeOptions::default(),
        )
        .unwrap();
        let on = song.events.iter().find(|e| e.on).expect("event");
        assert_eq!(on.end, 60_000);
        let off: Vec<i64> = song
            .events
            .iter()
            .filter(|e| !e.on && e.ch == 0)
            .map(|e| e.sample)
            .collect();
        assert_eq!(off, vec![60_000]);
    }

    #[test]
    fn test_retrigger_off_is_dropped() {
        let mut ws = fm_note_full(4, 1232, 100, 0, 0, 0, None, [0; 4], [0; 4]);
        ws.extend([
            ChipWrite::new(5000, "ym2203", 0, 0x28, 0x00),
            ChipWrite::new(5001, "ym2203", 0, 0x28, 0xF0),
        ]);
        let song = analyze(&synthetic(ws, CLOCK), AnalyzeOptions::default()).unwrap();
        let offs: Vec<&NoteEvent> = song
            .events
            .iter()
            .filter(|e| !e.on && e.ch == 0)
            .collect();
        assert!(offs.is_empty());
    }

    #[test]
    fn test_mid_note_fnum_becomes_pitch_step() {
        let mut ws = fm_note_full(4, 1204, 100, 60_000, 0, 0, None, [0; 4], [0; 4]);
        ws.extend([
            ChipWrite::new(30000, "ym2203", 0, 0xA4, (4 << 3) | ((1250 >> 8) & 7)),
            ChipWrite::new(30000, "ym2203", 0, 0xA0, 1250 & 0xFF),
        ]);
        let song = analyze(
            &synthetic(ws, CLOCK),
            AnalyzeOptions {
                speed: Some(2),
                ..Default::default()
            },
        )
        .unwrap();
        let ev = song.events.iter().find(|e| e.on).expect("event");
        assert!(ev.fm_pitch_pts.len() >= 2);
        assert!(!ev.pitch_steps.is_empty());
        assert_eq!(ev.ins, 0);
        assert!(song.fm_patches[ev.ins as usize].pitch_macro.is_empty());
        let mod_ = parse_fur(&write_fur(
            &song,
            WriteOptions {
                include_pcm: false,
                include_fm: true,
                ..Default::default()
            },
        ))
        .unwrap();
        let mut fx: Vec<(u8, u8)> = Vec::new();
        for rows in mod_.patterns.values() {
            for row in rows {
                for &(cmd, value) in &row.fx {
                    fx.push((cmd, value));
                }
            }
        }
        assert!(
            fx.iter()
                .any(|(cmd, _v)| [0xE5u8, 0xE8, 0xE9].contains(cmd)),
            "{fx:?}"
        );
        // The step lands on the driver's tick of the row, not at its start.
        let (_row, delay) = song.place(30000, None);
        assert!(delay > 0, "pick a step inside a row for this test");
        assert!(fx.contains(&(0xEDu8, delay as u8)));
    }

    #[test]
    fn test_rising_carrier_level_opens_the_instrument() {
        // The driver keys the voice with quiet carriers and then walks them
        // louder while the note holds. The volume column can only attenuate,
        // so the instrument must be opened to the loudest level and the note
        // hung beneath it; otherwise the sustain stays at the key-on TL and
        // sounds far too quiet (Eternal Dream's first FM note).
        let mut ws: Vec<ChipWrite> =
            fm_note_full(4, 1204, 100, 60_000, 0, 0, None, [0; 4], [0; 4])
                .into_iter()
                .filter(|w| !(w.sample == 100 && w.reg == 0xB0))
                .collect();
        let at = ws
            .iter()
            .position(|w| w.sample == 100 && w.reg == 0x28)
            .expect("key-on");
        // alg 4: op3/op4 are the carriers. Park both at TL 0x28.
        ws.splice(
            at..at,
            [
                ChipWrite::new(100, "ym2203", 0, 0xB0, 0x34),
                ChipWrite::new(100, "ym2203", 0, 0x48, 0x28),
                ChipWrite::new(100, "ym2203", 0, 0x4C, 0x28),
            ],
        );
        ws.push(ChipWrite::new(30000, "ym2203", 0, 0x48, 0x14));
        let song = analyze(
            &synthetic(ws, CLOCK),
            AnalyzeOptions {
                speed: Some(2),
                ..Default::default()
            },
        )
        .unwrap();
        let ev = song.events.iter().find(|e| e.on).expect("event");
        let patch = &song.fm_patches[ev.ins as usize];
        assert_eq!(patch.alg, 4);
        assert_eq!(
            [patch.ops[2].tl & 0x7F, patch.ops[3].tl & 0x7F],
            [0x14, 0x14]
        );
        assert_eq!(ev.vol_tl_base, Some(0x14));
        assert_eq!(ev.vol, 107); // 127 - (0x28 - 0x14)
        assert!(ev.tl_pts.contains(&(30000, 0x14)));
        let mod_ = parse_fur(&write_fur(
            &song,
            WriteOptions {
                include_pcm: false,
                include_fm: true,
                ..Default::default()
            },
        ))
        .unwrap();
        let rows = mod_
            .patterns
            .get(&(0, mod_.orders[0][0]))
            .expect("pattern");
        let vols: Vec<i32> = rows.iter().filter(|r| r.vol != -1).map(|r| r.vol).collect();
        assert!(vols.contains(&107));
        assert!(vols.contains(&127));
    }

    #[test]
    fn test_long_pitch_sweep_becomes_pitch_macro() {
        let mut ws = fm_note_full(4, 1204, 100, 60_000, 0, 0, None, [0; 4], [0; 4]);
        for (i, fnum) in [1210u32, 1260].iter().cycle().take(12).enumerate() {
            let t = 30000 + i as i64 * 1000;
            ws.push(ChipWrite::new(
                t,
                "ym2203",
                0,
                0xA4,
                (4 << 3) | ((fnum >> 8) & 7),
            ));
            ws.push(ChipWrite::new(t, "ym2203", 0, 0xA0, fnum & 0xFF));
        }
        let song = analyze(&synthetic(ws, CLOCK), AnalyzeOptions::default()).unwrap();
        let ev = song.events.iter().find(|e| e.on).expect("event");
        assert!(ev.fm_pitch_pts.len() > 4);
        assert_ne!(ev.ins, 0);
        let macro_ = &song.fm_patches[ev.ins as usize].pitch_macro;
        assert!(!macro_.is_empty());
        assert_eq!(macro_[0], 0);
        assert!(*macro_.iter().max().unwrap() > 0);
    }

    #[test]
    fn test_ssg_note_lands_on_voice3() {
        let song = analyze(&synthetic(ssg_note_default(), CLOCK), AnalyzeOptions::default())
            .unwrap();
        assert_eq!(song.ym2203_ssg_count, 1);
        assert_eq!(song.ay_count, 1);
        let ev = song.events.iter().find(|e| e.on).expect("event");
        assert_eq!(ev.ch, 3);
        assert_eq!(ev.tone_chip, 2);
        assert_eq!(ev.tone_clock, CLOCK);
        assert_eq!(ev.tone_div, 16);
        // 3.072 MHz / (16 * 284) = 676.06 Hz.
        assert_almost_eq!(ay_rate(CLOCK, 284), CLOCK as f64 / (16.0 * 284.0));
        assert!(song.ay_ins[0].vol.is_empty());
    }

    #[test]
    fn test_dual_chip_channels() {
        let mut ws = fm_note_full(4, 1232, 100, 60_000, 0, 0, None, [0; 4], [0; 4]);
        ws.extend(fm_note_full(4, 923, 200, 60_000, 1, 0, None, [0; 4], [0; 4]));
        ws.extend(ssg_note(300, 60_000, 1));
        let song = analyze(&synthetic(ws, CLOCK), AnalyzeOptions::default()).unwrap();
        assert_eq!(song.ym2203_ids, vec![0, 1]);
        let mut chs: Vec<i32> = song
            .events
            .iter()
            .filter(|e| e.on)
            .map(|e| e.ch)
            .collect();
        chs.sort();
        assert_eq!(chs, vec![0, 6, 9]);
        assert_eq!(song.ym2203_ssg_count, 1);
    }

    #[test]
    fn test_ssg_uses_the_same_ay_effects() {
        let mut ws = ssg_note_default();
        // envelope mode + shape write become 20xx/22xy on the PSG channel
        ws.push(ChipWrite::new(20000, "ym2203_ssg", 0, 8, 0x1F));
        ws.push(ChipWrite::new(20000, "ym2203_ssg", 0, 13, 0x0D));
        let song = analyze(&synthetic(ws, CLOCK), AnalyzeOptions::default()).unwrap();
        assert!(song
            .events
            .iter()
            .filter(|e| e.on)
            .any(|e| e.tone_fx_pts.iter().any(|&(_t, cmd, _v)| cmd == 0x22)));
    }
}

#[allow(non_snake_case)]
mod TestYm2203Writer {
    use super::*;

    fn song(ws: Vec<ChipWrite>) -> Song {
        analyze(&synthetic(ws, CLOCK), AnalyzeOptions::default()).unwrap()
    }

    #[test]
    fn test_single_chip_layout_and_ids() {
        let data = write_no_pcm(&song(fm_note(4, 1232)));
        let m = parse_fur(&data).unwrap();
        assert_eq!(m.chips, vec![CHIP_YM2203]);
        assert_eq!(m.n_ch, 6);
        assert_eq!(chip_channels(CHIP_YM2203), 6);
        assert!(has(&data, b"FM 1-1"));
        assert!(has(&data, b"PSG 1-1"));
        assert!(has(&data, b"customClock=3072000"));
        assert_eq!(ins_types(&data)[0], INS_FM);
    }

    #[test]
    fn test_opn_operator_fields() {
        let sr = [0x12u32, 0x13, 0x14, 0x15];
        let ssg = [0x05u32, 0x06, 0x07, 0x08];
        let ws = fm_note_full(4, 1232, 100, 60_000, 0, 0, None, sr, ssg);
        let data = write_no_pcm(&song(ws));
        let feat = fm_feature(&data);
        assert!(!feat.is_empty());
        assert_eq!(feat[3] & 0x3F, 0x20); // 4-op bit
        for op in 0..4usize {
            let base = 4 + op * 8;
            assert_eq!(feat[base + 4] & 0x1F, (0x12 + op) as u8);
            assert_eq!(feat[base + 6] & 0x0F, (0x05 + op) as u8);
        }
    }

    #[test]
    fn test_ssg_note_gets_an_ay_instrument() {
        let data = write_no_pcm(&song(ssg_note_default()));
        let m = parse_fur(&data).unwrap();
        assert_eq!(m.chips, vec![CHIP_YM2203]);
        let row = m.patterns[&(3, 0)]
            .iter()
            .find(|r| r.note >= 0)
            .expect("note row");
        assert_eq!(row.ins, 0);
        assert_eq!(ins_types(&data), vec![INS_AY]);
    }

    #[test]
    fn test_dual_chip_layout() {
        let mut ws = fm_note_full(4, 1232, 100, 60_000, 0, 0, None, [0; 4], [0; 4]);
        ws.extend(fm_note_full(4, 923, 200, 60_000, 1, 0, None, [0; 4], [0; 4]));
        let data = write_no_pcm(&song(ws));
        let m = parse_fur(&data).unwrap();
        assert_eq!(m.chips, vec![CHIP_YM2203, CHIP_YM2203]);
        assert_eq!(m.n_ch, 12);
        assert!(has(&data, b"FM 2-1"));
        assert!(has(&data, b"PSG 2-1"));
        // both notes share one OPN FM instrument
        assert_eq!(ins_types(&data), vec![INS_FM]);
    }

    #[test]
    fn test_pitch_macro_rides_on_an_instrument_copy() {
        let mut ws = fm_note_full(4, 1204, 100, 60_000, 0, 0, None, [0; 4], [0; 4]);
        for (i, fnum) in [1210u32, 1260].iter().cycle().take(12).enumerate() {
            let t = 30000 + i as i64 * 1000;
            ws.push(ChipWrite::new(
                t,
                "ym2203",
                0,
                0xA4,
                (4 << 3) | ((fnum >> 8) & 7),
            ));
            ws.push(ChipWrite::new(t, "ym2203", 0, 0xA0, fnum & 0xFF));
        }
        let song = song(ws);
        let ev = song.events.iter().find(|e| e.on).expect("event");
        assert!(!song.fm_patches[ev.ins as usize].pitch_macro.is_empty());
        let data = write_no_pcm(&song);
        assert!(count_pat(&data, b"INS2") >= 2);
    }
}

#[allow(non_snake_case)]
mod TestYm2203SpecialCases {
    use super::*;

    #[test]
    fn test_3slot_pitch_uses_per_operator_registers() {
        let mut ws = make_patch(100, 0, [0; 4], [0; 4]);
        ws.extend([
            ChipWrite::new(100, "ym2203", 0, 0x27, 0x40), // 3-slot / CSM mode
            ChipWrite::new(100, "ym2203", 0, 0xAC, 0x22), // op2 block 4, fnum high 2
            ChipWrite::new(100, "ym2203", 0, 0xA8, 0x05), // op2 fnum 0x205
            ChipWrite::new(100, "ym2203", 0, 0xA6, 0x01), // normal fnum, deliberately
            ChipWrite::new(100, "ym2203", 0, 0xA2, 0x05), // a different pitch
            // configure channel 3's own operators (MULT=1, alg 7) so the
            // carrier-octave normaliser does not see the all-zero reset patch
            ChipWrite::new(100, "ym2203", 0, 0x32, 0x01),
            ChipWrite::new(100, "ym2203", 0, 0x36, 0x01),
            ChipWrite::new(100, "ym2203", 0, 0x3A, 0x01),
            ChipWrite::new(100, "ym2203", 0, 0x3E, 0x01),
            ChipWrite::new(100, "ym2203", 0, 0xB2, 0x07),
            ChipWrite::new(100, "ym2203", 0, 0x28, 0xF2),
            ChipWrite::new(2000, "ym2203", 0, 0x28, 0xF2), // same mask: no retrigger
            ChipWrite::new(60000, "ym2203", 0, 0x28, 0x02),
        ]);
        let song = analyze(&synthetic(ws, CLOCK), AnalyzeOptions::default()).unwrap();
        let evs: Vec<&NoteEvent> = song.events.iter().filter(|e| e.on).collect();
        assert_eq!(evs.len(), 4);
        // The ext column that carries operator 0's frequency is module
        // channel 4 (column 2; Furnace's opChanOffs order is 1,2,0,3).
        let ev = evs.iter().find(|e| e.ch == 4).expect("ch4");
        assert_eq!((ev.note, ev.e5), opn_note_for(4, 0x205, CLOCK));
        assert!(song.ym2203_ext);
        assert!(!song.warnings.join(" ").contains("special-mode"));
        assert!(evs.iter().all(|e| [2, 3, 4, 5].contains(&e.ch)));
        let mod_ = parse_fur(&write_fur(&song, WriteOptions::default())).unwrap();
        assert_eq!(mod_.chips, vec![CHIP_YM2203_EXT]);
        assert_eq!(mod_.n_ch, 9);
    }

    #[test]
    fn test_mixed_carrier_labels_without_moving_the_audio() {
        // alg 5, carrier op2 MULT=2, other carriers MULT=1: mixed.  The
        // loudest carrier sets the note label (A#4), while the compensating
        // pitch macro keeps the source channel f-num/audio exactly.  A mixed
        // patch must not transpose the whole channel: that made the 1x
        // carriers (and, on other patches, the loudest carrier) an octave
        // high.
        let mut ws: Vec<ChipWrite> = make_patch(100, 0, [0; 4], [0; 4])
            .into_iter()
            .filter(|w| w.reg != 0xB0 && w.reg != 0x34)
            .collect();
        ws.extend([
            ChipWrite::new(100, "ym2203", 0, 0xB0, 0x05), // alg 5
            ChipWrite::new(100, "ym2203", 0, 0x34, 0x02), // op2 MULT=2
            ChipWrite::new(100, "ym2203", 0, 0xA4, 0x25), // block 4, fnum 0x5BA
            ChipWrite::new(100, "ym2203", 0, 0xA0, 0xBA),
            ChipWrite::new(100, "ym2203", 0, 0x28, 0xF0),
            ChipWrite::new(60000, "ym2203", 0, 0x28, 0x00),
        ]);
        let clock = 1_500_000u32;
        let song = analyze(&synthetic(ws, clock), AnalyzeOptions::default()).unwrap();
        let (note, e5) = opn_note_for(4, 1466, clock);
        let ev = song.events.iter().find(|e| e.on).expect("event");
        assert_eq!((ev.note, ev.e5), (note + 12, e5));
        let patch = &song.fm_patches[ev.ins as usize];
        let mut mults: Vec<u8> = [1usize, 2, 3]
            .iter()
            .map(|&i| patch.ops[i].dt_mul & 0x0F)
            .collect();
        mults.sort();
        assert_eq!(mults, vec![1, 1, 2]);
        assert_eq!(patch.pitch_macro, vec![-1536]);
        assert_eq!(furnace_note_name(ev.note), "A#4");
    }

    #[test]
    fn test_loudest_carrier_sets_the_label() {
        // alg 4, carriers S2 (0x38) MULT=4 and S4 (0x3C) MULT=2.  The 2x
        // carrier is 3 dB louder (lower TL), so it is the audible lead: the
        // label moves one octave, not two, and the macro keeps the source
        // f-num.
        let mut ws: Vec<ChipWrite> = make_patch(100, 0, [0; 4], [0; 4])
            .into_iter()
            .filter(|w| ![0xB0, 0x34, 0x38, 0x3C, 0x44, 0x48, 0x4C].contains(&w.reg))
            .collect();
        ws.extend([
            ChipWrite::new(100, "ym2203", 0, 0xB0, 0x04), // alg 4
            ChipWrite::new(100, "ym2203", 0, 0x38, 0x04), // S2 MULT=4
            ChipWrite::new(100, "ym2203", 0, 0x3C, 0x02), // S4 MULT=2
            ChipWrite::new(100, "ym2203", 0, 0x48, 0x14), // S2 TL 0x14 (quieter)
            ChipWrite::new(100, "ym2203", 0, 0x4C, 0x11), // S4 TL 0x11 (louder)
            ChipWrite::new(100, "ym2203", 0, 0xA4, 0x25),
            ChipWrite::new(100, "ym2203", 0, 0xA0, 0xBA),
            ChipWrite::new(100, "ym2203", 0, 0x28, 0xF0),
            ChipWrite::new(60000, "ym2203", 0, 0x28, 0x00),
        ]);
        let clock = 1_500_000u32;
        let song = analyze(&synthetic(ws, clock), AnalyzeOptions::default()).unwrap();
        let (note, e5) = opn_note_for(4, 1466, clock);
        let ev = song.events.iter().find(|e| e.on).expect("event");
        assert_eq!((ev.note, ev.e5), (note + 12, e5));
        assert_eq!(song.fm_patches[ev.ins as usize].pitch_macro, vec![-1536]);
    }

    #[test]
    fn test_non_octave_loudest_carrier_is_not_relabelled() {
        // The loudest carrier is MULT=3; there is no whole-octave label, so
        // the note stays at the channel f-num and no macro is added.
        let mut ws: Vec<ChipWrite> = make_patch(100, 0, [0; 4], [0; 4])
            .into_iter()
            .filter(|w| ![0xB0, 0x34, 0x44, 0x48, 0x4C].contains(&w.reg))
            .collect();
        ws.extend([
            ChipWrite::new(100, "ym2203", 0, 0xB0, 0x05), // alg 5
            ChipWrite::new(100, "ym2203", 0, 0x34, 0x03), // op2 MULT=3 (loudest)
            ChipWrite::new(100, "ym2203", 0, 0x44, 0x00), // op2 TL 0
            ChipWrite::new(100, "ym2203", 0, 0x48, 0x20), // other carriers quieter
            ChipWrite::new(100, "ym2203", 0, 0x4C, 0x20),
            ChipWrite::new(100, "ym2203", 0, 0xA4, 0x25),
            ChipWrite::new(100, "ym2203", 0, 0xA0, 0xBA),
            ChipWrite::new(100, "ym2203", 0, 0x28, 0xF0),
            ChipWrite::new(60000, "ym2203", 0, 0x28, 0x00),
        ]);
        let clock = 1_500_000u32;
        let song = analyze(&synthetic(ws, clock), AnalyzeOptions::default()).unwrap();
        let (note, e5) = opn_note_for(4, 1466, clock);
        let ev = song.events.iter().find(|e| e.on).expect("event");
        assert_eq!((ev.note, ev.e5), (note, e5));
        assert!(song.fm_patches[ev.ins as usize].pitch_macro.is_empty());
    }

    #[test]
    fn test_uniform_carrier_keeps_the_channel_octave() {
        // All three alg-5 carriers are MULT=2, so the channel already sounds
        // an octave up.  The pattern note moves to C-4/A#4 etc., while a
        // constant -1536 pitch macro preserves the source f-num/audio.
        let mut ws: Vec<ChipWrite> = make_patch(100, 0, [0; 4], [0; 4])
            .into_iter()
            .filter(|w| ![0xB0, 0x34, 0x38, 0x3C].contains(&w.reg))
            .collect();
        ws.extend([
            ChipWrite::new(100, "ym2203", 0, 0xB0, 0x05), // alg 5
            ChipWrite::new(100, "ym2203", 0, 0x34, 0x02), // op2 MULT=2
            ChipWrite::new(100, "ym2203", 0, 0x38, 0x02), // op3 MULT=2
            ChipWrite::new(100, "ym2203", 0, 0x3C, 0x02), // op4 MULT=2
            ChipWrite::new(100, "ym2203", 0, 0xA4, 0x25),
            ChipWrite::new(100, "ym2203", 0, 0xA0, 0xBA),
            ChipWrite::new(100, "ym2203", 0, 0x28, 0xF0),
            ChipWrite::new(60000, "ym2203", 0, 0x28, 0x00),
        ]);
        let clock = 1_500_000u32;
        let song = analyze(&synthetic(ws, clock), AnalyzeOptions::default()).unwrap();
        let (note, e5) = opn_note_for(4, 1466, clock);
        let ev = song.events.iter().find(|e| e.on).expect("event");
        assert_eq!((ev.note, ev.e5), (note + 12, e5));
        let patch = &song.fm_patches[ev.ins as usize];
        assert_eq!(patch.pitch_macro, vec![-1536]);
    }

    #[test]
    fn test_fm_fallback_pitch_tick_scale() {
        let ws: Vec<ChipWrite> = (0..13u32)
            .map(|i| ChipWrite::new(100 + i as i64 * 300, "ym2203", 0, 0xA0, i))
            .collect();
        let song = analyze(&synthetic(ws, CLOCK), AnalyzeOptions::default()).unwrap();
        assert_eq!(song.hz, 180.0);
        assert_eq!(song.speed, 3);
        assert!((44100.0 * song.speed as f64 / song.hz - 735.0).abs() <= 5.0);
    }

    #[test]
    fn test_ssg_fast_period_tick_scale() {
        let mut ws = ssg_note(100, 60_000, 0);
        ws.extend(
            (0..15u32).map(|i| {
                ChipWrite::new(150 + i as i64 * 50, "ym2203_ssg", 0, 0, 0x1C - i)
            }),
        );
        let song = analyze(&synthetic(ws, CLOCK), AnalyzeOptions::default()).unwrap();
        assert_eq!(song.hz, 900.0);
        assert_eq!(song.speed, 15);
        assert!((44100.0 * song.speed as f64 / song.hz - 735.0).abs() <= 5.0);
    }
}

#[allow(non_snake_case)]
mod TestYm2203Diag {
    use super::*;

    #[test]
    fn test_ssg_stream_skips_inaudible_period() {
        let mut ws = ssg_note(100, 60_000, 0);
        ws.extend([
            ChipWrite::new(20000, "ym2203_ssg", 0, 1, 0x00),
            ChipWrite::new(20001, "ym2203_ssg", 0, 0, 0x00), // period 0
        ]);
        let stream = opn_pitch_stream(&synthetic(ws, CLOCK), 3);
        assert!(!stream.is_empty());
        // period 0 would map to the maximum pitch unit (0); the converter
        // treats it as silence and never emits it
        assert!(!stream.iter().any(|&(_t, u)| u == 0.0));
    }

    #[test]
    fn test_pitch_stream_maps_channels() {
        let mut ws = fm_note(4, 1232);
        ws.extend(fm_note_full(4, 923, 200, 60_000, 1, 0, None, [0; 4], [0; 4]));
        let src = synthetic(ws, CLOCK);
        assert_eq!(opn_pitch_stream(&src, 6).len(), 1);
        assert_eq!(opn_pitch_stream(&src, 3), Vec::new()); // SSG voice
        assert_almost_eq!(
            opn_pitch_stream(&src, 0)[0].1,
            opn_pitch_units(4, 1232)
        );
    }

    #[test]
    fn test_pitch_runs_use_opn_units() {
        // a later unchanged f-num write gives pitch_runs an end time
        fn with_hold(block: u32, fnum: u32, mut ws: Vec<ChipWrite>) -> Vec<ChipWrite> {
            ws.push(ChipWrite::new(
                50000,
                "ym2203",
                0,
                0xA4,
                (block << 3) | ((fnum >> 8) & 7),
            ));
            ws.push(ChipWrite::new(50000, "ym2203", 0, 0xA0, fnum & 0xFF));
            ws
        }

        let a = synthetic(
            with_hold(4, 1204, fm_note_full(4, 1204, 100, 60_000, 0, 0, None, [0; 4], [0; 4])),
            CLOCK,
        );
        let b = synthetic(
            with_hold(4, 1232, fm_note_full(4, 1232, 100, 60_000, 0, 0, None, [0; 4], [0; 4])),
            CLOCK,
        );
        let runs = pitch_runs(&a, &b, 0, 0.2, 10.0);
        assert!(!runs.is_empty());
        assert!(runs[0].worst_st.abs() > 0.3);
    }
}

#[allow(non_snake_case)]
mod TestYm2203Furnace {
    use super::*;

    fn export(song: &Song, name: &str) -> VgmFile {
        let fur = tmp_dir().join(format!("{name}.fur"));
        let out = tmp_dir().join(format!("{name}.vgm"));
        std::fs::write(&fur, write_no_pcm(song)).unwrap();
        export_vgm(&fur, &out).unwrap();
        load_vgm(&out).unwrap()
    }

    #[test]
    fn test_note_pitch_round_trip() {
        skip_without_furnace!();
        let song = analyze(&synthetic(fm_note(4, 1232), CLOCK), AnalyzeOptions::default())
            .unwrap();
        let vgm = export(&song, "ym2203_note");
        assert_eq!(vgm.ym2203_clock, CLOCK);
        // Furnace writes the high byte then the low one; pair on the low byte.
        let mut pairs: Vec<(u32, u32)> = Vec::new();
        let mut lo = 0u32;
        let mut hi = 0u32;
        for w in &vgm.writes {
            if w.chip != "ym2203" || w.chip_id != 0 {
                continue;
            }
            if (0xA0..=0xA2).contains(&w.reg) {
                lo = w.val;
                pairs.push(((hi >> 3) & 7, ((hi & 7) << 8) | lo));
            } else if (0xA4..=0xA6).contains(&w.reg) {
                hi = w.val;
            }
        }
        assert!(pairs.contains(&(4, 1232)));
    }

    #[test]
    fn test_sr_and_ssg_eg_reach_the_chip() {
        skip_without_furnace!();
        let sr = [0x12u32, 0x13, 0x14, 0x15];
        let ssg = [0x05u32, 0x06, 0x07, 0x08];
        let song = analyze(
            &synthetic(fm_note_full(4, 1232, 100, 60_000, 0, 0, None, sr, ssg), CLOCK),
            AnalyzeOptions::default(),
        )
        .unwrap();
        let vgm = export(&song, "ym2203_ops");
        let mut reg: BTreeMap<u32, BTreeSet<u32>> = BTreeMap::new();
        for w in &vgm.writes {
            if w.chip == "ym2203" && w.chip_id == 0 {
                reg.entry(w.reg).or_default().insert(w.val);
            }
        }
        for op in 0..4u32 {
            let sr_vals: BTreeSet<u32> =
                reg[&(0x70 + op * 4)].iter().map(|v| v & 0x1F).collect();
            assert!(sr_vals.contains(&(0x12 + op)));
            let ssg_vals: BTreeSet<u32> =
                reg[&(0x90 + op * 4)].iter().map(|v| v & 0x0F).collect();
            assert!(ssg_vals.contains(&(0x05 + op)));
        }
    }

    #[test]
    fn test_ssg_period_round_trip() {
        skip_without_furnace!();
        let song = analyze(&synthetic(ssg_note_default(), CLOCK), AnalyzeOptions::default())
            .unwrap();
        let vgm = export(&song, "ym2203_ssg");
        let mut reg: BTreeMap<u32, BTreeSet<u32>> = BTreeMap::new();
        for w in &vgm.writes {
            if w.chip == "ym2203_ssg" && w.chip_id == 0 {
                reg.entry(w.reg).or_default().insert(w.val);
            }
        }
        assert!(reg.get(&0).is_some_and(|s| s.contains(&0x1C)));
        assert!(reg.get(&1).is_some_and(|s| s.contains(&0x01)));
        assert!(reg.get(&8).is_some_and(|s| s.contains(&0x0F)));
    }
}

#[allow(non_snake_case)]
mod TestYm2203OctaveConvention {
    use super::*;

    #[test]
    fn test_source_c3_maps_to_furnace_c3() {
        assert_eq!(opn_note_fnum(96, 1_500_000), (3, 1646));
        assert_eq!(opn_note_for(3, 1646, 1_500_000), (96, 0x80));
        assert_eq!(furnace_note_name(96), "C-3");
    }

    #[test]
    fn test_carrier_mult_does_not_transpose_the_channel_note() {
        // Legendary Wings Area 1 (OPN 00) has alg 5 and carrier op2 MULT=2.
        // The source channel is still block 3 / f-num 2933: A#3, with an
        // octave-above carrier component in the patch.
        assert_eq!(opn_note_fnum(106, 1_500_000), (3, 2933));
        assert_eq!(furnace_note_name(106), "A#3");
    }
}
