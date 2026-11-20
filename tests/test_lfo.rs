//! The driver-scripted vibrato ramp (PMS/AMS) becoming a Furnace macro.
//!
//! A macro holds its last value and its step cannot exceed 255 ticks, so the
//! ramp has to carry the driver's reset to 0 or the vibrato never stops.
//!
//! Port of `tests/test_lfo.py` at vgm2151fur a8edb6e.

mod common;

use common::*;
use indexmap::IndexMap;
use vgm2fur::analyze::{_flush_pms_ramp, analyze, format_report, AnalyzeOptions, PmsRamp};
use vgm2fur::furwrite::{_build_patterns, EmitHooks, PAT_LEN_MAX};
use vgm2fur::model::{NoteEvent, Song};
use vgm2fur::vgm::ChipWrite;

mod test_pms_ramp {
    use super::*;

    #[test]
    fn test_reset_is_kept_so_the_macro_turns_off() {
        let mut acc: IndexMap<i32, Vec<PmsRamp>> = IndexMap::new();
        _flush_pms_ramp(&mut acc, 2, &[(100, 0, 0), (500, 6, 0), (900, 0, 0)], 0);
        let (pms, _ams, deltas) = &acc[&2][0];
        assert_eq!(*pms, vec![0, 6, 0]);
        assert_eq!(deltas.len(), 3);
    }

    #[test]
    fn test_ramp_without_a_reset_is_unchanged() {
        let mut acc: IndexMap<i32, Vec<PmsRamp>> = IndexMap::new();
        _flush_pms_ramp(&mut acc, 1, &[(100, 0, 0), (500, 6, 0)], 0);
        let (pms, _ams, _deltas) = &acc[&1][0];
        assert_eq!(*pms, vec![0, 6]);
    }

    #[test]
    fn test_lead_in_zero_is_inserted() {
        let mut acc: IndexMap<i32, Vec<PmsRamp>> = IndexMap::new();
        _flush_pms_ramp(&mut acc, 3, &[(400, 6, 0)], 100);
        let (pms, _ams, deltas) = &acc[&3][0];
        assert_eq!(*pms, vec![0, 6]);
        assert_eq!(deltas[0], 300);
    }
}

mod test_per_note_pms_macro {
    use super::*;

    fn song() -> Song {
        let mut writes = Vec::new();
        for &(reg, val) in PATCH.iter() {
            writes.push(ChipWrite::new(0, "ym2151", 0, reg as u32, val as u32));
        }
        writes.extend([
            ChipWrite::new(0, "ym2151", 0, 0x18, 0xD0),
            ChipWrite::new(0, "ym2151", 0, 0x19, 0x88),
            ChipWrite::new(0, "ym2151", 0, 0x1B, 0x02),
            ChipWrite::new(59000, "ym2151", 0, 0x28, 0x48),
            ChipWrite::new(59000, "ym2151", 0, 0x30, 0x00),
            ChipWrite::new(60000, "ym2151", 0, 0x08, 0x78),
            ChipWrite::new(100000, "ym2151", 0, 0x08, 0x00),
            ChipWrite::new(159000, "ym2151", 0, 0x28, 0x4A),
            ChipWrite::new(159000, "ym2151", 0, 0x30, 0x00),
            ChipWrite::new(160000, "ym2151", 0, 0x08, 0x78),
        ]);
        for (k, val) in [0x10u32, 0x21, 0x31, 0x42, 0x52, 0x63, 0x73]
            .iter()
            .enumerate()
        {
            writes.push(ChipWrite::new(
                200000 + 200 * k as i64,
                "ym2151",
                0,
                0x38,
                *val,
            ));
        }
        writes.push(ChipWrite::new(260000, "ym2151", 0, 0x08, 0x00));
        writes.sort_by_key(|w| w.sample);
        analyze(&vgm(writes), AnalyzeOptions::default()).expect("analyze")
    }

    fn on_channel_zero(song: &Song) -> Vec<NoteEvent> {
        song.events
            .iter()
            .filter(|e| e.on && e.ch == 0)
            .cloned()
            .collect()
    }

    #[test]
    fn test_constant_note_may_share_a_ramp_that_starts_later() {
        // The first note is constant; the second starts its ramp after the
        // first has keyed off. Sharing the longer instrument is safe and saves
        // a slot: the first note only plays the leading zeros.
        let song = song();
        let notes = on_channel_zero(&song);
        assert_eq!(notes.len(), 2);
        let (first, second) = (&notes[0], &notes[1]);
        let patch = &song.fm_patches[first.ins as usize];
        let tick = 44100.0 / song.hz;
        let heard = std::cmp::max(1, ((first.end - first.sample) as f64 / tick).ceil() as usize);
        // Python slicing past the end returns the whole macro.
        let heard = heard.min(patch.fms_macro.len());
        assert!(patch.fms_macro[..heard].iter().all(|&v| v == 0));
        assert_eq!(first.ins, second.ins);
        assert_eq!(
            *song.fm_patches[second.ins as usize]
                .fms_macro
                .iter()
                .max()
                .unwrap(),
            7
        );
    }

