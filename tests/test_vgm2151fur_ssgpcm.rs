//! SSG/PSG PCM (volume-register DAC) detection and .fur emission.
//!
//! Port of `tests/test_vgm2151fur_ssgpcm.py`.

mod common;
use common::*;

use std::collections::HashSet;
use std::path::PathBuf;

use indexmap::IndexMap;

use vgm2fur::analyze::{analyze, AnalyzeOptions};
use vgm2fur::furio::{iter_blocks, parse_fur};
use vgm2fur::furwrite::{decompress_fur, write_fur, WriteOptions, INS_AY};
use vgm2fur::parity::{py_int, py_round};
use vgm2fur::ssgpcm::{
    cluster_ssg_pcm, collect_ssg_pcm, SsgPcmHit, SsgPcmSample, DAC_LOG_TABLE_AY,
};
use vgm2fur::vgm::{ChipWrite, DacStreamPlay, VgmFile};

const AY_CLOCK: u32 = 1_789_773;
const YM2203_CLOCK: u32 = 3_993_600;

fn _vgm(writes: Vec<ChipWrite>, total: i64, ay_clock: u32, ym2203_clock: u32) -> VgmFile {
    let mut v = VgmFile::default();
    v.path = PathBuf::from("synthetic.vgm");
    v.buf = Vec::new();
    v.version = 0x161;
    v.total_samples = total;
    v.loop_samples = 0;
    v.loop_file_pos = None;
    v.loop_sample = None;
    v.ym2151_clock = 3_579_545;
    v.k007232_clock = 0;
    v.gd3 = Default::default();
    v.roms = Vec::new();
    v.writes = writes;
    v.ay8910_clock = ay_clock;
    v.ym2203_clock = ym2203_clock;
    v
}

fn _ay(sample: i64, reg: u32, val: u32, chip: &'static str) -> ChipWrite {
    ChipWrite::new(sample, chip, 0, reg, val)
}

/// A square wave written through the volume register.
#[allow(clippy::too_many_arguments)]
fn _square(
    start: i64,
    writes: usize,
    dt: i64,
    level_hi: u32,
    level_lo: u32,
    half: usize,
    reg: u32,
    chip: &'static str,
) -> Vec<ChipWrite> {
    let mut out = Vec::new();
    for i in 0..writes {
        let val = if (i / half) % 2 == 0 { level_hi } else { level_lo };
        out.push(_ay(start + i as i64 * dt, reg, val, chip));
    }
    out
}

fn _pcm_writes(
    start: i64,
    count: usize,
    dt: i64,
    reg: u32,
    chip: &'static str,
    mixer: u32,
    half: usize,
) -> Vec<ChipWrite> {
    let mut out = vec![_ay(start, 7, mixer, chip)];
    out.extend(_square(start + 100, count, dt, 0x0F, 0x00, half, reg, chip));
    out
}

/// Default `_pcm_writes` arguments, except `reg`/`chip`/`mixer`/`half`.
fn _pcm(start: i64, count: usize) -> Vec<ChipWrite> {
    _pcm_writes(start, count, 5, 10, "ay8910", 0xBF, 22)
}

fn _ins_types(data: &[u8]) -> Vec<u16> {
    let raw = decompress_fur(data);
    let mut out = Vec::new();
    for b in iter_blocks(&raw).unwrap() {
        if &b.magic == b"INS2" {
            out.push(u16::from_le_bytes([b.payload[2], b.payload[3]]));
        }
    }
    out
}

/// The 120 note-map sample indices of the INS2 at block position `index`.
fn _ins_sm_maps(data: &[u8], index: usize) -> Vec<i32> {
    let raw = decompress_fur(data);
    let mut n = 0usize;
    for b in iter_blocks(&raw).unwrap() {
        if &b.magic != b"INS2" {
            continue;
        }
        if n == index {
            let blk = b.payload;
            let mut pos = 4usize;
            while pos + 2 <= blk.len() {
                let feat = &blk[pos..pos + 2];
                if feat == b"EN" {
                    break;
                }
                let fsz = u16::from_le_bytes([blk[pos + 2], blk[pos + 3]]) as usize;
                let body = &blk[pos + 4..pos + 4 + fsz];
                if feat == b"SM" {
                    return (0..120)
                        .map(|i| {
                            let off = 4 + i * 4 + 2;
                            i16::from_le_bytes([body[off], body[off + 1]]) as i32
                        })
                        .collect();
                }
                pos += 4 + fsz;
            }
            return Vec::new();
        }
        n += 1;
    }
    Vec::new()
}

