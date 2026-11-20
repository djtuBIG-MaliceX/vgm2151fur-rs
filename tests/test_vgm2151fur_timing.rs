//! Port of `tests/test_vgm2151fur_timing.py`: the measured key-on lattice
//! grid (vgm2151fur.timing + placement).

mod common;

use std::collections::BTreeMap;

use common::*;
use indexmap::IndexMap;
use vgm2fur::analyze::{analyze, AnalyzeOptions};
use vgm2fur::fm::{FMPatch, OpRegs, CARRIERS};
use vgm2fur::furio::{parse_fur, FurRow};
use vgm2fur::furwrite::{write_fur, WriteOptions};
use vgm2fur::model::{
    kc_kf_units, opm_pitch_offset, shift_note_e5, transpose_opm_events, NoteEvent, Song,
};
use vgm2fur::parity::{py_int, py_round};
use vgm2fur::timing::{estimate_grid, estimate_grid_default, TARGET_TICK_SAMPLES, MIN_ROW_SAMPLES};
use vgm2fur::vgm::{ChipWrite, VgmFile};

macro_rules! assert_almost_delta {
    ($a:expr, $b:expr, $delta:expr) => {{
        let a = ($a) as f64;
        let b = ($b) as f64;
        let delta = ($delta) as f64;
        assert!(
            (a - b).abs() <= delta,
            "{} != {} within delta {}",
            a,
            b,
            delta
        );
    }};
}

fn _vgm_with_keyons(onsets: &[(i64, u16)]) -> VgmFile {
    let mut writes: Vec<ChipWrite> = onsets
        .iter()
        .map(|&(s, ch)| ChipWrite::new(s, "ym2151", 0, 0x08, 0x78 | ch as u32))
        .collect();
    writes.sort_by_key(|w| w.sample);
    let mut v = VgmFile::default();
    v.path = std::path::PathBuf::from("synthetic.vgm");
    v.buf = Vec::new();
    v.version = 0x151;
    v.total_samples = writes.last().map_or(0, |w| w.sample + 1);
    v.loop_samples = 0;
    v.loop_file_pos = None;
    v.loop_sample = None;
    v.ym2151_clock = 3_579_545;
    v.k007232_clock = 3_579_545;
    v.gd3 = IndexMap::new();
    v.roms = Vec::new();
    v.writes = writes;
    v
}

fn _vgm_with_writes(mut writes: Vec<ChipWrite>) -> VgmFile {
    writes.sort_by_key(|w| w.sample);
    let mut v = VgmFile::default();
    v.path = std::path::PathBuf::from("synthetic.vgm");
    v.buf = Vec::new();
    v.version = 0x151;
    v.total_samples = writes.last().map_or(0, |w| w.sample + 1);
    v.loop_samples = 0;
    v.loop_file_pos = None;
    v.loop_sample = None;
    v.ym2151_clock = 3_579_545;
    v.k007232_clock = 3_579_545;
    v.gd3 = IndexMap::new();
    v.roms = Vec::new();
    v.writes = writes;
    v
}

/// A base `Song` built through the public analyzer, then adjusted to the
/// values the Python tests assign directly (the Rust `Song` has no all-default
/// constructor).
fn _song(hz: f64, speed: i32, subdiv: f64, vgm: VgmFile) -> Song {
    let mut song = analyze(
        &note_song(),
        AnalyzeOptions { pcm: false, ..Default::default() },
    )
    .expect("base song analyzes");
    song.vgm = vgm;
    song.t0 = 0;
    song.loop_sample = None;
    song.speed = speed;
    song.hz = hz;
    song.fm_patches = Vec::new();
    song.pcm_samples = Vec::new();
    song.events = Vec::new();
    song.row_subdiv = subdiv;
    song
}

fn contains_subslice(hay: &[u8], needle: &[u8]) -> bool {
    hay.windows(needle.len()).any(|w| w == needle)
}

mod estimate_grid {
    use super::*;

    #[test]
    fn test_lattice_sixteenths() {
        // 2500-sample subdivision on ch1, 5000-sample units on ch0/ch2.
        // The estimator must lock onto the *finest* playing unit.
        let mut onsets = Vec::new();
        let mut t = 0i64;
        for k in 0..80 {
            onsets.push((t, 0u16));
            onsets.push((t, 1u16));
            onsets.push((t + 2500, 1u16));
            if k % 2 == 0 {
                onsets.push((t, 2u16));
            }
            t += 5000;
        }
        let grid = estimate_grid_default(&_vgm_with_keyons(&onsets)).expect("grid");
        assert_almost_delta!(grid.unit, 2500.0, 60.0);
        assert!(grid.subdiv >= 4);
        assert_almost_eq!(grid.row, grid.unit / grid.subdiv as f64, 6);
        assert!(grid.speed >= 1);
        assert_almost_eq!(grid.hz, 44100.0 * grid.speed as f64 / grid.row, 6);
        assert_almost_eq!(grid.tick(), grid.row / grid.speed as f64, 6);
        assert!(grid.confidence >= 0.9);
    }

    #[test]
    fn test_order_floor_leaves_the_pulse() {
        // Two tempos, and a file long enough that song_length/1920 is coarser
        // than either. The FM unit absorbs that floor. The pulse stays the
        // fastest note value, and the SCC/AY row is built from the pulse.
        let mut onsets: IndexMap<i32, Vec<i64>> = IndexMap::new();
        onsets.insert(0, (0..48).map(|i| i * 4000).collect());
        onsets.insert(1, (0..30).map(|i| i * 6100).collect());
        let mut vgm = _vgm_with_keyons(&[(0, 0)]);
        vgm.total_samples = 20_000_000;
        let grid = estimate_grid(
            &vgm,
            0.75,
            TARGET_TICK_SAMPLES,
            MIN_ROW_SAMPLES,
            Some(&onsets),
            "medley",
            None,
        )
        .expect("grid");
        assert!(grid.source.contains("sections change unit"));
        assert_almost_delta!(grid.pulse, 4000.0, 80.0);
        assert_almost_delta!(grid.unit, 20_000_000.0 / 1920.0, 1.0);
        assert!(grid.unit > grid.pulse);
        let (hz, speed, _sub, _note) = vgm2fur::analyze::_musical_row(&grid);
        assert_eq!(
            speed,
            py_round(grid.pulse / (44100.0 / 60.0)) as i32
        );
        assert_almost_delta!(44100.0 * speed as f64 / hz, grid.pulse, 1.0);
    }

