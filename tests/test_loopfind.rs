//! Loop finding: the downbeat, a few rows before the commands that play it.
//!
//! The grids are synthetic and never render.
#![allow(non_snake_case)]

mod common;

use common::*;
use std::path::PathBuf;

use vgm2fur::furio::parse_fur;
use vgm2fur::furwrite::{_emit_steps, write_fur, Grid, Row, WriteOptions};
use vgm2fur::loopfind::{
    _exact_rows, _seat_cost, _seat_seam, apply_loop, find_loop, row_to_sample,
};
use vgm2fur::model::{NoteEvent, Song};
use vgm2fur::parity::py_round;
use vgm2fur::vgm::VgmFile;

const NOTE_OFF: i32 = 180;

fn _vgm(loop_samples: i64, loop_sample: Option<i64>) -> VgmFile {
    let mut v = VgmFile::default();
    v.path = PathBuf::from("synthetic.vgm");
    v.buf = Vec::new();
    v.version = 0x151;
    v.total_samples = 1;
    v.loop_samples = loop_samples;
    v.loop_file_pos = None;
    v.loop_sample = loop_sample;
    v.ym2151_clock = 3_579_545;
    v.k007232_clock = 3_579_545;
    v.gd3 = Default::default();
    v.roms = Vec::new();
    v.writes = Vec::new();
    v
}

fn _event(sample: i64, ch: i32, pitch: i32) -> NoteEvent {
    NoteEvent {
        sample,
        ch,
        on: true,
        note: pitch,
        ins: 0,
        vol: -1,
        e5: 0x80,
        pan: 0,
        ..Default::default()
    }
}

/// A repeating cell. Pulse 0 is the downbeat (three voices); the lead continues.
///
/// Returns notes, the planted loop row, and the planted end row.
fn _bar(loop_pulse: i32) -> (Vec<(i32, i32, i32)>, i32, i32) {
    let pulses = 8;
    let step = 4;
    let bars = 4;
    let intro = 1;
    let origin = intro * pulses * step;
    let mut notes: Vec<(i32, i32, i32)> = Vec::new();
    let total = (intro + bars) * pulses;
    for pulse in 0..total {
        let row = pulse * step;
        let lead = if pulse % pulses == 0 {
            60
        } else {
            64 + (pulse % pulses)
        };
        notes.push((row, 0, lead));
        if pulse % pulses == 0 {
            notes.push((row, 1, 48));
            notes.push((row, 2, 55));
        } else if pulse % pulses == 4 {
            notes.push((row, 1, 50));
        }
    }
    let loop_row = origin + loop_pulse * step;
    let length = bars * pulses * step;
    let end_row = loop_row + length - 1;
    // A note on the end row so the length is real content.
    if !notes.iter().any(|&(row, _ch, _pitch)| row == end_row) {
        notes.push((end_row, 0, 60));
    }
    (notes, loop_row, end_row)
}

