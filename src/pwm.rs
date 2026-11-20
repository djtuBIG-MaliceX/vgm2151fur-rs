//! Sega 32X PWM: streamed 12-bit left/right samples -> PCM bodies.
//!
//! Port of `vgm2151fur/pwm.py` (v0.11.0).
//!
//! The PWM chip is not a sample-ROM player.  The driver streams two 12-bit
//! values per stereo frame through the DAC-stream control commands:
//!
//! ```text
//!     0x90 stream, chip 0x11, port 2 (left) or 3 (right)
//!     0x91 stream, bank 3, step size 2, step base 0/1
//!     0x92 stream, playback frequency (usually 22020 Hz)
//!     0x93/0x95 stream, position / data block
//! ```
//!
//! The data bank is VGM type 0x03 (sometimes n-Bit compressed as type 0x43).
//! Items are two-byte little-endian words whose low 12 bits are the PWM value.
//! Furnace has no 32X PWM platform, so each channel is written as a 16-bit PCM
//! sample on a Generic PCM DAC system and panned hard left/right.

use std::collections::{BTreeSet, HashMap, HashSet};

use indexmap::IndexMap;

use crate::parity::{py_int, py_round};
use crate::samplecluster::{cluster_fragments16, DEFAULT_HOT, DEFAULT_TARGET};
use crate::vgm::{ChipWrite, DacStreamPlay, VgmFile, STREAM_PWM, VGM_RATE};

pub const PWM_RATE: i32 = 22020;
pub const PWM_SIDES: i32 = 2; // left, right
pub const PWM_SIDE_LEFT: i32 = 0;
pub const PWM_SIDE_RIGHT: i32 = 1;
// Split the stream into YM2612-DAC-style pieces so one channel is not a single
// multi-megabyte sample.  0.25 s at 22020 Hz.
pub const PWM_CHUNK_FRAMES: i64 = 5505;
pub const PWM_MAP_SLOTS: i64 = 120;
// Attack segmentation.  A near-silent stretch separates two hits; the body is
// trimmed to the audible part and long bodies are split so a single segment
// cannot become a multi-second sample.  If this yields more unique bodies than
// fixed chunks, the collector falls back to chunks.
pub const PWM_ATTACK_SILENCE: i64 = 250; // ~11 ms of near-silence splits a hit
// A body shorter than a quarter chunk (~62 ms at 22020 Hz) is a fragment, not
// a useful banked sample; it is folded into the following body.
pub const PWM_ATTACK_MIN_FRAMES: i64 = PWM_CHUNK_FRAMES / 4;
pub const PWM_ATTACK_MAX_FRAMES: i64 = 44100; // split a sustained body at ~1 s
pub const PWM_ATTACK_QUIET_DIV: i64 = 32; // quiet threshold = peak / 32
pub const PWM_ATTACK_MAX_SAMPLES: i64 = 480; // above this, use fixed chunks
// Attack-transient candidates: a short rise from the idle band to a strong
// level is a new sample start even when there was no long rest before it.
pub const PWM_ONSET_WINDOW: i64 = 8;
pub const PWM_ONSET_QUIET: i64 = 32;
pub const PWM_ONSET_MIN_GAP: i64 = 64;
pub const PWM_ONSET_LOOKAHEAD: i64 = 64;

/// One chunk occurrence: where to start it and which side plays it.
#[derive(Clone, Debug, PartialEq)]
pub struct PwmHit {
    pub time: i64,
    pub side: i32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct PwmSample {
    pub chip_id: u8,
    pub side: i32,
    /// signed 16-bit little-endian at `PWM_RATE`
    pub data: Vec<u8>,
    pub rate: i32,
    pub loop_start: Option<i64>,
    pub loop_end: Option<i64>,
    pub hits: Vec<PwmHit>,
    pub chunk: i32,
    pub first_chunk: bool,
}

impl Default for PwmSample {
    fn default() -> Self {
        PwmSample {
            chip_id: 0,
            side: 0,
            data: Vec::new(),
            rate: 0,
            loop_start: None,
            loop_end: None,
            hits: Vec::new(),
            chunk: 0,
            first_chunk: true,
        }
    }
}

impl PwmSample {
    pub fn frames(&self) -> i64 {
        self.data.len() as i64 / 2
    }

