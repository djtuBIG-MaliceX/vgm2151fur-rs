//! SSG/PSG PCM playback -> sample instruments.
//!
//! AY-3-8910, YM2149 and the SSG inside the YM2203/YM2608/YM2610 can play raw
//! samples without any sample hardware: the driver disables the voice's tone
//! and noise in the mixer and writes the volume register at audio rate. The
//! volume register then acts as a 4-bit DAC.
//!
//! This module turns such a stream into ordinary 8-bit PCM bodies plus notes.
//! A VGM DAC stream's `0x93` start is one note (bounded by its `0x94` stop);
//! direct writes are split at pauses. Furnace's AY platform plays the bodies
//! through a `DIV_INS_AY` instrument with `amiga.useSample` set. One
//! instrument carries up to 120 bodies in its sample map (notes 60-179), every
//! slot stored at C-4, so a drum track stays inside the 255-instrument module
//! limit and each body plays at its stored `c4_rate`.
//!
//! The level -> byte map is the inverse of Furnace's `dacLogTableAY`
//! (`platform/ay.cpp`). `runDAC` computes the register level as
//! `dacLogTableAY[sampleByte ^ 0x80]`, so picking the byte whose table entry
//! is the source level reproduces the source register stream exactly.
//!
//! Port of `vgm2151fur/ssgpcm.py` (v0.11.0).

use std::collections::{BTreeMap, HashMap};

use indexmap::IndexMap;

use crate::parity::{py_int, py_max_by, py_round};
use crate::samplecluster::cluster_fragments;
use crate::vgm::{DacStreamPlay, VgmFile, VGM_RATE};

// Furnace's AY sample DAC table (platform/ay.cpp). Index is the sample byte
// XOR 0x80, value is the 4-bit volume register level.
pub const DAC_LOG_TABLE_AY: [u8; 256] = [
    0, 2, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 5, 5, 5,
    6, 6, 6, 6, 6, 6, 6, 6, 6, 7, 7, 7, 7, 7, 7, 7,
    7, 7, 7, 7, 7, 7, 8, 8, 8, 8, 8, 8, 8, 8, 8, 8,
    8, 8, 8, 8, 8, 9, 9, 9, 9, 9, 9, 9, 9, 9, 9, 9,
    9, 9, 9, 9, 9, 9, 9, 9, 10, 10, 10, 10, 10, 10, 10, 10,
    10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 11,
    11, 11, 11, 11, 11, 11, 11, 11, 11, 11, 11, 11, 11, 11, 11, 11,
    11, 11, 11, 11, 11, 11, 11, 11, 11, 11, 12, 12, 12, 12, 12, 12,
    12, 12, 12, 12, 12, 12, 12, 12, 12, 12, 12, 12, 12, 12, 12, 12,
    12, 12, 12, 12, 12, 12, 12, 12, 12, 12, 13, 13, 13, 13, 13, 13,
    13, 13, 13, 13, 13, 13, 13, 13, 13, 13, 13, 13, 13, 13, 13, 13,
    13, 13, 13, 13, 13, 13, 13, 13, 13, 13, 13, 13, 13, 13, 13, 14,
    14, 14, 14, 14, 14, 14, 14, 14, 14, 14, 14, 14, 14, 14, 14, 14,
    14, 14, 14, 14, 14, 14, 14, 14, 14, 14, 14, 14, 14, 14, 14, 14,
    14, 14, 14, 14, 14, 14, 14, 14, 14, 14, 15, 15, 15, 15, 15, 15,
    15, 15, 15, 15, 15, 15, 15, 15, 15, 15, 15, 15, 15, 15, 15, 15,
];

/// First table index that produces each register level. Level 1 is not in the
/// table (it jumps 0 -> 2); map it to level 2 rather than silencing it.
const fn level_byte() -> [u8; 16] {
    let mut out = [0u8; 16];
    let mut level = 0usize;
    while level < 16 {
        let mut i = 0usize;
        while i < DAC_LOG_TABLE_AY.len() {
            if DAC_LOG_TABLE_AY[i] == level as u8 {
                out[level] = i as u8;
                break;
            }
            i += 1;
        }
        level += 1;
    }
    out[1] = out[2];
    out
}

pub const _LEVEL_BYTE: [u8; 16] = level_byte();

// The volume-register level is the DAC value only while tone and noise are off
// and the hardware envelope is not driving the channel.
pub const REG_VOL: [u32; 3] = [8, 9, 10];
pub const REG_MIXER: u32 = 7;