/// The synthetic [`Song`] every probe starts from.
fn _song(
    vgm: VgmFile,
    t0: i64,
    loop_sample: Option<i64>,
    speed: i32,
    hz: f64,
    events: Vec<NoteEvent>,
) -> Song {
    Song {
        vgm,
        t0,
        loop_sample,
        speed,
        hz,
        fm_patches: Vec::new(),
        pcm_samples: Vec::new(),
        events,
        warnings: Vec::new(),
        oki_samples: Vec::new(),
        msm6258_samples: Vec::new(),
        segapcm_samples: Vec::new(),
        segapcm_ids: Vec::new(),
        rf5c68_samples: Vec::new(),
        rf5c164_samples: Vec::new(),
        c140_samples: Vec::new(),
        c219_samples: Vec::new(),
        c219_ids: Vec::new(),
        c219_count: 0,
        c352_samples: Vec::new(),
        k053260_samples: Vec::new(),
        k054539_samples: Vec::new(),
        k054539_ids: Vec::new(),
        k054539_count: 0,
        ymz280b_samples: Vec::new(),
        ymz280b_ids: Vec::new(),
        ymz280b_count: 0,
        multipcm_samples: Vec::new(),
        multipcm_ids: Vec::new(),
        multipcm_count: 0,
        upd7759_samples: Vec::new(),
        upd7759_ids: Vec::new(),
        upd7759_count: 0,
        pwm_samples: Vec::new(),
        pwm_ids: Vec::new(),
        pwm_count: 0,
        msm5205_samples: Vec::new(),
        msm5205_ids: Vec::new(),
        msm5205_count: 0,
        ga20_samples: Vec::new(),
        ga20_ids: Vec::new(),
        ga20_count: 0,
        msm5232_ids: Vec::new(),
        msm5232_controls: Vec::new(),
        msm5232_count: 0,
        es5503_samples: Vec::new(),
        es5503_ids: Vec::new(),
        es5503_oscs: Vec::new(),
        es5503_rate: 0,
        es5503_count: 0,
        channel_vol_events: Vec::new(),
        mikey_ids: Vec::new(),
        mikey_ins: Vec::new(),
        mikey_samples: Vec::new(),
        mikey_count: 0,
        x1_010_ids: Vec::new(),
        x1_010_ins: Vec::new(),
        x1_010_samples: Vec::new(),
        x1_010_count: 0,
        x1_waves: Vec::new(),
        x1_envs: Vec::new(),
        fds_ids: Vec::new(),
        fds_ins: Vec::new(),
        fds_waves: Vec::new(),
        fds_mod_waves: Vec::new(),
        fds_count: 0,
        wonderswan_ids: Vec::new(),
        wonderswan_ins: Vec::new(),
        wonderswan_waves: Vec::new(),
        wonderswan_pcm_samples: Vec::new(),
        wonderswan_count: 0,
        wonderswan_pcm_count: 0,
        qsound_ids: Vec::new(),
        qsound_ins: Vec::new(),
        qsound_samples: Vec::new(),
        qsound_count: 0,
        qsound_adpcm_count: 0,
        channel_fx: Vec::new(),
        chip_fx: Vec::new(),
        grid_note: String::new(),
        legato_steps: 0,
        legato_ramps: 0,
        row_subdiv: 1.0,
        hilight_a: 4,
        hilight_b: 16,
        fm_chip: "ym2151".to_string(),
        fm_systems: Vec::new(),
        has_fm_source: true,
        ym2151_ids: Vec::new(),
        ym2203_ids: Vec::new(),
        ym2203_ext: false,
        ym2203_ssg_count: 0,
        ym2612_ids: Vec::new(),
        ym2612_ext: false,
        ym2612_csm: false,
        opl_ids: Vec::new(),
        opl_drums: false,
        opll_ids: Vec::new(),
        opll_drums: false,
        opll_vrc7: false,
        fm_vol_max: 127,
        fm_notes: 0,
        drum_notes: 0,
        ym2610_ids: Vec::new(),
        ym2610_b: false,
        ym2610_ssg_count: 0,
        ym2610_a_samples: Vec::new(),
        ym2610_b_samples: Vec::new(),
        ym2610_a_count: 0,
        ym2610_b_count: 0,
        ym2608_ids: Vec::new(),
        ym2608_ext: false,
        ym2608_ssg_count: 0,
        ym2608_rhythm_count: 0,
        ym2608_rhythm_notes: 0,
        ym2608_b_samples: Vec::new(),
        ym2608_b_count: 0,
        y8950_b_samples: Vec::new(),
        y8950_b_count: 0,
        opl4_wave_samples: Vec::new(),
        opl4_wave_count: 0,
        k007232_volume: 0.20,
        loop_end_sample: None,
        loop_start_row: None,
        loop_end_row: None,
        loop_marker_sample: None,
        loop_cut_sample: None,
        c352_quad: false,
        scc_ids: Vec::new(),
        scc_waves: Vec::new(),
        scc_ins: Vec::new(),
        scc_count: 0,
        k005289_ids: Vec::new(),
        k005289_waves: Vec::new(),
        k005289_ins: Vec::new(),
        k005289_count: 0,
        ay_ids: Vec::new(),
        ay_ins: Vec::new(),
        ay_count: 0,
        pokey_ids: Vec::new(),
        pokey_ins: Vec::new(),
        pokey_count: 0,
        ssg_pcm_samples: Vec::new(),
        ssg_pcm_count: 0,
        nes_ids: Vec::new(),
        nes_present: false,
        nes_ins: Vec::new(),
        nes_dpcm_samples: Vec::new(),
        nes_pcm_samples: Vec::new(),
        nes_count: 0,
        nes_pcm_count: 0,
        huc6280_ids: Vec::new(),
        huc6280_waves: Vec::new(),
        huc6280_ins: Vec::new(),
        huc6280_pcm_samples: Vec::new(),
        huc6280_count: 0,
        huc6280_pcm_count: 0,
        gb_ids: Vec::new(),
        gb_waves: Vec::new(),
        gb_ins: Vec::new(),
        gb_count: 0,
        vsu_ids: Vec::new(),
        vsu_waves: Vec::new(),
        vsu_mod_waves: Vec::new(),
        vsu_ins: Vec::new(),
        vsu_count: 0,
        vsu_dynamic: false,
        saa_ids: Vec::new(),
        saa_ins: Vec::new(),
        saa_count: 0,
        psg_ids: Vec::new(),
        psg_ins: Vec::new(),
        psg_count: 0,
        psg_stereo: false,
        psg_t6w28: false,
        psg_dac_samples: Vec::new(),
        psg_dac_plays: Vec::new(),
        psg_dac_voices: Vec::new(),
        psg_dac_rate: vgm2fur::psg_dac::PSG_DAC_RATE,
        psg_dac_count: 0,
        psg_dac_stats: indexmap::IndexMap::new(),
        dac_samples: Vec::new(),
        dac_count: 0,
        dac_stats: Default::default(),
    }
}

