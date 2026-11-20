//! Port of `tests/test_opm_keyon_burst.py`: key-on burst semantics on the
//! OPM, where an instrument is loaded after its key-on.

mod common;

use common::*;
use vgm2fur::analyze::{analyze, AnalyzeOptions};
use vgm2fur::furio::parse_fur;
use vgm2fur::furwrite::{write_fur, WriteOptions};
use vgm2fur::model::NoteEvent;
use vgm2fur::vgm::ChipWrite;

const CH: i32 = 2;
const KEY_ON: i64 = 60_000;
const KEY_OFF: i64 = 150_000;

/// The burst as the driver writes it: voice, then pitch, then key-on, and only
/// then the levels that make the carriers sound.
const MUTED_VOICE: [(u16, u16); 25] = [
    (0x20 | CH as u16, 0xC3),                       // RL both, fb 0, alg 3
    (0x40 | CH as u16, 0x5B), (0x48 | CH as u16, 0x10), // op DT1/MULT
    (0x50 | CH as u16, 0x32), (0x58 | CH as u16, 0x70),
    (0x60 | CH as u16, 0x7F), (0x68 | CH as u16, 0x7F), // every carrier parked
    (0x70 | CH as u16, 0x7F), (0x78 | CH as u16, 0x7F),
    (0x80 | CH as u16, 0x1F), (0x88 | CH as u16, 0x1F), // KS / AR
    (0x90 | CH as u16, 0x1F), (0x98 | CH as u16, 0x1F),
    (0xA0 | CH as u16, 0x0F), (0xA8 | CH as u16, 0x16), // AM / D1R
    (0xB0 | CH as u16, 0x0C), (0xB8 | CH as u16, 0x06),
    (0xC0 | CH as u16, 0x42), (0xC8 | CH as u16, 0x00), // DT2 / D2R
    (0xD0 | CH as u16, 0x00), (0xD8 | CH as u16, 0x00),
    (0xE0 | CH as u16, 0x61), (0xE8 | CH as u16, 0x63), // D1L / RR
    (0xF0 | CH as u16, 0x64), (0xF8 | CH as u16, 0x97),
];

const LOADED_TL: [(u16, u16); 4] = [
    (0x60 | CH as u16, 0x2A),
    (0x68 | CH as u16, 0x0C),
    (0x70 | CH as u16, 0x24),
    (0x78 | CH as u16, 0x00),
];

/// One note whose carrier levels arrive after its own key-on.
fn _burst_song(
    load_gap: i64,
    pitch_writes: &[(i64, u16, u16)],
    key_on: i64,
) -> (vgm2fur::model::Song, NoteEvent) {
    let mut writes = lattice(60, 5000);
    for &(reg, val) in MUTED_VOICE.iter() {
        writes.push(ChipWrite::new(key_on - 40, "ym2151", 0, reg as u32, val as u32));
    }
    writes.push(ChipWrite::new(
        key_on - 20,
        "ym2151",
        0,
        (0x28 | CH) as u32,
        0x40,
    ));
    writes.push(ChipWrite::new(
        key_on - 20,
        "ym2151",
        0,
        (0x30 | CH) as u32,
        0x14,
    ));
    writes.push(ChipWrite::new(
        key_on,
        "ym2151",
        0,
        0x08,
        (0x78 | CH) as u32,
    ));
    // The same command's tail: the levels that make it audible.
    for (i, &(reg, val)) in LOADED_TL.iter().enumerate() {
        writes.push(ChipWrite::new(
            key_on + load_gap + i as i64,
            "ym2151",
            0,
            reg as u32,
            val as u32,
        ));
    }
    for &(sample, reg, val) in pitch_writes {
        writes.push(ChipWrite::new(sample, "ym2151", 0, reg as u32, val as u32));
    }
    writes.push(ChipWrite::new(
        KEY_OFF,
        "ym2151",
        0,
        0x08,
        CH as u32,
    ));
    writes.sort_by_key(|w| w.sample);
    let song = analyze(
        &vgm(writes),
        AnalyzeOptions { pcm: false, ..Default::default() },
    )
    .expect("song");
    let ev = song
        .events
        .iter()
        .find(|e| e.on && e.ch == CH)
        .cloned()
        .expect("note event");
    (song, ev)
}

fn _carrier_tls(song: &vgm2fur::model::Song, ev: &NoteEvent) -> Vec<u8> {
    let patch = &song.fm_patches[ev.ins as usize];
    (0..4).map(|i| patch.ops[i].tl & 0x7F).collect()
}

mod late_instrument_load {
    use super::*;

    #[test]
    fn test_carrier_levels_written_after_the_key_on_are_the_instrument() {
        let (song, ev) = _burst_song(2, &[], KEY_ON);
        let patch = &song.fm_patches[ev.ins as usize];
        assert!(
            patch.carrier_tl() < 0x7F,
            "the note was given an instrument that cannot sound"
        );
        assert_eq!(
            _carrier_tls(&song, &ev),
            vec![
                LOADED_TL[0].1 as u8,
                LOADED_TL[1].1 as u8,
                LOADED_TL[2].1 as u8,
                LOADED_TL[3].1 as u8,
            ]
        );
    }

    #[test]
    fn test_the_loaded_levels_are_not_also_a_mid_note_fade() {
        // Folding the burst into the instrument must not leave a volume step on
        // the note: the fade column cannot rise, and a redundant step would make
        // the same attack read twice.
        let (_song, ev) = _burst_song(2, &[], KEY_ON);
        assert_eq!(ev.tl_pts, Vec::new());
    }

