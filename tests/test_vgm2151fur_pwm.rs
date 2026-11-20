//! Sega 32X PWM stream collection and Generic PCM DAC emission.
//!
//! Port of `tests/test_vgm2151fur_pwm.py` (v0.11.0).

mod common;

use std::collections::BTreeSet;
use std::path::PathBuf;

use indexmap::IndexMap;

use vgm2fur::analyze::{analyze, AnalyzeOptions};
use vgm2fur::furio::parse_fur;
use vgm2fur::furwrite::{write_fur, WriteOptions, CHIP_PCM_DAC};
use vgm2fur::parity::py_int;
use vgm2fur::pwm::{
    collect_pwm, PWM_ATTACK_MAX_FRAMES, PWM_ATTACK_MIN_FRAMES, PWM_CHUNK_FRAMES, PWM_RATE,
};
use vgm2fur::samplecluster::{cluster_fragments16, DEFAULT_HOT, DEFAULT_TARGET};
use vgm2fur::vgm::{ChipWrite, DacStreamPlay, VgmFile, STREAM_PWM};

fn bank(frames: usize) -> Vec<u8> {
    let mut out = Vec::new();
    for i in 0..frames {
        let left = 0x100 + i;
        let right = 0x200 + i;
        out.push((left & 0xFF) as u8);
        out.push(((left >> 8) & 0x0F) as u8);
        out.push((right & 0xFF) as u8);
        out.push(((right >> 8) & 0x0F) as u8);
    }
    out
}

fn play(sample: i64, reg: u8, base: u32) -> DacStreamPlay {
    DacStreamPlay {
        sample,
        chip_type: STREAM_PWM,
        chip_id: 0,
        stream: reg - 2,
        port: 0,
        reg,
        offset: 0,
        length: 0,
        mode: 3,
        rate: PWM_RATE as u32,
        step: 2,
        step_base: base,
        bank: 3,
        block: -1,
        ..Default::default()
    }
}

fn play_step1(sample: i64, reg: u8) -> DacStreamPlay {
    DacStreamPlay {
        sample,
        chip_type: STREAM_PWM,
        chip_id: 0,
        stream: 0,
        port: 0,
        reg,
        offset: 0,
        length: 0,
        mode: 3,
        rate: PWM_RATE as u32,
        step: 1,
        step_base: 0,
        bank: 3,
        block: -1,
        ..Default::default()
    }
}

fn make_vgm(pwm_dual: bool) -> VgmFile {
    let bank = bank(16);
    VgmFile {
        path: PathBuf::from("synthetic.vgm"),
        version: 0x161,
        total_samples: 22020,
        ym2151_clock: 0,
        k007232_clock: 0,
        writes: vec![ChipWrite::new(0, "pwm", 0, 1, 0x400)],
        dac_stream_plays: vec![play(0, 2, 0), play(0, 3, 1)],
        pwm_clock: 23_011_361,
        pwm_bank: bank.clone(),
        pwm_blocks: vec![bank],
        pwm_dual,
        ..Default::default()
    }
}

/// Audio bytes for a PWM bank item value (scaled at cycle 0x400).
fn word(value: u16) -> [u8; 2] {
    let value = value & 0xFFF;
    [(value & 0xFF) as u8, ((value >> 8) & 0x0F) as u8]
}

/// One rendered side's audio, split into attack bodies by the collector.
#[allow(non_snake_case)]
mod TestPwmCollector {
    use super::*;

    #[test]
    fn test_lossy_clustering16() {
        let mut bodies: Vec<Vec<u8>> = Vec::new();
        for i in 0..40u8 {
            let mut body = vec![0u8; 200];
            body.push(i);
            body.push(0);
            bodies.push(body);
        }
        let rates = vec![PWM_RATE; 40];
        let mut uses: IndexMap<i32, i64> = IndexMap::new();
        for i in 0..40 {
            uses.insert(i, 1);
        }
        let (keep, remap) = cluster_fragments16(&bodies, &rates, &uses, DEFAULT_TARGET, DEFAULT_HOT);
        assert!(keep.len() <= 32);
        assert!(remap.iter().all(|&r| r >= 0));
    }