fn write_no_pcm(song: &Song) -> Vec<u8> {
    write_fur(
        song,
        WriteOptions {
            include_pcm: false,
            include_fm: true,
            ..Default::default()
        },
    )
}

mod TestFindLoop {
    use super::*;

    #[test]
    fn test_mid_bar_marker_moves_to_the_downbeat() {
        let (notes, loop_row, end_row) = _bar(1);
        let fix = find_loop(&notes, loop_row, end_row, None).expect("no fix");
        // Pulse 0 of the cell, then the row before those commands.
        assert_eq!(fix.shift_pulses, -1);
        assert_eq!(fix.period, 8);
        assert_eq!(fix.start_row, loop_row - fix.step_rows - 1);
        assert_eq!(fix.end_row - fix.start_row, end_row - loop_row);
    }

    #[test]
    fn test_downbeat_marker_stays_on_the_beat() {
        let (notes, loop_row, end_row) = _bar(0);
        let fix = find_loop(&notes, loop_row, end_row, None).expect("no fix");
        assert_eq!(fix.shift_pulses, 0);
        assert_eq!(fix.start_row, loop_row - 1);
        assert_eq!(fix.end_row - fix.start_row, end_row - loop_row);
    }

    #[test]
    fn test_pickup_into_a_chord_is_left_alone() {
        let mut notes: Vec<(i32, i32, i32)> = Vec::new();
        for (i, pitch) in (40..72).enumerate() {
            notes.push((i as i32 * 4, 0, pitch));
        }
        let chord = 40 * 4;
        for (ch, pitch) in [60, 64, 67, 48].into_iter().enumerate() {
            notes.push((chord, ch as i32, pitch));
        }
        for i in 0..8 {
            notes.push((chord + 8 + i * 5, 1, 70 - i));
        }
        let fix = find_loop(&notes, chord, chord + 48, None);
        assert!(fix.is_none());
    }

