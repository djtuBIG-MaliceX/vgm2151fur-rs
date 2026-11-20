//! SN76489 sample playback -> Generic PCM DAC bodies.
//!
//! Literal port of `vgm2151fur/psg_dac.py` from vgm2151fur 0.11.0.
//!
//! Some Mega Drive drivers use a PSG tone channel as a 4-bit streaming DAC:
//! the tone period stays 0 and the driver rewrites the 4-bit attenuation at
//! audio rate. Furnace's SMS platform has no sample voice, so the stream is
//! rendered to 4-bit linear PCM (the 2 dB curve baked into the nibbles) and
//! played through a Generic PCM DAC system, like the YM2612 DAC maps.
//!
//! The stream is split into short chunks so repeated hits share one body and a
//! long track does not become one enormous sample. Chunk boundaries are also
//! placed at the VGM loop point, which keeps the pattern loop on a body start.

use std::collections::{BTreeMap, HashMap, HashSet};

use indexmap::IndexMap;

use crate::parity::{py_int, py_round};
use crate::vgm::{ChipWrite, VgmFile};

/// The source rewrites at a median of ~8 VGM samples (about 5.5 kHz). A
/// 22.05 kHz body keeps the attack edges with less than 0.1 ms error and is
/// half the size of a 44.1 kHz body.
pub const PSG_DAC_RATE: i32 = 22_050;
/// A run this long at the exact idle attenuation starts a new attack sample.
/// At 22.05 kHz this is about 11.6 ms; shorter gaps are waveform motion or
/// noisy fragments that belong in the previous sample's tail.
pub const PSG_DAC_REST_FRAMES: usize = 256;
/// Do not spend a body on a fragment shorter than 128 packed bytes (256
/// frames). At 4-bit amplitude resolution these micro-fragments are noise, not
/// useful independent attack samples, so they stay in the tail above.
pub const PSG_DAC_MIN_BODY_FRAMES: usize = 256;
/// Similarity clustering for reused attacks. Compare a substantial opening
/// window (0.19 s) with a small alignment tolerance; keep the longest body.
pub const PSG_DAC_SIMILAR_FRAMES: usize = 4096;
pub const PSG_DAC_SIMILAR_SHIFT: usize = 8;
pub const PSG_DAC_SIMILAR_STEP: usize = 4;
pub const PSG_DAC_SIMILAR_MAD: f64 = 0.6;
/// Do not bank a loud hit onto a quiet one merely because their low-level
/// waveforms are close. At 4-bit resolution a one-code difference is audible,
/// and the first Maximum Power attack was being mapped to a nearly silent body.
pub const PSG_DAC_SIMILAR_PEAK: i32 = 1;
/// Attacks inside a continuous stream. A short-window envelope that rises out
/// of a quiet lookback is a new hit even with no silent rest between.
pub const PSG_DAC_ATTACK_WINDOW: usize = 64;
pub const PSG_DAC_ATTACK_LOOKBACK: usize = 256;
pub const PSG_DAC_ATTACK_HIGH: f64 = 1.4;
pub const PSG_DAC_ATTACK_LOW: f64 = 0.5;
/// A leading fragment at this amplitude or below is a fade-in, not a first
/// note of its own. Fold it into the next body so the first note reaches the
/// real attack instead of playing a nearly silent sample.
pub const PSG_DAC_LEAD_PEAK: i32 = 1;
pub const PSG_DAC_MIN_WRITES: usize = 64;
pub const PSG_DAC_MIN_ZERO_PERIOD: f64 = 0.90;
pub const PSG_DAC_MAX_GAP: i64 = 256;