fn _play(
    sample: i64,
    chip_id: u8,
    reg: u8,
    offset: u32,
    length: u32,
    mode: u8,
    rate: u32,
) -> DacStreamPlay {
    DacStreamPlay {
        sample,
        chip_type: 0x06,
        chip_id,
        stream: 0,
        port: 0,
        reg,
        offset,
        length,
        mode,
        rate,
        loop_: false,
        reverse: false,
        step: 1,
        step_base: 0,
        bank: 0,
        block: -1,
        stop_sample: 0,
    }
}

/// A VGM with one 0x90-0x95 stream writing an SSG volume register.
fn _stream(body: &[u8], length: Option<u32>, mode: u8, rate: u32, sample: i64) -> VgmFile {
    let mut v = VgmFile::default();
    v.path = PathBuf::from("stream.vgm");
    v.buf = Vec::new();
    v.version = 0x161;
    v.total_samples = 200_000;
    v.loop_samples = 0;
    v.loop_file_pos = None;
    v.loop_sample = None;
    v.ym2151_clock = 3_579_545;
    v.k007232_clock = 0;
    v.gd3 = Default::default();
    v.roms = Vec::new();
    v.writes = Vec::new();
    v.ym2203_clock = YM2203_CLOCK;
    v.dac_data = body.to_vec();
    v.dac_stream_plays = vec![_play(
        sample,
        0,
        10,
        0,
        length.unwrap_or(body.len() as u32),
        mode,
        rate,
    )];
    v
}

mod TestSsgPcmDetect {
    use super::*;

    #[test]
    fn test_mixer_off_fast_volume_writes_become_one_sample() {
        let (samples, hits, _warn) =
            collect_ssg_pcm(&_vgm(_pcm(1000, 700), 200_000, AY_CLOCK, 0), "ay8910");
        assert_eq!(samples.len(), 1);
        assert_eq!(hits.len(), 1);
        assert_eq!((samples[0].voice, hits[0].voice), (2, 2));
        assert_eq!(samples[0].rate, 8820);
        assert!(samples[0].data.len() >= 512);
    }

    #[test]
    fn test_fast_writes_with_tone_on_are_not_pcm() {
        // Mixer bit clear = tone enabled; the register is not a DAC level.
        let (samples, hits, _warn) = collect_ssg_pcm(
            &_vgm(
                _pcm_writes(1000, 700, 5, 10, "ay8910", 0xB8, 22),
                200_000,
                AY_CLOCK,
                0,
            ),
            "ay8910",
        );
        assert!(samples.is_empty());
        assert!(hits.is_empty());
    }

    #[test]
    fn test_slow_volume_ramp_is_not_pcm() {
        let mut writes = vec![_ay(0, 7, 0xBF, "ay8910")];
        for i in 0..60usize {
            writes.push(_ay(i as i64 * 500, 10, (i % 16) as u32, "ay8910"));
        }
        let (samples, hits, _warn) = collect_ssg_pcm(&_vgm(writes, 200_000, AY_CLOCK, 0), "ay8910");
        assert!(samples.is_empty());
        assert!(hits.is_empty());
    }

    #[test]
    fn test_long_gap_splits_hits() {
        let mut writes = _pcm(1000, 700);
        writes.extend(_pcm(1000 + 700 * 5 + 4096, 700));
        let (samples, hits, _warn) = collect_ssg_pcm(&_vgm(writes, 200_000, AY_CLOCK, 0), "ay8910");
        // Identical bodies share one instrument; the two hits remain.
        assert_eq!(hits.len(), 2);
        assert!(samples.len() >= 1);
    }