    #[test]
    fn test_irregular_returns_none() {
        // Pseudo-random spacing must not be accepted as a lattice.
        let mut onsets = Vec::new();
        let mut t = 0i64;
        for k in 0..90i64 {
            t += 3000 + (k * 977) % 2500;
            onsets.push((t, (k % 4) as u16));
        }
        assert!(estimate_grid_default(&_vgm_with_keyons(&onsets)).is_none());
    }

    #[test]
    fn test_multiple_dominated_lattice_recovers_the_unit() {
        // A melody that mostly walks in 3-unit steps makes the mode 3x the row
        // unit (8400 = 3 x 2800), past the 16th-note band. Halving cannot fit
        // it (5600 and 11200 are not halves), so the estimator has to divide
        // the candidate back down instead of giving up.
        let gaps = [8400i64, 8400, 5600, 11200, 8400, 2800];
        let mut onsets = Vec::new();
        let mut t = 0i64;
        for k in 0..60usize {
            onsets.push((t, 0u16));
            t += gaps[k % gaps.len()];
        }
        let grid = estimate_grid_default(&_vgm_with_keyons(&onsets)).expect("grid");
        assert_almost_delta!(grid.unit, 2800.0, 60.0);
    }

    #[test]
    fn test_slow_unit_is_folded_not_dropped() {
        // Slow themes: the fastest melodic event on a channel is 9560 samples
        // (and often 2x that), well past the 16th-note band. The unit must
        // fold in halves instead of returning None.
        let mut onsets = Vec::new();
        let mut t = 0i64;
        for k in 0..60i64 {
            onsets.push((t, (k % 8) as u16));
            onsets.push((t + 4780, (k % 8) as u16));
            if k % 2 == 0 {
                onsets.push((t, ((k + 1) % 8) as u16));
            }
            t += 9560;
        }
        let grid = estimate_grid_default(&_vgm_with_keyons(&onsets)).expect("grid");
        assert!(grid.unit >= 800.0);
        assert!(grid.unit <= 8000.0);
        assert!(grid.hz / grid.speed as f64 >= 45.0); // rows/s
        assert!(grid.confidence >= 0.9);
    }
}

mod placement {
    use super::*;

    fn vgm() -> VgmFile {
        _vgm_with_keyons(&(0..8).map(|i| (i * 5000, 0u16)).collect::<Vec<_>>())
    }

    #[test]
    fn test_speed1_nearest_row() {
        let song = _song(44100.0 / 625.0, 1, 8.0, vgm());
        let tick = 625.0;
        for sample in [0i64, 100, 312, 313, 624, 625, 5000, 12345] {
            let (row, delay) = song.place(sample, None);
            assert_eq!(delay, 0);
            assert_eq!(row, py_int(sample as f64 / tick + 0.5) as i32);
            let placed = row as f64 * tick;
            assert!(placed.mul_add(-1.0, sample as f64).abs() <= tick / 2.0 + 0.5);
        }
    }

    #[test]
    fn test_speed7_floor_plus_tick_delay() {
        let song = _song(60.0, 7, 1.0, vgm());
        let row_len = 44100.0 / 60.0 * 7.0;
        let tick = 44100.0 / 60.0;
        for sample in [0i64, 100, 700, 3000, 3600, 5000, 10290, 12000] {
            let (row, delay) = song.place(sample, None);
            assert!(delay >= 0);
            assert!(delay < 7);
            let placed = row as f64 * row_len + delay as f64 * tick;
            assert!((placed - sample as f64).abs() <= tick / 2.0 + 0.5);
        }
    }
}

mod percussion_sweeps {
    use super::*;

    /// Twin16 channel-5 percussion pitch sweeps become 01xx/02xx ramps.
    fn _song_with_sweep() -> Song {
        // A key-on lattice on ch0 so the grid estimator has something to
        // measure, plus one drum note on ch5 with a KF ramp like the driver's
        // (-5 fraction steps per ~88 samples via register steps of 20).
        let mut writes: Vec<ChipWrite> = (0..60)
            .map(|k| ChipWrite::new(k * 5000, "ym2151", 0, 0x08, 0x78))
            .collect();
        writes.push(ChipWrite::new(1000, "ym2151", 0, 0x08, 0x78 | 5));
        writes.push(ChipWrite::new(1040, "ym2151", 0, 0x28 | 5, 0x46));
        // KC drops a semitone every ~12.5 KF steps, skipping the unused
        // nibbles (0x43/0x47 alias the neighbouring notes).
        let kc_steps: [(i64, u32); 3] = [(13, 0x45), (26, 0x44), (39, 0x42)];
        for k in 0..46i64 {
            writes.push(ChipWrite::new(
                1042 + k * 88,
                "ym2151",
                0,
                0x30 | 5,
                ((0xEC - 20 * k) & 0xFF) as u32,
            ));
            if let Some(&(_, val)) = kc_steps.iter().find(|&&(n, _)| n == k + 1) {
                writes.push(ChipWrite::new(
                    1040 + (k + 1) * 88,
                    "ym2151",
                    0,
                    0x28 | 5,
                    val,
                ));
            }
        }
        writes.push(ChipWrite::new(5100, "ym2151", 0, 0x08, 5));
        analyze(
            &_vgm_with_writes(writes),
            AnalyzeOptions { pcm: false, ..Default::default() },
        )
        .expect("song")
    }