#[derive(Clone, Debug, PartialEq)]
pub struct PsgDacSample {
    pub data: Vec<u8>,
    pub frames: i64,
    pub rate: i32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct PsgDacPlay {
    pub sample_time: i64,
    pub body: usize,
    pub chip_id: i32,
    pub voice: i32,
    /// Frames this event actually owns in the source. A longer bank body
    /// reuses a shorter event as a prefix, so playback must be cut here when
    /// no following note would do it.
    pub frames: i64,
}

impl Default for PsgDacPlay {
    fn default() -> Self {
        PsgDacPlay {
            sample_time: 0,
            body: 0,
            chip_id: 0,
            voice: 0,
            frames: 0,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct PsgDacCollect {
    pub samples: Vec<PsgDacSample>,
    pub plays: Vec<PsgDacPlay>,
    pub voices: Vec<(i32, i32)>,
    pub rate: i32,
    pub stats: IndexMap<String, i64>,
    pub warnings: Vec<String>,
}

impl Default for PsgDacCollect {
    fn default() -> Self {
        PsgDacCollect {
            samples: Vec::new(),
            plays: Vec::new(),
            voices: Vec::new(),
            rate: PSG_DAC_RATE,
            stats: IndexMap::new(),
            warnings: Vec::new(),
        }
    }
}

/// SN76489 2 dB/step curve, centred on the stream's idle attenuation.
fn _attenuation_amplitude(att: i32, base: i32) -> f64 {
    2.0f64.powf(-((att & 0x0F) as f64) / 3.0) - 2.0f64.powf(-((base & 0x0F) as f64) / 3.0)
}

/// Hold each attenuation write until the next one at `rate` frames/s.
///
/// Furnace's 4-bit depth decodes a nibble linearly to signed 16-bit, so the
/// nonlinear 2 dB attenuation curve is baked into the nibble value: 8 is the
/// idle/zero level and 0/15 are the negative/positive peaks.
fn _render(writes: &[(i64, i32)], total_samples: i64, base: i32, rate: i32) -> Vec<u8> {
    let frames = 1.max(py_int(
        ((total_samples * rate as i64) as f64 / 44100.0).ceil(),
    ));
    let mut out = vec![0u8; frames as usize];
    let peaks: Vec<f64> = (0..16)
        .map(|att| _attenuation_amplitude(att, base).abs())
        .collect();
    let max_peak = if peaks.is_empty() {
        1.0
    } else {
        peaks.iter().copied().fold(f64::NEG_INFINITY, f64::max)
    };
    let scale = 7.0 / max_peak;
    let mut idx = 0usize;
    let mut cur = base;
    for n in 0..frames {
        let t = n as f64 * 44100.0 / rate as f64;
        while idx < writes.len() && (writes[idx].0 as f64) <= t {
            cur = writes[idx].1;
            idx += 1;
        }
        let value = 8 + py_int(py_round(_attenuation_amplitude(cur, base) * scale)) as i32;
        out[n as usize] = value.clamp(0, 15) as u8;
    }
    out
}

/// Split at rests so every body starts on an attack transient.
///
/// Returns `(start, end, audible_end)`. `end` carries the body's decay/silent
/// tail to the next attack; `audible_end` is where the source event reached its
/// rest. A shorter event that reuses a longer bank body is cut at
/// `audible_end`, so the longer body's unrelated tail cannot play into the
/// rest.
fn _segments(frames: &[u8], loop_frame: i64) -> Vec<(usize, usize, usize)> {
    let first = frames
        .iter()
        .position(|&v| v != 8)
        .unwrap_or(frames.len());
    if first >= frames.len() {
        return Vec::new();
    }
    let amplitude: Vec<i64> = frames.iter().map(|&v| (v as i64 - 8).abs()).collect();
    let mut prefix: Vec<i64> = Vec::with_capacity(amplitude.len() + 1);
    prefix.push(0);
    for &value in &amplitude {
        prefix.push(prefix[prefix.len() - 1] + value);
    }

    let envelope = |at: usize| -> f64 {
        let end = (at + PSG_DAC_ATTACK_WINDOW).min(amplitude.len());
        if end <= at {
            return 0.0;
        }
        (prefix[end] - prefix[at]) as f64 / (end - at) as f64
    };

    let mut pieces: Vec<(usize, usize, usize)> = Vec::new();
    let mut start = first;
    let mut quiet_start = 0usize;
    let mut quiet = 0usize;
    for i in first..frames.len() {
        if frames[i] == 8 {
            if quiet == 0 {
                quiet_start = i;
            }
            quiet += 1;
            continue;
        }
        if quiet >= PSG_DAC_REST_FRAMES {
            // The previous body keeps the rest/decay tail all the way to this
            // attack. Its audible part ended where the idle run began.
            if i > start {
                pieces.push((start, i, quiet_start));
            }
            start = i;
        } else if i - start >= PSG_DAC_MIN_BODY_FRAMES
            && i % 16 == 0
            && envelope(i) >= PSG_DAC_ATTACK_HIGH
        {
            let lo = start.max(i.saturating_sub(PSG_DAC_ATTACK_LOOKBACK));
            let mut quiet_before = PSG_DAC_ATTACK_HIGH;
            let mut j = lo;
            while j < i {
                let value = envelope(j);
                if value < quiet_before {
                    quiet_before = value;
                }
                j += 32;
            }
            if quiet_before <= PSG_DAC_ATTACK_LOW {
                pieces.push((start, i, i));
                start = i;
            }
        }
        quiet = 0;
    }
    if start < frames.len() {
        if quiet >= PSG_DAC_REST_FRAMES {
            pieces.push((start, frames.len(), quiet_start));
        } else {
            pieces.push((start, frames.len(), frames.len()));
        }
    }
    // Fold micro-attacks into the previous body's tail. A chain of short
    // fragments extends that body until a real body is reached.
    let mut merged: Vec<(usize, usize, usize)> = Vec::new();
    for (a, b, audible) in pieces {
        if audible - a < PSG_DAC_MIN_BODY_FRAMES && !merged.is_empty() {
            let last = merged.len() - 1;
            let prev_a = merged[last].0;
            // The fragment stays inside the previous body's audible tail; it
            // is not a separate attack, but it must not be muted away either.
            merged[last] = (prev_a, b, audible);
        } else {
            merged.push((a, b, audible));
        }
    }
    if merged.len() > 1 && merged[0].2 - merged[0].0 < PSG_DAC_MIN_BODY_FRAMES {
        merged[1] = (merged[0].0, merged[1].1, merged[1].2);
        merged.remove(0);
    }
    let mut pieces = merged;
    while pieces.len() > 1 {
        let (a, b, _audible) = pieces[0];
        let mut peak = 0i32;
        for &v in &frames[a..b] {
            peak = peak.max((v as i32 - 8).abs());
        }
        if peak > PSG_DAC_LEAD_PEAK {
            break;
        }
        pieces[1] = (a, pieces[1].1, pieces[1].2);
        pieces.remove(0);
    }
    if (first as i64) < loop_frame && loop_frame < frames.len() as i64 {
        let mut split: Vec<(usize, usize, usize)> = Vec::new();
        for (a, b, audible) in pieces {
            if (a as i64) < loop_frame && loop_frame < b as i64 {
                split.push((
                    a,
                    loop_frame as usize,
                    (audible as i64).min(loop_frame) as usize,
                ));
                split.push((
                    loop_frame as usize,
                    b,
                    (audible as i64).max(loop_frame) as usize,
                ));
            } else {
                split.push((a, b, audible));
            }
        }
        pieces = split;
    }
    pieces.into_iter().filter(|&(a, b, _)| b > a).collect()
}

/// True when everything past `start` is the idle nibble.
fn _tail_is_quiet(data: &[u8], start: i64) -> bool {
    let start = start.max(0) as usize;
    if start >= data.len() {
        return true;
    }
    data[start..].iter().all(|&v| v == 8)
}

/// Shortest source event that uses each body.
fn _min_audible(plays: &[PsgDacPlay]) -> HashMap<usize, i64> {
    let mut out: HashMap<usize, i64> = HashMap::new();
    for play in plays {
        match out.get(&play.body) {
            None => {
                out.insert(play.body, play.frames);
            }
            Some(&old) if play.frames < old => {
                out.insert(play.body, play.frames);
            }
            _ => {}
        }
    }
    out
}

/// Keep the largest body when a shorter attack is its prefix.
///
/// The next note cuts a long body at the source's attack, so a shorter hit can
/// safely reuse a longer variant. This is the "largest variant wins" rule the
/// YM2612 attack splitter uses; it also makes sure a quiet decay tail is
/// present when no following note arrives.
fn _share_prefix_samples(
    samples: &mut Vec<PsgDacSample>,
    plays: &mut [PsgDacPlay],
    stats: &mut IndexMap<String, i64>,
) {
    let mut by_head: IndexMap<(i32, Vec<u8>), Vec<usize>> = IndexMap::new();
    for (i, smp) in samples.iter().enumerate() {
        if !smp.data.is_empty() {
            by_head
                .entry((smp.rate, smp.data[..2.min(smp.data.len())].to_vec()))
                .or_default()
                .push(i);
        }
    }
    let mut remap: Vec<usize> = (0..samples.len()).collect();
    let audible = _min_audible(plays);
    let mut reused = 0i64;
    for indexes in by_head.values_mut() {
        if indexes.len() < 2 {
            continue;
        }
        indexes.sort_by(|&x, &y| samples[y].frames.cmp(&samples[x].frames));
        for pos in 0..indexes.len() {
            let i = indexes[pos];
            let need = audible
                .get(&i)
                .copied()
                .unwrap_or(samples[i].frames);
            for &j in &indexes[..pos] {
                let longer = &samples[j].data;
                let shorter = &samples[i].data;
                if longer.starts_with(shorter) && _tail_is_quiet(longer, need) {
                    remap[i] = j;
                    reused += 1;
                    break;
                }
            }
        }
    }
    if reused == 0 {
        return;
    }
    for i in 0..remap.len() {
        while remap[i] != remap[remap[i]] {
            remap[i] = remap[remap[i]];
        }
    }
    for play in plays.iter_mut() {
        play.body = remap[play.body];
    }
    let used: HashSet<usize> = plays.iter().map(|play| play.body).collect();
    let mut compact: Vec<PsgDacSample> = Vec::new();
    let mut compact_map: HashMap<usize, usize> = HashMap::new();
    for (i, smp) in samples.iter().enumerate() {
        if !used.contains(&i) {
            continue;
        }
        compact_map.insert(i, compact.len());
        compact.push(smp.clone());
    }
    *samples = compact;
    for play in plays.iter_mut() {
        play.body = compact_map[&play.body];
    }
    stats.insert("psg_dac_prefix_reused".to_string(), reused);
}

/// Smallest mean code difference over small start-time shifts.
fn _best_similarity(a: &[u8], b: &[u8], limit: usize) -> f64 {
    let mut best = 99.0f64;
    for shift in -(PSG_DAC_SIMILAR_SHIFT as i64)..=(PSG_DAC_SIMILAR_SHIFT as i64) {
        let aa: &[u8] = if shift >= 0 {
            &a[(shift as usize).min(a.len())..]
        } else {
            a
        };
        let bb: &[u8] = if shift >= 0 {
            b
        } else {
            &b[((-shift) as usize).min(b.len())..]
        };
        let n = aa.len().min(bb.len()).min(limit);
        if n == 0 {
            continue;
        }
        let mut total = 0i64;
        let mut count = 0i64;
        let mut k = 0usize;
        while k < n {
            total += (aa[k] as i64 - bb[k] as i64).abs();
            count += 1;
            k += PSG_DAC_SIMILAR_STEP;
        }
        if count > 0 {
            let score = total as f64 / count as f64;
            if score < best {
                best = score;
                if best <= PSG_DAC_SIMILAR_MAD {
                    break;
                }
            }
        }
    }
    best
}

/// Bank similar attacks onto the longest body.
///
/// Exact prefixes already share a body. This pass catches the same hit with a
/// one- or two-frame phase difference, which is common in the 4-bit stream.
/// The longest variant wins because the next note cuts its tail at the
/// source's attack.
fn _share_similar_samples(
    samples: &mut Vec<PsgDacSample>,
    plays: &mut [PsgDacPlay],
    stats: &mut IndexMap<String, i64>,
) {
    if samples.len() < 2 {
        return;
    }
    let mut order: Vec<usize> = (0..samples.len()).collect();
    order.sort_by(|&x, &y| samples[y].frames.cmp(&samples[x].frames));
    let peaks: Vec<i32> = samples
        .iter()
        .map(|smp| {
            smp.data
                .iter()
                .map(|&v| (v as i32 - 8).abs())
                .fold(0, i32::max)
        })
        .collect();
    let audible = _min_audible(plays);
    let mut anchors: Vec<usize> = Vec::new();
    let mut remap: Vec<usize> = (0..samples.len()).collect();
    for i in order {
        let limit = (samples[i].frames as usize).min(PSG_DAC_SIMILAR_FRAMES);
        let need = audible
            .get(&i)
            .copied()
            .unwrap_or(samples[i].frames);
        let mut matched = false;
        for &anchor in &anchors {
            if (peaks[anchor] - peaks[i]).abs() > PSG_DAC_SIMILAR_PEAK {
                continue;
            }
            // Only borrow a longer body when its extra tail is pure silence.
            // An audible tail belongs to a different event; keeping this
            // shorter body is preferable to muting a wrong tail later.
            if !_tail_is_quiet(&samples[anchor].data, need) {
                continue;
            }
            if _best_similarity(&samples[i].data, &samples[anchor].data, limit)
                <= PSG_DAC_SIMILAR_MAD
            {
                remap[i] = anchor;
                matched = true;
                break;
            }
        }
        if !matched {
            anchors.push(i);
        }
    }
    if anchors.len() == samples.len() {
        return;
    }
    for i in 0..remap.len() {
        while remap[i] != remap[remap[i]] {
            remap[i] = remap[remap[i]];
        }
    }
    for play in plays.iter_mut() {
        play.body = remap[play.body];
    }
    let used: HashSet<usize> = plays.iter().map(|play| play.body).collect();
    let mut compact: Vec<PsgDacSample> = Vec::new();
    let mut compact_map: HashMap<usize, usize> = HashMap::new();
    for (i, smp) in samples.iter().enumerate() {
        if !used.contains(&i) {
            continue;
        }
        compact_map.insert(i, compact.len());
        compact.push(smp.clone());
    }
    *samples = compact;
    for play in plays.iter_mut() {
        play.body = compact_map[&play.body];
    }
    let reused = remap.iter().enumerate().filter(|&(i, &r)| i != r).count();
    stats.insert("psg_dac_similar_reused".to_string(), reused as i64);
}

/// Find SN76489 DAC voices and turn their streams into PCM bodies.
///
/// `rate` and `lossy` are keyword arguments in the reference. The reference
/// never consults `lossy`; the clustering passes above always run.
pub fn collect_psg_dac(vgm: &VgmFile, rate: i32, _lossy: bool) -> PsgDacCollect {
    let mut out = PsgDacCollect {
        rate,
        ..Default::default()
    };
    let mut by_chip: BTreeMap<u8, Vec<&ChipWrite>> = BTreeMap::new();
    for w in &vgm.writes {
        if w.chip == "sn76489" {
            by_chip.entry(w.chip_id).or_default().push(w);
        }
    }
    if by_chip.is_empty() {
        return out;
    }

    let total = vgm.total_samples;
    if total == 0 {
        return out;
    }
    let loop_frame = match vgm.loop_sample {
        Some(ls) if ls != 0 => py_int(py_round((ls * rate as i64) as f64 / 44100.0)),
        _ => 0,
    };
    let mut used_voices: Vec<(i32, i32)> = Vec::new();
    let mut body_index: IndexMap<(usize, Vec<u8>), usize> = IndexMap::new();

    for (&chip_id, writes) in &by_chip {
        let mut freq = [0i32; 3];
        let mut freq_high = [false; 3];
        let mut latch = 0usize;
        let mut latch_vol = false;
        let mut volume: HashMap<i32, Vec<(i64, i32, i32)>> = HashMap::new();
        for w in writes {
            let b = (w.val & 0xFF) as i32;
            let t = w.sample;
            if b & 0x80 != 0 {
                latch = ((b >> 5) & 3) as usize;
                latch_vol = b & 0x10 != 0;
                if latch_vol && latch < 3 {
                    let period = if freq_high[latch] { freq[latch] } else { 0 };
                    volume
                        .entry(latch as i32)
                        .or_default()
                        .push((t, b & 0x0F, period));
                }
            } else {
                if latch_vol || latch == 3 {
                    continue;
                }
                freq[latch] = (freq[latch] & 0x00F) | ((b & 0x3F) << 4);
                freq_high[latch] = true;
            }
        }

        for voice in 0..3i32 {
            let empty: Vec<(i64, i32, i32)> = Vec::new();
            let entries = volume.get(&voice).unwrap_or(&empty);
            if entries.len() < PSG_DAC_MIN_WRITES {
                continue;
            }
            let zero_period = entries.iter().filter(|entry| entry.2 == 0).count();
            if (zero_period as f64 / entries.len() as f64) < PSG_DAC_MIN_ZERO_PERIOD {
                continue;
            }
            let times: Vec<i64> = entries.iter().map(|entry| entry.0).collect();
            let mut gaps: Vec<i64> = times
                .windows(2)
                .filter(|pair| pair[1] > pair[0])
                .map(|pair| pair[1] - pair[0])
                .collect();
            gaps.sort();
            if gaps.is_empty() {
                continue;
            }
            if gaps[gaps.len() / 2] > PSG_DAC_MAX_GAP {
                continue;
            }
            let dac: Vec<(i64, i32)> = entries
                .iter()
                .filter(|entry| entry.2 == 0)
                .map(|entry| (entry.0, entry.1))
                .collect();
            if dac.len() < PSG_DAC_MIN_WRITES {
                continue;
            }
            let mut held: IndexMap<i32, i64> = IndexMap::new();
            for (i, &(t, att)) in dac.iter().enumerate() {
                let end = if i + 1 < dac.len() {
                    dac[i + 1].0
                } else {
                    total
                };
                *held.entry(att).or_insert(0) += (end - t).max(0);
            }
            let mut base_entry: Option<(i32, i64)> = None;
            for (&att, &count) in &held {
                if base_entry.is_none_or(|(_, best)| count > best) {
                    base_entry = Some((att, count));
                }
            }
            let base = base_entry.map(|(att, _)| att).unwrap_or(0);
            let frames = _render(&dac, total, base, rate);
            let pieces = _segments(&frames, loop_frame);
            if pieces.is_empty() {
                continue;
            }
            used_voices.push((chip_id as i32, voice));
            for (a, b, audible) in pieces {
                let values: Vec<u8> = frames[a..b].to_vec();
                if values.iter().all(|&v| v == 8) {
                    // A body must start on an audible attack.
                    continue;
                }
                let key = (values.len(), values.clone());
                let idx = match body_index.get(&key) {
                    Some(&idx) => idx,
                    None => {
                        let idx = out.samples.len();
                        body_index.insert(key, idx);
                        out.samples.push(PsgDacSample {
                            frames: values.len() as i64,
                            data: values,
                            rate,
                        });
                        idx
                    }
                };
                out.plays.push(PsgDacPlay {
                    sample_time: py_int(py_round(a as f64 * 44100.0 / rate as f64)),
                    body: idx,
                    chip_id: chip_id as i32,
                    voice,
                    frames: 1.max(audible as i64 - a as i64),
                });
                out.stats
                    .insert("psg_dac_bodies".to_string(), out.samples.len() as i64);
                out.stats
                    .insert("psg_dac_plays".to_string(), out.plays.len() as i64);
            }
        }
    }

    if out.samples.len() > 1 {
        _share_prefix_samples(&mut out.samples, &mut out.plays, &mut out.stats);
        _share_similar_samples(&mut out.samples, &mut out.plays, &mut out.stats);
    }

    out.voices = used_voices;
    if !out.voices.is_empty() {
        out.stats
            .insert("psg_dac_voices".to_string(), out.voices.len() as i64);
        out.stats.insert("psg_dac_rate".to_string(), rate as i64);
        out.stats
            .insert("psg_dac_chunks".to_string(), out.plays.len() as i64);
        out.stats
            .insert("psg_dac_bodies".to_string(), out.samples.len() as i64);
        let bytes: i64 = out
            .samples
            .iter()
            .map(|smp| (smp.frames + 1) / 2)
            .sum();
        out.stats.insert("psg_dac_bytes".to_string(), bytes);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(sample: i64, chip_id: u8, val: u32) -> ChipWrite {
        ChipWrite::new(sample, "sn76489", chip_id, 0, val)
    }

    #[test]
    fn attenuation_curve_is_the_two_db_steps() {
        assert_eq!(_attenuation_amplitude(0, 15), 0.96875);
        assert_eq!(_attenuation_amplitude(3, 15), 0.46875);
        assert_eq!(_attenuation_amplitude(15, 15), 0.0);
    }

    #[test]
    fn render_holds_each_write_until_the_next() {
        let out = _render(&[(0, 0), (10, 15)], 44100, 15, 44100);
        assert_eq!(out.len(), 44100);
        assert!(out[..10].iter().all(|&v| v == 15));
        assert!(out[10..].iter().all(|&v| v == 8));
    }

    #[test]
    fn segments_split_at_a_rest_run_and_the_loop_point() {
        let mut frames = vec![8u8; 900];
        for v in &mut frames[..300] {
            *v = 10;
        }
        for v in &mut frames[600..] {
            *v = 10;
        }
        assert_eq!(
            _segments(&frames, 0),
            vec![(0, 600, 300), (600, 900, 900)]
        );
        let all = vec![10u8; 900];
        assert_eq!(
            _segments(&all, 450),
            vec![(0, 450, 450), (450, 900, 900)]
        );
    }

    #[test]
    fn collect_turns_a_zero_period_stream_into_one_body() {
        let mut writes = vec![write(0, 0, 0x9F)];
        for k in 0..100i64 {
            let att = if k % 2 == 0 { 0x00 } else { 0x0F };
            writes.push(write(300 + 8 * k, 0, 0x90 | att));
        }
        let vgm = VgmFile {
            total_samples: 4000,
            writes,
            ..Default::default()
        };
        let out = collect_psg_dac(&vgm, PSG_DAC_RATE, false);
        assert_eq!(out.rate, PSG_DAC_RATE);
        assert_eq!(out.voices, vec![(0, 0)]);
        assert_eq!(out.samples.len(), 1);
        assert_eq!(out.samples[0].frames, 1850);
        assert_eq!(out.samples[0].data[0], 15);
        assert_eq!(out.samples[0].data[4], 8);
        assert_eq!(out.plays.len(), 1);
        assert_eq!(out.plays[0].sample_time, 300);
        assert_eq!(out.plays[0].body, 0);
        // The trailing 1454 idle frames are the decay tail; the source event
        // itself ends at frame 546.
        assert_eq!(out.plays[0].frames, 396);
        assert_eq!(out.stats["psg_dac_bodies"], 1);
        assert_eq!(out.stats["psg_dac_plays"], 1);
        assert_eq!(out.stats["psg_dac_voices"], 1);
        assert_eq!(out.stats["psg_dac_rate"], 22050);
        assert_eq!(out.stats["psg_dac_chunks"], 1);
        assert_eq!(out.stats["psg_dac_bytes"], 925);
        assert!(out.warnings.is_empty());
    }

    #[test]
    fn sharing_banks_prefixes_and_shifted_attacks() {
        // A shorter attack that is an exact prefix of a longer body reuses it.
        let mut samples = vec![
            PsgDacSample {
                data: vec![9, 9, 9, 9, 8, 8, 8, 8],
                frames: 8,
                rate: 22050,
            },
            PsgDacSample {
                data: vec![9, 9, 9, 9],
                frames: 4,
                rate: 22050,
            },
        ];
        let mut plays = vec![PsgDacPlay {
            sample_time: 0,
            body: 1,
            chip_id: 0,
            voice: 0,
            frames: 4,
        }];
        let mut stats = IndexMap::new();
        _share_prefix_samples(&mut samples, &mut plays, &mut stats);
        assert_eq!(samples.len(), 1);
        assert_eq!(plays[0].body, 0);
        assert_eq!(stats["psg_dac_prefix_reused"], 1);

        // One frame of phase difference still matches the same hit.
        let mut samples = vec![
            PsgDacSample {
                data: vec![15, 8, 8, 8, 8, 8, 8, 8],
                frames: 8,
                rate: 22050,
            },
            PsgDacSample {
                data: vec![8, 15, 8, 8, 8, 8],
                frames: 6,
                rate: 22050,
            },
        ];
        let mut plays = vec![PsgDacPlay {
            sample_time: 0,
            body: 1,
            chip_id: 0,
            voice: 0,
            frames: 6,
        }];
        let mut stats = IndexMap::new();
        _share_similar_samples(&mut samples, &mut plays, &mut stats);
        assert_eq!(samples.len(), 1);
        assert_eq!(plays[0].body, 0);
        assert_eq!(stats["psg_dac_similar_reused"], 1);
    }
}
