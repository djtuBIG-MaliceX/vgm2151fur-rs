//! The diagnostic helpers, on synthetic VGMs. No Furnace needed.

mod common;

use common::*;
use vgm2fur::diag::{kc_kf_stream, pitch_runs, register_writes, state_at};
use vgm2fur::vgm::ChipWrite;

mod TestPitchRuns {
    use super::*;

    #[test]
    fn test_identical_streams_have_no_runs() {
        let song = note_song_with(60_000, 160_000, 0x00, 0x00, &[], 0);
        assert_eq!(pitch_runs(&song, &song, 0, 0.5, 50.0), Vec::new());
    }

    #[test]
    fn test_held_divergence_is_reported_with_span() {
        let a = note_song_with(60_000, 160_000, 0x00, 0x00, &[], 0);
        let writes = [
            (100_000i64, 0x28u32, 0x02u32), // +2 st inside the held note
            (120_000, 0x28, 0x00),          // back
        ];
        let extra: Vec<ChipWrite> = writes
            .iter()
            .map(|&(sample, reg, val)| ChipWrite::new(sample, "ym2151", 0, reg, val))
            .collect();
        let mut all: Vec<ChipWrite> = a.writes.clone();
        all.extend(extra);
        all.sort_by_key(|w| w.sample);
        let b = vgm(all);
        let runs = pitch_runs(&a, &b, 0, 0.5, 100.0);
        assert_eq!(runs.len(), 1);
        assert_almost_eq!(runs[0].worst_st, 2.0, 2);
        assert!(runs[0].t1 - runs[0].t0 > 19_000);
    }

    #[test]
    fn test_short_glitch_is_ignored() {
        let a = note_song_with(60_000, 160_000, 0x00, 0x00, &[], 0);
        let extra = [ChipWrite::new(100_000, "ym2151", 0, 0x28, 0x02)];
        let mut all: Vec<ChipWrite> = a.writes.clone();
        all.extend(extra);
        all.sort_by_key(|w| w.sample);
        let b = vgm(all);
        assert_eq!(pitch_runs(&a, &b, 0, 0.5, 500.0), Vec::new());
    }
}

mod TestStreams {
    use super::*;

    #[test]
    fn test_kc_kf_stream_tracks_each_write() {
        let song = note_song_with(60_000, 160_000, 0x42, 0x14, &[], 0);
        let stream = kc_kf_stream(&song, 0);
        let first = state_at(&stream, 60_000);
        let second = state_at(&stream, 100_000);
        assert_eq!(first, second); // nothing moves in a plain note
    }

    #[test]
    fn test_register_writes_filter_changes() {
        let song = note_song_with(60_000, 160_000, 0x00, 0x00, &[], 0);
        let rows = register_writes(&song, "ym2151", 0, &[0x28, 0x30], 0, None, true);
        assert!(!rows.is_empty());
        assert!(rows.iter().all(|(_t, reg, _v)| *reg == 0x28 || *reg == 0x30));
    }
}