    #[test]
    fn test_sweep_captured_with_end() {
        let song = _song_with_sweep();
        let drum = song
            .events
            .iter()
            .find(|e| e.on && e.ch == 5)
            .expect("drum note");
        assert!(drum.sweep.len() > 4);
        let deltas: Vec<i32> = drum.sweep.iter().map(|&(_s, d)| d).collect();
        assert!(*deltas.last().unwrap() < -200); // ~ -3.6 semitones in 1/64 units
        assert!(deltas[0] > *deltas.last().unwrap()); // descending
        assert_eq!(drum.end, 5100);
    }

    #[test]
    fn test_note_off_keeps_sweep_end() {
        let song = _song_with_sweep();
        let data = write_fur(
            &song,
            WriteOptions { include_pcm: false, include_fm: true, ..Default::default() },
        );
        let m = parse_fur(&data).expect("fur");
        let mut ramps: Vec<u8> = Vec::new();
        let mut stops: Vec<u8> = Vec::new();
        for (&(ch, _idx), rows) in m.patterns.iter() {
            if ch != 5 {
                continue;
            }
            for r in rows {
                for &(c, v) in &r.fx {
                    if c == 0x02 {
                        if v == 0 {
                            stops.push(v);
                        } else {
                            ramps.push(v);
                        }
                    }
                }
            }
        }
        assert!(!ramps.is_empty(), "no pitch ramp written");
        assert!(*ramps.iter().max().unwrap() > 0);
        assert!(!stops.is_empty(), "no ramp stop written");
    }

    #[test]
    fn test_short_setup_note_does_not_own_next_kc() {
        let writes = vec![
            ChipWrite::new(900, "ym2151", 0, 0x28 | 4, 0x40),
            ChipWrite::new(1000, "ym2151", 0, 0x08, 0x78 | 4),
            ChipWrite::new(1500, "ym2151", 0, 0x08, 4),
            ChipWrite::new(1900, "ym2151", 0, 0x28 | 4, 0x30),
            ChipWrite::new(2000, "ym2151", 0, 0x08, 0x78 | 4),
        ];
        let song = analyze(
            &_vgm_with_writes(writes),
            AnalyzeOptions { pcm: false, ..Default::default() },
        )
        .expect("song");
        let notes: Vec<&NoteEvent> =
            song.events.iter().filter(|e| e.on && e.ch == 4).collect();
        assert_eq!(notes.len(), 2);
        assert!(notes[0].sweep.is_empty());
        assert_eq!(notes[1].note, 97);
    }

    #[test]
    fn test_single_mid_note_step_lands_at_once() {
        // A driver that slams the octave mid-note and holds it is a hard pitch
        // change, not a glide: it lands as an instant transpose on its own row.
        let mut writes: Vec<ChipWrite> = (0..60)
            .map(|k| ChipWrite::new(k * 5000, "ym2151", 0, 0x08, 0x78))
            .collect();
        writes.push(ChipWrite::new(1000, "ym2151", 0, 0x08, 0x78 | 4));
        writes.push(ChipWrite::new(995, "ym2151", 0, 0x28 | 4, 0x40)); // set before key-on
        writes.push(ChipWrite::new(2000, "ym2151", 0, 0x28 | 4, 0x30)); // -12 key codes
        writes.push(ChipWrite::new(5100, "ym2151", 0, 0x08, 4));
        let song = analyze(
            &_vgm_with_writes(writes),
            AnalyzeOptions { pcm: false, ..Default::default() },
        )
        .expect("song");
        let note = song
            .events
            .iter()
            .find(|e| e.on && e.ch == 4)
            .expect("note");
        assert_eq!(note.pitch_steps, vec![(2000i64, -768i32)]);
        assert!(note.sweep.is_empty());
        let data = write_fur(
            &song,
            WriteOptions { include_pcm: false, include_fm: true, ..Default::default() },
        );
        let m = parse_fur(&data).expect("fur");
        let rows: Vec<&Vec<FurRow>> = m
            .patterns
            .iter()
            .filter(|&(&(ch, _i), _pat)| ch == 4)
            .map(|(_k, pat)| pat)
            .collect();
        let down: Vec<u8> = rows
            .iter()
            .flat_map(|pat| pat.iter())
            .flat_map(|r| r.fx.iter())
            .filter(|&&(c, _v)| c == 0xE9)
            .map(|&(_c, v)| v)
            .collect();
        assert_eq!(down, vec![12u8], "the octave should land as one hard transpose");
        let rates: Vec<u8> = rows
            .iter()
            .flat_map(|pat| pat.iter())
            .flat_map(|r| r.fx.iter())
            .filter(|&&(c, v)| (c == 0x01 || c == 0x02) && v != 0)
            .map(|&(_c, v)| v)
            .collect();
        assert!(rates.is_empty());
        let fx: Vec<(u8, u8)> = rows
            .iter()
            .flat_map(|pat| pat.iter())
            .flat_map(|r| r.fx.iter().copied())
            .collect();
        assert!(fx.iter().any(|&(c, _v)| c == 0xE9), "{:?}", fx);
        // The transpose carries the whole octave, so E5 keeps its full range.
        let edge: Vec<(u8, u8)> = fx
            .iter()
            .copied()
            .filter(|&(c, v)| c == 0xE5 && (v == 0x40 || v == 0xBF))
            .collect();
        assert!(edge.is_empty(), "{:?}", fx);
    }
}

fn _kc_for_units(units: i32) -> u16 {
    for kc in 0..256i32 {
        if kc_kf_units(kc, 0) == units {
            return kc as u16;
        }
    }
    panic!("{}", units);
}

mod legato_steps {
    use super::*;

    const BASE_KC: u16 = 0x42;
    const CH: u16 = 4;