// A voice is PCM when the register writes are this close together. A normal
// tone driver writes the volume at most once a frame (~735 samples); these
// streams write every 2-16 samples.
pub const PCM_MAX_GAP: i64 = 64;
pub const PCM_MAX_MEDIAN_DT: i64 = 16;
pub const PCM_MIN_WRITES: usize = 8;
// Split a PCM window into samples at pauses longer than this (46 ms). This
// separates drum hits while keeping a continuous stream in one body.
pub const PCM_SPLIT_GAP: i64 = 2048;
// Give up per-hit detail before the module runs out of instrument slots.
pub const PCM_INSTRUMENT_BUDGET: usize = 160;
// The rate is the source write rate; clamp to sane sample rates.
pub const PCM_MIN_RATE: i32 = 4000;
pub const PCM_MAX_RATE: i32 = 32000;
// VGM DAC-stream (0x90-0x95) chip types that can name an SSG volume register.
// These are the header chip IDs the stream setup uses: YM2203, YM2608,
// YM2610 and the stand-alone AY-8910.
//
// Python keeps this as a dict and only ever calls `.get`, so the port stores
// the insertion-ordered pairs and looks them up linearly.
pub const STREAM_CHIP: &[(u8, &str)] = &[
    (0x06, "ym2203_ssg"),
    (0x07, "ym2608_ssg"),
    (0x08, "ym2610_ssg"),
    (0x12, "ay8910"),
];
// A body shorter than this is a click or a segmentation fragment, not a
// sample. Short direct-write segments are folded into the next one on the
// same voice so the source audio is kept instead of disappearing; a short
// DAC-stream play is dropped outright.
pub const PCM_MIN_FRAMES: i64 = 128;
// Only bodies longer than this are candidates for the DC drop/trim; short
// percussion hits must keep their exact shape.
pub const SSG_PCM_DC_MIN_LEN: usize = 2048;
// Pointers this close into the same bank range are the same sample with a
// few-frame start shift, not distinct bodies.
pub const SSG_PCM_OFFSET_TOLERANCE: u32 = 4;

/// One 8-bit body and the register timing it came from.
#[derive(Clone, Debug, PartialEq)]
pub struct SsgPcmSample {
    pub name: String,
    pub data: Vec<u8>,
    pub rate: i32,
    pub start: i64,
    pub end: i64,
    pub voice: i32,
    pub chip_id: u8,
}

impl Default for SsgPcmSample {
    fn default() -> Self {
        SsgPcmSample {
            name: String::new(),
            data: Vec::new(),
            rate: 0,
            start: 0,
            end: 0,
            voice: 0,
            chip_id: 0,
        }
    }
}

/// One note that plays a body from the start.
#[derive(Clone, Debug, PartialEq)]
pub struct SsgPcmHit {
    pub sample_time: i64,
    pub end_time: i64,
    pub voice: i32,
    pub chip_id: u8,
    pub sample_index: i32,
}

impl Default for SsgPcmHit {
    fn default() -> Self {
        SsgPcmHit {
            sample_time: 0,
            end_time: 0,
            voice: 0,
            chip_id: 0,
            sample_index: 0,
        }
    }
}

fn _dac_level(val: u32) -> i32 {
    (val & 0x0F) as i32
}

/// Sample byte that `runDAC` maps back to this register level.
fn _volume_to_byte(level: i32) -> u8 {
    _LEVEL_BYTE[level.clamp(0, 15) as usize] ^ 0x80
}

/// True while the voice's volume register is a DC DAC level.
fn _dac_mode(mixer: u8, voice: usize, val: u32) -> bool {
    let tone_off = mixer & (1 << voice) != 0;
    let noise_off = mixer & (1 << (voice + 3)) != 0;
    tone_off && noise_off && (val & 0x10) == 0
}

fn stream_chip(type_: u8) -> Option<&'static str> {
    STREAM_CHIP
        .iter()
        .find(|(t, _)| *t == type_)
        .map(|(_, name)| *name)
}

/// Per (chip instance, voice), the writes that landed in DAC mode.
///
/// Each entry is `(sample, level)`. A later write at the same sample wins;
/// the hardware only sounds the last one.
fn _volume_streams(vgm: &VgmFile, chip: &str) -> IndexMap<(u8, usize), Vec<(i64, i32)>> {
    let mut streams: IndexMap<(u8, usize), Vec<(i64, i32)>> = IndexMap::new();
    let mut state: HashMap<u8, [u8; 16]> = HashMap::new();
    for w in &vgm.writes {
        if w.chip != chip || w.reg > 0x0F {
            continue;
        }
        let cid = w.chip_id;
        let reg = state.entry(cid).or_insert_with(|| {
            let mut reg = [0u8; 16];
            reg[REG_MIXER as usize] = 0x38; // reset: tone on, noise off
            reg
        });
        reg[w.reg as usize] = (w.val & 0xFF) as u8;
        let voice = match REG_VOL.iter().position(|&r| r == w.reg) {
            Some(voice) => voice,
            None => continue,
        };
        if !_dac_mode(reg[REG_MIXER as usize], voice, w.val) {
            continue;
        }
        let times = streams.entry((cid, voice)).or_default();
        let level = _dac_level(w.val);
        // Same-sample writes: keep only the final value.
        match times.last_mut() {
            Some(last) if last.0 == w.sample => last.1 = level,
            _ => times.push((w.sample, level)),
        }
    }
    streams
}