    pub fn name(&self) -> String {
        format!("PWM{} {:03}", self.chip_id + 1, self.chunk)
    }
}

fn signed12(value: i64) -> i64 {
    let value = value & 0xFFF;
    if value & 0x800 != 0 { value - 0x1000 } else { value }
}

/// (offset, scale) from a PWM cycle register value, matching libvgm.
fn cycle_scale(cycle: i64) -> (i64, i64) {
    let cycle = (cycle - 1) & 0xFFF;
    let mut offset = (cycle / 2) + 1;
    if offset <= 0 {
        offset = 1;
    }
    (offset, 0x7FFF00 / offset)
}

fn scale_value(value: i64, cycle: i64) -> i64 {
    if value == 0 {
        return 0;
    }
    let (offset, scale) = cycle_scale(cycle);
    let out = ((signed12(value) - offset) * scale) >> 8;
    out.max(-32768).min(32767)
}

/// Data buffer and first byte for one DAC-stream play.
fn stream_positions<'a>(play: &DacStreamPlay, bank: &'a [u8], blocks: &'a [Vec<u8>]) -> (&'a [u8], i64) {
    let block = play.block as i64;
    let (data, mut start): (&[u8], i64) = if block >= 0 {
        let data = if block >= 0 && (block as usize) < blocks.len() {
            blocks[block as usize].as_slice()
        } else {
            bank
        };
        (data, 0)
    } else {
        let offset = play.offset as i64;
        (bank, if offset >= 0 { offset } else { 0 })
    };
    start += 2 * (play.step_base as i64).max(0);
    if start < 0 {
        start = 0;
    }
    if start > data.len() as i64 {
        start = data.len() as i64;
    }
    (data, start)
}

/// (values, idle level, quiet width, peak) for one rendered side.
fn pcm_stats(data: &[u8]) -> (Vec<i16>, i64, i64, i64) {
    let vals: Vec<i16> = data
        .chunks_exact(2)
        .map(|c| i16::from_le_bytes([c[0], c[1]]))
        .collect();
    if vals.is_empty() {
        return (vals, 0, 64, 0);
    }
    let peak = vals.iter().map(|&v| (v as i64).abs()).max().unwrap();
    // Counter(vals): value -> (count, first index); most_common(1) picks the
    // highest count and, on ties, the value encountered first.
    let mut counts: HashMap<i16, (i64, usize)> = HashMap::new();
    for (i, &v) in vals.iter().enumerate() {
        let entry = counts.entry(v).or_insert((0, i));
        entry.0 += 1;
    }
    let mut best: Option<(i64, usize, i16)> = None;
    for (&v, &(count, first)) in &counts {
        let better = match best {
            None => true,
            Some((bc, bf, _)) => count > bc || (count == bc && first < bf),
        };
        if better {
            best = Some((count, first, v));
        }
    }
    let (idle_count, _, idle_value) = best.unwrap();
    let mut idle = idle_value as i64;
    // Only a dominant low-level value is a real idle/rest level.  A common
    // musical sustain must not be mistaken for silence.
    if idle_count * 100 < vals.len() as i64 || idle.abs() > 64.max(peak / 4) {
        idle = 0;
    }
    let width = 64.max(peak / PWM_ATTACK_QUIET_DIV);
    (vals, idle, width, peak)
}

fn audible_pcm(data: &[u8], idle: i64, width: i64) -> bool {
    data.chunks_exact(2)
        .map(|c| i16::from_le_bytes([c[0], c[1]]))
        .any(|v| (v as i64 - idle).abs() > width)
}