    #[test]
    fn test_short_gap_keeps_one_body() {
        let mut writes = _pcm(1000, 700);
        writes.extend(_pcm(1000 + 700 * 5 + 512, 700));
        let (samples, hits, _warn) = collect_ssg_pcm(&_vgm(writes, 200_000, AY_CLOCK, 0), "ay8910");
        assert_eq!(samples.len(), 1);
        assert_eq!(hits.len(), 1);
    }

    #[test]
    fn test_identical_bodies_are_shared() {
        let mut writes = _pcm(1000, 700);
        writes.extend(_pcm(1000 + 700 * 5 + 4096, 700));
        let (_samples, hits, _warn) = collect_ssg_pcm(&_vgm(writes, 200_000, AY_CLOCK, 0), "ay8910");
        assert_eq!(hits.len(), 2);
        let indices: HashSet<i32> = hits.iter().map(|h| h.sample_index).collect();
        assert_eq!(indices, HashSet::from([0i32]));
    }

    #[test]
    fn test_levels_round_trip_through_the_dac_table() {
        // Alternate 15 and 0 levels; the stored bytes must decode back to
        // exactly those levels via Furnace's runDAC table.
        let (samples, _hits, _warn) = collect_ssg_pcm(
            &_vgm(
                _pcm_writes(1000, 700, 5, 10, "ay8910", 0xBF, 5),
                200_000,
                AY_CLOCK,
                0,
            ),
            "ay8910",
        );
        let levels: HashSet<u8> = samples[0]
            .data
            .iter()
            .map(|&b| DAC_LOG_TABLE_AY[(b ^ 0x80) as usize])
            .collect();
        assert_eq!(levels, HashSet::from([0u8, 15u8]));
    }

    #[test]
    fn test_three_voices_run_together() {
        let mut writes = vec![_ay(0, 7, 0xBF, "ay8910")];
        for voice in 0..3u32 {
            writes.extend(_square(
                100,
                700,
                5,
                0x0F,
                0x00,
                22,
                8 + voice,
                "ay8910",
            ));
        }
        let (_samples, hits, _warn) = collect_ssg_pcm(&_vgm(writes, 200_000, AY_CLOCK, 0), "ay8910");
        // The three voices play at once; identical bodies share one instrument.
        let mut voices: Vec<i32> = hits.iter().map(|h| h.voice).collect();
        voices.sort();
        assert_eq!(voices, vec![0, 1, 2]);
        let indices: HashSet<i32> = hits.iter().map(|h| h.sample_index).collect();
        assert_eq!(indices, HashSet::from([0i32]));
    }

    #[test]
    fn test_dominant_level_with_short_runs_is_a_waveform() {
        // A body can spend most of its time at one level without reaching the
        // DC threshold: the run length, not the overall share, decides.
        let mut writes = vec![_ay(0, 7, 0xBF, "ay8910")];
        writes.extend(_square(100, 2500, 5, 0x0F, 0x00, 22, 8, "ay8910"));
        writes.extend(_square(100, 2500, 5, 0x0F, 0x00, 27, 9, "ay8910"));
        let mut t = 100i64;
        for i in 0..2500usize {
            writes.push(_ay(
                t,
                10,
                if i % 625 == 624 { 0 } else { 15 },
                "ay8910",
            ));
            t += 5;
        }
        let (samples, hits, _warn) = collect_ssg_pcm(&_vgm(writes, 200_000, AY_CLOCK, 0), "ay8910");
        assert_eq!(samples.len(), 3);
        let mut voices: Vec<i32> = samples.iter().map(|s| s.voice).collect();
        voices.sort();
        assert_eq!(voices, vec![0, 1, 2]);
        assert_eq!(hits.len(), 3);
        assert!(samples.iter().all(|s| s.data.len() > 2048));
    }

    #[test]
    fn test_ym2203_ssg_uses_the_same_path() {
        let mut writes = vec![_ay(0, 7, 0xBF, "ym2203_ssg")];
        writes.extend(_square(100, 700, 5, 0x0F, 0x00, 22, 10, "ym2203_ssg"));
        let (samples, hits, _warn) =
            collect_ssg_pcm(&_vgm(writes, 200_000, AY_CLOCK, YM2203_CLOCK), "ym2203_ssg");
        assert_eq!(samples.len(), 1);
        assert_eq!(hits.len(), 1);
    }
}