    #[test]
    fn test_irregular_gaps_are_left_alone() {
        let notes: Vec<(i32, i32, i32)> = [0, 3, 7, 8, 20, 21, 50, 80, 81, 140, 200]
            .into_iter()
            .map(|row| (row, 0, 60))
            .collect();
        assert!(find_loop(&notes, 50, 200, None).is_none());
    }

    #[test]
    fn test_empty_row_without_a_clock_stays() {
        // No regular gap. The row before the marker happens to be a chord.
        // That is not enough to move the VGM marker.
        let notes = [
            (0, 0, 60),
            (0, 1, 64),
            (5, 0, 62),
            (9, 1, 65),
            (14, 0, 67),
            (20, 0, 60),
            (20, 1, 64),
        ];
        assert!(find_loop(&notes, 21, 40, None).is_none());
    }

    #[test]
    fn test_a_bar_still_wins_when_a_forward_window_is_open() {
        // The phrase start is one pulse behind the marker. The four-second
        // search must not pull that loop forward.
        let (notes, loop_row, end_row) = _bar(1);
        let fix = find_loop(&notes, loop_row, end_row, Some(200)).expect("no fix");
        assert_eq!(fix.shift_pulses, -1);
        assert_eq!(fix.start_row, loop_row - fix.step_rows - 1);
    }

    #[test]
    fn test_phrase_ahead_of_the_marker_is_not_a_seam() {
        // The head of the loop repeats 96 rows in, an inner repetition. The
        // loop keeps the length the VGM gave it, so a copy that close is not
        // the seam the marker is missing; the marker stays where it is.
        let mut notes: Vec<(i32, i32, i32)> =
            (0..48).step_by(3).map(|row| (row, 0, 40 + row / 3)).collect();
        notes.extend([(48, 0, 60), (48, 1, 64), (48, 2, 67)]);
        notes.extend((0..24).map(|i| (48 + i * 2, 3, 72)));
        notes.extend([(144, 0, 60), (144, 1, 64), (144, 2, 67)]);
        notes.extend((0..24).map(|i| (144 + i * 2, 3, 72)));
        notes.extend((0..35).map(|i| (100 + i * 4, 1, 50 + i)));
        assert!(find_loop(&notes, 96, 239, Some(64)).is_none());
    }

    #[test]
    fn test_phrase_behind_the_marker_becomes_the_loop() {
        // The body starts 48 rows in and the marker sits inside it. The head
        // of the body comes back at the end of the track, so the loop has to
        // open at the body, 87 rows behind the marker, and keep its length.
        let mut notes: Vec<(i32, i32, i32)> =
            (0..48).step_by(3).map(|row| (row, 0, 40 + row / 3)).collect();
        notes.extend([(48, 0, 60), (48, 2, 67)]);
        notes.extend((50..98).enumerate().map(|(j, pitch)| (48 + j as i32 * 4, 1, pitch)));
        notes.extend([(240, 0, 60), (240, 2, 67)]);
        notes.extend((50..71).enumerate().map(|(j, pitch)| (240 + j as i32 * 4, 1, pitch)));
        let fix = find_loop(&notes, 132, 320, Some(128)).expect("no fix");
        assert_eq!(fix.start_row, 47);
        assert_eq!(fix.end_row, 238);
    }

    #[test]
    fn test_marker_one_row_after_the_chord_lands_before_the_chord() {
        let (notes, loop_row, end_row) = _bar(0);
        // The planted row is the downbeat. Move the marker one row later,
        // onto a row with no note.
        let fix = find_loop(&notes, loop_row + 1, end_row + 1, None).expect("no fix");
        assert_eq!(fix.shift_pulses, 0);
        assert_eq!(fix.start_row, loop_row - 1);
        assert_eq!(fix.end_row - fix.start_row, end_row - loop_row);
    }
}

