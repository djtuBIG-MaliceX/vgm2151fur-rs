#!/usr/bin/env rust
//! OPL operator macros: a scripted modulator level must reach the chip.
//!
//! Port of `tests/test_opl_operator_macro.py`.
//!
//! Furnace rebuilds an OPL voice from its instrument at key-on and scales only
//! an operator marked KVS with the channel volume. A driver that scripts a
//! modulator's `$4n` therefore cannot be followed by the volume column at all:
//! the voice used to play one colour where the log moved it, and the
//! conversion looked perfect at every key-on because only the sustained part
//! differed.
//!
//! The route is a per-operator macro: feature `O1`..`O4`, code 6
//! (DIV_MACRO_OP_TL). Measured end to end through the bundled Furnace build,
//! the chip receives `$4n = 63 - value`. Furnace stores a *level* (up is
//! louder) while the log holds attenuation, and the writer inverts. A macro
//! runs from the key-on and has no delay of its own. Entry zero is the attack
//! level, and a move that starts several ticks in costs that many repeated
//! entries, which is why the curve is quantized to a `speed` rather than being
//! sampled per tick.

mod common;
#[allow(unused_imports)]
use common::*;

use std::collections::BTreeSet;
use std::path::PathBuf;

use indexmap::IndexMap;

use vgm2fur::analyze::{analyze, AnalyzeOptions};
use vgm2fur::furio::{decompress_fur, iter_blocks, parse_fur};
use vgm2fur::furwrite::{write_fur, WriteOptions};
use vgm2fur::model::Song;
use vgm2fur::vgm::{load_vgm, ChipWrite, VgmFile};

const CH: i32 = 2;
const MOD: u32 = 0x02;
const CAR: u32 = 0x05; // ch2's op1 (modulator) and op2 (carrier) slots
const KEY_ON: i64 = 60_000;

fn opl_vgm(writes: Vec<ChipWrite>) -> VgmFile {
    let mut writes = writes;
    writes.sort_by_key(|w| w.sample);
    VgmFile {
        path: PathBuf::from("synthetic.vgm"),
        buf: Vec::new(),
        version: 0x151,
        total_samples: 400_000,
        loop_samples: 0,
        loop_file_pos: None,
        loop_sample: None,
        ym2151_clock: 0,
        k007232_clock: 0,
        gd3: Default::default(),
        roms: Vec::new(),
        writes,
        opl_clock: 0,
        opl2_clock: 3_579_545,
        opl3_clock: 0,
        ..VgmFile::default()
    }
}

fn wr(sample: i64, reg: u32, val: u32) -> ChipWrite {
    ChipWrite::new(sample, "opl2", 0, reg, val)
}

fn voice(mod_tl: u32) -> Vec<ChipWrite> {
    // A heartbeat plus a held ch2 voice whose modulator level walks a ramp.
    let mut w: Vec<ChipWrite> = Vec::new();
    for k in 1..40i64 {
        // key-on lattice on channel 5
        w.push(wr(k * 5000, 0xA0 | 5, 0x57));
        w.push(wr(k * 5000, 0xB0 | 5, 0x35));
        w.push(wr(k * 5000 + 20, 0xB0 | 5, 0x15));
    }
    for base in [MOD, CAR] {
        w.push(wr(3000, 0x20 | base, 0x01));
        w.push(wr(
            3000,
            0x40 | base,
            if base == MOD { mod_tl } else { 21 },
        ));
        w.push(wr(3000, 0x60 | base, 0xF8));
        w.push(wr(3000, 0x80 | base, 0xF4));
        w.push(wr(3000, 0xE0 | base, 0x00));
    }
    w.push(wr(3000, 0xC0 | CH as u32, 0x0A)); // algorithm 0: op2 is the carrier
    w.push(wr(KEY_ON, 0xA0 | CH as u32, 0x57));
    w.push(wr(KEY_ON + 2, 0xB0 | CH as u32, 0x31));
    w
}