    /// A held one- or two-step retune becomes E8/E9. Other motion stays a ramp.
    fn _song(semis: i32, key_on: i64, key_off: i64, at: i64) -> Song {
        let target = _kc_for_units(kc_kf_units(BASE_KC as i32, 0) + semis * 64);
        analyze(
            &note_song_with(
                key_on,
                key_off,
                BASE_KC,
                0,
                &[(at, 0x28 | CH, target)],
                CH,
            ),
            AnalyzeOptions { pcm: false, ..Default::default() },
        )
        .expect("song")
    }

    #[test]
    fn test_held_semitone_steps_become_e8_e9() {
        for semis in [1i32, 2, 7, 12, -12] {
            let song = _song(semis, 60000, 160000, 70000);
            let note = song
                .events
                .iter()
                .find(|e| e.on && e.ch == CH as i32)
                .expect("note");
            assert_eq!(note.pitch_steps, vec![(70000i64, semis * 64)]);
            assert!(note.sweep.is_empty());
            assert_eq!(song.legato_steps, 1);
            let m = parse_fur(&write_fur(
                &song,
                WriteOptions { include_pcm: false, include_fm: true, ..Default::default() },
            ))
            .expect("fur");
            let fx: Vec<(u8, u8)> = m
                .patterns
                .iter()
                .filter(|&(&(ch, _i), _rows)| ch == CH as u16)
                .flat_map(|(_k, rows)| rows.iter())
                .flat_map(|r| r.fx.iter().copied())
                .collect();
            let want = if semis < 0 { 0xE9u8 } else { 0xE8u8 };
            assert!(
                fx.iter().any(|&(c, _v)| c == want),
                "{}: {:?}",
                semis,
                fx
            );
        }
    }

    #[test]
    fn test_a_staircase_stays_a_ramp() {
        let writes = staircase(70000, 20, 500, BASE_KC, 0, CH);
        let song = analyze(
            &note_song_with(60000, 160000, BASE_KC, 0, &writes, CH),
            AnalyzeOptions { pcm: false, ..Default::default() },
        )
        .expect("song");
        let note = song
            .events
            .iter()
            .find(|e| e.on && e.ch == CH as i32)
            .expect("note");
        assert!(note.pitch_steps.is_empty());
        assert!(!note.sweep.is_empty());
    }

    #[test]
    fn test_a_release_tail_retune_stays_a_ramp() {
        let song = _song(7, 60000, 70000, 72000);
        let note = song
            .events
            .iter()
            .find(|e| e.on && e.ch == CH as i32)
            .expect("note");
        assert!(note.pitch_steps.is_empty());
        assert_eq!(song.legato_steps, 0);
    }
}

/// Step function over (sample, delta) pairs; 0 before the first write.
fn _traj_at(pts: &[(i64, i32)], t: f64) -> f64 {
    let mut value = 0i32;
    for &(s, d) in pts {
        if s as f64 > t {
            break;
        }
        value = d;
    }
    value as f64
}

mod sweep_encoder {
    use super::*;

    /// Structural guarantees of furwrite._emit_sweep.
    const HZ: f64 = 510.2018127441406; // speed 7 -> 605.1-sample rows

    fn _song(events: Vec<NoteEvent>) -> Song {
        let mut song = super::_song(HZ, 7, 4.0, _vgm_with_keyons(&[]));
        song.fm_patches = vec![FMPatch {
            alg: 4,
            fb: 0,
            ams: 0,
            pms: 0,
            ops: core::array::from_fn(|_| OpRegs::default()),
            ..Default::default()
        }];
        song.events = events;
        song
    }

    fn _rows(song: &Song, ch: i32) -> BTreeMap<i32, FurRow> {
        let m = parse_fur(&write_fur(
            song,
            WriteOptions { include_pcm: false, ..Default::default() },
        ))
        .expect("fur");
        let mut rows: BTreeMap<i32, FurRow> = BTreeMap::new();
        for (oi, &pat) in m.orders[ch as usize].iter().enumerate() {
            for (i, row) in m.patterns.get(&(ch as u16, pat)).expect("pattern").iter().enumerate() {
                rows.insert(oi as i32 * m.pat_len as i32 + i as i32, row.clone());
            }
        }
        rows
    }

    fn _rate(row: &FurRow) -> i32 {
        let mut rate = 0i32;
        for &(c, v) in &row.fx {
            if c == 0x02 {
                rate = -(v as i32);
            } else if c == 0x01 {
                rate = v as i32;
            }
        }
        rate
    }

    fn _note(sample: i64, ch: i32, on: bool, note: i32, ins: i32, vol: i32, end: i64) -> NoteEvent {
        let mut ev = NoteEvent::new(sample, ch, on, note, ins, vol, 0x80, 0);
        ev.end = end;
        ev
    }

    #[test]
    fn test_last_row_rates_full_span() {
        // A staircase dive that bottoms out and holds: the row containing the
        // final write must carry a rate that reaches the held value at the
        // row's *end*, or the pitch overshoots the bottom.
        let pts: Vec<(i64, i32)> = (0..18i64).map(|k| (1190 + 190 * k, -64 * (k as i32 + 1))).collect();
        let end = pts.last().unwrap().0;
        let mut ev = _note(1000, 2, true, 108, 0, -1, 20000);
        ev.sweep = pts.clone();
        let song = _song(vec![
            ev,
            _note(20000, 2, false, 180, -1, -1, 0),
        ]);
        let rows = _rows(&song, 2);
        let end_row = song.place(end, None).0;
        let expected = (_traj_at(&pts, song.samples_per_row() * (end_row + 1) as f64)
            - _traj_at(&pts, song.samples_per_row() * end_row as f64))
            / song.speed as f64;
        let got = _rate(rows.get(&end_row).expect("end row"));
        assert_almost_delta!(got, py_round(expected) as i32, 2);
        // The ramp lands on the final value at the row's end (no overshoot).
        let entry = _traj_at(&pts, song.samples_per_row() * end_row as f64);
        assert_almost_delta!(
            entry + got as f64 * song.speed as f64,
            pts.last().unwrap().1,
            8
        );
    }