/// Attack-transient candidates: rises out of the idle band.
fn onset_starts(vals: &[i16], idle: i64, width: i64, peak: i64) -> Vec<i64> {
    if vals.is_empty() {
        return Vec::new();
    }
    let amp: Vec<i64> = vals.iter().map(|&v| (v as i64 - idle).abs()).collect();
    let mut env: Vec<i64> = Vec::with_capacity(amp.len());
    let mut window: Vec<i64> = Vec::new();
    for &value in &amp {
        window.push(value);
        if window.len() > PWM_ONSET_WINDOW as usize {
            window.remove(0);
        }
        env.push(*window.iter().max().unwrap());
    }
    let low = width;
    let high = (width * 4).max(peak / 8);
    let mut starts: Vec<i64> = Vec::new();

    fn add(starts: &mut Vec<i64>, pos: i64) {
        if starts.is_empty() || pos - starts[starts.len() - 1] >= PWM_ONSET_MIN_GAP {
            starts.push(pos);
        }
    }

    if env[0] >= high {
        add(&mut starts, 0);
    }
    let mut low_start: Option<i64> = None;
    for (i, &value) in env.iter().enumerate() {
        let i = i as i64;
        if value <= low {
            if low_start.is_none() {
                low_start = Some(i);
            }
            continue;
        }
        if let Some(ls) = low_start {
            if i - ls >= PWM_ONSET_QUIET {
                let end = (i as usize + PWM_ONSET_LOOKAHEAD as usize).min(env.len());
                let look = &env[i as usize..end];
                if !look.is_empty() && *look.iter().max().unwrap() >= high {
                    add(&mut starts, i);
                }
            }
        }
        low_start = None;
    }
    for i in 1..env.len() {
        if env[i] >= high && env[i - 1] <= low {
            add(&mut starts, i as i64);
        }
    }
    starts.sort();
    starts
}

/// Split one rendered side into bodies starting at attack transients.
///
/// Every detected attack is a candidate start, so pieces reused inside a
/// longer musical passage can be banked separately.  When no attacks are
/// found the older rest-based split is used; both paths trim silence and
/// drop inaudible bodies.
fn attack_bodies(data: &[u8], vals: &[i16], idle: i64, width: i64) -> Vec<(i64, Vec<u8>)> {
    if vals.is_empty() {
        return Vec::new();
    }
    let peak = vals
        .iter()
        .map(|&v| (v as i64 - idle).abs())
        .max()
        .unwrap();
    let mut starts = onset_starts(vals, idle, width, peak);
    if starts.len() < 2 {
        starts = Vec::new();
    }

    let quiet = |value: i16| (value as i64 - idle).abs() <= width;

    let mut bounds: Vec<(i64, i64)> = Vec::new();
    if !starts.is_empty() {
        for (i, &a) in starts.iter().enumerate() {
            let mut b = if i + 1 < starts.len() {
                starts[i + 1]
            } else {
                vals.len() as i64
            };
            while b > a && quiet(vals[(b - 1) as usize]) {
                b -= 1;
            }
            // Keep short attacks as bounds too: the fold pass below merges
            // them into the next body, so their transient is not lost.
            if b > a {
                bounds.push((a, b));
            }
        }
    } else {
        let mut start = 0i64;
        let mut quiet_start: Option<i64> = None;
        for (i, &value) in vals.iter().enumerate() {
            let i = i as i64;
            if quiet(value) {
                if quiet_start.is_none() {
                    quiet_start = Some(i);
                }
                continue;
            }
            if let Some(qs) = quiet_start {
                if i - qs >= PWM_ATTACK_SILENCE {
                    let mut a = start;
                    let mut b = qs;
                    while a < b && quiet(vals[a as usize]) {
                        a += 1;
                    }
                    while b > a && quiet(vals[(b - 1) as usize]) {
                        b -= 1;
                    }
                    if b - a >= PWM_ATTACK_MIN_FRAMES {
                        bounds.push((a, b));
                    }
                    start = i;
                }
            }
            quiet_start = None;
        }
        let mut a = start;
        let mut b = vals.len() as i64;
        while a < b && quiet(vals[a as usize]) {
            a += 1;
        }
        while b > a && quiet(vals[(b - 1) as usize]) {
            b -= 1;
        }
        if b - a >= PWM_ATTACK_MIN_FRAMES {
            bounds.push((a, b));
        }
    }
    let mut out: Vec<(i64, Vec<u8>)> = Vec::new();
    // Fold short fragments into the following body so their attack is kept
    // without spending a sample slot on a few milliseconds of audio.
    let mut merged: Vec<(i64, i64)> = Vec::new();
    let mut i = 0;
    while i < bounds.len() {
        let (a, b) = bounds[i];
        if b - a < PWM_ATTACK_MIN_FRAMES && i + 1 < bounds.len() {
            bounds[i + 1] = (a, bounds[i + 1].1);
            i += 1;
            continue;
        }
        merged.push((a, b));
        i += 1;
    }
    if merged
        .last()
        .map_or(false, |&(la, lb)| lb - la < PWM_ATTACK_MIN_FRAMES)
    {
        if merged.len() > 1 {
            let last = merged.pop().unwrap();
            let index = merged.len() - 1;
            merged[index].1 = last.1;
        } else {
            merged.pop();
        }
    }
    for (mut a, b) in merged {
        let body_end = ((b * 2) as usize).min(data.len());
        let body_begin = ((a * 2) as usize).min(data.len());
        let body = &data[body_begin..body_end];
        if !audible_pcm(body, idle, width) {
            continue;
        }
        while b - a > PWM_ATTACK_MAX_FRAMES {
            let mut cut = a + PWM_ATTACK_MAX_FRAMES;
            // A tiny tail would become its own sub-minimum sample; split the
            // body in half instead so both pieces are useful.
            if b - cut < PWM_ATTACK_MIN_FRAMES {
                cut = a + (b - a) / 2;
            }
            let begin = ((a * 2) as usize).min(data.len());
            let end = ((cut * 2) as usize).min(data.len());
            out.push((a, data[begin..end].to_vec()));
            a = cut;
        }
        let begin = ((a * 2) as usize).min(data.len());
        out.push((a, data[begin..body_end].to_vec()));
    }
    out
}