const RAMP: [i32; 10] = [3, 7, 11, 15, 19, 23, 27, 31, 35, 39];

fn song_vgm(script_mod: bool) -> VgmFile {
    let mut w = voice(25);
    if script_mod {
        for (i, &v) in RAMP.iter().enumerate() {
            // one step every ~5 ticks
            w.push(wr(KEY_ON + 500 + i as i64 * 460, 0x40 | MOD, v as u32));
        }
    } else {
        // same script on the carrier
        for (i, &v) in RAMP.iter().enumerate() {
            w.push(wr(KEY_ON + 500 + i as i64 * 460, 0x40 | CAR, v as u32));
        }
    }
    w.push(wr(KEY_ON + 90_000, 0xB0 | CH as u32, 0x11));
    opl_vgm(w)
}

fn analyze_opl(vgm: &VgmFile) -> Song {
    analyze(
        vgm,
        AnalyzeOptions {
            pcm: false,
            ..AnalyzeOptions::default()
        },
    )
    .expect("analyze")
}

/// `{feature tag: (code, len, delay, speed, values)}` from a raw .fur.
fn macro_features(data: &[u8]) -> IndexMap<String, (i32, i32, i32, i32, Vec<i32>)> {
    let mut out: IndexMap<String, (i32, i32, i32, i32, Vec<i32>)> = IndexMap::new();
    let dec = decompress_fur(data);
    for blk in iter_blocks(&dec).expect("blocks") {
        if &blk.magic != b"INS2" {
            continue;
        }
        let blk = blk.payload;
        let mut pos = 4usize;
        while pos + 4 <= blk.len() {
            let tag = &blk[pos..pos + 2];
            if tag == b"EN" {
                break;
            }
            let sz =
                u16::from_le_bytes([blk[pos + 2], blk[pos + 3]]) as usize;
            let body_end = (pos + 4 + sz).min(blk.len());
            let body = &blk[pos + 4..body_end];
            if tag[0] == b'O' && body.len() > 10 {
                let code = body[2] as i32;
                let ln = body[3] as i32;
                let delay = body[8] as i32;
                let speed = body[9] as i32;
                let values: Vec<i32> = body[10..10 + ln as usize]
                    .iter()
                    .map(|&b| b as i32)
                    .collect();
                out.insert(
                    String::from_utf8_lossy(tag).into_owned(),
                    (code, ln, delay, speed, values),
                );
            }
            pos += 4 + sz;
        }
    }
    out
}

fn setup() -> (Song, usize) {
    let song = analyze_opl(&song_vgm(true));
    let idx = song
        .events
        .iter()
        .position(|e| e.on && e.ch == CH)
        .expect("first note event");
    (song, idx)
}

mod TestOperatorMacroCapture {
    use super::*;

    #[test]
    fn test_modulator_curve_is_captured_per_operator() {
        let (song, idx) = setup();
        let ev = &song.events[idx];
        let got: IndexMap<i32, Vec<(i64, i32)>> = ev.opl_op_tl.iter().cloned().collect();
        assert!(
            got.contains_key(&0),
            "the modulator's levels were never recorded"
        );
        let levels: Vec<i32> = got[&0].iter().map(|(_s, v)| *v).collect();
        // Entry zero is the level the note attacks with; the ramp follows.
        let mut expected = vec![25];
        expected.extend_from_slice(&RAMP);
        assert_eq!(levels, expected);
    }

    #[test]
    fn test_macro_targets_the_modulator_with_absolute_levels() {
        let (song, idx) = setup();
        let ev = &song.events[idx];
        let patch = &song.fm_patches[ev.ins as usize];
        let entries: IndexMap<i32, (&Vec<i32>, i32)> = patch
            .op_tl_macro
            .iter()
            .map(|(oi, values, speed)| (*oi, (values, *speed)))
            .collect();
        assert!(entries.contains_key(&0));
        assert!(
            !entries.contains_key(&1),
            "the carrier rides the volume column"
        );
        let (values, _speed) = entries[&0];
        // A macro starts with the note and cannot wait: entry 0 is the level the
        // voice attacks on, and the first logged move follows it.
        assert_eq!(values[0], 25, "entry zero must be the attack level");
        let seen: Vec<i32> = values.clone();
        let seen_ramp: Vec<i32> = seen.iter().copied().filter(|v| RAMP.contains(v)).collect();
        assert_eq!(
            seen_ramp, RAMP,
            "the logged steps must arrive in order, unturned"
        );
    }