    #[test]
    fn test_later_note_row_is_kept_clean() {
        // The dive's last writes run into the next note's row; the writer must
        // clip the sweep there instead of writing its tail rate/E5 onto the
        // new note's own row.
        let pts: Vec<(i64, i32)> = (0..22i64).map(|k| (1190 + 190 * k, -64 * (k as i32 + 1))).collect();
        assert!((pts.last().unwrap().0 as f64) > 8.0 * 605.1); // crosses into row 8
        let mut ev1 = _note(1000, 2, true, 108, 0, -1, 4900);
        ev1.sweep = pts.clone();
        let ev2 = _note(5000, 2, true, 108, 0, -1, 20000);
        let song = _song(vec![ev1, ev2.clone(), _note(20000, 2, false, 180, -1, -1, 0)]);
        let rows = _rows(&song, 2);
        let next_row = song.place(5000, None).0;
        // At most a stop: the key-on reads its own pitch.
        assert_eq!(_rate(rows.get(&next_row).expect("next row")), 0);
        // Its E5 may only be the note's own value (the replay restores it).
        assert!(
            !rows
                .get(&next_row)
                .unwrap()
                .fx
                .iter()
                .any(|&(c, v)| c == 0xE5 && v as i32 != ev2.e5),
            "the next note's row carries the previous sweep's fine tune"
        );
        // The tail still travels on the last row this sweep owns.
        assert_ne!(_rate(rows.get(&(next_row - 1)).expect("tail row")), 0);
    }

    #[test]
    fn test_hard_step_inside_a_row_lands_at_once() {
        // A hard step with still rows around it is a note change, not the
        // start of a slur.
        let row_len = 44100.0 * 7.0 / HZ;
        let pts = [
            ((2.0 * row_len) as i64, 0i32),
            ((2.0 * row_len) as i64 + 86, -300i32),
            (12000, -300i32),
        ];
        let mut ev = _note(1000, 2, true, 108, 0, -1, 20000);
        ev.sweep = pts.to_vec();
        let song = _song(vec![ev, _note(20000, 2, false, 180, -1, -1, 0)]);
        let rows = _rows(&song, 2);
        let fx = &rows.get(&2).expect("row 2").fx;
        let transpose: Vec<u8> = fx.iter().filter(|&&(c, _)| c == 0xE9).map(|&(_c, v)| v).collect();
        assert_eq!(transpose, vec![5u8]);
        // Five semitones down is -320, so the fine tune owes the row +20.
        let fine: Vec<u8> = fx.iter().filter(|&&(c, _)| c == 0xE5).map(|&(_c, v)| v).collect();
        assert_eq!(fine, vec![0x80 + 20]);
        assert!(!fx.iter().any(|&(c, v)| (c == 0x01 || c == 0x02) && v != 0));
    }

    #[test]
    fn test_isolated_jump_lands_on_transpose_plus_fine() {
        // The transpose and the E5 remainder have to add up to the driver's
        // step.
        let row_len = 44100.0 * 7.0 / HZ;
        let pts = [
            (1000 + (2.0 * row_len) as i64, 0i32),
            (1000 + (3.3 * row_len) as i64, -420i32),
        ];
        let mut ev = _note(1000, 2, true, 108, 0, -1, 20000);
        ev.sweep = pts.to_vec();
        let song = _song(vec![ev, _note(20000, 2, false, 180, -1, -1, 0)]);
        let rows = _rows(&song, 2);
        let cell = rows
            .values()
            .find(|r| r.fx.iter().any(|&(c, v)| c == 0xE9 && v != 0))
            .expect("transposed cell");
        let up: i32 = cell.fx.iter().filter(|&&(c, _)| c == 0xE8).map(|&(_c, v)| v as i32).sum();
        let down: i32 = cell.fx.iter().filter(|&&(c, _)| c == 0xE9).map(|&(_c, v)| v as i32).sum();
        let e5: Vec<u8> = cell.fx.iter().filter(|&&(c, _)| c == 0xE5).map(|&(_c, v)| v).collect();
        assert_eq!(down - up, 7); // the whole semitones it can spend
        assert!(!e5.is_empty(), "the remainder needs a fine tune");
        // Transpose plus fine tune have to add up to the driver's step.
        assert_eq!((up - down) * 64 + (e5[0] as i32 - 0x80), -420);
    }

    #[test]
    fn test_glides_keep_their_ramp() {
        // A chain of steps that carries on in the same direction is a slide:
        // it must stay a per-row ramp, however big each row's move is.
        let row_len = 44100.0 * 7.0 / HZ;
        let pts: Vec<(i64, i32)> = (1..=8i64)
            .map(|k| (1000 + (k as f64 * row_len / 2.0) as i64, -80i32 * k as i32))
            .collect();
        let mut ev = _note(1000, 2, true, 108, 0, -1, 20000);
        ev.sweep = pts;
        let song = _song(vec![ev, _note(20000, 2, false, 180, -1, -1, 0)]);
        let rows = _rows(&song, 2);
        assert!(
            !rows
                .values()
                .any(|r| r.fx.iter().any(|&(c, _v)| c == 0xE8 || c == 0xE9)),
            "a glide must not be chopped into quick legatos"
        );
        assert!(!rows.is_empty(), "no ramp written");
        for (row, cell) in rows.iter() {
            for &(c, v) in &cell.fx {
                if c == 0xE5 {
                    assert!(
                        (v as i32 - 0x80).abs() <= 16,
                        "row {}: E5 {:02X}",
                        row,
                        v
                    );
                }
            }
        }
    }
}

mod key_on_operator_mask {
    use super::*;

    const CH: u16 = 2;

