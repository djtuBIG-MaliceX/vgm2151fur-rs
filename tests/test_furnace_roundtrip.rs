//! Furnace round trips: read back what the engine does with our effects.
//!
//! Port of `tests/test_furnace_roundtrip.py`. Every case is skipped unless the
//! bundled Furnace build is found, exactly like the reference's
//! `skipUnless(HAVE_FURNACE, ...)`.

mod common;

use std::path::PathBuf;

use vgm2fur::analyze::{analyze, AnalyzeOptions};
use vgm2fur::diag::{kc_kf_stream, register_writes, state_at};
use vgm2fur::furwrite::{write_fur, WriteOptions};
use vgm2fur::model::Song;
use vgm2fur::vgm::{load_vgm, ChipWrite, VgmFile};

const BEND_CH: i32 = 4;
const BASE_KC: u16 = 0x3E;

fn tmp_dir() -> PathBuf {
    let dir = std::env::temp_dir().join("vgm2fur_roundtrip");
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir
}

fn write_options() -> WriteOptions<'static> {
    WriteOptions { include_pcm: false, include_fm: true, ..Default::default() }
}

/// Write the module and export it to VGM through Furnace, then read it back.
fn export(song: &Song, name: &str) -> VgmFile {
    let dir = tmp_dir();
    let fur = dir.join(format!("{name}.fur"));
    std::fs::write(&fur, write_fur(song, write_options())).expect("write fur");
    let out = dir.join(format!("{name}.vgm"));
    vgm2fur::furnace::export_vgm(&fur, &out).expect("export");
    load_vgm(&out).expect("load export")
}

mod TestLegatoSemantics {
    use super::*;

    #[test]
    fn test_transposes_accumulate() {
        skip_without_furnace!();
        let mut song = analyze(
            &common::note_song_with(60_000, 160_000, 0, 0, &[], 0),
            AnalyzeOptions { pcm: false, ..Default::default() },
        )
        .expect("analyze");
        let dir = tmp_dir();
        let fur_path = dir.join("legato.fur");
        let plan = [(2, 0xE8, 0x02), (5, 0xE8, 0x03), (8, 0xE9, 0x01)];
        let row0 = common::inject_rows(&mut song, &plan, 60_000, |song, row0, hooks| {
            let data = vgm2fur::furwrite::write_fur_with_hooks(
                song,
                write_options(),
                hooks,
            );
            std::fs::write(&fur_path, data).expect("write fur");
            row0
        });
        let out = dir.join("legato.vgm");
        vgm2fur::furnace::export_vgm(&fur_path, &out).expect("export");
        let export_vgm_file = load_vgm(&out).expect("load export");

        let stream = kc_kf_stream(&export_vgm_file, 0);
        let base = state_at(&stream, 60_500).expect("a base pitch");
        let sprow = song.samples_per_row();
        let at = |row: i32| -> i32 {
            let t = song.t0 as f64 + (row + 2) as f64 * sprow;
            state_at(&stream, t as i64).expect("a pitch")
        };
        assert_eq!(at(row0 + 2) - base, 2 * 64);
        assert_eq!(at(row0 + 5) - base, 5 * 64);
        assert_eq!(at(row0 + 8) - base, 4 * 64);
    }
}

mod TestLfoEffects {
    use super::*;

    #[test]
    fn test_roundtrip_to_registers() {
        skip_without_furnace!();
        let mut song = analyze(
            &common::note_song_with(60_000, 160_000, 0, 0, &[], 0),
            AnalyzeOptions { pcm: false, ..Default::default() },
        )
        .expect("analyze");
        song.chip_fx = vec![
            (60_500, 0x17, 0xCA, 0, 0),
            (61_500, 0x18, 0x02, 0, 0),
            (62_500, 0x1F, 0x2D, 0, 0),
            (64_000, 0x1E, 0x11, 0, 0),
        ];
        let export_file = export(&song, "lfo");
        let rows = register_writes(&export_file, "ym2151", 0, &[0x18, 0x19, 0x1B], 0, None, true);
        let values: Vec<(i32, i32)> = rows.iter().map(|(_t, reg, val)| (*reg, *val as i32)).collect();
        assert!(values.contains(&(0x18, 0xCA)), "{values:?}");
        assert!(values.contains(&(0x1B, 0x02)), "{values:?}");
        assert!(values.contains(&(0x19, 0xAD)), "{values:?}");
        assert!(values.contains(&(0x19, 0x11)), "{values:?}");
    }
}