mod TestSeatSeam {
    use super::*;

    /// Content repeating every `span` rows: row x copies to x + span.
    /// `strays` extra notes at the window start have no copy in the tail.
    fn _content(strays: usize) -> Vec<(i32, i32, i32)> {
        let span = 40;
        let mut notes: Vec<(i32, i32, i32)> = (100..180)
            .filter(|row| row % 4 == 0)
            .map(|row| (row, 0, 48 + (row % span) / 4))
            .collect();
        notes.extend((100..100 + strays as i32).map(|row| (row, 1, 60 + row)));
        notes
    }

    #[test]
    fn test_the_cost_counts_voices_that_miss_across_the_seam() {
        let exact = _exact_rows(&_content(0));
        assert_eq!(_seat_cost(&exact, 100, 139), 0);
        let strayed = _exact_rows(&_content(1));
        assert_eq!(_seat_cost(&strayed, 100, 139), 1);
    }

    #[test]
    fn test_a_seat_with_an_exact_window_moves_with_its_end() {
        // Pairs (100, 140) and (101, 141) miss; two rows later every pair
        // in the window is exact, so the cut moves and the end follows.
        let exact = _exact_rows(&_content(2));
        assert_eq!(_seat_seam(&exact, 100, 139, 0, 10_000, 176), (102, 141));
    }

    #[test]
    fn test_a_thin_improvement_leaves_the_seat_alone() {
        // One stray still faces the window from two rows later. A thinner
        // mismatch is the tail itself, not the seat, and moving would only
        // drag the wobble along.
        let exact = _exact_rows(&_content(3));
        assert_eq!(_seat_seam(&exact, 100, 139, 0, 10_000, 176), (100, 139));
    }

    #[test]
    fn test_no_tail_past_the_jump_leaves_the_seat_alone() {
        // The window has to fit inside the log. Comparing against the end
        // of the track proves nothing, so no move is made on it.
        let exact = _exact_rows(&_content(2));
        assert_eq!(_seat_seam(&exact, 100, 139, 0, 10_000, 150), (100, 139));
    }

    #[test]
    fn test_the_end_never_leaves_the_log() {
        // The winning seat would put the jump past the last content row.
        let exact = _exact_rows(&_content(2));
        assert_eq!(_seat_seam(&exact, 100, 139, 0, 140, 176), (100, 139));
    }
}

mod TestApplyAndWrite {
    use super::*;

    #[test]
    fn test_adjusted_end_is_where_bxx_sits() {
        // speed 1 at 60 Hz is 735 samples/row. Rows 0, 16 and 40 carry notes.
        let events: Vec<NoteEvent> = [0i64, 16, 40]
            .into_iter()
            .map(|row| _event(row * 735, 0, 60))
            .collect();
        let mut song = _song(
            _vgm(0, None),
            0,
            Some(16 * 735),
            1,
            60.0,
            events,
        );
        song.loop_end_sample = Some(40 * 735);
        let module = parse_fur(&write_no_pcm(&song)).expect("parse");
        assert_eq!(&module.orders[0][0..2], &[0, 1]);
        // Order 1 starts at row 16. Bxx is on absolute row 40.
        let rows = &module.patterns[&(0u16, module.orders[0][1])];
        let bxx: Vec<usize> = rows
            .iter()
            .enumerate()
            .filter(|(_, row)| row.fx.iter().any(|&(c, _v)| c == 0x0B))
            .map(|(i, _)| i)
            .collect();
        assert_eq!(bxx, vec![40 - 16]);
    }

    #[test]
    fn test_row_to_sample_round_trips() {
        let song = _song(_vgm(0, None), 100, None, 4, 120.0, Vec::new());
        for row in [0, 1, 17, 158] {
            assert_eq!(song.row_of(row_to_sample(&song, row)), row);
        }
    }