    /// KON is per operator: only a 0->1 bit transition restarts an envelope.
    fn _writes(onsets: &[(i64, u32)], offs: &[i64]) -> Vec<ChipWrite> {
        let mut w: Vec<ChipWrite> = onsets
            .iter()
            .map(|&(t, m)| ChipWrite::new(t, "ym2151", 0, 0x08, (m << 3) | CH as u32))
            .collect();
        w.extend(
            offs.iter()
                .map(|&t| ChipWrite::new(t, "ym2151", 0, 0x08, CH as u32)),
        );
        w
    }

    fn _analyze(writes: Vec<ChipWrite>) -> Song {
        analyze(
            &_vgm_with_writes(writes),
            AnalyzeOptions { pcm: false, ..Default::default() },
        )
        .expect("song")
    }

    #[test]
    fn test_redundant_key_on_is_not_a_note() {
        let song = _analyze(_writes(&[(0, 0xF), (1000, 0xF)], &[2000]));
        assert_eq!(song.events.iter().filter(|e| e.on).count(), 1);
    }

    #[test]
    fn test_partial_operator_change_keeps_the_note() {
        // 0x78 -> 0x70 releases operator 4 while the other three sustain.
        let song = _analyze(_writes(&[(0, 0xF), (1000, 0xE)], &[2000]));
        assert_eq!(song.events.iter().filter(|e| e.on).count(), 1);
        assert_eq!(song.events.iter().filter(|e| !e.on).count(), 1);
    }

    #[test]
    fn test_partial_key_on_silences_unkeyed_operator() {
        let mut writes: Vec<ChipWrite> = [0x10u32, 0x20, 0x30, 0x40]
            .iter()
            .enumerate()
            .map(|(op, &tl)| {
                ChipWrite::new(0, "ym2151", 0, 0x60 + op as u32 * 8 + CH as u32, tl)
            })
            .collect();
        // The SS bits are the datasheet's S1,S3,S2,S4 register order.
        writes.extend(_writes(&[(10, 0xB)], &[1000])); // 1011: operator 2 un-keyed
        let song = _analyze(writes);
        let ev = song.events.iter().find(|e| e.on).expect("event");
        let patch = &song.fm_patches[ev.ins as usize];
        let tls: Vec<u8> = (0..4).map(|i| patch.ops[i].tl).collect();
        assert_eq!(tls, vec![0x10u8, 0x7F, 0x30, 0x40]);
    }

    #[test]
    fn test_keyon_bits_key_the_sounding_operators() {
        // Armed Police Batrider's real voice: $08 bits 0+1 key op1 and op3.
        let mut writes: Vec<ChipWrite> = [0x19u32, 0x7F, 0x06, 0x7F]
            .iter()
            .enumerate()
            .map(|(op, &tl)| {
                ChipWrite::new(0, "ym2151", 0, 0x60 + op as u32 * 8 + CH as u32, tl)
            })
            .collect();
        writes.push(ChipWrite::new(0, "ym2151", 0, 0x20 + CH as u32, 4)); // CON=4
        writes.extend(_writes(&[(10, 0x3)], &[1000])); // bits 0 and 1
        let song = _analyze(writes);
        let ev = song.events.iter().find(|e| e.on).expect("event");
        let patch = &song.fm_patches[ev.ins as usize];
        let tls: Vec<u8> = (0..4).map(|i| patch.ops[i].tl).collect();
        assert_eq!(tls, vec![0x19u8, 0x7F, 0x06, 0x7F]);
        assert!(
            patch.carrier_tl() < 0x7F,
            "a carrier left at TL 7F is a silent instrument"
        );
    }
}

mod opm_level_reach {
    use super::*;

    const CH: u32 = 0;
    const QUIET: i32 = 0x39;
    const LOUD: i32 = 0x1E;

    /// A crescendo that gets louder than its instrument must stay reachable.
    fn _writes() -> Vec<ChipWrite> {
        let mut w = vec![
            ChipWrite::new(1, "ym2151", 0, 0x60 + CH, 0x1E),
            ChipWrite::new(2, "ym2151", 0, 0x68 + CH, 0x1E),
            ChipWrite::new(3, "ym2151", 0, 0x70 + CH, QUIET as u32),
            ChipWrite::new(4, "ym2151", 0, 0x78 + CH, QUIET as u32),
            ChipWrite::new(5, "ym2151", 0, 0x20 + CH, 0xC4),
            // key on all four operators
            ChipWrite::new(10, "ym2151", 0, 0x08, (0xF << 3) | CH),
        ];
        // the climb: op3/op4 walk down (louder) while the note sounds.
        let mut t = 8000i64;
        let mut v = QUIET - 2;
        while v > LOUD {
            w.push(ChipWrite::new(t, "ym2151", 0, 0x70 + CH, v as u32));
            w.push(ChipWrite::new(t + 2, "ym2151", 0, 0x78 + CH, v as u32));
            t += 4000;
            v -= 2;
        }
        w.push(ChipWrite::new(t, "ym2151", 0, 0x70 + CH, LOUD as u32));
        w.push(ChipWrite::new(t + 2, "ym2151", 0, 0x78 + CH, LOUD as u32));
        w.push(ChipWrite::new(90000, "ym2151", 0, 0x08, CH));
        w
    }

    fn setup() -> (Song, NoteEvent) {
        let song = analyze(
            &_vgm_with_writes(_writes()),
            AnalyzeOptions { pcm: false, ..Default::default() },
        )
        .expect("song");
        let ons: Vec<&NoteEvent> = song.events.iter().filter(|e| e.on && !e.pcm).collect();
        assert_eq!(ons.len(), 1, "the synthetic voice is one held note");
        let ev = ons[0].clone();
        (song, ev)
    }

    #[test]
    fn test_algorithm_4_carriers_are_op3_and_op4() {
        // Measured across 49 YM2151 rips: automation is always op3+op4.
        assert_eq!(CARRIERS[4], &[2, 3][..]);
    }

