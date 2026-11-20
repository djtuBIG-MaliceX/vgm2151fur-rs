//! Left-to-right macro merging: a truncated copy shares the longer instrument.
//!
//! A short note often produces a macro that is the prefix of the same note held
//! longer. The next key-on comes before the extra tail is heard, so the longer
//! instrument can play both. The merge only applies when *all* macros agree from
//! the beginning and the non-macro timbre is identical.

mod common;

use indexmap::IndexMap;

use vgm2fur::analyze::{
    _merge_ay_volume_offsets, _merge_fm_prefix_macros, _merge_tone_prefixes,
};
use vgm2fur::fm::{FMPatch, OpRegs};
use vgm2fur::model::{NoteEvent, ToneIns};

fn note(ins: i32) -> NoteEvent {
    NoteEvent::new(ins as i64 * 1000, 0, true, 60, ins, 127, 0x80, 0)
}

fn with_end(mut ev: NoteEvent, ins: i32) -> NoteEvent {
    ev.end = ins as i64 * 1000 + 500;
    ev
}

fn fm(macro_vals: Vec<i32>, speed: i32) -> FMPatch {
    FMPatch {
        alg: 7,
        fb: 0,
        ams: 0,
        pms: 0,
        ops: core::array::from_fn(|_| OpRegs::default()),
        op_mul_macro: vec![(2, macro_vals, speed)],
        ..FMPatch::default()
    }
}

mod TestFMPrefixMerge {
    use super::*;

    #[test]
    fn test_a_prefix_copy_shares_the_longer_macro() {
        let mut patches = vec![fm(vec![0, 1], 1), fm(vec![0, 1, 2, 3], 1)];
        let mut events = vec![with_end(note(0), 0), with_end(note(1), 1)];
        assert_eq!(_merge_fm_prefix_macros(&mut events, &mut patches, 1000.0), 1);
        assert_eq!(patches.len(), 1);
        assert_eq!(events.iter().map(|e| e.ins).collect::<Vec<_>>(), vec![0, 0]);
        assert_eq!(patches[0].op_mul_macro[0].1, vec![0, 1, 2, 3]);
    }

    #[test]
    fn test_a_different_speed_or_middle_value_does_not_merge() {
        let mut patches = vec![fm(vec![0, 1], 1), fm(vec![0, 1, 2], 2)];
        let mut events = vec![with_end(note(0), 0), with_end(note(1), 1)];
        assert_eq!(_merge_fm_prefix_macros(&mut events, &mut patches, 1000.0), 0);
        assert_eq!(patches.len(), 2);

        let mut patches = vec![fm(vec![0, 1], 1), fm(vec![0, 9, 1], 1)];
        let mut events = vec![with_end(note(0), 0), with_end(note(1), 1)];
        assert_eq!(_merge_fm_prefix_macros(&mut events, &mut patches, 1000.0), 0);
        assert_eq!(patches.len(), 2);
    }

    #[test]
    fn test_a_different_timbre_does_not_merge() {
        let short = fm(vec![0, 1], 1);
        let mut long = fm(vec![0, 1, 2], 1);
        long.alg = 6;
        let mut patches = vec![short, long];
        let mut events = vec![with_end(note(0), 0), with_end(note(1), 1)];
        assert_eq!(_merge_fm_prefix_macros(&mut events, &mut patches, 1000.0), 0);
        assert_eq!(patches.len(), 2);
    }
}

fn ay_note(ins: i32, vol: i32) -> NoteEvent {
    let mut ev = NoteEvent::new(ins as i64 * 1000, 16 + ins, true, 60, ins, vol, 0x80, 0);
    ev.end = ins as i64 * 1000 + 500;
    ev.tone_chip = 2;
    ev
}

fn tone_ins(wave: i32, vol: Vec<i32>, pitch: Vec<i32>, pitch_speed: i32) -> ToneIns {
    ToneIns { wave, vol, pitch, pitch_speed, ..ToneIns::default() }
}

mod TestAyVolumeOffsetMerge {
    use super::*;

    #[test]
    fn test_offsets_up_to_four_share_the_loudest_macro() {
        let mut bucket = vec![
            tone_ins(1, vec![10, 9, 8], Vec::new(), 1),
            tone_ins(1, vec![12, 11, 10], Vec::new(), 1),
            tone_ins(1, vec![14, 13, 12], Vec::new(), 1),
        ];
        let mut events = vec![ay_note(0, 15), ay_note(1, 15), ay_note(2, 15)];
        let notes = vec![(0usize, 0usize), (1, 1), (2, 2)];
        let mapping = _merge_ay_volume_offsets(&mut bucket, &notes, &mut events);
        assert_eq!(bucket.len(), 1);
        assert_eq!(mapping[&0], mapping[&1]);
        assert_eq!(mapping[&0], mapping[&2]);
        assert_eq!(bucket[0].vol, vec![14, 13, 12]);
        // Each quieter note loses exactly the macro offset on its column:
        // effective = macro + column - 15 is unchanged.
        assert_eq!(events.iter().map(|ev| ev.vol).collect::<Vec<_>>(), vec![11, 13, 15]);
    }