    #[test]
    fn test_apply_loop_keeps_the_marker_and_sets_both_ends() {
        let (notes, loop_row, end_row) = _bar(1);
        let length = end_row - loop_row + 1;
        let mut song = _song(
            _vgm(length as i64 * 735, Some(loop_row as i64 * 735)),
            0,
            Some(loop_row as i64 * 735),
            1,
            60.0,
            notes
                .iter()
                .map(|&(row, ch, pitch)| _event(row as i64 * 735, ch, pitch))
                .collect(),
        );
        let text = apply_loop(&mut song);
        assert!(text.is_some());
        assert_eq!(song.loop_start_row, Some(loop_row));
        assert_eq!(song.loop_end_row, Some(end_row));
        assert_eq!(song.row_of(song.loop_sample.unwrap()), loop_row);
        assert_eq!(song.row_of(song.loop_end_sample.unwrap()), end_row);
        assert!(!song.warnings.is_empty());
    }

    #[test]
    fn test_apply_loop_does_not_open_on_a_repeating_pickup() {
        // The marker sits after a chord that is released before it, and the
        // tail repeats that chord one loop later. It is intro material. The
        // loop opens on the marker. The chord plays once per iteration at the
        // end, never twice at the start.
        let length = 32i32;
        let notes = [
            (8, 0, 60),
            (8, 1, 64),
            (14, 0, 62),
            (16, 0, 67),
            (16, 1, 55),
            (24, 0, 69),
            (40, 0, 60),
            (40, 1, 64),
            (46, 0, 62),
            (48, 0, 67),
            (48, 1, 55),
        ];
        let mut song = _song(
            _vgm(length as i64 * 735, Some(16 * 735)),
            0,
            Some(16 * 735),
            1,
            60.0,
            notes
                .iter()
                .map(|&(row, ch, pitch)| _event(row * 735, ch, pitch))
                .collect(),
        );
        let _ = apply_loop(&mut song);
        assert_eq!(song.loop_start_row, Some(16));
        assert_eq!(song.loop_end_row, Some(16 + length - 1));
    }

    #[test]
    fn test_unknown_loop_length_without_a_repeat_keeps_the_whole_track() {
        // A VGM can carry a loop marker without a declared length. If the
        // finder sees no repeating body, the fallback is the whole remaining
        // track, never a one-row loop that collapses the module to one order.
        let notes = [(0, 0, 60), (16, 0, 64), (40, 0, 67), (80, 0, 60)];
        let mut song = _song(
            _vgm(0, Some(0)),
            0,
            Some(0),
            1,
            60.0,
            notes
                .iter()
                .map(|&(row, ch, pitch)| _event(row * 735, ch, pitch))
                .collect(),
        );
        let _ = apply_loop(&mut song);
        assert_eq!(song.loop_start_row, Some(0));
        assert_eq!(song.loop_end_row, Some(80));
        let module = parse_fur(&write_no_pcm(&song)).expect("parse");
        assert!(module.orders[0].len() > 1);
        assert!(
            module.patterns[&(0u16, module.orders[0][0])]
                .iter()
                .filter(|row| row.note >= 0)
                .count()
                > 0
        );
    }

    #[test]
    fn test_apply_loop_calibrates_the_row_to_the_loop() {
        // 100000 samples do not divide into the measured 735-sample row. The
        // written row is re-derived to make one iteration sample exact.
        let length = py_round(100000.0 / 735.0) as i32;
        let mut song = _song(
            _vgm(100_000, Some(10 * 735)),
            0,
            Some(10 * 735),
            1,
            60.0,
            vec![_event(10 * 735, 0, 60)],
        );
        let _ = apply_loop(&mut song);
        assert_eq!(
            song.loop_end_row.unwrap() - song.loop_start_row.unwrap() + 1,
            length
        );
        assert!(
            (song.samples_per_row() * length as f64 - 100_000.0).abs() <= 1.0,
            "{} != 100000 within 1.0",
            song.samples_per_row() * length as f64
        );
    }