    #[test]
    fn test_instrument_is_opened_to_the_loudest_requested_level() {
        let (song, ev) = setup();
        let patch = &song.fm_patches[ev.ins as usize];
        assert_eq!(
            patch.carrier_tl(),
            LOUD,
            "the carriers must reach the top of the crescendo"
        );
    }

    #[test]
    fn test_note_row_starts_attenuated_not_maxed() {
        // vol such that 127 - vol + LOUD == QUIET: the entrance is quiet
        // because the column says so, not because the instrument cannot go
        // louder.
        let (_song, ev) = setup();
        assert_eq!(ev.vol, 127 - (QUIET - LOUD));
        assert!(ev.vol < 127);
    }

    #[test]
    fn test_mid_note_steps_scale_from_the_same_floor() {
        let (song, ev) = setup();
        let patch = &song.fm_patches[ev.ins as usize];
        assert_eq!(ev.vol_tl_base, Some(LOUD));
        assert!(!ev.tl_pts.is_empty(), "the climb must still be recorded");
        // No step may ask for a level the instrument cannot reach at vol 127.
        let unreachable: Vec<i32> = ev
            .tl_pts
            .iter()
            .map(|&(_t, tl)| tl)
            .filter(|&tl| tl < patch.carrier_tl())
            .collect();
        assert!(unreachable.is_empty(), "levels below the instrument floor: {:?}", unreachable);
    }

    #[test]
    fn test_writers_volume_column_reaches_max_only_at_the_top() {
        let (song, ev) = setup();
        let patch = &song.fm_patches[ev.ins as usize];
        let data = write_fur(
            &song,
            WriteOptions { include_pcm: false, include_fm: true, ..Default::default() },
        );
        let byte = (patch.ops[2].tl & 0x7F) as u8;
        assert!(data.iter().any(|&b| b == byte));
        assert_eq!(ev.vol, 127 - (QUIET - LOUD));
    }
}

mod chip_clock_flags {
    use super::*;

    /// The rip's OPM clock must reach Furnace (its default is 3.579545 MHz).
    fn _song_with_clock(clock: u32) -> Song {
        let mut song = _song(60.0, 1, 1.0, _vgm_with_writes(Vec::new()));
        song.vgm.ym2151_clock = clock;
        song.events.push(NoteEvent::new(0, 0, true, 60, 0, -1, 0x80, 0));
        song
    }

    #[test]
    fn test_non_default_clock_is_written() {
        let data = write_fur(
            &_song_with_clock(4_000_000),
            WriteOptions { include_pcm: false, include_fm: true, ..Default::default() },
        );
        assert!(contains_subslice(&data, b"customClock=4000000"));
    }

    #[test]
    fn test_default_clock_writes_no_override() {
        let data = write_fur(
            &_song_with_clock(3_579_545),
            WriteOptions { include_pcm: false, include_fm: true, ..Default::default() },
        );
        assert!(!contains_subslice(&data, b"customClock"));
    }
}

mod clock_transposition {
    use super::*;

    #[test]
    fn test_offset_values() {
        assert_eq!(opm_pitch_offset(Some(3_579_545)), 0);
        assert_eq!(opm_pitch_offset(None), 0);
        assert_eq!(opm_pitch_offset(Some(4_000_000)), 123); // +1.92 st (Sega 16C)
    }

    #[test]
    fn test_shift_carries_the_fraction() {
        // 60 st + 32/64 st, shifted by +1 st + 59/64 st -> 62 st + 27/64 st
        let (note, e5) = shift_note_e5(60, 0x80 + 32, 123);
        assert_eq!(note, 62);
        assert_eq!(e5, 0x80 + 27);
    }

    #[test]
    fn test_song_notes_move_by_the_offset() {
        fn pitch(clock: u32) -> i32 {
            let writes = vec![
                ChipWrite::new(0, "ym2151", 0, 0x28, 0x4E),
                ChipWrite::new(0, "ym2151", 0, 0x30, 0x00),
                ChipWrite::new(1, "ym2151", 0, 0x08, 0x78),
            ];
            let mut vgm = _vgm_with_writes(writes);
            vgm.ym2151_clock = clock;
            let song = analyze(
                &vgm,
                AnalyzeOptions { pcm: false, ..Default::default() },
            )
            .expect("song");
            let ev = song.events.iter().find(|e| e.on).expect("event");
            ev.note * 64 + (ev.e5 - 0x80)
        }
        assert_eq!(pitch(4_000_000) - pitch(3_579_545), 123);
    }

    #[test]
    fn test_pcm_notes_are_not_transposed() {
        // PCM (ch 8-9) is pitched by its own chip clock, not the OPM's.
        let mut events = vec![
            NoteEvent::new(0, 0, true, 60, 0, -1, 0x80, 0),
            {
                let mut e = NoteEvent::new(0, 8, true, 60, 0, 15, 0x80, 0);
                e.pcm = true;
                e
            },
            {
                let mut e = NoteEvent::new(100, 9, true, 72, 1, 15, 0x80, 0);
                e.pcm = true;
                e
            },
            {
                let mut e = NoteEvent::new(0, 5, true, 60, 8, 127, 0x80, 0xFF);
                e.dac = true;
                e
            },
        ];
        transpose_opm_events(&mut events, Some(4_000_000), 8);
        assert_eq!((events[0].note, events[0].e5), (61, 0x80 + 59));
        assert_eq!((events[1].note, events[1].e5), (60, 0x80));
        assert_eq!((events[2].note, events[2].e5), (72, 0x80));
        assert_eq!((events[3].note, events[3].e5), (60, 0x80));
    }