    #[test]
    fn test_late_ramp_keeps_its_lead_in() {
        let song = song();
        let notes = on_channel_zero(&song);
        let patch = &song.fm_patches[notes[1].ins as usize];
        assert_eq!(*patch.fms_macro.iter().max().unwrap(), 7);
        assert!(patch.fms_macro.len() > 8);
        assert_eq!(patch.fms_macro[0], 0);
    }
}

mod test_opm_test_pulse {
    use super::*;

    fn song() -> (Song, i64) {
        let key_on = 60000i64;
        let mut writes = lattice(40, 5000);
        for &(reg, val) in PATCH.iter() {
            writes.push(ChipWrite::new(
                key_on - 1000,
                "ym2151",
                0,
                reg as u32,
                val as u32,
            ));
        }
        writes.push(ChipWrite::new(key_on - 3000, "ym2151", 0, 0x18, 0x0A));
        writes.push(ChipWrite::new(key_on, "ym2151", 0, 0x28, 0x48));
        writes.push(ChipWrite::new(key_on, "ym2151", 0, 0x30, 0x00));
        writes.push(ChipWrite::new(key_on, "ym2151", 0, 0x08, 0x78));
        // The driver pulses TEST bit 1 right after the key-on.
        writes.push(ChipWrite::new(key_on, "ym2151", 0, 0x01, 0x02));
        writes.push(ChipWrite::new(key_on, "ym2151", 0, 0x01, 0x00));
        writes.sort_by_key(|w| w.sample);
        (
            analyze(&vgm(writes), AnalyzeOptions::default()).expect("analyze"),
            key_on,
        )
    }

    #[test]
    fn test_pulse_and_restore_are_both_emitted() {
        let (song, key_on) = song();
        // chip_fx entries are (sample, effect, value, tick offset, channel);
        // the Python 3-tuples are the tick=0, channel=0 form and the 4-tuples
        // carry a nonzero tick offset.
        let fx: Vec<&(i64, i32, i32, i32, i32)> =
            song.chip_fx.iter().filter(|e| e.1 == 0x17).collect();
        assert!(
            fx.iter().any(|e| e.3 == 0 && e.4 == 0 && e.2 == 0),
            "the 0x02 edge must become 17 00"
        );
        assert!(
            fx.iter()
                .any(|e| e.4 == 0 && e.2 == 0x0A && e.3 == 1),
            "the restore must carry the LFO speed one tick later"
        );
        assert!(fx.iter().all(|e| e.0 <= key_on + 2));
    }

    #[test]
    fn test_report_handles_the_tick_offset_restore() {
        // The restore entry is a 4-tuple with the tick offset. Report's LFO
        // count must not unpack it as a 3-tuple.
        let (song, _key_on) = song();
        assert!(format_report(&song).contains("LFO fx"));
    }

    #[test]
    fn test_restore_lands_one_tick_after_the_pulse() {
        let (song, _key_on) = song();
        let mut warnings = Vec::new();
        let mut hooks = EmitHooks::default();
        let (_orders, patterns, _loop_order, _pat_len) = _build_patterns(
            &song,
            8,
            PAT_LEN_MAX,
            8,
            0,
            None,
            0,
            &mut warnings,
            &mut hooks,
        );
        let mut cells: Vec<(i32, i32, i32, i32)> = Vec::new();
        for (ch, _idx, rows) in &patterns {
            for cell in rows {
                let Some(cell) = cell else { continue };
                let Some(fx) = cell.fx.as_ref() else {
                    continue;
                };
                for &(cmd, val) in fx {
                    if cmd == 0x17 && (val == 0 || val == 0x0A) {
                        let ed = fx
                            .iter()
                            .find(|&&(c, _)| c == 0xED)
                            .map(|&(_, v)| v)
                            .unwrap_or(0);
                        cells.push((*ch, cmd, val, ed));
                    }
                }
            }
        }
        assert!(cells.contains(&(0, 0x17, 0x00, 0)));
        assert!(
            cells
                .iter()
                .any(|c| c.1 == 0x17 && c.2 == 0x0A && c.3 == 1),
            "{:?}",
            cells
        );
    }
}