mod TestSsgPcmDacStream {
    use super::*;

    #[test]
    fn test_stream_body_uses_the_whole_length() {
        let body: Vec<u8> = (0..1200usize).map(|i| ((i / 8) % 16) as u8).collect();
        let vgm = _stream(&body, None, 1, 15964, 1000);
        let (samples, hits, _warn) = collect_ssg_pcm(&vgm, "ym2203_ssg");
        assert_eq!(hits.len(), 1);
        // 1200 bytes at 15964 Hz is ~3315 VGM samples.
        assert!(samples[0].data.len() > 1000);
        assert_eq!(samples[0].rate, 15964);
        assert_eq!(samples[0].voice, 2);
    }

    #[test]
    fn test_same_offset_plays_share_the_longest_body() {
        let body: Vec<u8> = (0..1200usize).map(|i| ((i / 8) % 16) as u8).collect();
        let mut vgm = _stream(&body, Some(1200), 1, 15964, 1000);
        vgm.dac_stream_plays.push(_play(10000, 0, 10, 0, 600, 1, 15964));
        let (samples, hits, _warn) = collect_ssg_pcm(&vgm, "ym2203_ssg");
        // Both plays read bank offset 0; the 600-byte play is a prefix of the
        // 1,200-byte one, so it reuses that body and is cut at its own stop.
        assert_eq!(samples.len(), 1);
        assert_eq!(hits.len(), 2);
        assert!(samples[0].data.len() > 1000);
        let first = hits.iter().min_by_key(|h| h.sample_time).unwrap();
        assert_eq!(first.sample_time, 1000);
        assert!(first.end_time < 1000 + 1200 * 44100 / 15964);
        let indices: HashSet<i32> = hits.iter().map(|h| h.sample_index).collect();
        assert_eq!(indices, HashSet::from([0i32]));
    }

    #[test]
    fn test_near_offsets_share_a_body() {
        // A pointer a few bytes into the same sample is a start shift, not a
        // second body.
        let body: Vec<u8> = (0..3000usize).map(|i| ((i / 4) % 16) as u8).collect();
        let mut vgm = _stream(&body, Some(2000), 1, 15964, 1000);
        vgm.dac_stream_plays.push(_play(10000, 0, 10, 4, 2000, 1, 15964));
        let (samples, hits, _warn) = collect_ssg_pcm(&vgm, "ym2203_ssg");
        assert_eq!(samples.len(), 1);
        assert_eq!(hits.len(), 2);
    }

    #[test]
    fn test_distant_offsets_stay_separate() {
        let body: Vec<u8> = (0..3000usize).map(|i| ((i / 4) % 16) as u8).collect();
        let mut vgm = _stream(&body, Some(1000), 1, 15964, 1000);
        vgm.dac_stream_plays.push(_play(10000, 0, 10, 8, 1000, 1, 15964));
        let (samples, _hits, _warn) = collect_ssg_pcm(&vgm, "ym2203_ssg");
        // The reference asserts only that the two distant offsets stay
        // separate bodies (each keeps its own 1000 bytes at the stream rate).
        assert_eq!(samples.len(), 2);
        assert_eq!(samples[0].data.len(), 1000);
        assert_eq!(samples[1].data.len(), 1000);
    }

    #[test]
    fn test_stream_levels_round_trip() {
        let body: Vec<u8> = std::iter::repeat([0u8, 15]).take(600).flatten().collect();
        let vgm = _stream(&body, None, 1, 15964, 1000);
        let (samples, _hits, _warn) = collect_ssg_pcm(&vgm, "ym2203_ssg");
        let levels: HashSet<u8> = samples[0]
            .data
            .iter()
            .map(|&b| DAC_LOG_TABLE_AY[(b ^ 0x80) as usize])
            .collect();
        assert_eq!(levels, HashSet::from([0u8, 15u8]));
    }