    #[test]
    fn test_two_sides() {
        let (samples, warn) = collect_pwm(&make_vgm(false), false);
        assert_eq!(warn, Vec::<String>::new());
        assert_eq!(samples.len(), 2);
        let sides: BTreeSet<i32> = samples.iter().map(|s| s.side).collect();
        assert_eq!(sides, BTreeSet::from([0, 1]));
        assert!(samples[0].frames() > 0);
        assert_ne!(samples[0].data, samples[1].data);
        assert!(samples[0].data.iter().any(|&b| b > 0));
    }

    #[test]
    fn test_mono_stream_feeds_both_sides() {
        let mut v = make_vgm(false);
        v.dac_stream_plays = vec![DacStreamPlay {
            sample: 0,
            chip_type: STREAM_PWM,
            chip_id: 0,
            stream: 0,
            port: 0,
            reg: 4,
            offset: 0,
            length: 0,
            mode: 3,
            rate: PWM_RATE as u32,
            step: 1,
            step_base: 0,
            bank: 3,
            block: -1,
            ..Default::default()
        }];
        let (samples, _warn) = collect_pwm(&v, false);
        assert_eq!(samples.len(), 1);
        let sides: BTreeSet<i32> = samples[0].hits.iter().map(|h| h.side).collect();
        assert_eq!(sides, BTreeSet::from([0, 1]));
    }

    #[test]
    fn test_long_steady_stream_is_one_attack_body() {
        let frames = PWM_CHUNK_FRAMES * 2 + 10;
        let mut v = make_vgm(false);
        v.dac_stream_plays = vec![play_step1(0, 2), play_step1(0, 3)];
        v.pwm_bank = bank(frames as usize);
        v.pwm_blocks = vec![v.pwm_bank.clone()];
        v.total_samples = py_int(frames as f64 / PWM_RATE as f64 * 44100.0) + 100;
        let (samples, _warn) = collect_pwm(&v, false);
        // A steady body has no attack split, so it is one sample on both sides.
        assert_eq!(samples.len(), 1);
        let sides: BTreeSet<i32> = samples[0].hits.iter().map(|h| h.side).collect();
        assert_eq!(sides, BTreeSet::from([0, 1]));
    }

    #[test]
    fn test_repeated_chunks_share_one_body() {
        // Python patches PWM_ATTACK_MAX_SAMPLES to 0 to force the chunk
        // fallback. The Rust constants are compile-time, so this reproduces
        // the fallback through the single-sided case: one attack occurrence
        // (len == 1) also selects chunks. Four identical chunks on one side
        // keep the "one body, four hits" assertion.
        let mut v = make_vgm(false);
        v.dac_stream_plays = vec![play_step1(0, 2)];
        let item = word(0x100);
        let mut chunk = Vec::new();
        for _ in 0..PWM_CHUNK_FRAMES {
            chunk.extend_from_slice(&item);
        }
        let mut body = Vec::new();
        for _ in 0..4 {
            body.extend_from_slice(&chunk);
        }
        v.pwm_bank = body.clone();
        v.pwm_blocks = vec![body];
        // 44100 samples render exactly 4 * PWM_CHUNK_FRAMES frames.
        v.total_samples = 44100;
        let (samples, _warn) = collect_pwm(&v, false);
        assert_eq!(samples.len(), 1);
        assert_eq!(samples[0].hits.len(), 4); // four chunks, one side
    }