/// The full byte range a 0x93 stream start plays out of the data bank.
///
/// Mode 1 counts bytes, mode 2 milliseconds of the 0x92 rate and mode 3 the
/// rest of the bank. The stream is used in its entirety: a 2,000-byte slice
/// is 2,000 register writes, not one block.
fn _stream_body(vgm: &VgmFile, play: &DacStreamPlay) -> Vec<u8> {
    let bank = &vgm.dac_data;
    if bank.is_empty() {
        return Vec::new();
    }
    let start = (play.offset as usize).min(bank.len());
    let mode = play.mode & 0x07;
    let end = match mode {
        1 => start.saturating_add(play.length as usize),
        2 => {
            let rate = if play.rate != 0 {
                play.rate as i64
            } else {
                VGM_RATE
            };
            let played = py_int(py_round(
                (play.length as u128 * rate as u128) as f64 / 1000.0,
            ));
            start.saturating_add(played.max(0) as usize)
        }
        3 => bank.len(),
        _ => return Vec::new(),
    };
    let end = end.min(bank.len());
    let step = if play.step > 1 {
        play.step as usize
    } else {
        1
    };
    let mut body: Vec<u8> = bank[start..end].iter().copied().step_by(step).collect();
    if play.reverse {
        body.reverse();
    }
    // A 0x94 stop (or the next 0x93 start) is the real end of the play. The
    // declared length normally matches it; a driver that stops early keeps
    // only the bytes it had time to write.
    if play.stop_sample > play.sample && !body.is_empty() {
        let rate = if play.rate != 0 {
            play.rate as i64
        } else {
            VGM_RATE
        };
        let played = py_int(py_round(
            ((play.stop_sample - play.sample) as i128 * rate as i128) as f64
                / VGM_RATE as f64,
        ));
        if played > 0 && played < body.len() as i64 {
            body.truncate(played as usize);
        }
    }
    body
}

/// `(chip_id, voice, rate, points, bank offset)` for one expanded play.
type StreamTake = (u8, usize, i32, Vec<(i64, i32)>, u32);

/// Expand DAC-stream plays that write an SSG volume register.
///
/// Every `0x93` start is its own note, at its own rate, bounded by its
/// `0x94` stop. Returns the takes and a count of looping streams. The bank
/// offset lets the caller store the longest body once when several plays read
/// the same source range.
fn _stream_points(vgm: &VgmFile, chip: &str) -> (Vec<StreamTake>, i64) {
    let mut out: Vec<StreamTake> = Vec::new();
    let mut loops = 0i64;
    if vgm.dac_stream_plays.is_empty() || vgm.dac_data.is_empty() {
        return (out, loops);
    }
    for play in &vgm.dac_stream_plays {
        if stream_chip(play.chip_type) != Some(chip) {
            continue;
        }
        let reg = (play.reg & 0x0F) as u32;
        let voice = match REG_VOL.iter().position(|&r| r == reg) {
            Some(voice) => voice,
            None => continue,
        };
        let body = _stream_body(vgm, play);
        if body.is_empty() {
            continue;
        }
        let rate = if play.rate != 0 {
            play.rate as i32
        } else {
            VGM_RATE as i32
        };
        if play.loop_ {
            loops += 1;
        }
        let step = VGM_RATE as f64 / rate as f64;
        let points = body
            .iter()
            .enumerate()
            .map(|(i, &byte)| {
                (
                    py_int(py_round(play.sample as f64 + i as f64 * step)),
                    (byte & 0x0F) as i32,
                )
            })
            .collect();
        out.push((play.chip_id, voice, rate, points, play.offset));
    }
    (out, loops)
}

/// Group a voice's DAC writes into high-rate bursts.
fn _bursts(
    points: &[(i64, i32)],
    max_gap: i64,
    min_writes: usize,
    max_median_dt: i64,
) -> Vec<Vec<(i64, i32)>> {
    let mut out: Vec<Vec<(i64, i32)>> = Vec::new();
    let mut cur: Vec<(i64, i32)> = Vec::new();
    let mut prev: Option<i64> = None;
    for &point in points {
        if let Some(prev) = prev {
            if point.0 - prev <= max_gap {
                cur.push(point);
            } else {
                if cur.len() >= min_writes && _median_dt(&cur) <= max_median_dt {
                    out.push(std::mem::take(&mut cur));
                }
                cur = vec![point];
            }
        } else {
            cur = vec![point];
        }
        prev = Some(point.0);
    }
    if cur.len() >= min_writes && _median_dt(&cur) <= max_median_dt {
        out.push(cur);
    }
    out
}

fn _median_dt(points: &[(i64, i32)]) -> i64 {
    if points.len() < 2 {
        return 0;
    }
    let mut dts: Vec<i64> = (0..points.len() - 1)
        .map(|i| points[i + 1].0 - points[i].0)
        .collect();
    dts.sort();
    dts[dts.len() / 2]
}