    #[test]
    fn test_a_full_range_offset_merges_when_the_column_allows() {
        let mut bucket = vec![
            tone_ins(1, vec![0, 0], Vec::new(), 1),
            tone_ins(1, vec![15, 15], Vec::new(), 1),
        ];
        let mut events = vec![ay_note(0, 15), ay_note(1, 15)];
        let notes = vec![(0usize, 0usize), (1, 1)];
        let mapping = _merge_ay_volume_offsets(&mut bucket, &notes, &mut events);
        assert_eq!(bucket.len(), 1);
        assert_eq!(mapping[&0], mapping[&1]);
        assert_eq!(bucket[0].vol, vec![15, 15]);
        assert_eq!(events.iter().map(|ev| ev.vol).collect::<Vec<_>>(), vec![0, 15]);
    }

    #[test]
    fn test_the_column_floor_stops_a_large_offset() {
        let mut bucket = vec![
            tone_ins(1, vec![0, 0], Vec::new(), 1),
            tone_ins(1, vec![15, 15], Vec::new(), 1),
        ];
        let mut events = vec![ay_note(0, 14), ay_note(1, 15)];
        let notes = vec![(0usize, 0usize), (1, 1)];
        let mapping = _merge_ay_volume_offsets(&mut bucket, &notes, &mut events);
        assert_eq!(bucket.len(), 2);
        assert_ne!(mapping[&0], mapping[&1]);
    }

    #[test]
    fn test_a_nonconstant_difference_stays_separate() {
        let mut bucket = vec![
            tone_ins(1, vec![10, 9], Vec::new(), 1),
            tone_ins(1, vec![10, 8], Vec::new(), 1),
        ];
        let mut events = vec![ay_note(0, 15), ay_note(1, 15)];
        let notes = vec![(0usize, 0usize), (1, 1)];
        let mapping = _merge_ay_volume_offsets(&mut bucket, &notes, &mut events);
        assert_eq!(bucket.len(), 2);
        assert_ne!(mapping[&0], mapping[&1]);
    }

    #[test]
    fn test_another_macro_must_match() {
        let mut bucket = vec![
            tone_ins(1, vec![10, 9], vec![0], 1),
            tone_ins(1, vec![11, 10], vec![1], 1),
        ];
        let mut events = vec![ay_note(0, 15), ay_note(1, 15)];
        let notes = vec![(0usize, 0usize), (1, 1)];
        let mapping = _merge_ay_volume_offsets(&mut bucket, &notes, &mut events);
        assert_eq!(bucket.len(), 2);
        assert_ne!(mapping[&0], mapping[&1]);
    }
}

mod TestTonePrefixMerge {
    use super::*;

    #[test]
    fn test_pitch_and_volume_prefixes_share_the_longer_instrument() {
        let mut bucket = vec![
            tone_ins(1, vec![5, 6], vec![10, 20], 1),
            tone_ins(1, vec![5, 6, 7], vec![10, 20, 30], 1),
            tone_ins(2, vec![5, 6], vec![10, 20], 1),
        ];
        let mut notes: IndexMap<usize, Vec<(i64, i64)>> = IndexMap::new();
        notes.insert(0, vec![(0, 500)]);
        notes.insert(1, vec![(1000, 1500)]);
        notes.insert(2, vec![(2000, 2500)]);
        let mapping = _merge_tone_prefixes(&mut bucket, Some(&notes), 1000.0, 1);
        assert_eq!(bucket.len(), 2);
        assert_eq!(mapping[&0], mapping[&1]);
        assert_ne!(mapping[&2], mapping[&0]);
        let kept = &bucket[mapping[&0]];
        assert_eq!(kept.pitch, vec![10, 20, 30]);
        assert_eq!(kept.vol, vec![5, 6, 7]);
    }

    #[test]
    fn test_every_macro_must_match() {
        let mut bucket = vec![
            tone_ins(1, vec![5, 6], vec![10], 1),
            tone_ins(1, vec![5, 8], vec![10, 20], 1),
        ];
        let mapping = _merge_tone_prefixes(&mut bucket, None, 0.0, 1);
        assert_eq!(bucket.len(), 2);
        assert_ne!(mapping[&0], mapping[&1]);
    }

    #[test]
    fn test_a_different_macro_speed_does_not_merge() {
        let mut bucket = vec![
            tone_ins(1, Vec::new(), vec![10], 1),
            tone_ins(1, Vec::new(), vec![10, 20], 2),
        ];
        let mapping = _merge_tone_prefixes(&mut bucket, None, 0.0, 1);
        assert_eq!(bucket.len(), 2);
        assert_ne!(mapping[&0], mapping[&1]);
    }
}
