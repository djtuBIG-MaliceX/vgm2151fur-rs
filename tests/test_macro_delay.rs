//! Port of `tests/test_macro_delay.py`: leading macro repeats become the
//! macro's delay field.

mod common;

use indexmap::IndexMap;
use vgm2fur::analyze::{
    _compress_tone_delays, _lfo_tail_cycle, _macro_series_delay, _merge_tone_prefixes,
    _tone_macro_same_for,
};
use vgm2fur::furwrite::{_write_macro, Buf};
use vgm2fur::model::ToneIns;

fn _lfo_pattern() -> Vec<i32> {
    let pattern = vec![10, 10, 10, 20, 20, 20, 20, 10, 10, 10, 10, 20, 20, 20];
    let mut series = vec![0; 20];
    series.extend_from_slice(&pattern);
    series.extend_from_slice(&pattern);
    series
}

mod pitch_lfo_loop {
    use super::*;

    #[test]
    fn test_an_lfo_tail_loops_at_its_first_cycle() {
        let pattern = vec![10, 10, 10, 20, 20, 20, 20, 10, 10, 10, 10, 20, 20, 20];
        let mut values = vec![0; 20];
        values.extend_from_slice(&pattern);
        values.extend_from_slice(&pattern);
        assert_eq!(
            _lfo_tail_cycle(&values, 128, 1, 2048, 0),
            Some((20, pattern.len()))
        );
    }

    #[test]
    fn test_the_delay_and_loop_are_kept_together() {
        let pattern = vec![10, 10, 10, 20, 20, 20, 20, 10, 10, 10, 10, 20, 20, 20];
        let series = _lfo_pattern();
        let (values, _speed, r#loop, delay) = _macro_series_delay(&series, true, false);
        assert_eq!(delay, 19);
        assert_eq!(r#loop, 1);
        let mut expected = vec![0];
        expected.extend_from_slice(&pattern);
        assert_eq!(values, expected);
    }

    #[test]
    fn test_a_monotonic_tail_is_not_an_lfo() {
        let mut values = vec![0; 5];
        values.extend(0..20);
        assert_eq!(_lfo_tail_cycle(&values, 128, 1, 2048, 0), None);
    }

    #[test]
    fn test_step_length_compresses_a_long_hold() {
        let mut pattern = vec![10; 8];
        pattern.extend_from_slice(&[20; 8]);
        let mut series = vec![0; 300];
        series.extend_from_slice(&pattern);
        series.extend_from_slice(&pattern);
        let (values, speed, r#loop, delay) = _macro_series_delay(&series, true, true);
        assert_eq!(delay, 255);
        assert!(speed > 1);
        assert!(values.len() < 20);
        assert_eq!(r#loop, values.iter().position(|&v| v == 10).unwrap() as i32);
    }
}

mod macro_delay {
    use super::*;

    #[test]
    fn test_a_leading_run_is_held_by_the_delay() {
        let mut series = vec![5; 10];
        series.extend_from_slice(&[6, 7, 8]);
        let (values, speed, r#loop, delay) = _macro_series_delay(&series, false, false);
        assert_eq!(values, vec![5, 6, 7, 8]);
        assert_eq!((speed, r#loop), (1, 255));
        assert_eq!(delay, 9, "ten ticks held: first tick + delay 9");
    }

    #[test]
    fn test_a_very_long_run_caps_the_delay_and_keeps_copies() {
        let mut series = vec![5; 300];
        series.push(6);
        let (values, _speed, _loop, delay) = _macro_series_delay(&series, false, false);
        assert_eq!(delay, 255);
        assert_eq!(values.iter().filter(|&&v| v == 5).count(), 45);
        assert_eq!(*values.last().unwrap(), 6);
    }

    #[test]
    fn test_a_cycle_is_not_turned_into_a_loop() {
        let (values, _speed, r#loop, delay) =
            _macro_series_delay(&[1, 1, 2, 1, 2], false, false);
        assert_eq!(delay, 1);
        assert_eq!(r#loop, 255);
        assert_eq!(values, vec![1, 2, 1, 2]);
    }

    #[test]
    fn test_recompress_tone_macros_before_grouping() {
        let mut bucket = vec![ToneIns {
            wave: 1,
            pitch: vec![0, 0, 0, 0, 0, 0, 5, 6],
            vol: vec![3, 3, 3, 3, 2],
            vol_speed: 2,
            ..Default::default()
        }];
        _compress_tone_delays(&mut bucket);
        let spec = &bucket[0];
        assert_eq!(spec.pitch, vec![0, 5, 6]);
        assert_eq!(spec.pitch_delay, 5);
        assert_eq!(spec.vol, vec![3, 2]);
        assert_eq!(spec.vol_delay, 7);
    }

    #[test]
    fn test_writer_emits_the_delay_byte() {
        let mut b = Buf::new();
        _write_macro(&mut b, 4, &[5, 6], 1, 255, 255, 9, 2);
        let data = &b.data;
        let code = data[0];
        let length = data[1];
        let _loop = data[2];
        let _rel = data[3];
        let _mode = data[4];
        let word = data[5];
        let delay = data[6];
        let speed = data[7];
        assert_eq!((code, length), (4, 2));
        assert_eq!(word, 0x81, "16-bit value encoding");
        assert_eq!((delay, speed), (9, 1));
        assert_eq!(
            (
                i16::from_le_bytes([data[8], data[9]]),
                i16::from_le_bytes([data[10], data[11]]),
            ),
            (5, 6)
        );
    }
}

mod tone_macro_timing {
    use super::*;

    fn _pair() -> (ToneIns, ToneIns) {
        let simple = ToneIns {
            pitch: vec![0, 1],
            pitch_speed: 28,
            pitch_loop: 255,
            pitch_delay: 22,
            ..Default::default()
        };
        let tail = ToneIns {
            pitch: vec![0, 1, 1, 1, 1, 1, 1, 1, 1, 121, 121, 121, 129, 129],
            pitch_speed: 8,
            pitch_loop: 10,
            pitch_delay: 24,
            ..Default::default()
        };
        (simple, tail)
    }

    #[test]
    fn test_each_macro_uses_its_own_timing() {
        let (simple, tail) = _pair();
        assert!(!_tone_macro_same_for(&tail, &simple, 335));
        assert!(_tone_macro_same_for(&tail, &simple, 4));
    }

    #[test]
    fn test_a_long_note_does_not_merge_into_the_tail_instrument() {
        let (simple, tail) = _pair();
        let mut bucket = vec![simple, tail];
        let mut notes: IndexMap<usize, Vec<(i64, i64)>> = IndexMap::new();
        notes.insert(0, vec![(0, 335 * 100)]);
        let mapping = _merge_tone_prefixes(&mut bucket, Some(&notes), 100.0, 1);
        let mut expected: IndexMap<usize, usize> = IndexMap::new();
        expected.insert(0, 0);
        expected.insert(1, 1);
        assert_eq!(mapping, expected);
    }
}