/// Join bursts separated by less than `split_gap`.
///
/// The fast-run grouper cuts at every gap over `PCM_MAX_GAP`; this puts the
/// short holes back so a continuous stream stays one body while a real pause
/// starts a new sample. It also gives the instrument budget something to
/// turn: a larger gap merges neighbouring hits.
fn _merge_bursts(
    bursts: Vec<Vec<(i64, i32)>>,
    split_gap: i64,
) -> Vec<Vec<(i64, i32)>> {
    let mut out: Vec<Vec<(i64, i32)>> = Vec::new();
    for burst in bursts {
        if !burst.is_empty()
            && !out.is_empty()
            && burst[0].0 - out.last().unwrap().last().unwrap().0 < split_gap
        {
            out.last_mut().unwrap().extend(burst);
        } else if !burst.is_empty() {
            out.push(burst);
        }
    }
    out.into_iter().filter(|seg| seg.len() >= 2).collect()
}

/// Playback rate from the typical write spacing.
///
/// The nominal spacing carries the high-frequency content; a pause merely
/// holds the current level and must not lower the sample rate. The resample
/// still spans the real timestamps, so the duration is unchanged.
fn _rate_for(points: &[(i64, i32)]) -> i32 {
    let median = _median_dt(points);
    if median <= 0 {
        return VGM_RATE as i32;
    }
    let rate = py_int(py_round(VGM_RATE as f64 / median as f64)) as i32;
    rate.clamp(PCM_MIN_RATE, PCM_MAX_RATE)
}

fn _shared_rate(segments: &[&[(i64, i32)]]) -> i32 {
    let mut rates: Vec<i32> = segments
        .iter()
        .filter(|seg| seg.len() >= 2)
        .map(|seg| _rate_for(seg))
        .collect();
    rates.sort();
    if rates.is_empty() {
        return VGM_RATE as i32;
    }
    rates[rates.len() / 2]
}

/// Sample-and-hold the register values on a uniform timeline.
fn _resample(points: &[(i64, i32)], rate: i32) -> Vec<u8> {
    let t0 = points[0].0;
    let t1 = points[points.len() - 1].0;
    let span = (t1 - t0).max(1);
    let frames =
        (py_int(py_round(span as f64 * rate as f64 / VGM_RATE as f64)) + 1).max(1) as usize;
    let mut out = vec![0u8; frames];
    let mut i = 0usize;
    let mut cur = points[0].1;
    let step = VGM_RATE as f64 / rate as f64;
    for k in 0..frames {
        let t = t0 as f64 + k as f64 * step;
        while i + 1 < points.len() && (points[i + 1].0 as f64) <= t {
            i += 1;
            cur = points[i].1;
        }
        out[k] = _volume_to_byte(cur);
    }
    out
}

fn _body_frames(seg: &[(i64, i32)], rate: i32) -> i64 {
    let span = seg[seg.len() - 1].0 - seg[0].0;
    (py_int(py_round(span as f64 * rate as f64 / VGM_RATE as f64)) + 1).max(1)
}

/// Longest consecutive run of one level.
fn _longest_run(data: &[u8]) -> usize {
    if data.is_empty() {
        return 0;
    }
    let mut best = 1usize;
    let mut cur = 1usize;
    for i in 1..data.len() {
        if data[i] == data[i - 1] {
            cur += 1;
            if cur > best {
                best = cur;
            }
        } else {
            cur = 1;
        }
    }
    best
}

/// Drop DC-heavy bodies and trim a mostly-constant tail.
///
/// A stream play can declare a long length whose tail is just the held AY
/// level: a short transient followed by a flat line. If more than 80% of
/// the body is one consecutive run at a single level, it is a DC offset
/// rather than audio and the whole body is dropped. Otherwise trim a
/// trailing constant run when it is at least half the body and at least 64
/// frames, and reject a body that is one level throughout or a bare step
/// between two levels.
fn _trim_dc(data: Vec<u8>) -> Vec<u8> {
    if data.len() <= SSG_PCM_DC_MIN_LEN {
        return data;
    }
    if _longest_run(&data) as f64 > data.len() as f64 * 0.80 {
        return Vec::new();
    }
    let last = data[data.len() - 1];
    let mut start = data.len() - 1;
    while start > 0 && data[start - 1] == last {
        start -= 1;
    }
    if start == 0 {
        return data[..1].to_vec(); // one constant level throughout
    }
    let run = data.len() - start;
    if run >= 64 && run * 2 >= data.len() - 2 {
        let trimmed = data[..start + 1].to_vec();
        let mut seen = [false; 256];
        let mut distinct = 0usize;
        for &b in &trimmed {
            if !seen[b as usize] {
                seen[b as usize] = true;
                distinct += 1;
            }
        }
        if distinct < 2 {
            return trimmed[..1].to_vec();
        }
        return trimmed;
    }
    data
}

/// Fold sub-minimum segments into the next one on the same voice.
///
/// A 2 ms fragment is not worth an instrument, but dropping it would lose
/// the hit. Extending the next body back to the fragment's start keeps the
/// source audio (including the pause between them) in one playable sample.
/// If the last segment is still short it joins the previous one; a lone
/// short segment is dropped.
fn _merge_short(
    segments: &[Vec<(i64, i32)>],
    rate: i32,
) -> Vec<Vec<(i64, i32)>> {
    let mut out: Vec<Vec<(i64, i32)>> = Vec::new();
    let mut i = 0usize;
    while i < segments.len() {
        let mut seg = segments[i].clone();
        while _body_frames(&seg, rate) < PCM_MIN_FRAMES && i + 1 < segments.len() {
            i += 1;
            seg.extend_from_slice(&segments[i]);
        }
        out.push(seg);
        i += 1;
    }
    if out.len() >= 2 && _body_frames(out.last().unwrap(), rate) < PCM_MIN_FRAMES {
        let last = out.pop().unwrap();
        out.last_mut().unwrap().extend(last);
    }
    out.into_iter()
        .filter(|seg| _body_frames(seg, rate) >= PCM_MIN_FRAMES)
        .collect()
}