    #[test]
    fn test_stream_ignores_other_registers() {
        let mut vgm = _stream(&[15u8; 600], None, 1, 15964, 1000);
        vgm.dac_stream_plays[0].reg = 0; // tone period, not a DAC level
        let (samples, hits, _warn) = collect_ssg_pcm(&vgm, "ym2203_ssg");
        assert!(samples.is_empty());
        assert!(hits.is_empty());
    }

    #[test]
    fn test_millisecond_length_mode() {
        let body: Vec<u8> = (0..3000usize).map(|i| ((i / 8) % 16) as u8).collect();
        // 100 ms at 15964 Hz = 1596 bytes of the bank.
        let vgm = _stream(&body, Some(100), 2, 15964, 1000);
        let (samples, hits, _warn) = collect_ssg_pcm(&vgm, "ym2203_ssg");
        assert_eq!(hits.len(), 1);
        assert!(samples[0].data.len() > 1400);
    }

    #[test]
    fn test_stream_and_direct_writes_coexist() {
        // A driver can mix DAC-stream notes with direct volume-register PCM.
        // Both paths must keep their own bodies and rates.
        let body: Vec<u8> = (0..2400usize).map(|i| ((i / 8) % 16) as u8).collect();
        let mut vgm = _stream(&body, None, 1, 15964, 1000);
        vgm.writes.extend(_pcm_writes(
            1000, 700, 5, 8, "ym2203_ssg", 0xBF, 22,
        ));
        for k in 0..4i64 {
            vgm.dac_stream_plays
                .push(_play(10000 + k * 5000, 0, 10, 0, 2400, 1, 15964));
        }
        let (samples, hits, _warn) = collect_ssg_pcm(&vgm, "ym2203_ssg");
        assert!(hits.len() >= 5);
        assert!(samples.iter().all(|s| s.data.len() >= 128));
        assert!(samples.iter().any(|s| s.rate > 10000));
        let rates: HashSet<i32> = samples.iter().map(|s| s.rate).collect();
        assert!(rates.len() > 1);
    }

    #[test]
    fn test_high_dc_stream_is_dropped() {
        // A tiny peak followed by a flat DC tail is not worth a sample slot.
        let mut body = vec![15u8; 200];
        body.extend(std::iter::repeat(0u8).take(4000));
        let vgm = _stream(&body, Some(body.len() as u32), 1, 15964, 1000);
        let (samples, hits, _warn) = collect_ssg_pcm(&vgm, "ym2203_ssg");
        assert!(samples.is_empty());
        assert!(hits.is_empty());
    }

    #[test]
    fn test_dc_tail_is_trimmed() {
        // 50% DC: keep the body but drop the trailing flat half.
        let mut body = vec![15u8; 1500];
        body.extend(std::iter::repeat(0u8).take(1500));
        let vgm = _stream(&body, Some(body.len() as u32), 1, 15964, 1000);
        let (samples, hits, _warn) = collect_ssg_pcm(&vgm, "ym2203_ssg");
        assert_eq!(hits.len(), 1);
        assert!(samples[0].data.len() > 1400);
        assert!(samples[0].data.len() < 1700);
    }

    #[test]
    fn test_short_body_is_left_alone() {
        // 2,048 frames or fewer keep their exact shape even with a DC tail.
        let mut body = vec![15u8; 1000];
        body.extend(std::iter::repeat(0u8).take(1000));
        let vgm = _stream(&body, Some(body.len() as u32), 1, 15964, 1000);
        let (samples, hits, _warn) = collect_ssg_pcm(&vgm, "ym2203_ssg");
        assert_eq!(hits.len(), 1);
        assert!(samples[0].data.len() > 1900);
    }

    #[test]
    fn test_pure_dc_stream_is_dropped() {
        let body = vec![15u8; 3000];
        let vgm = _stream(&body, Some(body.len() as u32), 1, 15964, 1000);
        let (samples, hits, _warn) = collect_ssg_pcm(&vgm, "ym2203_ssg");
        assert!(samples.is_empty());
        assert!(hits.is_empty());
    }