    #[test]
    fn test_file_stores_levels_in_furnaces_domain_and_no_delay() {
        let (song, _idx) = setup();
        let data = write_fur(
            &song,
            WriteOptions {
                include_pcm: false,
                include_fm: true,
                ..WriteOptions::default()
            },
        );
        let feats = macro_features(&data);
        let keys: Vec<&str> = feats.keys().map(|s| s.as_str()).collect();
        assert!(feats.contains_key("O1"), "{keys:?}");
        assert!(!feats.contains_key("O2"));
        let (code, ln, delay, speed, values) = &feats["O1"];
        assert_eq!(*code, 6, "DIV_MACRO_OP_TL");
        assert_eq!(*delay, 0, "an operator macro has no delay concept");
        assert!(*speed >= 1);
        assert_eq!(*ln as usize, values.len());
        // byte = 127 - $4n, so the editor reads back exactly the logged level.
        assert_eq!(values[0], 127 - 25, "entry zero is the attack level");
        let filtered: Vec<i32> = values
            .iter()
            .copied()
            .filter(|v| RAMP.contains(&(127 - v)))
            .collect();
        let expected: Vec<i32> = RAMP.iter().map(|t| 127 - t).collect();
        assert_eq!(filtered, expected);
    }

    #[test]
    fn test_editor_visible_level_stays_in_the_opl_six_bit_range() {
        // What the UI shows for an OPL operator TL macro must be a $4n.
        //
        // The file byte is Furnace's level domain (`$4n = 63 - (byte & 63)`,
        // editor shows `127 - byte`), so the test checks the number a person
        // reads in the instrument editor, not the raw byte.
        let (song, _idx) = setup();
        let data = write_fur(
            &song,
            WriteOptions {
                include_pcm: false,
                include_fm: true,
                ..WriteOptions::default()
            },
        );
        let mut bad: Vec<(String, i32, i32)> = Vec::new();
        for (tag, (_code, _ln, _delay, _speed, values)) in macro_features(&data) {
            for v in values {
                let shown = 127 - v;
                if !(0..=63).contains(&shown) {
                    bad.push((tag.clone(), v, shown));
                }
            }
        }
        assert!(
            bad.is_empty(),
            "editor levels outside $4n range: {:?}",
            &bad[..bad.len().min(6)]
        );
    }

    #[test]
    fn test_carrier_script_stays_on_the_volume_column() {
        let song = analyze_opl(&song_vgm(false));
        for p in &song.fm_patches {
            for (oi, _vals, _s) in &p.op_tl_macro {
                assert_ne!(*oi, 1, "a carrier level rides the volume column, not a macro");
            }
        }
    }

    #[test]
    fn test_macro_lives_on_a_copy_so_plain_notes_keep_their_instrument() {
        let (song, idx) = setup();
        let ev = &song.events[idx];
        let patch = &song.fm_patches[ev.ins as usize];
        assert!(patch.name.contains("+lvl"));
        let plain = song.fm_patches.iter().filter(|p| p.op_tl_macro.is_empty());
        assert!(
            plain.count() > 0,
            "the instrument without a macro must survive"
        );
    }