    #[test]
    fn test_short_attacks_fold_and_long_bodies_split() {
        // Python monkeypatches PWM_ATTACK_MAX_FRAMES to 4 * MIN and calls the
        // private _attack_bodies. The portable scenario uses the real
        // constant: a short attack, a quiet gap and one long body that must
        // be folded and then split. The same structural assertions apply.
        let short = 100usize;
        let gap = 300usize;
        let max_frames = PWM_ATTACK_MAX_FRAMES;
        let long = (max_frames + PWM_ATTACK_MIN_FRAMES / 2 - (short as i64 + gap as i64)) as usize;
        let mut items: Vec<[u8; 2]> = Vec::new();
        for i in 0..short {
            items.push(word(if i % 2 == 0 { 498 } else { 526 }));
        }
        for _ in 0..gap {
            items.push(word(0));
        }
        for i in 0..long {
            items.push(word(if i % 2 == 0 { 498 } else { 526 }));
        }
        let mut bank = Vec::new();
        for item in &items {
            bank.extend_from_slice(item);
        }
        let mut v = make_vgm(false);
        v.dac_stream_plays = vec![play_step1(0, 2), play_step1(0, 3)];
        v.pwm_bank = bank.clone();
        v.pwm_blocks = vec![bank];
        v.total_samples = py_int(
            items.len() as f64 / PWM_RATE as f64 * 44100.0,
        ) + 100;
        let (samples, _warn) = collect_pwm(&v, false);
        assert_eq!(samples.len(), 2);
        // The short attack was kept: the first body starts at frame 0.
        assert_eq!(samples[0].chunk, 0);
        assert!(samples[0].first_chunk);
        assert!(samples.iter().all(|s| {
            PWM_ATTACK_MIN_FRAMES <= s.frames() && s.frames() <= max_frames
        }));
    }

    #[test]
    fn test_attack_bodies_share_repeated_hits() {
        let mut v = make_vgm(false);
        v.dac_stream_plays = vec![play_step1(0, 2), play_step1(0, 3)];
        let burst = PWM_ATTACK_MIN_FRAMES as usize + 8;
        let a_item = word(0x100);
        let b_item = word(0x300);
        let mut a = Vec::new();
        let mut b = Vec::new();
        for _ in 0..burst {
            a.extend_from_slice(&a_item);
            b.extend_from_slice(&b_item);
        }
        let gap = vec![0u8; 2 * 300]; // 300 frames of silence
        let mut body = Vec::new();
        for part in [&a, &gap, &b, &gap, &a, &gap, &b, &gap] {
            body.extend_from_slice(part);
        }
        v.pwm_bank = body.clone();
        v.pwm_blocks = vec![body.clone()];
        v.total_samples = py_int(
            (body.len() / 2) as f64 / PWM_RATE as f64 * 44100.0,
        ) + 100;
        let (samples, _warn) = collect_pwm(&v, false);
        assert_eq!(samples.len(), 2);
        assert_eq!(
            samples.iter().map(|s| s.hits.len()).collect::<Vec<_>>(),
            vec![4, 4]
        );
    }
}

#[allow(non_snake_case)]
mod TestPwmModule {
    use super::*;

    #[test]
    fn test_pcm_dac_pair() {
        let song = analyze(&make_vgm(false), AnalyzeOptions::default()).expect("analyze");
        assert_eq!(song.pwm_ids, vec![0]);
        assert_eq!(song.pwm_count, 2);
        let data = write_fur(&song, WriteOptions {
            include_fm: false,
            ..Default::default()
        });
        let mod_ = parse_fur(&data).expect("parse_fur");
        assert_eq!(mod_.chips, vec![CHIP_PCM_DAC, CHIP_PCM_DAC]);
        assert_eq!(mod_.n_ch, 2);
    }

    #[test]
    fn test_dual_header_keeps_silent_second_chip() {
        let song = analyze(&make_vgm(true), AnalyzeOptions::default()).expect("analyze");
        assert_eq!(song.pwm_ids, vec![0, 1]);
        let data = write_fur(&song, WriteOptions {
            include_fm: false,
            ..Default::default()
        });
        let mod_ = parse_fur(&data).expect("parse_fur");
        assert_eq!(
            mod_.chips,
            vec![CHIP_PCM_DAC, CHIP_PCM_DAC, CHIP_PCM_DAC, CHIP_PCM_DAC]
        );
        assert_eq!(mod_.n_ch, 4);
    }
}