    #[test]
    fn test_stop_sample_ends_the_play_early() {
        let body: Vec<u8> = (0..3000usize).map(|i| ((i / 8) % 16) as u8).collect();
        let mut vgm = _stream(&body, Some(3000), 1, 15964, 1000);
        // A 0x94 600 frames in cuts the 3,000-byte declared length short.
        vgm.dac_stream_plays[0].stop_sample =
            1000 + py_int(py_round(600.0 * 44100.0 / 15964.0));
        let (samples, hits, _warn) = collect_ssg_pcm(&vgm, "ym2203_ssg");
        assert_eq!(hits.len(), 1);
        assert!(samples[0].data.len() > 500);
        assert!(samples[0].data.len() < 800);
    }
}

mod TestSsgPcmUnused {
    use super::*;

    #[test]
    fn test_writer_drops_a_body_no_note_selects() {
        let mut writes = vec![_ay(0, 7, 0xBF, "ay8910")];
        writes.extend(_square(100, 700, 5, 0x0F, 0x00, 22, 8, "ay8910"));
        let mut song = analyze(&_vgm(writes, 4000, AY_CLOCK, 0), AnalyzeOptions::default()).unwrap();
        assert_eq!(song.ssg_pcm_samples.len(), 1);
        song.ssg_pcm_samples.push(SsgPcmSample {
            name: "unused".to_string(),
            data: vec![0x01, 0x02, 0x03],
            rate: 8820,
            start: 0,
            end: 0,
            voice: 0,
            chip_id: 0,
        });
        let mut stats: IndexMap<String, i64> = IndexMap::new();
        let data = write_fur(
            &song,
            WriteOptions {
                stats: Some(&mut stats),
                ..Default::default()
            },
        );
        let mod_ = parse_fur(&data).unwrap();
        assert_eq!(stats.get("ssg_pcm_samples_unused"), Some(&1));
        assert_eq!(mod_.samples.len(), 1);
        let pcm = song.events.iter().find(|e| e.on && e.pcm).expect("pcm event");
        assert_eq!(
            _ins_sm_maps(&data, pcm.ins as usize)[(pcm.note - 60) as usize],
            0
        );
    }

    #[test]
    fn test_unreferenced_bodies_are_not_written() {
        // A stream aimed at a second YM2203 instance the module does not have.
        // The body must not survive as an unused SMP2 block.
        let mut vgm = VgmFile::default();
        vgm.path = PathBuf::from("unused.vgm");
        vgm.buf = Vec::new();
        vgm.version = 0x161;
        vgm.total_samples = 50_000;
        vgm.loop_samples = 0;
        vgm.loop_file_pos = None;
        vgm.loop_sample = None;
        vgm.ym2151_clock = 3_579_545;
        vgm.k007232_clock = 0;
        vgm.gd3 = Default::default();
        vgm.roms = Vec::new();
        vgm.writes = vec![ChipWrite::new(0, "ym2203", 0, 0x27, 0)];
        vgm.ym2203_clock = YM2203_CLOCK;
        vgm.dac_data = vec![15u8; 600];
        vgm.dac_stream_plays = vec![_play(1000, 1, 10, 0, 600, 1, 15964)];
        let song = analyze(&vgm, AnalyzeOptions::default()).unwrap();
        assert!(song.ssg_pcm_samples.is_empty());
        assert_eq!(song.ssg_pcm_count, 0);
        let data = write_fur(&song, WriteOptions::default());
        let m = parse_fur(&data).unwrap();
        assert_eq!(m.samples.len(), 0);
        assert!(!_ins_types(&data).iter().any(|&t| t == INS_AY as u16));
    }
}

mod TestSsgPcmEmission {
    use super::*;

    fn _song() -> vgm2fur::model::Song {
        let mut writes = vec![_ay(0, 7, 0xB8, "ay8910")];
        // A normal tone on voice 0, then PCM on voice 2.
        writes.extend([
            _ay(100, 0, 0x00, "ay8910"),
            _ay(100, 1, 0x01, "ay8910"),
            _ay(120, 8, 0x0F, "ay8910"),
            _ay(1500, 8, 0x00, "ay8910"),
        ]);
        writes.extend(_pcm(3000, 700));
        analyze(&_vgm(writes, 4000, AY_CLOCK, 0), AnalyzeOptions::default()).unwrap()
    }