    #[test]
    fn test_a_per_note_modulator_level_is_a_row_effect() {
        // A single level per note stays on the shared instrument.
        //
        // The loudest level is the instrument's modulator TL; the quieter notes
        // carry 12xx (operator 1 level) instead of one instrument copy each.
        let mut w: Vec<ChipWrite> = Vec::new();
        for k in 1..40i64 {
            w.push(wr(k * 5000, 0xA0 | 5, 0x57));
            w.push(wr(k * 5000, 0xB0 | 5, 0x35));
            w.push(wr(k * 5000 + 20, 0xB0 | 5, 0x15));
        }
        for (k, tl) in [3u32, 7, 11, 15].into_iter().enumerate() {
            let t = 30_000 + k as i64 * 20_000;
            for base in [MOD, CAR] {
                w.push(wr(t, 0x20 | base, 0x01));
                w.push(wr(t, 0x40 | base, if base == MOD { tl } else { 21 }));
                w.push(wr(t, 0x60 | base, 0xF8));
                w.push(wr(t, 0x80 | base, 0xF4));
                w.push(wr(t, 0xE0 | base, 0x00));
            }
            w.push(wr(t, 0xC0 | CH as u32, 0x0A));
            w.push(wr(t + 100, 0xA0 | CH as u32, 0x57));
            w.push(wr(t + 100, 0xB0 | CH as u32, 0x31));
            w.push(wr(t + 9000, 0xB0 | CH as u32, 0x11));
        }
        let song = analyze_opl(&opl_vgm(w));
        let evs: Vec<_> = song
            .events
            .iter()
            .filter(|e| e.on && e.ch == CH)
            .collect();
        assert!(evs.iter().all(|e| e.ins == evs[0].ins));
        let fx: Vec<i32> = evs
            .iter()
            .filter(|e| !e.op_tl_fx.is_empty())
            .map(|e| e.op_tl_fx[0].2)
            .collect();
        assert_eq!(fx, vec![7, 11, 15]);
        let m = parse_fur(&write_fur(
            &song,
            WriteOptions {
                include_pcm: false,
                ..WriteOptions::default()
            },
        ))
        .expect("parse");
        let mut coded: BTreeSet<u8> = BTreeSet::new();
        for order in &m.orders[CH as usize] {
            if let Some(rows) = m.patterns.get(&(CH as u16, *order)) {
                for row in rows {
                    for (code, val) in &row.fx {
                        if *code == 0x12 {
                            coded.insert(*val);
                        }
                    }
                }
            }
        }
        let expected: BTreeSet<u8> = [7u8, 11, 15].into_iter().collect();
        assert!(
            expected.is_subset(&coded),
            "{:?}",
            coded.iter().collect::<Vec<_>>()
        );
    }
}

mod TestOperatorMacroPlayback {
    use super::*;

    #[test]
    fn test_played_modulator_follows_the_ramp() {
        // The real player must put the logged `$4n` back on the chip.
        skip_without_furnace!();
        let song = analyze_opl(&song_vgm(true));
        let td = tempfile::tempdir().expect("temp dir");
        let fur = td.path().join("m.fur");
        std::fs::write(
            &fur,
            write_fur(
                &song,
                WriteOptions {
                    include_pcm: false,
                    include_fm: true,
                    ..WriteOptions::default()
                },
            ),
        )
        .unwrap();
        let out = td.path().join("m.vgm");
        vgm2fur::furnace::export_vgm(&fur, &out).expect("export");
        let got: Vec<i32> = load_vgm(&out)
            .expect("load")
            .writes
            .iter()
            .filter(|w| w.chip == "opl2" && (w.reg & 0xFF) == (0x40 | MOD))
            .map(|w| (w.val & 0x3F) as i32)
            .collect();
        let seen: Vec<i32> = got.iter().copied().filter(|v| RAMP.contains(v)).collect();
        let missing: Vec<i32> = RAMP.iter().copied().filter(|v| !got.contains(v)).collect();
        assert!(
            missing.is_empty(),
            "player never reached $4n levels {missing:?}; wrote {:?}",
            &got[..got.len().min(16)]
        );
        // The module loops, so the ramp plays more than once: compare one pass.
        assert_eq!(
            &seen[..RAMP.len()],
            &RAMP,
            "the ramp must arrive in order and unturned"
        );
    }
}