    #[test]
    fn test_a_pre_wrap_note_rolling_past_the_end_stays_on_the_last_row() {
        // At speed 1 the writer rounds the note to the row after the loop
        // end. The note is still before the wrap sample. It belongs to the
        // last iteration row, not to the next iteration.
        let length = 41;
        let end_row = length - 1;
        let mut song = _song(
            _vgm(length as i64 * 735, Some(0)),
            0,
            Some(0),
            1,
            60.0,
            vec![
                _event(0, 0, 60),
                _event(((end_row as f64 + 0.6) * 735.0) as i64, 0, 64),
            ],
        );
        let _ = apply_loop(&mut song);
        let module = parse_fur(&write_no_pcm(&song)).expect("parse");
        let order = end_row / module.pat_len as i32;
        let rows = &module.patterns[&(0u16, module.orders[0][order as usize])];
        assert_eq!(rows[(end_row % module.pat_len as i32) as usize].note, 64);
    }

    #[test]
    fn test_the_jump_rides_a_channel_without_a_delay() {
        // Both channels carry a delayed note on the loop end row. One gives
        // up its delay to carry Bxx. The other keeps ED and still sounds just
        // before the wrap.
        let (speed, hz) = (6, 480.7);
        let row = (44100.0 / hz) * speed as f64;
        let tick = row / speed as f64;
        let (length, end_row) = (9i64, 8i64);
        let mut song = _song(
            _vgm((length as f64 * row) as i64, Some(0)),
            0,
            Some(0),
            speed,
            hz,
            (0..8)
                .map(|ch| {
                    _event(
                        (end_row as f64 * row + 3.0 * tick) as i64,
                        ch,
                        60 + ch,
                    )
                })
                .collect(),
        );
        let _ = apply_loop(&mut song);
        let module = parse_fur(&write_no_pcm(&song)).expect("parse");
        let ch0 = &module.patterns[&(0u16, module.orders[0][0])][end_row as usize];
        let ch1 = &module.patterns[&(1u16, module.orders[1][0])][end_row as usize];
        assert!(ch0.fx.iter().any(|&(c, _v)| c == 0x0B));
        assert!(ch1.fx.iter().any(|&(c, _v)| c == 0xED));
        assert!(!ch1.fx.iter().any(|&(c, _v)| c == 0x0B));
    }
}

mod TestClippedPitchStep {
    use super::*;

    #[test]
    fn test_a_step_on_the_clipped_note_row_moves_after_the_note() {
        // The loop-start clip can move a note forward one row. A late rate
        // step that lands on the note's new row must move to the next row
        // instead of being mistaken for a foreign note.
        let song = _song(_vgm(0, None), 0, None, 1, 60.0, Vec::new());
        assert_eq!(song.place(1000, None).0, 1);
        assert_eq!(song.place(1470, None).0, 2);
        let ev = NoteEvent {
            sample: 1000,
            ch: 0,
            on: true,
            note: 60,
            ins: 0,
            vol: -1,
            e5: 0x80,
            pan: 0,
            pcm: true,
            pitch_steps: vec![(1470, 73)],
            e5_lo: 0x00,
            e5_hi: 0xFF,
            ..Default::default()
        };
        let mut grid: Grid = vec![indexmap::IndexMap::new()];
        grid[0].insert(
            2,
            Row {
                note: 60,
                ..Default::default()
            },
        );
        let mut e5: indexmap::IndexMap<(i32, i32), i32> = indexmap::IndexMap::new();
        _emit_steps(
            &mut grid,
            &song,
            &ev,
            2,
            10,
            &mut e5,
            None,
            None,
            Some(2),
        );
        let row = grid[0].get(&3).expect("row");
        assert!(
            row.fx
                .as_ref()
                .map(|fx| fx.iter().any(|&(cmd, _value)| cmd == 0xE8 || cmd == 0xE9))
                .unwrap_or(false)
        );
    }
}