/// Share identical bodies. Returns `(samples, hits, shared)`.
fn _dedupe(
    samples: Vec<SsgPcmSample>,
    mut hits: Vec<SsgPcmHit>,
) -> (Vec<SsgPcmSample>, Vec<SsgPcmHit>, i64) {
    let mut by_body: HashMap<(i32, Vec<u8>), usize> = HashMap::new();
    let mut out: Vec<SsgPcmSample> = Vec::new();
    let mut remap: Vec<usize> = vec![0; samples.len()];
    let mut shared = 0i64;
    for (i, smp) in samples.into_iter().enumerate() {
        let key = (smp.rate, smp.data.clone());
        let idx = match by_body.get(&key) {
            Some(&idx) => {
                shared += 1;
                idx
            }
            None => {
                let idx = out.len();
                by_body.insert(key, idx);
                out.push(smp);
                idx
            }
        };
        remap[i] = idx;
    }
    for hit in &mut hits {
        hit.sample_index = remap[hit.sample_index as usize] as i32;
    }
    (out, hits, shared)
}

fn build_segments(
    entries: &[(usize, Vec<(i64, i32)>)],
    split_gap: i64,
) -> Vec<(usize, Vec<(i64, i32)>)> {
    let mut segments: Vec<(usize, Vec<(i64, i32)>)> = Vec::new();
    for (voice, points) in entries {
        let bursts = _bursts(
            points,
            PCM_MAX_GAP,
            PCM_MIN_WRITES,
            PCM_MAX_MEDIAN_DT,
        );
        for seg in _merge_bursts(bursts, split_gap) {
            segments.push((*voice, seg));
        }
    }
    segments
}