    #[test]
    fn test_a_wider_gap_is_a_real_change_and_stays_out_of_the_snapshot() {
        // Past the burst the driver means a mid-note change: the instrument keeps
        // the level it had at the key-on.
        let (song, ev) = _burst_song(60_000, &[], KEY_ON);
        let patch = &song.fm_patches[ev.ins as usize];
        assert_eq!(patch.carrier_tl(), 0x7F);
    }

    #[test]
    fn test_a_note_that_is_already_audible_keeps_its_own_snapshot() {
        // The common case must not absorb a later voice's setup: the levels at
        // the key-on stay the instrument, whatever is written while it sounds.
        let mut writes = lattice(60, 5000);
        for &(reg, val) in MUTED_VOICE.iter() {
            writes.push(ChipWrite::new(
                KEY_ON - 40,
                "ym2151",
                0,
                reg as u32,
                val as u32,
            ));
        }
        for &(reg, val) in LOADED_TL.iter() {
            writes.push(ChipWrite::new(
                KEY_ON - 30,
                "ym2151",
                0,
                reg as u32,
                val as u32,
            ));
        }
        writes.push(ChipWrite::new(KEY_ON - 20, "ym2151", 0, (0x28 | CH) as u32, 0x40));
        writes.push(ChipWrite::new(KEY_ON - 20, "ym2151", 0, (0x30 | CH) as u32, 0x14));
        writes.push(ChipWrite::new(KEY_ON, "ym2151", 0, 0x08, (0x78 | CH) as u32));
        // A different voice arrives while the note sounds.
        for &(reg, val) in LOADED_TL.iter() {
            writes.push(ChipWrite::new(
                KEY_ON + 20_000,
                "ym2151",
                0,
                reg as u32,
                (val ^ 0x11) as u32,
            ));
        }
        writes.push(ChipWrite::new(KEY_OFF, "ym2151", 0, 0x08, CH as u32));
        writes.sort_by_key(|w| w.sample);
        let song = analyze(
            &vgm(writes),
            AnalyzeOptions { pcm: false, ..Default::default() },
        )
        .expect("song");
        let ev = song
            .events
            .iter()
            .find(|e| e.on && e.ch == CH)
            .cloned()
            .expect("note event");
        assert_eq!(
            _carrier_tls(&song, &ev),
            vec![
                LOADED_TL[0].1 as u8,
                LOADED_TL[1].1 as u8,
                LOADED_TL[2].1 as u8,
                LOADED_TL[3].1 as u8,
            ]
        );
    }
}

mod opm_pan_and_dual_lfo {
    use super::*;

    fn _note(rl: u16, cid: u8, t: i64) -> Vec<ChipWrite> {
        let mut writes = Vec::new();
        for &(reg, val) in PATCH.iter() {
            let val = if reg == 0x20 {
                (rl << 6) | 0x07
            } else {
                val
            };
            writes.push(ChipWrite::new(t - 500, "ym2151", cid, reg as u32, val as u32));
        }
        writes.push(ChipWrite::new(t, "ym2151", cid, 0x28, 0x40));
        writes.push(ChipWrite::new(t, "ym2151", cid, 0x30, 0x00));
        writes.push(ChipWrite::new(t, "ym2151", cid, 0x08, 0x78));
        writes.push(ChipWrite::new(t + 20_000, "ym2151", cid, 0x08, 0x00));
        writes.sort_by_key(|w| w.sample);
        writes
    }

    #[test]
    fn test_rl_bits_match_the_hardware() {
        // $20 RL: bit 6 = left, bit 7 = right (libvgm ym2151.c pan[]). The
        // 2-bit field is 1 for left-only and 2 for right-only.
        for (rl, pan) in [(0x1u16, 0xF0), (0x2, 0x0F), (0x3, 0xFF)] {
            let song = analyze(
                &vgm(_note(rl, 0, 1000)),
                AnalyzeOptions::default(),
            )
            .expect("song");
            let ev = song
                .events
                .iter()
                .find(|e| e.on && e.ch == 0)
                .expect("note event");
            assert_eq!(ev.pan, pan, "RL={}", rl);
        }
    }

    #[test]
    fn test_second_ym2151_keeps_its_own_lfo() {
        // Chip-wide LFO writes are per instance. Chip 1's speed/wave/depth
        // must travel as effects on one of its own eight columns; sending
        // them to column 0 leaves the second chip at Furnace's defaults.
        let mut writes = _note(0x3, 1, 1000);
        writes.push(ChipWrite::new(1000, "ym2151", 1, 0x18, 245));
        writes.push(ChipWrite::new(1001, "ym2151", 1, 0x19, 0x87));
        writes.push(ChipWrite::new(1002, "ym2151", 1, 0x1B, 3));
        let mut file = vgm(writes);
        file.ym2151_dual = true;
        let song = analyze(&file, AnalyzeOptions::default()).expect("song");
        assert!(song.chip_fx.contains(&(1000, 0x17, 245, 0, 8)));
        assert!(song.chip_fx.contains(&(1001, 0x1F, 7, 0, 8)));
        assert!(song.chip_fx.contains(&(1002, 0x18, 3, 0, 8)));
        let mod_ = parse_fur(&write_fur(
            &song,
            WriteOptions { include_pcm: false, ..Default::default() },
        ))
        .expect("module");
        let mut on_chip1 = false;
        for ((ch, _pat), rows) in mod_.patterns.iter() {
            if *ch < 8 {
                continue;
            }
            for row in rows.iter() {
                for &(cmd, val) in row.fx.iter() {
                    if (cmd, val) == (0x17, 245) {
                        on_chip1 = true;
                    }
                }
            }
        }
        assert!(on_chip1, "chip 1's LFO speed landed on chip 0");
    }
}