/// One note on channel 4 that bends up steeply from its own key-on.
fn bend_song(key_on: i64, semitones: usize, step_samples: i64) -> (Song, VgmFile, Vec<(i64, u16, u16)>) {
    let mut writes = common::lattice(80, 5000);
    let mut regs: Vec<(u16, u16)> = vec![(0x20 | BEND_CH as u16, 0xC7)];
    for op in 0..4u16 {
        regs.push((0x40 + 8 * op | BEND_CH as u16, 0x11));
        regs.push((0x60 + 8 * op | BEND_CH as u16, 0x20));
        regs.push((0x80 + 8 * op | BEND_CH as u16, 0x1F));
        regs.push((0xA0 + 8 * op | BEND_CH as u16, 0x05));
        regs.push((0xC0 + 8 * op | BEND_CH as u16, 0x00));
        regs.push((0xE0 + 8 * op | BEND_CH as u16, 0x5F));
    }
    for (reg, val) in regs {
        writes.push(ChipWrite::new(key_on - 100, "ym2151", 0, reg as u32, val as u32));
    }
    writes.push(ChipWrite::new(key_on - 50, "ym2151", 0, (0x28 | BEND_CH as u16) as u32, BASE_KC as u32));
    writes.push(ChipWrite::new(key_on - 50, "ym2151", 0, (0x30 | BEND_CH as u16) as u32, 0x00));
    writes.push(ChipWrite::new(key_on, "ym2151", 0, 0x08, (0x78 | BEND_CH) as u32));
    let bend = common::climb(
        key_on + 30,
        semitones,
        step_samples,
        BASE_KC,
        0x00,
        BEND_CH as u16,
    );
    for &(sample, reg, val) in &bend {
        writes.push(ChipWrite::new(sample, "ym2151", 0, reg as u32, val as u32));
    }
    writes.push(ChipWrite::new(
        bend.last().expect("a bend").0 + 40_000,
        "ym2151",
        0,
        0x08,
        BEND_CH as u32,
    ));
    writes.sort_by_key(|w| w.sample);
    let v = common::vgm(writes);
    let song = analyze(&v, AnalyzeOptions { pcm: false, ..Default::default() }).expect("analyze");
    (song, v, bend)
}

mod TestSweepModel {
    use super::*;

    #[test]
    fn test_staircase_climb_lands_on_pitch() {
        skip_without_furnace!();
        let writes = common::staircase(61_000, 20, 500, 0x00, 0x00, 0);
        let src_vgm = common::note_song_with(60_000, 160_000, 0x00, 0x00, &writes, 0);
        let song = analyze(&src_vgm, AnalyzeOptions { pcm: false, ..Default::default() })
            .expect("analyze");
        assert!(song.events.iter().any(|ev| ev.on && !ev.sweep.is_empty()));
        let export_file = export(&song, "climb");

        let src_stream = kc_kf_stream(&src_vgm, 0);
        let exp_stream = kc_kf_stream(&export_file, 0);
        let t_end = writes.last().expect("a step").0 + 2000;
        let src_pitch = state_at(&src_stream, t_end).expect("source pitch");
        let exp_pitch = state_at(&exp_stream, t_end).expect("export pitch");
        let delta = exp_pitch - src_pitch;
        assert!(
            delta.abs() <= 8,
            "pitch drifted {delta} units ({:.2} st) after the climb",
            delta as f64 / 64.0
        );
    }

    #[test]
    fn test_bend_on_a_delayed_key_on_lands_on_pitch() {
        skip_without_furnace!();
        let probe = analyze(
            &common::note_song_with(60_000, 160_000, 0, 0, &[], 0),
            AnalyzeOptions { pcm: false, ..Default::default() },
        )
        .expect("analyze");
        let tick = probe.samples_per_row() / probe.speed as f64;
        let row0 = probe.place(60_000, None).0;
        // int(probe.t0 + ((row0 + 3) * probe.speed + 3.6) * tick)
        let key_on = (probe.t0 as f64
            + (((row0 + 3) * probe.speed) as f64 + 3.6) * tick) as i64;
        let (song, src_vgm, bend) = bend_song(key_on, 8, 130);
        let (row1, ed1) = song.place(key_on, None);
        let ev = song
            .events
            .iter()
            .find(|e| e.on && e.ch == BEND_CH)
            .expect("the bend note");
        assert!(!ev.sweep.is_empty(), "the bend should stay a ramp");
        // ((song.t0 + (row1 + 1) * song.samples_per_row) - key_on) / tick
        let span_model = (song.t0 as f64
            + (row1 + 1) as f64 * song.samples_per_row()
            - key_on as f64)
            / tick;
        assert!(
            span_model - (song.speed - ed1) as f64 > 0.3,
            "no tick to lose: model {span_model:.2}, engine {}",
            song.speed - ed1
        );

        let export_file = export(&song, "delayed_bend");
        let src_stream = kc_kf_stream(&src_vgm, BEND_CH);
        let exp_stream = kc_kf_stream(&export_file, BEND_CH);
        let t_end = bend.last().expect("a bend").0 + 6000;
        let src_pitch = state_at(&src_stream, t_end).expect("source pitch");
        let exp_pitch = state_at(&exp_stream, t_end).expect("export pitch");
        let delta = exp_pitch - src_pitch;
        assert!(
            delta.abs() <= 8,
            "a bend on a row delayed {ed1} ticks landed {delta} units ({:.2} st) off",
            delta as f64 / 64.0
        );
    }
}