/// Extract PCM-played SSG voices into 8-bit sample bodies.
///
/// Returns `(samples, hits, warnings)`. One sample per segment, one hit per
/// segment start; the caller maps `voice`/`chip_id` to a module channel. The
/// Python default keyword is `chip="ay8910"`.
pub fn collect_ssg_pcm(
    vgm: &VgmFile,
    chip: &str,
) -> (Vec<SsgPcmSample>, Vec<SsgPcmHit>, Vec<String>) {
    let mut warnings: Vec<String> = Vec::new();
    let direct = _volume_streams(vgm, chip);
    let (streamed, stream_loops) = _stream_points(vgm, chip);
    if direct.is_empty() && streamed.is_empty() {
        return (Vec::new(), Vec::new(), warnings);
    }

    // Direct register writes: group into bursts, split at pauses, and merge
    // fragments under the minimum length. DAC streams are handled separately:
    // each 0x93 start is already one note, and the stop/start gap is only a
    // few samples, so merging them would rebuild the giant body this avoids.
    let mut per_chip: BTreeMap<u8, Vec<(usize, Vec<(i64, i32)>)>> = BTreeMap::new();
    for ((cid, voice), points) in &direct {
        per_chip.entry(*cid).or_default().push((*voice, points.clone()));
    }

    // (chip, voice, rate, body data, start, end, hits), where each hit is its
    // own (voice, start, end). Direct-write segments have one hit; a bank
    // range shared by several stream plays gets one body and many hits.
    type Plays = Vec<(i32, i64, i64)>;
    let mut all_segments: Vec<(u8, i32, i32, Vec<u8>, i64, i64, Plays)> = Vec::new();

    for (&cid, entries) in &per_chip {
        let mut gap = PCM_SPLIT_GAP;
        let mut segments = build_segments(entries, gap);
        while segments.len() > PCM_INSTRUMENT_BUDGET && gap < (1 << 20) {
            gap *= 2;
            segments = build_segments(entries, gap);
        }
        let seg_refs: Vec<&[(i64, i32)]> =
            segments.iter().map(|(_, seg)| seg.as_slice()).collect();
        let rate = _shared_rate(&seg_refs);
        let mut by_voice: IndexMap<usize, Vec<Vec<(i64, i32)>>> = IndexMap::new();
        for (voice, seg) in segments {
            by_voice.entry(voice).or_default().push(seg);
        }
        for (voice, segs) in by_voice {
            for seg in _merge_short(&segs, rate) {
                let data = _trim_dc(_resample(&seg, rate));
                if data.len() < 2 {
                    continue;
                }
                all_segments.push((
                    cid,
                    voice as i32,
                    rate,
                    data,
                    seg[0].0,
                    seg[seg.len() - 1].0,
                    vec![(voice as i32, seg[0].0, seg[seg.len() - 1].0)],
                ));
            }
        }
    }

    // Plays that start at the same bank offset read the same source from the
    // start; a short one is a prefix of the longest. Store the longest body
    // once and let each play's stop cut it, the same rule the banked YM2612
    // DAC path uses. Trim a held DC tail first: a long declared play is often
    // a short transient followed by the register's constant level.
    //
    // A driver can also latch a pointer a few bytes into the same sample.
    // Cluster offsets within SSG_PCM_OFFSET_TOLERANCE so those become one
    // body with a few-frame start shift instead of adjacent duplicates. Use
    // one median stream rate per chip; the log's integer rates differ by a
    // hertz or two and would otherwise resample the same source into distinct
    // arrays.
    let mut stream_groups: IndexMap<(u8, u32), Vec<(usize, i32, Vec<(i64, i32)>)>> =
        IndexMap::new();
    let mut stream_rates: BTreeMap<u8, Vec<i32>> = BTreeMap::new();
    for (cid, voice, rate, points, offset) in &streamed {
        if points.len() < 2 || _body_frames(points, *rate) < PCM_MIN_FRAMES {
            continue;
        }
        stream_groups
            .entry((*cid, *offset))
            .or_default()
            .push((*voice, *rate, points.clone()));
        stream_rates.entry(*cid).or_default().push(*rate);
    }
    for (&cid, rates_all) in &stream_rates {
        let mut offsets: Vec<u32> = stream_groups
            .keys()
            .filter(|(c, _)| *c == cid)
            .map(|(_, off)| *off)
            .collect();
        offsets.sort();
        let mut clusters: Vec<Vec<u32>> = Vec::new();
        let mut current: Vec<u32> = Vec::new();
        for off in offsets {
            if !current.is_empty() && off - current[current.len() - 1] <= SSG_PCM_OFFSET_TOLERANCE
            {
                current.push(off);
            } else {
                if !current.is_empty() {
                    clusters.push(current);
                }
                current = vec![off];
            }
        }
        if !current.is_empty() {
            clusters.push(current);
        }
        let mut rates = rates_all.clone();
        rates.sort();
        let rate = rates[rates.len() / 2];
        for cluster in clusters {
            let mut takes: Vec<(usize, i32, Vec<(i64, i32)>)> = Vec::new();
            for off in &cluster {
                if let Some(list) = stream_groups.get(&(cid, *off)) {
                    takes.extend(list.iter().cloned());
                }
            }
            let mut candidates: Vec<(usize, Vec<(i64, i32)>, Vec<u8>)> = Vec::new();
            for (voice, _take_rate, points) in &takes {
                let data = _trim_dc(_resample(points, rate));
                if data.len() >= 2 {
                    candidates.push((*voice, points.clone(), data));
                }
            }
            if candidates.is_empty() {
                continue;
            }
            let best = py_max_by(&candidates, |c| c.2.len()).unwrap();
            let voice = best.0;
            let seg = best.1.clone();
            let data = best.2.clone();
            let plays: Vec<(i32, i64, i64)> = takes
                .iter()
                .map(|(hit_voice, _rate, pts)| {
                    (*hit_voice as i32, pts[0].0, pts[pts.len() - 1].0)
                })
                .collect();
            all_segments.push((
                cid,
                voice as i32,
                rate,
                data,
                seg[0].0,
                seg[seg.len() - 1].0,
                plays,
            ));
        }
    }

    let mut samples: Vec<SsgPcmSample> = Vec::new();
    let mut hits: Vec<SsgPcmHit> = Vec::new();
    for (cid, voice, rate, data, start, end, plays) in all_segments {
        let idx = samples.len() as i32;
        samples.push(SsgPcmSample {
            name: format!("SSG {:02} {} {:08x}", cid, voice + 1, start),
            data,
            rate,
            start,
            end,
            voice,
            chip_id: cid,
        });
        for (hit_voice, hit_start, hit_end) in plays {
            hits.push(SsgPcmHit {
                sample_time: hit_start,
                end_time: hit_end,
                voice: hit_voice,
                chip_id: cid,
                sample_index: idx,
            });
        }
    }

    if samples.is_empty() {
        return (Vec::new(), Vec::new(), warnings);
    }
    let (samples, mut hits, shared) = _dedupe(samples, hits);
    hits.sort_by(|a, b| {
        (a.sample_time, a.chip_id, a.voice).cmp(&(b.sample_time, b.chip_id, b.voice))
    });
    let label = match chip {
        "ay8910" => "AY-8910",
        "ym2203_ssg" => "YM2203 SSG",
        "ym2608_ssg" => "YM2608 SSG",
        "ym2610_ssg" => "YM2610 SSG",
        other => other,
    };
    let mut msg = format!(
        "{label}: {} PCM sample note(s) in {} 8-bit bod{}",
        hits.len(),
        samples.len(),
        if samples.len() == 1 { "y" } else { "ies" }
    );
    if shared != 0 {
        msg.push_str(&format!(" ({shared} shared)"));
    }
    warnings.push(msg);
    if stream_loops != 0 {
        warnings.push(format!(
            "{label}: {stream_loops} looping DAC stream(s) played once"
        ));
    }
    (samples, hits, warnings)
}