/// Render the VGM's PWM streams into one PCM body per side.
pub fn collect_pwm(vgm: &VgmFile, lossy: bool) -> (Vec<PwmSample>, Vec<String>) {
    let mut warnings: Vec<String> = Vec::new();
    let plays: Vec<&DacStreamPlay> = vgm
        .dac_stream_plays
        .iter()
        .filter(|p| p.chip_type as i64 == STREAM_PWM as i64)
        .collect();
    let writes: Vec<&ChipWrite> = vgm.writes.iter().filter(|w| w.chip == "pwm").collect();
    let mut ids: BTreeSet<u8> = plays.iter().map(|p| p.chip_id).collect();
    ids.extend(writes.iter().map(|w| w.chip_id));
    if ids.is_empty() {
        return (Vec::new(), warnings);
    }
    if vgm.pwm_bank.is_empty() && vgm.pwm_blocks.is_empty() {
        warnings.push(
            "PWM clock present but no type-0x03/0x43 PCM data block; the stream is silent"
                .to_string(),
        );
        return (Vec::new(), warnings);
    }

    // Cycle/control changes over time.  Only register 1 affects the audio.
    let mut cycle_pts: Vec<(i64, i64)> = vec![(0, 0)];
    let mut direct: IndexMap<u8, Vec<(i64, i64, i64)>> = IndexMap::new();
    for w in &writes {
        let reg = w.reg as i64;
        if reg == 1 {
            cycle_pts.push((w.sample, (w.val & 0xFFF) as i64));
        } else if reg == 2 || reg == 3 || reg == 4 {
            direct
                .entry(w.chip_id)
                .or_default()
                .push((w.sample, reg, (w.val & 0xFFF) as i64));
        }
    }
    cycle_pts.sort();
    let cycle_times: Vec<i64> = cycle_pts.iter().map(|&(t, _)| t).collect();
    let cycle_vals: Vec<i64> = cycle_pts.iter().map(|&(_, v)| v).collect();

    let cycle_at = |t: i64| -> i64 {
        let idx = cycle_times.partition_point(|&x| x <= t) as i64 - 1;
        if idx >= 0 { cycle_vals[idx as usize] } else { 0 }
    };

    let mut total = vgm.total_samples;
    if let Some(m) = plays.iter().map(|p| p.sample).max() {
        total = total.max(m);
    }
    let frames = 1.max(py_int(py_round(
        total as f64 / VGM_RATE as f64 * PWM_RATE as f64,
    )));
    let loop_frame = vgm.loop_sample.map(|ls| {
        0.max(py_int(py_round(
            ls as f64 / VGM_RATE as f64 * PWM_RATE as f64,
        )))
    });

    let mut samples: IndexMap<(u8, i32), PwmSample> = IndexMap::new();
    for &cid in &ids {
        for side in [PWM_SIDE_LEFT, PWM_SIDE_RIGHT] {
            samples.insert(
                (cid, side),
                PwmSample {
                    chip_id: cid,
                    side,
                    data: vec![0u8; frames as usize * 2],
                    rate: PWM_RATE,
                    loop_start: loop_frame,
                    loop_end: if loop_frame.is_some() {
                        Some(frames)
                    } else {
                        None
                    },
                    ..Default::default()
                },
            );
        }
    }

    // Group plays per chip and side; a mono stream (port 4 or a single stream)
    // feeds both sides.
    let mut by_channel: IndexMap<(u8, i32), Vec<&DacStreamPlay>> = IndexMap::new();
    let mut mono: IndexMap<u8, Vec<&DacStreamPlay>> = IndexMap::new();
    for play in &plays {
        let side = if play.reg as i64 == 2 {
            PWM_SIDE_LEFT
        } else if play.reg as i64 == 3 {
            PWM_SIDE_RIGHT
        } else {
            -1
        };
        if side < 0 {
            mono.entry(play.chip_id).or_default().push(play);
        } else {
            by_channel.entry((play.chip_id, side)).or_default().push(play);
        }
    }
    for (cid, sides) in mono {
        for side in [PWM_SIDE_LEFT, PWM_SIDE_RIGHT] {
            by_channel
                .entry((cid, side))
                .or_default()
                .extend(sides.iter().copied());
        }
    }

    for ((cid, side), mut channel_plays) in by_channel {
        channel_plays.sort_by_key(|p| (p.sample, p.stream as i64));
        let sample = match samples.get_mut(&(cid, side)) {
            Some(sample) => sample,
            None => continue,
        };
        let mut dst = sample.data.clone();
        for (ci, play) in channel_plays.iter().enumerate() {
            let t0 = play.sample;
            let t1 = if ci + 1 < channel_plays.len() {
                channel_plays[ci + 1].sample
            } else {
                total
            };
            if t1 <= t0 {
                continue;
            }
            let (data, start) = stream_positions(play, &vgm.pwm_bank, &vgm.pwm_blocks);
            let stride = 2 * (play.step as i64).max(1);
            let rate = if play.rate == 0 {
                PWM_RATE as i64
            } else {
                play.rate as i64
            };
            if rate <= 0 {
                continue;
            }
            let items = if data.len() as i64 > start {
                (data.len() as i64 - start) / stride
            } else {
                0
            };
            if items <= 0 {
                continue;
            }
            let frame0 = 0.max(py_int(py_round(
                t0 as f64 / VGM_RATE as f64 * PWM_RATE as f64,
            )));
            let frame1 = frames.min(py_int(py_round(
                t1 as f64 / VGM_RATE as f64 * PWM_RATE as f64,
            )));
            let mut phase = 0.0f64;
            for fr in frame0..frame1 {
                let mut idx = phase as i64;
                if idx >= items {
                    if play.loop_ && items > 0 {
                        idx %= items;
                    } else {
                        break;
                    }
                }
                let off = (start + idx * stride) as usize;
                let value = (data[off] as i64) | (((data[off + 1] & 0x0F) as i64) << 8);
                let scaled = scale_value(
                    value,
                    cycle_at(((fr * VGM_RATE as i64) as f64 / PWM_RATE as f64) as i64),
                );
                let at = fr as usize * 2;
                dst[at..at + 2].copy_from_slice(&(scaled as i16).to_le_bytes());
                phase += rate as f64 / PWM_RATE as f64;
            }
        }
        sample.data = dst;
    }

    // Direct level writes are not part of the usual log shape; apply them at
    // their exact sample and report when they are the only source, since the
    // stream normally overwrites the register at audio rate.
    for (cid, entries) in &direct {
        for &(t, reg, value) in entries {
            let sides: &[i32] = if reg == 4 {
                &[PWM_SIDE_LEFT, PWM_SIDE_RIGHT]
            } else if reg == 2 {
                &[PWM_SIDE_LEFT]
            } else {
                &[PWM_SIDE_RIGHT]
            };
            let fr = 0.max((frames - 1).min(py_int(py_round(
                t as f64 / VGM_RATE as f64 * PWM_RATE as f64,
            ))));
            for &side in sides {
                let sample = match samples.get_mut(&(*cid, side)) {
                    Some(sample) => sample,
                    None => continue,
                };
                let scaled = scale_value(value, cycle_at(t));
                let mut dst = sample.data.clone();
                let at = fr as usize * 2;
                dst[at..at + 2].copy_from_slice(&(scaled as i16).to_le_bytes());
                sample.data = dst;
            }
        }
    }

    let occurrences = |use_attack: bool| -> Vec<(u8, i32, i64, Vec<u8>)> {
        let mut out: Vec<(u8, i32, i64, Vec<u8>)> = Vec::new();
        let mut keys: Vec<(u8, i32)> = samples.keys().copied().collect();
        keys.sort();
        for key in keys {
            let sample = &samples[&key];
            let full = &sample.data;
            let frames = full.len() / 2;
            if frames == 0 {
                continue;
            }
            let (vals, idle, width, _peak) = pcm_stats(full);
            if use_attack {
                for (start, body) in attack_bodies(full, &vals, idle, width) {
                    out.push((sample.chip_id, sample.side, start, body));
                }
            } else {
                let mut start = 0;
                while start < frames {
                    let end = frames.min(start + PWM_CHUNK_FRAMES as usize);
                    let body = full[start * 2..end * 2].to_vec();
                    if audible_pcm(&body, idle, width) {
                        out.push((sample.chip_id, sample.side, start as i64, body));
                    }
                    start += PWM_CHUNK_FRAMES as usize;
                }
            }
        }
        out
    };

    let attack_occ = occurrences(true);
    let chunk_occ = occurrences(false);
    let attack_unique: HashSet<(u8, &[u8])> = attack_occ
        .iter()
        .map(|(cid, _side, _s, body)| (*cid, body.as_slice()))
        .collect();
    // Prefer attack segmentation whenever it finds real splits.  Fixed chunks
    // are the fallback for a steady/continuous stream or a body budget blow-up.
    let use_attack = attack_occ.len() > 1 && attack_unique.len() as i64 <= PWM_ATTACK_MAX_SAMPLES;
    let chosen = if use_attack { attack_occ } else { chunk_occ };

    let mut audible: Vec<PwmSample> = Vec::new();
    let mut seen: HashMap<(u8, Vec<u8>), usize> = HashMap::new();
    for (cid, side, start, body) in chosen {
        let hit = PwmHit {
            time: py_int(py_round(
                start as f64 / PWM_RATE as f64 * VGM_RATE as f64,
            )),
            side,
        };
        let key = (cid, body.clone());
        if let Some(&found) = seen.get(&key) {
            audible[found].hits.push(hit);
            continue;
        }
        let chunk = PwmSample {
            chip_id: cid,
            side,
            data: body,
            rate: PWM_RATE,
            hits: vec![hit],
            chunk: audible.len() as i32,
            first_chunk: start == 0,
            ..Default::default()
        };
        seen.insert(key, audible.len());
        audible.push(chunk);
    }
    if audible.is_empty() {
        warnings.push("PWM streams decoded to silence".to_string());
    }
    if lossy && audible.len() > 32 {
        let bodies: Vec<Vec<u8>> = audible.iter().map(|s| s.data.clone()).collect();
        let rates: Vec<i32> = audible.iter().map(|s| s.rate).collect();
        let uses: IndexMap<i32, i64> = audible
            .iter()
            .enumerate()
            .map(|(i, s)| (i as i32, s.hits.len() as i64))
            .collect();
        let (keep, remap) = cluster_fragments16(
            &bodies,
            &rates,
            &uses,
            DEFAULT_TARGET,
            DEFAULT_HOT,
        );
        let mut compact: Vec<PwmSample> = keep.iter().map(|&i| audible[i as usize].clone()).collect();
        for (old, smp) in audible.iter().enumerate() {
            let new = remap[old];
            if new < 0 || keep[new as usize] == old as i32 {
                continue;
            }
            compact[new as usize].hits.extend(smp.hits.iter().cloned());
        }
        if compact.len() < audible.len() {
            warnings.push(format!(
                "PWM: lossy sample clustering {} -> {} bodies",
                audible.len(),
                compact.len()
            ));
        }
        audible = compact;
    }
    let mut seen_warn: HashSet<String> = HashSet::new();
    warnings.retain(|w| seen_warn.insert(w.clone()));
    (audible, warnings)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(vals: &[i16]) -> Vec<u8> {
        vals.iter().flat_map(|v| v.to_le_bytes()).collect()
    }

    #[test]
    fn signed12_masks_and_sign_extends() {
        assert_eq!(signed12(0), 0);
        assert_eq!(signed12(0x7FF), 2047);
        assert_eq!(signed12(0x800), -2048);
        assert_eq!(signed12(0xFFF), -1);
        assert_eq!(signed12(0x1000), 0);
        assert_eq!(signed12(0x1FFF), -1);
    }

    #[test]
    fn cycle_scale_matches_libvgm() {
        // cycle 0 -> (0 - 1) & 0xFFF = 4095, offset 2048.
        assert_eq!(cycle_scale(0), (2048, 0x7FFF00 / 2048));
        assert_eq!(cycle_scale(1), (1, 0x7FFF00));
        assert_eq!(cycle_scale(2), (1, 0x7FFF00));
        assert_eq!(cycle_scale(3), (2, 0x7FFF00 / 2));
        // Zero input is always silence; out-of-range values clamp.
        assert_eq!(scale_value(0, 1), 0);
        assert_eq!(scale_value(0x7FF, 0), -16);
        assert_eq!(scale_value(0x7FF, 1), 32767);
        assert_eq!(scale_value(0x800, 1), -32768);
    }

    #[test]
    fn pcm_stats_finds_idle_and_width() {
        let mut vals = vec![0i16; 20];
        vals.extend([1000i16; 5]);
        vals.extend([-1000i16; 5]);
        let (parsed, idle, width, peak) = pcm_stats(&words(&vals));
        assert_eq!(parsed, vals);
        assert_eq!(idle, 0);
        assert_eq!(width, 64);
        assert_eq!(peak, 1000);

        // No dominant value: the idle level falls back to zero.
        let distinct: Vec<i16> = (1..=200).collect();
        let (_, idle, _, _) = pcm_stats(&words(&distinct));
        assert_eq!(idle, 0);
        assert_eq!(pcm_stats(&[]), (Vec::new(), 0, 64, 0));
    }

    #[test]
    fn audible_and_onset_detection() {
        assert!(audible_pcm(&words(&[0, 0, 100]), 0, 64));
        assert!(!audible_pcm(&words(&[0, 50, 64]), 0, 64));

        let mut vals = vec![0i16; 100];
        vals.extend([1000i16; 100]);
        vals.extend([0i16; 100]);
        vals.extend([1000i16; 100]);
        let starts = onset_starts(&vals, 0, 64, 1000);
        assert_eq!(starts, vec![100, 300]);
    }

    #[test]
    fn attack_bodies_rest_split_keeps_audible_body() {
        let mut vals = vec![0i16; 300];
        vals.extend([1000i16; 2000]);
        vals.extend([0i16; 300]);
        let data = words(&vals);
        let bodies = attack_bodies(&data, &vals, 0, 64);
        assert_eq!(bodies.len(), 1);
        assert_eq!(bodies[0].0, 300);
        assert_eq!(bodies[0].1, words(&vals[300..2300]));
    }
}