    #[test]
    fn test_appended_fm_instruments_rebase_sample_slots() {
        let mut events = vec![
            {
                let mut e = NoteEvent::new(0, 5, true, 60, 8, 127, 0x80, 0xFF);
                e.dac = true;
                e
            },
            {
                let mut e = NoteEvent::new(0, 6, true, 60, 9, 15, 0x80, 0);
                e.tone_chip = 3;
                e
            },
            NoteEvent::new(0, 0, true, 60, 8, -1, 0x80, 0),
        ];
        vgm2fur::analyze::_rebase_non_fm_ins(&mut events, 8, 9);
        let ins: Vec<i32> = events.iter().map(|e| e.ins).collect();
        assert_eq!(ins, vec![9, 10, 8]);
    }
}

mod opm_lfo_depth {
    use super::*;

    const AMD: i32 = 0x1E;
    const PMD: i32 = 0x1F;

    /// The OPM resets PMD/AMD to 0, but Furnace's arcade core defaults 0x7f.
    #[test]
    fn test_reset_depth_is_sent_when_0x19_is_never_written() {
        let writes = vec![ChipWrite::new(0, "ym2151", 0, 0x20, 0xC0)];
        let song = analyze(
            &_vgm_with_writes(writes),
            AnalyzeOptions { pcm: false, ..Default::default() },
        )
        .expect("song");
        assert!(song
            .chip_fx
            .iter()
            .any(|&(s, c, v, _t, _ch)| s == 0 && c == AMD && v == 0));
        assert!(song
            .chip_fx
            .iter()
            .any(|&(s, c, v, _t, _ch)| s == 0 && c == PMD && v == 0));
    }

    #[test]
    fn test_pcm_only_module_carries_no_ym2151_effects() {
        let writes = vec![ChipWrite::new(0, "c140", 0, 0x00, 0x00)];
        let song = analyze(
            &_vgm_with_writes(writes),
            AnalyzeOptions { pcm: true, ..Default::default() },
        )
        .expect("song");
        assert_eq!(song.chip_fx, Vec::new());
    }

    #[test]
    fn test_driver_writes_override_the_reset() {
        let writes = vec![
            ChipWrite::new(100, "ym2151", 0, 0x19, 0x30),
            ChipWrite::new(200, "ym2151", 0, 0x19, 0x90),
        ];
        let song = analyze(
            &_vgm_with_writes(writes),
            AnalyzeOptions { pcm: false, ..Default::default() },
        )
        .expect("song");
        let amd: Vec<i32> = song
            .chip_fx
            .iter()
            .filter(|e| e.1 == AMD)
            .map(|e| e.2)
            .collect();
        let pmd: Vec<i32> = song
            .chip_fx
            .iter()
            .filter(|e| e.1 == PMD)
            .map(|e| e.2)
            .collect();
        assert_eq!(amd, vec![0, 0x30]); // reset, then the log's own value
        assert_eq!(pmd, vec![0, 0x10]);
    }
}

mod mid_note_carrier_tl {
    use super::*;

    /// A held voice's carrier TL writes are a software volume envelope.
    fn _song() -> Song {
        let writes = vec![
            ChipWrite::new(0, "ym2151", 0, 0x20, 0xC0),
            ChipWrite::new(0, "ym2151", 0, 0x60, 0x10),
            ChipWrite::new(0, "ym2151", 0, 0x68, 0x10),
            ChipWrite::new(0, "ym2151", 0, 0x70, 0x10),
            ChipWrite::new(0, "ym2151", 0, 0x78, 0x10),
            ChipWrite::new(0, "ym2151", 0, 0x28, 0x4A),
            ChipWrite::new(1000, "ym2151", 0, 0x08, 0x78),
            ChipWrite::new(5000, "ym2151", 0, 0x78, 0x20),
            ChipWrite::new(9000, "ym2151", 0, 0x78, 0x30),
            ChipWrite::new(20000, "ym2151", 0, 0x08, 0x00),
        ];
        analyze(
            &_vgm_with_writes(writes),
            AnalyzeOptions { pcm: false, ..Default::default() },
        )
        .expect("song")
    }

    #[test]
    fn test_fade_steps_are_captured() {
        let song = _song();
        let ev = song.events.iter().find(|e| e.on).expect("event");
        assert_eq!(ev.tl_pts, vec![(5000i64, 0x20i32), (9000, 0x30)]);
    }

    #[test]
    fn test_modulator_tl_write_is_not_a_fade() {
        // op 1 (0x60) is a modulator under alg 0; moving it must not fake a
        // carrier volume step.
        let writes = vec![
            ChipWrite::new(0, "ym2151", 0, 0x20, 0xC0),
            ChipWrite::new(0, "ym2151", 0, 0x78, 0x10),
            ChipWrite::new(1000, "ym2151", 0, 0x08, 0x78),
            ChipWrite::new(5000, "ym2151", 0, 0x60, 0x50),
            ChipWrite::new(20000, "ym2151", 0, 0x08, 0x00),
        ];
        let song = analyze(
            &_vgm_with_writes(writes),
            AnalyzeOptions { pcm: false, ..Default::default() },
        )
        .expect("song");
        let ev = song.events.iter().find(|e| e.on).expect("event");
        assert!(ev.tl_pts.is_empty());
    }

    #[test]
    fn test_fade_becomes_a_volume_column() {
        let song = _song();
        let m = parse_fur(&write_fur(
            &song,
            WriteOptions { include_pcm: false, include_fm: true, ..Default::default() },
        ))
        .expect("fur");
        let rows = m
            .patterns
            .get(&(0u16, m.orders[0][0]))
            .expect("pattern");
        let vols: std::collections::BTreeSet<i32> = rows
            .iter()
            .map(|r| r.vol)
            .filter(|&v| v != -1 && v != 0xFFFF)
            .collect();
        assert!(vols.contains(&127)); // the note's own instrument TL
        assert!(vols.contains(&(127 - 0x10))); // carrier TL 0x20 vs 0x10
        assert!(vols.contains(&(127 - 0x20))); // carrier TL 0x30 vs 0x10
    }
}