/// Optionally fold short SSG PCM bodies into the hottest/longest ones.
///
/// This is the same lossy reduction the YM2612 DAC offers. A hit keeps its
/// time and cut; only its sample index changes.
pub fn cluster_ssg_pcm(
    samples: Vec<SsgPcmSample>,
    mut hits: Vec<SsgPcmHit>,
    target: i32,
    hot: i32,
) -> (Vec<SsgPcmSample>, Vec<SsgPcmHit>) {
    if samples.len() as i64 <= target as i64 {
        return (samples, hits);
    }
    let mut uses: IndexMap<i32, i64> = IndexMap::new();
    for hit in &hits {
        *uses.entry(hit.sample_index).or_insert(0) += 1;
    }
    let bodies: Vec<Vec<u8>> = samples.iter().map(|smp| smp.data.clone()).collect();
    let rates: Vec<i32> = samples.iter().map(|smp| smp.rate).collect();
    let (keep, remap) = cluster_fragments(&bodies, &rates, &uses, target, hot);
    if keep.len() == samples.len() {
        return (samples, hits);
    }
    let new_samples: Vec<SsgPcmSample> = keep
        .iter()
        .map(|&i| samples[i as usize].clone())
        .collect();
    for hit in &mut hits {
        hit.sample_index = remap[hit.sample_index as usize];
    }
    (new_samples, hits)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vgm::ChipWrite;

    fn w(sample: i64, reg: u32, val: u32) -> ChipWrite {
        ChipWrite::new(sample, "ay8910".into(), 0, reg, val)
    }

    fn vgm_with(writes: Vec<ChipWrite>) -> VgmFile {
        let mut vgm = VgmFile::default();
        vgm.writes = writes;
        vgm
    }

    #[test]
    fn level_table_inverts_the_furnace_dac_table() {
        assert_eq!(_volume_to_byte(0), 0x80);
        // Level 1 is missing from the table; it maps to level 2's byte.
        assert_eq!(_volume_to_byte(1), _volume_to_byte(2));
        for level in 0..16u8 {
            let byte = _volume_to_byte(level as i32);
            let mapped = DAC_LOG_TABLE_AY[(byte ^ 0x80) as usize];
            if level != 1 {
                assert_eq!(mapped, level);
            }
        }
        assert_eq!(_volume_to_byte(99), _volume_to_byte(15));
        assert_eq!(_volume_to_byte(-4), _volume_to_byte(0));
        // Tone and noise off for voice 0; bit 4 would mean envelope mode.
        assert!(_dac_mode(0x09, 0, 7));
        assert!(!_dac_mode(0x38, 0, 7));
        assert!(!_dac_mode(0x09, 0, 0x17));
    }

    #[test]
    fn direct_writes_become_one_body_and_one_hit() {
        let mut writes = vec![w(0, REG_MIXER, 0x09)];
        for i in 0..400i64 {
            writes.push(w(100 + i * 8, 8, (i % 15) as u32));
        }
        let (samples, hits, warnings) = collect_ssg_pcm(&vgm_with(writes), "ay8910");
        assert_eq!(samples.len(), 1);
        assert_eq!(hits.len(), 1);
        // 44100 / 8 = 5512.5 rounds half-to-even to 5512.
        assert_eq!(samples[0].rate, 5512);
        assert_eq!(samples[0].start, 100);
        assert_eq!(samples[0].end, 100 + 399 * 8);
        assert_eq!(samples[0].voice, 0);
        assert_eq!(samples[0].chip_id, 0);
        assert_eq!(samples[0].data.len(), 400);
        assert_eq!(samples[0].name, "SSG 00 1 00000064");
        assert_eq!(hits[0].sample_time, 100);
        assert_eq!(hits[0].end_time, samples[0].end);
        assert_eq!(hits[0].sample_index, 0);
        assert_eq!(warnings.len(), 1);
        assert_eq!(
            warnings[0],
            "AY-8910: 1 PCM sample note(s) in 1 8-bit body"
        );
    }

    #[test]
    fn a_long_pause_splits_a_voice_into_two_bodies() {
        let mut writes = vec![w(0, REG_MIXER, 0x09)];
        for i in 0..300i64 {
            writes.push(w(100 + i * 8, 8, (i % 15) as u32));
        }
        let gap_start = 100 + 299 * 8 + 4000;
        for i in 0..300i64 {
            writes.push(w(gap_start + i * 8, 8, ((i + 5) % 15) as u32));
        }
        let (samples, hits, warnings) = collect_ssg_pcm(&vgm_with(writes), "ay8910");
        assert_eq!(samples.len(), 2);
        assert_eq!(hits.len(), 2);
        assert!(warnings[0].contains("2 PCM sample note(s) in 2 8-bit bodies"));
    }

    #[test]
    fn stream_body_honours_mode_step_reverse_and_stop() {
        let mut vgm = VgmFile::default();
        vgm.dac_data = (0..64u8).collect();
        let play = DacStreamPlay {
            sample: 1000,
            offset: 8,
            length: 16,
            mode: 1,
            step: 2,
            ..Default::default()
        };
        assert_eq!(
            _stream_body(&vgm, &play),
            vec![8, 10, 12, 14, 16, 18, 20, 22]
        );

        let mut rev = play.clone();
        rev.reverse = true;
        assert_eq!(
            _stream_body(&vgm, &rev),
            vec![22, 20, 18, 16, 14, 12, 10, 8]
        );

        // A 0x94 stop at 8 samples at 44.1 kHz plays eight bytes.
        let mut stopped = play.clone();
        stopped.step = 1;
        stopped.length = 64;
        stopped.stop_sample = 1008;
        assert_eq!(_stream_body(&vgm, &stopped).len(), 8);

        // Mode 3 takes the rest of the bank.
        let mut rest = play.clone();
        rest.mode = 3;
        rest.offset = 60;
        rest.step = 1;
        assert_eq!(_stream_body(&vgm, &rest), vec![60, 61, 62, 63]);
    }

    #[test]
    fn dac_stream_plays_expand_into_volume_points() {
        let mut vgm = VgmFile::default();
        vgm.dac_data = vec![0x80, 0x8F, 0x80, 0x8F];
        vgm.dac_stream_plays = vec![DacStreamPlay {
            sample: 4410,
            chip_type: 0x12,
            chip_id: 3,
            reg: 8,
            offset: 0,
            length: 4,
            mode: 1,
            rate: 0,
            loop_: true,
            ..Default::default()
        }];
        let (streams, loops) = _stream_points(&vgm, "ay8910");
        assert_eq!(loops, 1);
        assert_eq!(streams.len(), 1);
        let (cid, voice, rate, points, offset) = &streams[0];
        assert_eq!((*cid, *voice, *rate, *offset), (3, 0, 44100, 0));
        assert_eq!(points.len(), 4);
        assert_eq!(points[0], (4410, 0));
        assert_eq!(points[1], (4411, 15));
        assert_eq!(points[3], (4413, 15));
        // A different chip name does not match the take.
        assert!(_stream_points(&vgm, "ym2608_ssg").0.is_empty());
    }

    #[test]
    fn dedupe_shares_identical_bodies_and_remaps_hits() {
        let body = vec![0x80, 0x81, 0x82];
        let samples = vec![
            SsgPcmSample { name: "a".into(), data: body.clone(), rate: 8000, start: 0, end: 10, voice: 0, chip_id: 0 },
            SsgPcmSample { name: "b".into(), data: body.clone(), rate: 8000, start: 100, end: 110, voice: 1, chip_id: 0 },
            SsgPcmSample { name: "c".into(), data: vec![1, 2, 3], rate: 8000, start: 200, end: 210, voice: 2, chip_id: 0 },
        ];
        let hits = vec![
            SsgPcmHit { sample_time: 0, end_time: 10, voice: 0, chip_id: 0, sample_index: 0 },
            SsgPcmHit { sample_time: 100, end_time: 110, voice: 1, chip_id: 0, sample_index: 1 },
            SsgPcmHit { sample_time: 200, end_time: 210, voice: 2, chip_id: 0, sample_index: 2 },
        ];
        let (samples, hits, shared) = _dedupe(samples, hits);
        assert_eq!(samples.len(), 2);
        assert_eq!(shared, 1);
        assert_eq!(hits[0].sample_index, 0);
        assert_eq!(hits[1].sample_index, 0);
        assert_eq!(hits[2].sample_index, 1);
    }

    #[test]
    fn trim_dc_drops_flat_tails_and_keeps_short_hits() {
        // Short bodies are never touched.
        let short = vec![7u8; 100];
        assert_eq!(_trim_dc(short.clone()), short);

        // A long body that is one level throughout is dropped.
        assert!(_trim_dc(vec![5u8; 3000]).is_empty());

        // A transient followed by a long held tail is trimmed to the transient.
        let mut data = Vec::new();
        for i in 0..1000 {
            data.push(if i % 2 == 0 { 0x80 } else { 0x81 });
        }
        data.extend(std::iter::repeat(0x83).take(2000));
        let trimmed = _trim_dc(data);
        assert_eq!(trimmed.len(), 1001);
        assert_eq!(trimmed[0], 0x80);
    }

    #[test]
    fn clustering_is_a_noop_below_the_target() {
        let samples = vec![
            SsgPcmSample { name: "a".into(), data: vec![1, 2], rate: 8000, start: 0, end: 1, voice: 0, chip_id: 0 },
            SsgPcmSample { name: "b".into(), data: vec![3, 4], rate: 8000, start: 2, end: 3, voice: 1, chip_id: 0 },
        ];
        let hits = vec![
            SsgPcmHit { sample_time: 0, end_time: 1, voice: 0, chip_id: 0, sample_index: 0 },
            SsgPcmHit { sample_time: 2, end_time: 3, voice: 1, chip_id: 0, sample_index: 1 },
        ];
        let (out_samples, out_hits) = cluster_ssg_pcm(samples.clone(), hits.clone(), 32, 16);
        assert_eq!(out_samples, samples);
        assert_eq!(out_hits, hits);
    }
}