    #[test]
    fn test_tone_note_survives_beside_the_pcm_note() {
        let song = _song();
        let tone: Vec<_> = song
            .events
            .iter()
            .filter(|e| e.on && e.tone_chip == 2)
            .collect();
        let pcm: Vec<_> = song.events.iter().filter(|e| e.on && e.pcm).collect();
        assert!(!tone.is_empty());
        assert_eq!(pcm.len(), 1);
        assert!(tone[0].sample < pcm[0].sample);
    }

    #[test]
    fn test_pcm_note_is_c4_and_8_bit() {
        let song = _song();
        let pcm = song.events.iter().find(|e| e.on && e.pcm).expect("pcm note");
        // Slot 0 of the sample map; every map slot stores C-4.
        assert_eq!(pcm.note, 60);
        assert_eq!(pcm.vol, 15);
        assert!(pcm.no_pan);
        let data = write_fur(&song, WriteOptions::default());
        let m = parse_fur(&data).unwrap();
        assert_eq!(song.ssg_pcm_samples.len(), 1);
        assert_eq!(m.samples.last().unwrap().depth, 8);
        assert_eq!(m.samples.last().unwrap().c4_rate, 8820);
        assert_eq!(_ins_types(&data)[pcm.ins as usize], INS_AY as u16);
        assert_eq!(
            _ins_sm_maps(&data, pcm.ins as usize)[(pcm.note - 60) as usize],
            0
        );
    }

    #[test]
    fn test_three_pcm_notes_get_three_channels() {
        let mut writes = vec![_ay(0, 7, 0xBF, "ay8910")];
        for voice in 0..3u32 {
            // Different duty per voice so the bodies stay distinct.
            writes.extend(_square(
                100,
                700,
                5,
                0x0F,
                0x00,
                22 + voice as usize * 5,
                8 + voice,
                "ay8910",
            ));
        }
        let song = analyze(&_vgm(writes, 4000, AY_CLOCK, 0), AnalyzeOptions::default()).unwrap();
        let pcm: Vec<_> = song.events.iter().filter(|e| e.on && e.pcm).collect();
        assert_eq!(pcm.len(), 3);
        let chans: HashSet<i32> = pcm.iter().map(|e| e.ch).collect();
        assert_eq!(chans.len(), 3);
        let data = write_fur(&song, WriteOptions::default());
        let m = parse_fur(&data).unwrap();
        assert_eq!(m.samples.len(), 3);
        // All three bodies fit one sample-map instrument.
        let ins: HashSet<i32> = pcm.iter().map(|e| e.ins).collect();
        assert_eq!(ins.len(), 1);
        let maps = _ins_sm_maps(&data, pcm[0].ins as usize);
        assert_eq!(
            pcm.iter()
                .map(|e| maps[(e.note - 60) as usize])
                .collect::<Vec<_>>(),
            vec![0, 1, 2]
        );
    }
}

mod TestSsgPcmClustering {
    use super::*;

    #[test]
    fn test_cluster_folds_fragments_and_remaps_hits() {
        let samples: Vec<SsgPcmSample> = (0..40usize)
            .map(|i| SsgPcmSample {
                name: format!("pcm{i}"),
                data: (0..32usize).map(|j| ((i * 41 + j * 17) & 0xFF) as u8).collect(),
                rate: 11025,
                start: i as i64 * 1000,
                end: i as i64 * 1000 + 32,
                voice: 0,
                chip_id: 0,
            })
            .collect();
        let hits: Vec<SsgPcmHit> = (0..40usize)
            .map(|i| SsgPcmHit {
                sample_time: i as i64 * 4410,
                end_time: 0,
                voice: 0,
                chip_id: 0,
                sample_index: i as i32,
            })
            .collect();
        let before = samples.len();
        let (out, out_hits) = cluster_ssg_pcm(samples, hits, 32, 16);
        assert!(out.len() <= 32);
        assert_eq!(out_hits.len(), before);
        assert!(out_hits
            .iter()
            .all(|h| 0 <= h.sample_index && (h.sample_index as usize) < out.len()));
    }
}
