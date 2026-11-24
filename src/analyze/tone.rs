//! Tone-chip instrument binding and macro encoding (SCC, AY, PSG, NES, PCE,
//! GB, VSU, SAA, POKEY, Mikey, X1, FDS, WonderSwan, QSound, K005289).
//!
//! A tone note is bound to one `ToneIns`; pitch, wave, volume, noise, duty and
//! envelope motion inside the note become per-tick macros with their own step
//! rates. These are the post-collection passes: they see the final tick rate
//! and the module instrument budget.

use std::collections::HashSet;
use std::sync::Arc;

use indexmap::IndexMap;

use crate::ay8910::{FX_ENV_HI, FX_ENV_LO, FX_ENV_SET, FX_MODE, FX_NOISE};
use crate::fds::{
    FDS_DEFAULT_CLOCK, FDS_FREQ_DIV, FDS_MAX_FREQ,
};
use crate::fm::FMPatch;
use crate::gb::{GB_DEFAULT_CLOCK, GB_PITCH_DIVIDER};
use crate::huc6280::{huc6280_note, huc6280_noise_note, HUC6280_MAX_PERIOD};
use crate::k005289::{K005289_DEFAULT_CLOCK, K005289_DIVIDER};
use crate::mikey::{MIKEY_DIVIDER, MIKEY_MAX_PERIOD};
use crate::model::{
    kc_kf_units, kc_to_furnace_note, kf_to_e5, NoteEvent, ToneIns,
};
use crate::nes::{NES_CPU_DIVIDER, NES_DEFAULT_CLOCK, NES_FURNACE_CLOCK};
use crate::parity::{py_int, py_round};
use crate::pitch::{
    frequency_pitch_table, module_clock, pitch_table, FrequencyPitchTable,
    PeriodPitchTable, E5_CENTER,
};
use crate::pokey::{
    FX_AUDCTL as POKEY_FX_AUDCTL, POKEY_DEFAULT_CLOCK, POKEY_PITCH_MAX,
};
use crate::qsound::{QSOUND_FREQ_DIV, QSOUND_MAX_FREQ};
use crate::saa1099::{
    saa_module_clock, SAA_DEFAULT_CLOCK, SAA_DIVIDER, SAA_PITCH_MAX,
};
use crate::sn76489::{
    PSG_DEFAULT_CLOCK, PSG_DIVIDER, PSG_FURNACE_CLOCK, PSG_MAX_PERIOD,
};
use crate::vgm::{
    HUC6280_DEFAULT_CLOCK, MIKEY_DEFAULT_CLOCK, VGM_RATE, VSU_DEFAULT_CLOCK,
    WSWAN_DEFAULT_CLOCK, X1_010_DEFAULT_CLOCK,
};
use crate::vsu::VSU_PITCH_DIVIDER;
use crate::wonderswan::{WS_MAX_PERIOD, WS_PITCH_DIVIDER};
use crate::x1_010::{X1_FREQ_DIV, X1_MAX_FREQ};

/// The format stores a macro length in one byte, so 255 is the longest macro
/// that can be declared. 256 wraps to 0 and the reader then never consumes the
/// data block, reporting "invalid macro code" on the bytes after it.
pub const MACRO_MAX: usize = 255;

/// Instrument IDs are one byte, so 0xFF is the highest addressable instrument.
/// Furnace itself allows 256 entries, but a 256th would have no valid ID in a
/// pattern cell. The allocator targets 250 to keep a little headroom for the
/// passes that append copies; INS_HARD_LIMIT is the absolute ceiling.
pub const INS_TARGET: i32 = 250;
pub const INS_HARD_LIMIT: i32 = 0xFF;
/// A pattern stores the instrument as one byte, and 0xFF is the empty marker.
/// Keep the base OPN timbres below that with room for the sample/tone groups
/// the writer appends after the FM block.
pub const OPN_BASE_INS_CAP: usize = 220;

/// A space-safe existing OPN patch for a note past the base timbre cap.
pub fn _opn_reuse_key<K>(
    snap: &FMPatch,
    patches: &IndexMap<K, FMPatch>,
    patch_order: &[K],
) -> K
where
    K: Clone + Eq + std::hash::Hash,
{
    let alg = snap.alg & 7;
    for key in patch_order {
        if let Some(patch) = patches.get(key) {
            if (patch.alg & 7) == alg {
                return key.clone();
            }
        }
    }
    patch_order[0].clone()
}

/// Value in force at each tick from `start` (each write holds until the
/// next one).
pub fn _tick_values(
    pts: &[(i64, i32)],
    start: i64,
    ticks: i64,
    tick_samples: f64,
) -> Vec<i32> {
    let mut out: Vec<i32> = Vec::new();
    let mut i = 0usize;
    let mut cur = pts.first().map(|p| p.1).unwrap_or(0);
    for k in 0..ticks {
        let t = start as f64 + k as f64 * tick_samples;
        while i < pts.len() && (pts[i].0 as f64) <= t {
            cur = pts[i].1;
            i += 1;
        }
        out.push(cur);
    }
    out
}

/// Length of the shortest exact repeating cycle of a series (2 or more).
///
/// A series that only rarely changes must not pass as a one-step cycle, so
/// the repeat has to be exact.
pub fn _cycle(values: &[i32], limit: i32) -> Option<usize> {
    let n = values.len();
    let hi = (limit.max(0) as usize).min(n / 2);
    for p in 2..=hi {
        let mut ok = true;
        for i in p..n {
            if values[i] != values[i - p] {
                ok = false;
                break;
            }
        }
        if ok {
            return Some(p);
        }
    }
    None
}

fn has_variation(values: &[i32]) -> bool {
    let mut first: Option<i32> = None;
    for &v in values {
        match first {
            None => first = Some(v),
            Some(f) if v != f => return true,
            _ => {}
        }
    }
    false
}

/// Exact (loop start, period) for a periodic tail after a lead-in.
pub fn _tail_cycle(
    values: &[i32],
    limit: i32,
    window: i32,
    min_start: i32,
) -> Option<(usize, usize)> {
    let offset = values.len().saturating_sub(window.max(0) as usize);
    let vals = &values[offset..];
    let n = vals.len();
    if n < 2 {
        return None;
    }
    let lo = std::cmp::max(0, min_start - offset as i32) as usize;
    for start in lo..n - 1 {
        let tail = &vals[start..];
        let p = _cycle(tail, limit);
        if let Some(p) = p {
            if p <= tail.len() && has_variation(&tail[..p]) {
                return Some((offset + start, p));
            }
        }
    }
    None
}

/// Find a quantized LFO at the tail by walking its peaks backwards.
///
/// The last local maximum and minimum establish the top and bottom of the
/// LFO. Extrema are accepted backwards while they still match those peaks;
/// the first mismatch starts the range. Allow a few one-unit mismatches in
/// the period, since run lengths of a quantized LFO can vary by a tick.
pub fn _lfo_tail_cycle(
    values: &[i32],
    limit: i32,
    tol: i32,
    window: i32,
    min_start: i32,
) -> Option<(usize, usize)> {
    let offset = values.len().saturating_sub(window.max(0) as usize);
    let vals = &values[offset..];
    let n = vals.len();
    if n < 6 {
        return None;
    }
    let mut runs: Vec<(usize, i32)> = Vec::new();
    let mut i = 0usize;
    while i < n {
        let mut j = i + 1;
        while j < n && vals[j] == vals[i] {
            j += 1;
        }
        runs.push((i, vals[i]));
        i = j;
    }
    let mut extrema: Vec<(usize, i32, i32)> = Vec::new();
    for k in 1..runs.len().saturating_sub(1) {
        let prev = runs[k - 1].1;
        let cur = runs[k].1;
        let nxt = runs[k + 1].1;
        if cur > prev && cur > nxt {
            extrema.push((runs[k].0, cur, 1));
        } else if cur < prev && cur < nxt {
            extrema.push((runs[k].0, cur, -1));
        }
    }
    if extrema.len() < 3 {
        return None;
    }
    let mut top: Option<i32> = None;
    let mut bottom: Option<i32> = None;
    let mut left: Option<usize> = None;
    let mut max_count = 0i32;
    let mut min_count = 0i32;
    for &(idx, value, kind) in extrema.iter().rev() {
        if kind > 0 {
            match top {
                None => top = Some(value),
                Some(t) => {
                    if (value - t).abs() > tol {
                        break;
                    }
                }
            }
            max_count += 1;
        } else {
            match bottom {
                None => bottom = Some(value),
                Some(b) => {
                    if (value - b).abs() > tol {
                        break;
                    }
                }
            }
            min_count += 1;
        }
        left = Some(idx);
    }
    let (top, bottom) = match (top, bottom) {
        (Some(t), Some(b)) if t != b => (t, b),
        _ => return None,
    };
    if max_count < 2 && min_count < 2 {
        return None;
    }
    let mut left = left?;
    // Extend over the peak run itself, but not over unrelated lead-in values.
    while left > 0
        && ((vals[left - 1] - top).abs() <= tol
            || (vals[left - 1] - bottom).abs() <= tol)
    {
        left -= 1;
    }
    left = std::cmp::max(left, std::cmp::max(0, min_start - offset as i32) as usize);
    let region = &vals[left..];
    let rn = region.len();
    let mut best: Option<(f64, usize, usize)> = None;
    let hi = (limit.max(0) as usize).min(rn / 2);
    for p in 2..=hi {
        if rn < 2 * p {
            continue;
        }
        let shifts = p.min(rn - 2 * p + 1);
        for shift in 0..shifts {
            let block = &region[shift..shift + p];
            if !has_variation(block) {
                continue;
            }
            let dirs: Vec<i32> = block
                .windows(2)
                .filter(|w| w[1] != w[0])
                .map(|w| if w[1] > w[0] { 1 } else { -1 })
                .collect();
            if !dirs.windows(2).any(|w| w[0] != w[1]) {
                continue;
            }
            let mut errs: Vec<i32> = Vec::new();
            for i in shift + p..rn {
                errs.push((region[i] - region[i - p]).abs());
            }
            if errs.is_empty() {
                continue;
            }
            let bad = errs.iter().filter(|&&e| e > tol).count();
            if bad > std::cmp::max(1, errs.len() / 10) {
                continue;
            }
            let score =
                bad as f64 / errs.len() as f64 + p as f64 * 0.0005
                    + shift as f64 * 0.0001;
            if best.is_none() || score < best.unwrap().0 {
                best = Some((score, p, shift));
            }
        }
    }
    let (_, p, shift) = best?;
    Some((offset + left + shift, p))
}

/// Shorten per-tick repeats by raising the macro's step length.
///
/// Runs of similar length are rounded to one value per step. The timing
/// error is bounded to a few percent of the macro's total; the caller keeps
/// the leading hold in ticks, so this only affects the moving part.
pub fn _step_compress(
    series: &[i32],
    max_speed: i32,
    ratio: f64,
) -> (Vec<i32>, i32) {
    if series.len() < 4 {
        return (series.to_vec(), 1);
    }
    let mut runs: Vec<(i32, usize)> = Vec::new();
    let mut i = 0usize;
    while i < series.len() {
        let mut j = i + 1;
        while j < series.len() && series[j] == series[i] {
            j += 1;
        }
        runs.push((series[i], j - i));
        i = j;
    }
    if runs.len() == series.len() {
        return (series.to_vec(), 1);
    }
    let total = series.len();
    let allowed = std::cmp::max(1, py_int(total as f64 * ratio) as usize);
    let mut best: Option<(usize, i32, Vec<i32>)> = None;
    let hi = std::cmp::min(max_speed, (total / 2) as i32);
    for speed in 2..=hi.max(1) {
        let mut values: Vec<i32> = Vec::new();
        let mut error: i64 = 0;
        for &(value, length) in &runs {
            let count = std::cmp::max(
                1,
                py_round(length as f64 / speed as f64) as i64,
            );
            for _ in 0..count {
                values.push(value);
            }
            error += (length as i64 - count * speed as i64).abs();
        }
        if values.len() as f64 >= total as f64 * 0.75 {
            continue;
        }
        if values.len() > MACRO_MAX {
            continue;
        }
        if error > allowed as i64 {
            continue;
        }
        let take = match best {
            None => true,
            Some((blen, bspeed, _)) => {
                values.len() < blen
                    || (values.len() == blen && speed > bspeed)
            }
        };
        if take {
            best = Some((values.len(), speed, values));
        }
    }
    match best {
        Some((_, speed, values)) => (values, speed),
        None => (series.to_vec(), 1),
    }
}

/// (values, speed, loop) for one tone macro from a per-tick series.
pub fn _macro_series(
    series: &[i32],
    tail_loop: bool,
    initial_speed: i32,
    loop_min: i32,
    tail_min: i32,
) -> (Vec<i32>, i32, i32) {
    if series.is_empty() || !has_variation(series) {
        return (Vec::new(), 1, 255);
    }
    let mut speed = std::cmp::max(1, initial_speed);
    let loop_ = 255i32;
    let mut series = series.to_vec();
    if series.len() > MACRO_MAX {
        let p = _cycle(&series, 128);
        if let Some(p) = p {
            if loop_min == 0 {
                return (series[..p].to_vec(), 1, 0);
            }
        }
        if tail_loop && series.len() > tail_min.max(0) as usize {
            let mut cyc = _tail_cycle(&series, 128, 2048, loop_min);
            if cyc.is_none() {
                cyc = _lfo_tail_cycle(&series, 128, 1, 2048, loop_min);
            }
            if let Some((start, p)) = cyc {
                if start + p <= MACRO_MAX {
                    return (series[..start + p].to_vec(), 1, start as i32);
                }
            }
        }
        speed = std::cmp::min(
            255,
            ((series.len() + MACRO_MAX - 1) / MACRO_MAX) as i32,
        );
        series = series
            .iter()
            .step_by(speed.max(1) as usize)
            .copied()
            .collect();
    }
    let p = _cycle(&series, 128);
    if let Some(p) = p {
        if p < series.len() && loop_min == 0 {
            return (series[..p].to_vec(), speed, 0);
        }
    }
    if tail_loop && series.len() > tail_min.max(0) as usize {
        let mut cyc = _tail_cycle(&series, 128, 2048, loop_min);
        if cyc.is_none() {
            cyc = _lfo_tail_cycle(&series, 128, 1, 2048, loop_min);
        }
        if let Some((start, p)) = cyc {
            return (series[..start + p].to_vec(), speed, start as i32);
        }
    }
    while series.len() > 1 && series[series.len() - 1] == series[series.len() - 2] {
        series.pop();
    }
    (series, speed, loop_)
}

/// `_macro_series` plus a leading-hold delay.
pub fn _macro_series_delay(
    series: &[i32],
    tail_loop: bool,
    step_compress: bool,
) -> (Vec<i32>, i32, i32, i32) {
    if series.is_empty() {
        return (Vec::new(), 1, 255, 0);
    }
    let first = series[0];
    let mut run = 1usize;
    while run < series.len() && series[run] == first {
        run += 1;
    }
    if run <= 1 {
        let mut speed = 1i32;
        let mut body = series.to_vec();
        let use_tail = tail_loop && body.len() > 16;
        if step_compress && body.len() > 16 {
            let (b, s) = _step_compress(&body, 32, 0.03);
            body = b;
            speed = s;
        }
        let (values, out_speed, loop_) =
            _macro_series(&body, use_tail, speed, 0, 0);
        return (values, out_speed, loop_, 0);
    }
    let mut speed = 1i32;
    let mut body = series[run..].to_vec();
    let use_tail = tail_loop && body.len() > 16;
    if step_compress && body.len() > 16 {
        let (b, s) = _step_compress(&body, 32, 0.03);
        body = b;
        speed = s;
    }
    let (short, delay) = if run <= 256 {
        let mut short = vec![first];
        short.extend(body.iter().copied());
        (short, (run - 1) as i32)
    } else {
        let extra = run - 256;
        let extra_copies =
            (extra + speed.max(1) as usize - 1) / speed.max(1) as usize;
        let mut short = vec![first; 1 + extra_copies];
        short.extend(body.iter().copied());
        (short, 255)
    };
    let (values, out_speed, loop_) =
        _macro_series(&short, use_tail, speed, 1, 0);
    let speed = out_speed;
    // A loop that starts at the first value repeats the leading run on every
    // cycle; collapsing it would remove that repeat. A loop later in the
    // macro is unaffected by the initial hold, and the shortened value list
    // already has its loop index shifted correctly.
    if values.is_empty() || loop_ == 0 {
        let (values, speed, loop_) =
            _macro_series(series, tail_loop, 1, 0, 16);
        return (values, speed, loop_, 0);
    }
    (values, speed, loop_, delay)
}

/// True when the series steps by one level, mostly downward.
pub fn _unit_saw(values: &[i32]) -> bool {
    if values.len() < 3 {
        return false;
    }
    let nonzero: Vec<i32> = values
        .windows(2)
        .map(|w| w[1] - w[0])
        .filter(|&d| d != 0)
        .collect();
    if nonzero.is_empty() {
        return false;
    }
    let ones = nonzero.iter().filter(|d| d.abs() == 1).count();
    if ones * 4 < nonzero.len() * 3 {
        return false;
    }
    let down = nonzero.iter().filter(|&&d| d == -1).count();
    down >= nonzero.len() - down
}

/// 1 if the note starts by stepping down, 2 if the opening level holds.
pub fn _slide_open(series: &[i32]) -> i32 {
    if series.len() >= 2 && series[1] == series[0] {
        return 2;
    }
    1
}

/// Volume anchors after tick 0 for an FA slide.
pub fn _saw_anchors(
    series: &[i32],
    start: i64,
    tick_samples: f64,
) -> Vec<(i64, i32, i32)> {
    let mut anchors: Vec<(i64, i32, i32)> = Vec::new();
    let n = series.len();
    if n < 2 {
        return Vec::new();
    }
    let mut sliding = series[1] == series[0] - 1;
    let mut i = 1usize;
    while i < n {
        if sliding && series[i] == series[i - 1] - 1 {
            i += 1;
            continue;
        }
        let level = series[i];
        if series[i] == series[i - 1]
            || (i + 1 < n && series[i + 1] == level)
        {
            anchors.push((
                py_int(py_round(start as f64 + i as f64 * tick_samples)) as i64,
                level,
                0,
            ));
            i += 1;
            while i < n && series[i] == level {
                i += 1;
            }
            sliding = false;
            continue;
        }
        anchors.push((
            py_int(py_round(start as f64 + i as f64 * tick_samples)) as i64,
            level,
            1,
        ));
        sliding = true;
        i += 1;
    }
    anchors
}

/// A repeating one-level saw as `(tooth, speed 1, loop 0)`.
pub fn _fold_saw(series: &[i32]) -> Option<(Vec<i32>, i32, i32)> {
    if series.len() < 3 {
        return None;
    }
    let mut end = series.len();
    while end > 1 && series[end - 1] == series[end - 2] {
        end -= 1;
    }
    if series.len() - end > 1 {
        return None;
    }
    let body = &series[..end];
    if !_unit_saw(body) {
        return None;
    }
    let jumps: Vec<usize> =
        (1..body.len()).filter(|&i| body[i] > body[i - 1]).collect();
    let cycle: Vec<i32>;
    let phase;
    if jumps.len() >= 2 {
        let gaps: Vec<usize> = jumps.windows(2).map(|w| w[1] - w[0]).collect();
        let p = gaps[0];
        if !(2..=64).contains(&p) || gaps.iter().any(|&g| g != p) {
            return None;
        }
        phase = jumps[0];
        if phase + p > body.len() {
            return None;
        }
        cycle = body[phase..phase + p].to_vec();
        for i in 0..body.len() {
            let idx = (i as i64 - phase as i64).rem_euclid(p as i64) as usize;
            if body[i] != cycle[idx] {
                return None;
            }
        }
    } else if jumps.len() == 1 {
        let p = jumps[0];
        if !(2..=64).contains(&p) || p >= body.len() {
            return None;
        }
        phase = 0;
        cycle = body[..p].to_vec();
        for i in 0..body.len() {
            if body[i] != cycle[i % p] {
                return None;
            }
        }
    } else {
        return None;
    }
    for i in 0..cycle.len() {
        if cycle[i] != cycle[0] - i as i32 {
            return None;
        }
    }
    let clen = cycle.len() as i64;
    let start = (-(phase as i64)).rem_euclid(clen) as usize;
    let mut rotated: Vec<i32> = cycle[start..].to_vec();
    rotated.extend_from_slice(&cycle[..start]);
    if rotated[0] != body[0] {
        return None;
    }
    Some((rotated, 1, 0))
}

/// One note's PMS/AMS ramp: (pms values, ams values, tick deltas).
pub type PmsRamp = (Vec<i32>, Vec<i32>, Vec<i32>);

/// Record one note's PMS/AMS writes (drop the key-off reset to 0).
pub fn _flush_pms_ramp(
    acc: &mut IndexMap<i32, Vec<PmsRamp>>,
    ins: i32,
    ramp: &[(i64, i32, i32)],
    kon_sample: i64,
) {
    if ins < 0 || ramp.is_empty() {
        return;
    }
    let mut pms: Vec<i32> = Vec::new();
    let mut ams: Vec<i32> = Vec::new();
    let mut deltas: Vec<i32> = Vec::new();
    let mut prev_t = kon_sample;
    for &(t, p, a) in ramp {
        if !pms.is_empty() && p == *pms.last().unwrap() && a == *ams.last().unwrap()
        {
            continue;
        }
        if p == 0 && a == 0 && !pms.is_empty() {
            // The reset switches vibrato off. Keep it as the final step.
            // A macro holds its last value for the rest of the song.
            pms.push(0);
            ams.push(0);
            let last = deltas
                .last()
                .copied()
                .unwrap_or(std::cmp::max(1, t - prev_t) as i32);
            deltas.push(last);
            break;
        }
        pms.push(p);
        ams.push(a);
        deltas.push(std::cmp::max(1, t - prev_t) as i32);
        prev_t = t;
    }
    if pms.is_empty() {
        return;
    }
    // Key-on is always PMS 0; the first write is usually 1 a few frames later.
    if pms[0] != 0 {
        pms.insert(0, 0);
        ams.insert(0, 0);
        let d0 = deltas.first().copied().unwrap_or(735);
        deltas.insert(0, d0);
    }
    acc.entry(ins).or_default().push((pms, ams, deltas));
}

fn most_common_entry<K: Eq + std::hash::Hash>(
    map: &IndexMap<K, usize>,
) -> Option<(&K, usize)> {
    let mut best: Option<(&K, usize)> = None;
    for (k, &c) in map {
        match best {
            None => best = Some((k, c)),
            Some((_, bc)) if c > bc => best = Some((k, c)),
            _ => {}
        }
    }
    best
}

/// Driver-scripted vibrato: PMS/AMS step up as the note is held.
pub fn _pick_lfo_macros(
    ramps: &[PmsRamp],
    samples_per_tick: f64,
) -> (Vec<i32>, Vec<i32>, i32) {
    if ramps.is_empty() {
        return (Vec::new(), Vec::new(), 1);
    }
    // Prefer notes that actually complete a ramp (PMS reached >= 4).
    let long_ones: Vec<&PmsRamp> = ramps
        .iter()
        .filter(|r| r.0.iter().copied().max().unwrap_or(0) >= 4)
        .collect();
    let pool: Vec<&PmsRamp> = if !long_ones.is_empty() {
        long_ones
    } else {
        ramps.iter().collect()
    };
    let mut seqs: IndexMap<Vec<i32>, usize> = IndexMap::new();
    for r in &pool {
        *seqs.entry(r.0.clone()).or_insert(0) += 1;
    }
    let (best_p, n) = match most_common_entry(&seqs) {
        Some((k, c)) => (k.clone(), c),
        None => (Vec::new(), 0),
    };
    if n < 1 || best_p.iter().copied().max().unwrap_or(0) < 2 {
        return (Vec::new(), Vec::new(), 1);
    }
    let mut ams_seqs: IndexMap<Vec<i32>, usize> = IndexMap::new();
    for r in &pool {
        if r.0 == best_p {
            let take = r.1.len().min(best_p.len());
            *ams_seqs.entry(r.1[..take].to_vec()).or_insert(0) += 1;
        }
    }
    let mut best_a = match most_common_entry(&ams_seqs) {
        Some((k, _)) => k.clone(),
        None => vec![0; best_p.len()],
    };
    if best_a.len() < best_p.len() {
        let last = best_a.last().copied().unwrap_or(0);
        while best_a.len() < best_p.len() {
            best_a.push(last);
        }
    }
    let mut best_p = best_p;
    // Collapse a trailing hold (7,7,7,…) to one step; macros hold the last value.
    while best_p.len() >= 2 && best_p[best_p.len() - 1] == best_p[best_p.len() - 2]
    {
        best_p.pop();
        best_a.pop();
    }
    let mut step_samples: Vec<i32> = Vec::new();
    for r in &pool {
        if r.0 != best_p {
            continue; // only the ramps the macro actually models
        }
        // d[0] is the note-on -> first-write lead-in, not a step.
        let take = best_p.len().saturating_sub(1);
        for d in r.2.iter().skip(1).take(take) {
            step_samples.push(*d);
        }
    }
    if step_samples.is_empty() {
        step_samples = vec![py_int(samples_per_tick) as i32];
    }
    step_samples.sort();
    let med = step_samples[step_samples.len() / 2];
    let speed = std::cmp::max(
        1,
        std::cmp::min(255, py_int(py_round(med as f64 / samples_per_tick)) as i32),
    );
    let n_out = best_a.len().min(best_p.len());
    (best_p, best_a[..n_out].to_vec(), speed)
}

/// Pick ticks/row from inter-onset frames. Faithful default is 1.
pub fn suggest_speed(keyon_samples: &[i64], hz: f64) -> i32 {
    if keyon_samples.len() < 8 {
        return 1;
    }
    let frame = VGM_RATE as f64 / hz;
    let frames: Vec<i64> = keyon_samples
        .iter()
        .map(|&s| py_int(py_round(s as f64 / frame)))
        .collect();
    let mut diffs: Vec<i64> = Vec::new();
    for w in frames.windows(2) {
        let d = w[1] - w[0];
        if (1..=16).contains(&d) {
            diffs.push(d);
        }
    }
    if diffs.is_empty() {
        return 1;
    }
    let mut hist: IndexMap<i64, usize> = IndexMap::new();
    for d in diffs {
        *hist.entry(d).or_insert(0) += 1;
    }
    let mut ordered: Vec<(i64, usize)> = hist.iter().map(|(k, &c)| (*k, c)).collect();
    ordered.sort_by(|a, b| b.1.cmp(&a.1));
    let candidates: Vec<(i64, usize)> = ordered
        .into_iter()
        .filter(|(d, _)| (3..=8).contains(d))
        .collect();
    let total: usize = hist.values().sum();
    if let Some(&(d, c)) = candidates.first() {
        // Prefer a 3–8 frame grid if it is a real peak, else speed 1.
        if c as f64 >= f64::max(12.0, total as f64 * 0.18) {
            return d as i32;
        }
    }
    1
}

/// Attach a note's mid-note pitch writes to it.
pub fn _finalize_sweep(
    ev: &mut NoteEvent,
    pts: &[(i64, i32, i32)],
    base_kc: i32,
    base_kf: i32,
) {
    if pts.is_empty() {
        return;
    }
    let mut merged: Vec<(i64, i32, i32)> = Vec::new();
    for &p in pts {
        if !merged.is_empty() && p.0 - merged[merged.len() - 1].0 <= 16 {
            let last = merged.len() - 1;
            merged[last] = p;
        } else {
            merged.push(p);
        }
    }
    let (ref_kc, ref_kf) = if merged[0].0 - ev.sample <= 120 {
        (merged[0].1, merged[0].2)
    } else {
        (base_kc, base_kf)
    };
    if ev.on && (ref_kc, ref_kf) != (base_kc, base_kf) {
        ev.note = kc_to_furnace_note(ref_kc);
        ev.e5 = kf_to_e5(ref_kf);
    }
    let r0 = kc_kf_units(ref_kc, ref_kf);
    let deltas: Vec<(i64, i32)> = merged
        .iter()
        .map(|&(s, kc, kf)| (s, kc_kf_units(kc, kf) - r0))
        .collect();
    // The note's own pitch counts as a trajectory point. Without it a single
    // held mid-note step has no spread and is dropped.
    let mut vals: Vec<i32> = vec![0];
    vals.extend(deltas.iter().map(|p| p.1));
    let mx = *vals.iter().max().unwrap();
    let mn = *vals.iter().min().unwrap();
    if mx - mn >= 4 {
        ev.sweep = deltas;
    }
}

// A held legato move is at least half a semitone in 1/64 units. Smaller
// changes stay in the ramp model, where the row slide can express them.
pub const _LEGATO_MIN_UNITS: i32 = 32;

/// Turn a held one- or two-step YM2151 move into a quick transposition.
pub fn _classify_legato(
    events: &mut [NoteEvent],
    row_samples: f64,
    channels: Option<&HashSet<i32>>,
) -> (i32, i32) {
    let mut steps = 0i32;
    let mut ramps = 0i32;
    for ev in events.iter_mut() {
        if !ev.on
            || ev.pcm
            || ev.tone_chip != 0
            || ev.dac
            || ev.sweep.is_empty()
            || channels.map_or(false, |c| !c.contains(&ev.ch))
        {
            continue;
        }
        let mut seq: Vec<(i64, i32)> = Vec::new();
        let mut last = 0i32;
        for &(sample, delta) in &ev.sweep {
            if delta != last {
                seq.push((sample, delta));
                last = delta;
            }
        }
        if !(1..=2).contains(&seq.len()) {
            continue;
        }
        if seq[seq.len() - 1].1.abs() < _LEGATO_MIN_UNITS {
            ramps += 1;
            continue;
        }
        // A step that does not hold for a row before the key-off is a slide,
        // not a retune. So is a write in the release tail.
        if ev.end != 0 {
            let last = seq[seq.len() - 1];
            if last.0 >= ev.end || last.0 as f64 + row_samples > ev.end as f64 {
                ramps += 1;
                continue;
            }
        }
        if seq.len() == 2 {
            // Two steps in one direction that keep moving. A return is a
            // vibrato half-cycle and stays a ramp.
            if (seq[0].1 > 0) != (seq[1].1 > 0) {
                ramps += 1;
                continue;
            }
            if seq[1].1.abs() <= seq[0].1.abs() {
                ramps += 1;
                continue;
            }
        }
        ev.pitch_steps = seq;
        ev.sweep.clear();
        steps += 1;
    }
    (steps, ramps)
}

fn value_at_opt(
    pts: &[(i64, Option<i32>)],
    start: i64,
    default: Option<i32>,
) -> Option<i32> {
    let mut cur = default;
    for item in pts {
        if item.0 <= start {
            cur = item.1;
        } else {
            break;
        }
    }
    cur
}

/// Split a PCE tone gate at each volume step (and every five seconds).
pub fn _split_pce_volume_peaks(
    events: &[NoteEvent],
    clock: u32,
    track_end: i64,
) -> Vec<NoteEvent> {
    let cap = 5 * VGM_RATE;
    let mut out: Vec<NoteEvent> = Vec::new();
    for ev in events {
        if !(ev.on && ev.tone_chip == 5 && !ev.vol_pts.is_empty()) {
            out.push(ev.clone());
            continue;
        }
        let end = if ev.end != 0 { ev.end } else { track_end };
        if end <= ev.sample {
            out.push(ev.clone());
            continue;
        }
        let mut timeline: Vec<(i64, i32, Option<i32>)> =
            vec![(ev.sample, ev.vol, Some(ev.pan))];
        timeline.extend(ev.vol_pts.iter().copied());
        timeline.sort_by_key(|item| item.0);
        let mut pts: Vec<(i64, i32, Option<i32>)> = Vec::new();
        for (t, v, p) in timeline {
            if !pts.is_empty() && pts[pts.len() - 1].0 == t {
                let last = pts.len() - 1;
                pts[last] = (t, v, p);
            } else {
                pts.push((t, v, p));
            }
        }
        let vol_at: Vec<(i64, i32)> = pts.iter().map(|p| (p.0, p.1)).collect();
        let pan_at: Vec<(i64, Option<i32>)> =
            pts.iter().map(|p| (p.0, p.2)).collect();
        let mut splits: Vec<i64> = (1..pts.len())
            .filter(|&i| pts[i].1 != pts[i - 1].1)
            .map(|i| pts[i].0)
            .collect();
        let mut t = ev.sample;
        while t + cap < end {
            t += cap;
            splits.push(t);
        }
        splits.retain(|&s| ev.sample < s && s < end);
        splits.sort();
        splits.dedup();
        if splits.is_empty() {
            out.push(ev.clone());
            continue;
        }
        let mut starts: Vec<i64> = vec![ev.sample];
        starts.extend(splits.iter().copied());
        for (i, &st) in starts.iter().enumerate() {
            let seg_end = if i + 1 < starts.len() {
                starts[i + 1]
            } else {
                ev.end
            };
            let bound = if seg_end != 0 { seg_end } else { track_end };
            let vol = super::util::_value_at(&vol_at, st, ev.vol);
            let pan = value_at_opt(&pan_at, st, Some(ev.pan)).unwrap_or(ev.pan);
            let (note, e5, base_period, pitch_pts): (
                i32,
                i32,
                i32,
                Vec<(i64, i32)>,
            );
            if ev.tone_noise {
                let mut noise_pts: Vec<(i64, i32)> =
                    vec![(ev.sample, ev.note - 60)];
                noise_pts.extend(ev.tone_pitch_pts.iter().copied());
                let base_noise = super::util::_value_at(
                    &noise_pts,
                    st,
                    ev.note - 60,
                );
                note = huc6280_noise_note(base_noise);
                e5 = 0x80;
                base_period = 0;
                pitch_pts = ev
                    .tone_pitch_pts
                    .iter()
                    .filter(|p| st < p.0 && p.0 < bound)
                    .copied()
                    .collect();
            } else {
                let mut per_pts: Vec<(i64, i32)> =
                    vec![(ev.sample, ev.tone_period)];
                per_pts.extend(ev.tone_pitch_pts.iter().copied());
                base_period =
                    super::util::_value_at(&per_pts, st, ev.tone_period);
                let (n, e) = huc6280_note(clock, base_period);
                note = n;
                e5 = e;
                pitch_pts = ev
                    .tone_pitch_pts
                    .iter()
                    .filter(|p| st < p.0 && p.0 < bound)
                    .copied()
                    .collect();
            }
            let mut wave_pts: Vec<(i64, i32)> =
                vec![(ev.sample, ev.tone_wave)];
            wave_pts.extend(ev.tone_wave_pts.iter().copied());
            let wave = super::util::_value_at(&wave_pts, st, ev.tone_wave);
            let mut seg = ev.clone();
            seg.sample = st;
            seg.note = note;
            seg.e5 = e5;
            seg.vol = vol;
            seg.pan = pan;
            seg.end = seg_end;
            seg.vol_pts = pts
                .iter()
                .filter(|p| st < p.0 && p.0 < bound)
                .map(|p| (p.0, p.1, p.2))
                .collect();
            seg.tone_pitch_pts = pitch_pts;
            seg.tone_wave_pts = ev
                .tone_wave_pts
                .iter()
                .filter(|p| st < p.0 && p.0 < bound)
                .copied()
                .collect();
            seg.tone_wave = wave;
            seg.tone_period = base_period;
            out.push(seg);
        }
    }
    out
}

/// How many tone instruments the notes want with an unlimited budget.
#[allow(clippy::too_many_arguments)]
pub fn _tone_instrument_demand(
    events: &[NoteEvent],
    scc_clock: u32,
    ay_clock: u32,
    psg_clock: u32,
    nes_clock: u32,
    huc6280_clock: u32,
    gb_clock: u32,
    vsu_clock: u32,
    saa_clock: u32,
    pokey_clock: u32,
    mikey_clock: u32,
    x1_clock: u32,
    fds_clock: u32,
    wonderswan_clock: u32,
    k005289_clock: u32,
    hz: f64,
    total_samples: i64,
    use_speed: i32,
    lfo_loop: bool,
) -> i32 {
    let mut tone: Vec<NoteEvent> = events
        .iter()
        .filter(|e| e.on && e.tone_chip != 0)
        .cloned()
        .collect();
    if tone.is_empty() {
        return 0;
    }
    let mut scc: Vec<ToneIns> = Vec::new();
    let mut ay: Vec<ToneIns> = Vec::new();
    let mut psg: Vec<ToneIns> = Vec::new();
    let mut nes: Vec<ToneIns> = Vec::new();
    let mut huc: Vec<ToneIns> = Vec::new();
    let mut gb: Vec<ToneIns> = Vec::new();
    let mut vsu: Vec<ToneIns> = Vec::new();
    let mut saa: Vec<ToneIns> = Vec::new();
    let mut pokey: Vec<ToneIns> = Vec::new();
    let mut mikey: Vec<ToneIns> = Vec::new();
    let mut x1: Vec<ToneIns> = Vec::new();
    let mut fds: Vec<ToneIns> = Vec::new();
    let mut wonderswan: Vec<ToneIns> = Vec::new();
    let mut qsound: Vec<ToneIns> = Vec::new();
    let mut k005289: Vec<ToneIns> = Vec::new();
    _bind_tone_instruments(
        &mut tone,
        &mut scc,
        &mut ay,
        &mut psg,
        &mut nes,
        &mut huc,
        &mut gb,
        &mut vsu,
        &mut saa,
        &mut pokey,
        &mut mikey,
        &mut x1,
        &mut fds,
        &mut wonderswan,
        &mut qsound,
        &mut k005289,
        // A negative base leaves the instrument cap effectively unlimited, so
        // every distinct macro shape is counted.
        -1000,
        scc_clock,
        ay_clock,
        psg_clock,
        nes_clock,
        huc6280_clock,
        gb_clock,
        vsu_clock,
        saa_clock,
        pokey_clock,
        mikey_clock,
        x1_clock,
        fds_clock,
        wonderswan_clock,
        k005289_clock,
        hz,
        total_samples,
        use_speed,
        0,
        lfo_loop,
    );
    scc.len() as i32
        + ay.len() as i32
        + psg.len() as i32
        + nes.len() as i32
        + huc.len() as i32
        + gb.len() as i32
        + vsu.len() as i32
        + saa.len() as i32
        + pokey.len() as i32
        + mikey.len() as i32
        + x1.len() as i32
        + fds.len() as i32
        + wonderswan.len() as i32
        + qsound.len() as i32
        + k005289.len() as i32
}

/// Fill the tone instrument buckets and point each tone note at one.
#[allow(clippy::too_many_arguments)]
pub fn _bind_tone_instruments(
    events: &mut [NoteEvent],
    scc_ins: &mut Vec<ToneIns>,
    ay_ins: &mut Vec<ToneIns>,
    psg_ins: &mut Vec<ToneIns>,
    nes_ins: &mut Vec<ToneIns>,
    huc6280_ins: &mut Vec<ToneIns>,
    gb_ins: &mut Vec<ToneIns>,
    vsu_ins: &mut Vec<ToneIns>,
    saa_ins: &mut Vec<ToneIns>,
    pokey_ins: &mut Vec<ToneIns>,
    mikey_ins: &mut Vec<ToneIns>,
    x1_ins: &mut Vec<ToneIns>,
    fds_ins: &mut Vec<ToneIns>,
    wonderswan_ins: &mut Vec<ToneIns>,
    qsound_ins: &mut Vec<ToneIns>,
    k005289_ins: &mut Vec<ToneIns>,
    tone_ins_base: i32,
    scc_clock: u32,
    ay_clock: u32,
    psg_clock: u32,
    nes_clock: u32,
    huc6280_clock: u32,
    gb_clock: u32,
    vsu_clock: u32,
    saa_clock: u32,
    pokey_clock: u32,
    mikey_clock: u32,
    x1_clock: u32,
    fds_clock: u32,
    wonderswan_clock: u32,
    k005289_clock: u32,
    hz: f64,
    total_samples: i64,
    use_speed: i32,
    merge_tol: i32,
    lfo_loop: bool,
) -> i32 {
    if !events.iter().any(|e| e.tone_chip != 0) {
        return 0;
    }
    let tick_samples = if hz != 0.0 {
        VGM_RATE as f64 / hz
    } else {
        VGM_RATE as f64 / 60.0
    };
    let track_end = if total_samples != 0 {
        total_samples
    } else {
        events.iter().map(|e| e.sample).max().unwrap_or(0)
    };
    let mut scc_map: IndexMap<ToneIns, usize> = IndexMap::new();
    let mut ay_map: IndexMap<ToneIns, usize> = IndexMap::new();
    let mut psg_map: IndexMap<ToneIns, usize> = IndexMap::new();
    let mut nes_map: IndexMap<ToneIns, usize> = IndexMap::new();
    let mut huc6280_map: IndexMap<ToneIns, usize> = IndexMap::new();
    let mut gb_map: IndexMap<ToneIns, usize> = IndexMap::new();
    let mut vsu_map: IndexMap<ToneIns, usize> = IndexMap::new();
    let mut saa_map: IndexMap<ToneIns, usize> = IndexMap::new();
    let mut pokey_map: IndexMap<ToneIns, usize> = IndexMap::new();
    let mut mikey_map: IndexMap<ToneIns, usize> = IndexMap::new();
    let mut x1_map: IndexMap<ToneIns, usize> = IndexMap::new();
    let mut fds_map: IndexMap<ToneIns, usize> = IndexMap::new();
    let mut wonderswan_map: IndexMap<ToneIns, usize> = IndexMap::new();
    let mut qsound_map: IndexMap<ToneIns, usize> = IndexMap::new();
    let mut k005289_map: IndexMap<ToneIns, usize> = IndexMap::new();
    let ins_cap = std::cmp::max(1, INS_TARGET - tone_ins_base);
    // QSound is sample-based: every distinct sample needs a base instrument
    // before pitch/volume macro copies can spend the budget.
    if qsound_ins.is_empty() {
        let mut seen_qs: HashSet<ToneIns> = HashSet::new();
        for event in events.iter() {
            if !(event.on
                && (event.tone_chip == 14 || event.tone_chip == 15)
                && event.tone_sample >= 0)
            {
                continue;
            }
            let base = ToneIns {
                sample: event.tone_sample,
                sample_kind: event.tone_sample_kind.clone(),
                ..ToneIns::default()
            };
            if seen_qs.contains(&base) {
                continue;
            }
            seen_qs.insert(base.clone());
            qsound_map.insert(base.clone(), qsound_ins.len());
            qsound_ins.push(base);
        }
    }
    // The later families bind after the early ones. Reserve a share so a
    // large SCC/AY/PCE list cannot spend every slot before them.
    let has_late = events.iter().any(|e| {
        matches!(e.tone_chip, 6 | 7 | 8 | 9 | 10 | 11 | 12 | 13 | 14 | 15)
            && e.on
    });
    let mut early_cap = ins_cap;
    if has_late {
        let mut fams: HashSet<i32> = HashSet::new();
        for e in events.iter() {
            if e.on && e.tone_chip != 0 {
                fams.insert(e.tone_chip);
            }
        }
        let families = fams.len() as i32;
        early_cap = std::cmp::max(
            1,
            ins_cap - std::cmp::max(8, ins_cap / std::cmp::max(2, families)),
        );
    }
    let mut bound: Vec<(usize, usize)> = Vec::new();
    let mut merged = 0i32;

    let mut take = |spec: ToneIns,
                    mapping: &mut IndexMap<ToneIns, usize>,
                    bucket: &mut Vec<ToneIns>,
                    others: usize,
                    cap: i32,
                    chip: i32|
     -> (usize, ToneIns) {
        let original = spec.clone();
        let (idx, kept) = super::util::_take_tone(
            spec,
            mapping,
            bucket,
            others,
            cap.max(0) as usize,
            chip,
            merge_tol,
        );
        if merge_tol > 0 && kept != original {
            merged += 1;
        }
        (idx, kept)
    };

    let mut tone_order: Vec<usize> = Vec::new();
    for chip in [2i32, 1, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16] {
        for (i, e) in events.iter().enumerate() {
            if e.on && e.tone_chip == chip {
                tone_order.push(i);
            }
        }
    }
    for ei in tone_order {
        let e = &mut events[ei];
        let end = if e.end != 0 { e.end } else { track_end };
        // 255 macro values times the largest speed byte. Long SSG pads can
        // hold a sparse late pitch step; a 4096-tick cutoff used to drop it.
        let ticks = std::cmp::max(
            1,
            std::cmp::min(
                py_int((end - e.sample) as f64 / tick_samples) + 1,
                65025,
            ),
        );
        let clock: f64;
        let divider: f64;
        let maximum: i32;
        if e.tone_chip == 1 {
            clock = scc_clock as f64;
            divider = 16.0;
            maximum = 0xFFF;
        } else if e.tone_chip == 3 {
            // The SMS core defaults to the full COLOR_NTSC clock (not the
            // half-clock AY default) and a 10-bit period with divider 64.
            divider = if e.tone_div != 0 {
                e.tone_div as f64
            } else {
                PSG_DIVIDER as f64
            };
            let mut c = if e.tone_clock != 0 {
                e.tone_clock as f64
            } else if psg_clock != 0 {
                psg_clock as f64
            } else {
                PSG_DEFAULT_CLOCK as f64
            };
            if divider == PSG_DIVIDER as f64
                && (c - PSG_FURNACE_CLOCK).abs() <= 1.0
            {
                c = PSG_FURNACE_CLOCK;
            }
            clock = c;
            maximum = PSG_MAX_PERIOD;
        } else if e.tone_chip == 4 {
            let mut c = if e.tone_clock != 0 {
                e.tone_clock as f64
            } else if nes_clock != 0 {
                nes_clock as f64
            } else {
                NES_DEFAULT_CLOCK as f64
            };
            if (c - NES_FURNACE_CLOCK).abs() <= 1.0 {
                c = NES_FURNACE_CLOCK;
            }
            clock = c;
            divider = NES_CPU_DIVIDER as f64;
            maximum = 0x7FF;
        } else if e.tone_chip == 5 {
            clock = if e.tone_clock != 0 {
                e.tone_clock as f64
            } else if huc6280_clock != 0 {
                huc6280_clock as f64
            } else {
                HUC6280_DEFAULT_CLOCK as f64
            };
            divider = 32.0;
            maximum = HUC6280_MAX_PERIOD;
        } else if e.tone_chip == 6 {
            clock = if e.tone_clock != 0 {
                e.tone_clock as f64
            } else if gb_clock != 0 {
                gb_clock as f64
            } else {
                GB_DEFAULT_CLOCK as f64
            };
            divider = GB_PITCH_DIVIDER as f64;
            maximum = 0x7FF;
        } else if e.tone_chip == 7 {
            clock = if e.tone_clock != 0 {
                e.tone_clock as f64
            } else if vsu_clock != 0 {
                vsu_clock as f64
            } else {
                VSU_DEFAULT_CLOCK as f64
            };
            divider = VSU_PITCH_DIVIDER as f64;
            maximum = 0x7FF;
        } else if e.tone_chip == 8 {
            clock = if e.tone_clock != 0 {
                e.tone_clock as f64
            } else if saa_clock != 0 {
                saa_clock as f64
            } else {
                SAA_DEFAULT_CLOCK as f64
            };
            divider = SAA_DIVIDER as f64;
            maximum = SAA_PITCH_MAX;
        } else if e.tone_chip == 9 {
            clock = if e.tone_clock != 0 {
                e.tone_clock as f64
            } else if pokey_clock != 0 {
                pokey_clock as f64
            } else {
                POKEY_DEFAULT_CLOCK as f64
            };
            divider = 1.0;
            maximum = POKEY_PITCH_MAX;
        } else if e.tone_chip == 10 {
            clock = if e.tone_clock != 0 {
                e.tone_clock as f64
            } else if mikey_clock != 0 {
                mikey_clock as f64
            } else {
                MIKEY_DEFAULT_CLOCK as f64
            };
            divider = MIKEY_DIVIDER as f64;
            maximum = MIKEY_MAX_PERIOD;
        } else if e.tone_chip == 11 {
            clock = if e.tone_clock != 0 {
                e.tone_clock as f64
            } else if x1_clock != 0 {
                x1_clock as f64
            } else {
                X1_010_DEFAULT_CLOCK as f64
            };
            divider = X1_FREQ_DIV as f64;
            maximum = X1_MAX_FREQ;
        } else if e.tone_chip == 12 {
            clock = if e.tone_clock != 0 {
                e.tone_clock as f64
            } else if fds_clock != 0 {
                fds_clock as f64
            } else {
                FDS_DEFAULT_CLOCK as f64
            };
            divider = FDS_FREQ_DIV as f64;
            maximum = FDS_MAX_FREQ;
        } else if e.tone_chip == 13 {
            clock = if e.tone_clock != 0 {
                e.tone_clock as f64
            } else if wonderswan_clock != 0 {
                wonderswan_clock as f64
            } else {
                WSWAN_DEFAULT_CLOCK as f64
            };
            divider = WS_PITCH_DIVIDER as f64;
            maximum = WS_MAX_PERIOD;
        } else if e.tone_chip == 14 || e.tone_chip == 15 {
            clock = crate::qsound::QSOUND_OUT_RATE;
            divider = QSOUND_FREQ_DIV as f64;
            maximum = QSOUND_MAX_FREQ;
        } else if e.tone_chip == 16 {
            clock = if e.tone_clock != 0 {
                e.tone_clock as f64
            } else if k005289_clock != 0 {
                k005289_clock as f64
            } else {
                K005289_DEFAULT_CLOCK as f64
            };
            divider = K005289_DIVIDER as f64;
            maximum = 0xFFF;
        } else {
            // AY and the YM2203's SSG share the instrument path but not the
            // clock or Furnace's divider (8 vs 16).
            clock = if e.tone_clock != 0 {
                e.tone_clock as f64
            } else {
                ay_clock as f64
            };
            divider = if e.tone_div != 0 {
                e.tone_div as f64
            } else {
                8.0
            };
            maximum = 0xFFF;
        }
        let tbl = if e.tone_chip == 8 {
            PitchTableRef::Period(pitch_table(
                saa_module_clock(clock),
                divider,
                maximum,
            ))
        } else if e.tone_chip == 9 {
            PitchTableRef::Period(pitch_table(
                module_clock(clock),
                divider,
                maximum,
            ))
        } else if e.tone_chip == 10 {
            PitchTableRef::Period(pitch_table(clock, divider, maximum))
        } else if matches!(e.tone_chip, 11 | 12 | 14 | 15) {
            PitchTableRef::Frequency(frequency_pitch_table(
                clock, divider, maximum,
            ))
        } else if matches!(e.tone_chip, 3 | 4 | 5 | 6 | 7 | 13) {
            PitchTableRef::Period(pitch_table(clock, divider, maximum))
        } else {
            PitchTableRef::Period(pitch_table(module_clock(clock), divider, 0xFFF))
        };
        let base_off = (e.note << 7) + (e.e5 - E5_CENTER);
        if e.tone_chip == 5 && !e.tone_pitch_pts.is_empty() && !e.tone_noise {
            // A PCE driver that retunes every frame writes a staircase, not
            // a vibrato. When every step is at least 0.75 semitone, put the
            // trajectory on the pattern rows as E8/E9 + E5 instead.
            let steps: Vec<(i64, i32)> = e
                .tone_pitch_pts
                .iter()
                .map(|&(t, per)| {
                    (
                        t,
                        py_int(py_round(
                            (tbl.offset_for(e.note, per + e.tone_period_offset)
                                - base_off)
                                as f64
                                / 2.0,
                        )) as i32,
                    )
                })
                .collect();
            if !steps.is_empty() && steps.iter().all(|p| p.1.abs() >= 48) {
                e.pitch_steps = steps;
                e.tone_pitch_pts.clear();
                e.sweep_div = 2.0;
                e.e5_lo = 0x00;
                e.e5_hi = 0xFF;
            }
        } else if matches!(e.tone_chip, 3 | 6 | 11)
            && !e.tone_pitch_pts.is_empty()
            && !e.tone_noise
        {
            // SN76489, Game Boy and X1-010 drivers play legato melodies,
            // arpeggios and slides by rewriting the period/frequency without
            // a gate/trigger.
            let row_len = tick_samples * std::cmp::max(1, use_speed) as f64;
            let mut rows: HashSet<i64> = HashSet::new();
            for &(t, _per) in &e.tone_pitch_pts {
                rows.insert(((t - e.sample) as f64 / row_len).floor() as i64);
            }
            if row_len > 0.0
                && (rows.len() * 3 >= e.tone_pitch_pts.len() * 2)
            {
                // Round half up rather than to even: the pitch table's short
                // periods sit on plateaus where two adjacent source registers
                // differ by one engine unit.
                let mut steps: Vec<(i64, i32)> = e
                    .tone_pitch_pts
                    .iter()
                    .map(|&(t, per)| {
                        (
                            t,
                            ((
                                (tbl.offset_for(
                                    e.note,
                                    per + e.tone_period_offset,
                                ) - base_off) as f64
                                    / 2.0
                                    + 0.5
                            )
                                .floor()) as i32,
                        )
                    })
                    .collect();
                steps.retain(|p| p.1 != 0);
                if !steps.is_empty() {
                    e.pitch_steps = steps;
                    e.tone_pitch_pts.clear();
                    e.sweep_div = 2.0;
                    e.e5_lo = 0x00;
                    e.e5_hi = 0xFF;
                }
            }
        }
        let mut pitch: Vec<i32> = Vec::new();
        let mut pitch_speed = 1i32;
        let mut pitch_loop = 255i32;
        let mut pitch_delay = 0i32;
        if !e.tone_pitch_pts.is_empty() {
            let mut pts: Vec<(i64, i32)> = vec![(e.sample, 0)];
            if e.tone_noise {
                // PCE noise: the register follows note-60 and `pitch2`
                // directly, so the macro value is the raw register offset.
                let gate_n = e.note - 60;
                let modes = e.tone_noise_modes.clone();
                let mut mode = if modes.is_empty() { 1 } else { modes[0].1 };
                let mut mi = 0usize;
                for &(t, per) in &e.tone_pitch_pts {
                    while mi + 1 < modes.len() && modes[mi + 1].0 <= t {
                        mi += 1;
                        mode = modes[mi].1;
                    }
                    if mode != 0 {
                        pts.push((t, per - gate_n));
                    } else {
                        pts.push((
                            t,
                            tbl.offset_for(
                                e.note,
                                per + e.tone_period_offset,
                            ) - base_off,
                        ));
                    }
                }
            } else {
                for &(t, per) in &e.tone_pitch_pts {
                    pts.push((
                        t,
                        tbl.offset_for(e.note, per + e.tone_period_offset)
                            - base_off,
                    ));
                }
            }
            let series = _tick_values(&pts, e.sample, ticks, tick_samples);
            if series.iter().any(|&v| v != 0) {
                if e.tone_noise {
                    // A noise pitch is a register offset, not a tone bend;
                    // leave its leading values uncollapsed.
                    let (p, s, l) = _macro_series(&series, false, 1, 0, 16);
                    pitch = p;
                    pitch_speed = s;
                    pitch_loop = l;
                } else if e.tone_chip == 8 {
                    // A SAA1099 driver uses fast detunes as a pseudo-duty
                    // cycle; thinning or step-compressing them changes the
                    // duty ratio.
                    let (p, s, l, d) =
                        _macro_series_delay(&series, lfo_loop, false);
                    pitch = p;
                    pitch_speed = s;
                    pitch_loop = l;
                    pitch_delay = d;
                    if pitch_delay == 0 {
                        pitch = super::util::_absorb_detune(
                            e,
                            &pitch,
                            pitch_speed,
                        );
                    }
                } else {
                    // X1-010 drivers use pitch slides, and the loop detector
                    // can mistake a repeated slide fragment for an LFO.
                    let (p, s, l, d) = _macro_series_delay(
                        &series,
                        lfo_loop && e.tone_chip != 11,
                        true,
                    );
                    pitch = p;
                    pitch_speed = s;
                    pitch_loop = l;
                    pitch_delay = d;
                    if pitch_delay == 0 {
                        pitch = super::util::_absorb_detune(
                            e,
                            &pitch,
                            pitch_speed,
                        );
                    }
                }
            }
            if e.tone_chip == 8 && !pitch.is_empty() && pitch.len() <= 8 {
                // The short residual bend after a legato split is the
                // transition into the next note, not an instrument identity.
                let mut steps: Vec<(i64, i32)> = e
                    .tone_pitch_pts
                    .iter()
                    .map(|&(t, per)| {
                        (
                            t,
                            py_int(py_round(
                                (tbl.offset_for(
                                    e.note,
                                    per + e.tone_period_offset,
                                ) - base_off) as f64
                                    / 2.0,
                            )) as i32,
                        )
                    })
                    .collect();
                steps.retain(|p| p.1 != 0);
                if !steps.is_empty() {
                    e.pitch_steps = steps;
                    e.sweep_div = 2.0;
                    e.e5_lo = 0x00;
                    e.e5_hi = 0xFF;
                    pitch.clear();
                    pitch_speed = 1;
                    pitch_loop = 255;
                    pitch_delay = 0;
                }
            }
        }
        let mut wave_vals: Vec<i32> = Vec::new();
        let mut wave_speed = 1i32;
        let mut wave_loop = 255i32;
        if e.tone_chip == 11 && e.tone_wave >= 0 {
            let mut pts: Vec<(i64, i32)> = vec![(e.sample, e.tone_wave)];
            pts.extend(e.x1_wave_pts.iter().copied());
            let (w, s, l) = _macro_series(
                &_tick_values(&pts, e.sample, ticks, tick_samples),
                false,
                1,
                0,
                16,
            );
            wave_vals = w;
            wave_speed = s;
            wave_loop = l;
            if wave_vals.is_empty() {
                wave_vals = vec![e.tone_wave];
                wave_speed = 1;
                wave_loop = 255;
            }
        } else if !e.tone_wave_pts.is_empty() {
            let mut pts: Vec<(i64, i32)> =
                vec![(e.sample, std::cmp::max(0, e.tone_wave))];
            pts.extend(e.tone_wave_pts.iter().copied());
            let (w, s, l) = _macro_series(
                &_tick_values(&pts, e.sample, ticks, tick_samples),
                false,
                1,
                0,
                16,
            );
            wave_vals = w;
            wave_speed = s;
            wave_loop = l;
            if e.tone_chip == 9 && wave_vals.is_empty() {
                // A constant distortion still belongs in the instrument.
                wave_vals = vec![std::cmp::max(0, e.tone_wave)];
                wave_speed = 1;
                wave_loop = 255;
            }
        }
        let column_vol = e.vol;
        let mut pce_peak = 0i32;
        let mut saa_peak = 0i32;
        let mut vol_vals: Vec<i32> = Vec::new();
        let mut vol_speed = 1i32;
        let mut vol_loop = 255i32;
        let mut vol_delay = 0i32;
        let mut series: Vec<i32> = Vec::new();
        if !e.vol_pts.is_empty()
            && !matches!(e.tone_chip, 3 | 11)
            && !(e.tone_chip == 6 && e.tone_gb_wave)
        {
            // PSG volume is the raw 4-bit register and the SMS platform reads
            // it straight from the column; a std.vol macro would scale it.
            let mut pts: Vec<(i64, i32)> =
                vec![(e.sample, std::cmp::max(0, e.vol))];
            for &(t, v, _) in &e.vol_pts {
                pts.push((t, std::cmp::max(0, v)));
            }
            series = _tick_values(&pts, e.sample, ticks, tick_samples);
            if e.tone_chip == 5 {
                // Normalize the PCE envelope to its own peak.
                pce_peak = if series.is_empty() {
                    0
                } else {
                    std::cmp::max(1, series[0])
                };
            }
            if e.tone_chip == 8 {
                // Same idea for the SAA level.
                saa_peak = if series.is_empty() {
                    0
                } else {
                    std::cmp::max(1, *series.iter().max().unwrap())
                };
                if saa_peak != 0 {
                    series = series
                        .iter()
                        .map(|&v| {
                            std::cmp::max(
                                0,
                                std::cmp::min(
                                    15,
                                    py_int(py_round(
                                        v as f64 * 15.0 / saa_peak as f64,
                                    )) as i32,
                                ),
                            )
                        })
                        .collect();
                }
            }
            if e.tone_chip == 6 {
                // The GB platform applies a software-envelope volume macro
                // directly; never fold the trajectory into a slide.
                let (v, s, l, d) = _macro_series_delay(&series, false, false);
                vol_vals = v;
                vol_speed = s;
                vol_loop = l;
                vol_delay = d;
            } else {
                let folded = if matches!(e.tone_chip, 5 | 8) {
                    None
                } else {
                    _fold_saw(&series)
                };
                // Past 255 ticks a non-repeating series is stored as every
                // Nth value. A one-level saw is played as a slide instead.
                let thin = !matches!(e.tone_chip, 5 | 8)
                    && folded.is_none()
                    && series.len() > MACRO_MAX
                    && _cycle(&series, 128).is_none()
                    && _unit_saw(&series);
                if let Some((v, s, l)) = folded {
                    vol_vals = v;
                    vol_speed = s;
                    vol_loop = l;
                } else if thin {
                    e.vol = series[0];
                    e.vol_slide = _slide_open(&series);
                } else {
                    let (v, s, l, d) =
                        _macro_series_delay(&series, false, false);
                    vol_vals = v;
                    vol_speed = s;
                    vol_loop = l;
                    vol_delay = d;
                    if e.tone_chip == 5 && !vol_vals.is_empty() && pce_peak != 0
                    {
                        vol_vals = vol_vals
                            .iter()
                            .map(|&v| {
                                std::cmp::max(
                                    0,
                                    std::cmp::min(
                                        31,
                                        py_int(py_round(
                                            v as f64 * 31.0 / pce_peak as f64,
                                        )) as i32,
                                    ),
                                )
                            })
                            .collect();
                        if !vol_vals.is_empty() {
                            vol_vals[0] = 31;
                        }
                    }
                    let used = scc_ins.len() + ay_ins.len();
                    // AY volume is the 4-bit register.
                    if e.tone_chip == 1
                        && !vol_vals.is_empty()
                        && used > 160
                        && !_unit_saw(&vol_vals)
                    {
                        let step = if used <= 208 { 2 } else { 4 };
                        for v in vol_vals.iter_mut() {
                            *v = (*v / step) * step;
                        }
                        let mut distinct = vol_vals.clone();
                        distinct.sort();
                        distinct.dedup();
                        if distinct.len() <= 1 {
                            vol_vals.clear();
                        }
                    }
                }
            }
        }
        let mut noise: Vec<i32> = Vec::new();
        let mut mode: Vec<i32> = Vec::new();
        let mut env_shape: Vec<i32> = Vec::new();
        let mut env_period: Vec<i32> = Vec::new();
        let mut env_hw = false;
        let mut noise_speed = 1i32;
        let mut mode_speed = 1i32;
        let mut env_shape_speed = 1i32;
        let mut env_period_speed = 1i32;
        let mut noise_loop = 255i32;
        let mut mode_loop = 255i32;
        let mut env_shape_loop = 255i32;
        let mut env_period_loop = 255i32;
        let mut panl: Vec<i32> = Vec::new();
        let mut panr: Vec<i32> = Vec::new();
        let mut env: Vec<i32> = Vec::new();
        let mut panl_speed = 1i32;
        let mut panr_speed = 1i32;
        let mut env_speed = 1i32;
        let mut panl_loop = 255i32;
        let mut panr_loop = 255i32;
        let mut env_loop = 255i32;
        let mut panl_delay = 0i32;
        let mut panr_delay = 0i32;
        let mut env_delay = 0i32;
        let mut duty: Vec<i32> = Vec::new();
        let mut lfsr: Vec<i32> = Vec::new();
        let mut duty_speed = 1i32;
        let mut lfsr_speed = 1i32;
        let mut duty_loop = 255i32;
        let mut lfsr_loop = 255i32;
        if e.tone_chip == 8 {
            // SAA1099: the amplitude register carries volume and soft pan
            // together; put the raw nibbles on the two panning macros and
            // leave the volume column at 15.
            if !e.tone_amp_pts.is_empty() {
                let pl: Vec<(i64, i32)> =
                    e.tone_amp_pts.iter().map(|p| (p.0, p.1)).collect();
                let pr: Vec<(i64, i32)> =
                    e.tone_amp_pts.iter().map(|p| (p.0, p.2)).collect();
                let pl_series = _tick_values(&pl, e.sample, ticks, tick_samples);
                let pr_series = _tick_values(&pr, e.sample, ticks, tick_samples);
                let (a, b, c, d) = _macro_series_delay(&pl_series, false, false);
                panl = a;
                panl_speed = b;
                panl_loop = c;
                panl_delay = d;
                let (a, b, c, d) = _macro_series_delay(&pr_series, false, false);
                panr = a;
                panr_speed = b;
                panr_loop = c;
                panr_delay = d;
                // Both panning macros must be present.
                if panl.is_empty() && !pl_series.is_empty() {
                    panl = vec![pl_series[0]];
                    panl_speed = 1;
                    panl_loop = 255;
                    panl_delay = 0;
                }
                if panr.is_empty() && !pr_series.is_empty() {
                    panr = vec![pr_series[0]];
                    panr_speed = 1;
                    panr_loop = 255;
                    panr_delay = 0;
                }
            }
            let npts: Vec<(i64, i32)> = e
                .tone_fx_pts
                .iter()
                .filter(|p| p.1 == 0x11)
                .map(|p| (p.0, p.2))
                .collect();
            let (a, b, c) = super::util::_macro_if_moving(
                &npts,
                e.sample,
                ticks,
                tick_samples,
                super::util::_AY_MERGE_GAP as i32,
            );
            noise = a;
            noise_speed = b;
            noise_loop = c;
            let mpts: Vec<(i64, i32)> = e
                .tone_fx_pts
                .iter()
                .filter(|p| p.1 == 0x10)
                .map(|p| (p.0, p.2 & 3))
                .collect();
            let (a, b, c) = super::util::_macro_if_moving(
                &mpts,
                e.sample,
                ticks,
                tick_samples,
                super::util::_AY_MERGE_GAP as i32,
            );
            mode = a;
            mode_speed = b;
            mode_loop = c;
            let epts: Vec<(i64, i32)> = e
                .tone_fx_pts
                .iter()
                .filter(|p| p.1 == 0x12)
                .map(|p| (p.0, p.2))
                .collect();
            let (a, b, c, d) = _macro_series_delay(
                &_tick_values(&epts, e.sample, ticks, tick_samples),
                false,
                false,
            );
            env = a;
            env_speed = b;
            env_loop = c;
            env_delay = d;
        } else if e.tone_chip == 13 {
            // WonderSwan: the volume register contains the left/right nibbles.
            let mut pl: Vec<(i64, i32)> = vec![(e.sample, e.tone_pan_l)];
            let mut pr: Vec<(i64, i32)> = vec![(e.sample, e.tone_pan_r)];
            pl.extend(e.tone_amp_pts.iter().map(|p| (p.0, p.1)));
            pr.extend(e.tone_amp_pts.iter().map(|p| (p.0, p.2)));
            let pl_series = _tick_values(&pl, e.sample, ticks, tick_samples);
            let pr_series = _tick_values(&pr, e.sample, ticks, tick_samples);
            let (a, b, c, d) = _macro_series_delay(&pl_series, false, false);
            panl = a;
            panl_speed = b;
            panl_loop = c;
            panl_delay = d;
            let (a, b, c, d) = _macro_series_delay(&pr_series, false, false);
            panr = a;
            panr_speed = b;
            panr_loop = c;
            panr_delay = d;
            if panl.is_empty() {
                panl = vec![e.tone_pan_l];
                panl_speed = 1;
                panl_loop = 255;
                panl_delay = 0;
            }
            if panr.is_empty() {
                panr = vec![e.tone_pan_r];
                panr_speed = 1;
                panr_loop = 255;
                panr_delay = 0;
            }
        } else if e.tone_chip == 9 {
            // POKEY: AUDCTL is the instrument's duty macro.
            let audctl_pts: Vec<(i64, i32)> = e
                .tone_fx_pts
                .iter()
                .filter(|p| p.1 == POKEY_FX_AUDCTL)
                .map(|p| (p.0, p.2))
                .collect();
            let (a, b, c) = super::util::_macro_if_moving(
                &audctl_pts,
                e.sample,
                ticks,
                tick_samples,
                super::util::_AY_MERGE_GAP as i32,
            );
            noise = a;
            noise_speed = b;
            noise_loop = c;
            if noise.is_empty() && !audctl_pts.is_empty() {
                noise = vec![audctl_pts[0].1];
                noise_speed = 1;
                noise_loop = 255;
            }
        } else if e.tone_chip == 11 {
            // X1-010: envelope mode, shape and period are always present as
            // macros, even when constant.
            let mut mode_pts: Vec<(i64, i32)> =
                vec![(e.sample, e.tone_env_mode)];
            mode_pts.extend(e.x1_env_mode_pts.iter().copied());
            let (a, b, c, d) = _macro_series_delay(
                &_tick_values(&mode_pts, e.sample, ticks, tick_samples),
                false,
                false,
            );
            env = a;
            env_speed = b;
            env_loop = c;
            env_delay = d;
            if env.is_empty() {
                env = vec![e.tone_env_mode];
                env_speed = 1;
                env_loop = 255;
                env_delay = 0;
            }
            let mut shape_pts: Vec<(i64, i32)> =
                vec![(e.sample, e.tone_env_shape)];
            shape_pts.extend(e.x1_env_shape_pts.iter().copied());
            let (a, b, c) = _macro_series(
                &_tick_values(&shape_pts, e.sample, ticks, tick_samples),
                false,
                1,
                0,
                16,
            );
            env_shape = a;
            env_shape_speed = b;
            env_shape_loop = c;
            if env_shape.is_empty() {
                env_shape = vec![e.tone_env_shape];
                env_shape_speed = 1;
                env_shape_loop = 255;
            }
            let mut period_pts: Vec<(i64, i32)> =
                vec![(e.sample, e.tone_env_period)];
            period_pts.extend(e.x1_env_period_pts.iter().copied());
            let (a, b, c) = _macro_series(
                &_tick_values(&period_pts, e.sample, ticks, tick_samples),
                false,
                1,
                0,
                16,
            );
            env_period = a;
            env_period_speed = b;
            env_period_loop = c;
            if env_period.is_empty() {
                env_period = vec![e.tone_env_period];
                env_period_speed = 1;
                env_period_loop = 255;
            }
        } else if e.tone_chip == 10 {
            // Lynx: the feedback/control taps and the LFSR seed are 10- and
            // 12-bit values, so they live in the 16-bit macro encoding.
            let mut duty_pts: Vec<(i64, i32)> =
                vec![(e.sample, e.tone_duty)];
            duty_pts.extend(e.tone_duty_pts.iter().copied());
            let duty_series =
                _tick_values(&duty_pts, e.sample, ticks, tick_samples);
            let mut dset = duty_series.clone();
            dset.sort();
            dset.dedup();
            if dset.len() > 1 {
                let (a, b, c, _d) =
                    _macro_series_delay(&duty_series, false, false);
                duty = a;
                duty_speed = b;
                duty_loop = c;
            } else if e.tone_duty != 0 || !e.tone_duty_pts.is_empty() {
                duty = vec![e.tone_duty];
                duty_speed = 1;
                duty_loop = 255;
            }
            let mut lfsr_pts: Vec<(i64, i32)> =
                vec![(e.sample, e.tone_lfsr)];
            lfsr_pts.extend(e.tone_lfsr_pts.iter().copied());
            let lfsr_series =
                _tick_values(&lfsr_pts, e.sample, ticks, tick_samples);
            let mut lset = lfsr_series.clone();
            lset.sort();
            lset.dedup();
            if lset.len() > 1 {
                let (a, b, c, _d) =
                    _macro_series_delay(&lfsr_series, false, false);
                lfsr = a;
                lfsr_speed = b;
                lfsr_loop = c;
            } else if e.tone_lfsr != 0 || !e.tone_lfsr_pts.is_empty() {
                lfsr = vec![e.tone_lfsr];
                lfsr_speed = 1;
                lfsr_loop = 255;
            }
        } else if !e.tone_fx_pts.is_empty() {
            let npts: Vec<(i64, i32)> = e
                .tone_fx_pts
                .iter()
                .filter(|p| p.1 == FX_NOISE)
                .map(|p| (p.0, p.2))
                .collect();
            let (a, b, c) = super::util::_macro_if_moving(
                &npts,
                e.sample,
                ticks,
                tick_samples,
                super::util::_AY_MERGE_GAP as i32,
            );
            noise = a;
            noise_speed = b;
            noise_loop = c;
            let mpts: Vec<(i64, i32)> = e
                .tone_fx_pts
                .iter()
                .filter(|p| p.1 == FX_MODE)
                .map(|p| (p.0, (p.2 + 1) & 7))
                .collect();
            let (a, b, c) = super::util::_macro_if_moving(
                &mpts,
                e.sample,
                ticks,
                tick_samples,
                super::util::_AY_MERGE_GAP as i32,
            );
            mode = a;
            mode_speed = b;
            mode_loop = c;
            env_hw = e
                .tone_fx_pts
                .iter()
                .any(|p| p.1 == FX_MODE && ((p.2 + 1) & 4) != 0);
            let spts: Vec<(i64, i32)> = e
                .tone_fx_pts
                .iter()
                .filter(|p| p.1 == FX_ENV_SET)
                .map(|p| (p.0, (p.2 >> 4) & 0x0F))
                .collect();
            let (a, b, c) = super::util::_macro_if_moving(
                &spts,
                e.sample,
                ticks,
                tick_samples,
                super::util::_AY_MERGE_GAP as i32,
            );
            env_shape = a;
            env_shape_speed = b;
            env_shape_loop = c;
            let mut lo: Option<i32> = None;
            let mut hi: Option<i32> = None;
            let mut period_pts: Vec<(i64, i32)> = Vec::new();
            for &(t, cmd, val) in &e.tone_fx_pts {
                if cmd == FX_ENV_LO {
                    lo = Some(val);
                } else if cmd == FX_ENV_HI {
                    hi = Some(val);
                } else {
                    continue;
                }
                if let (Some(lo), Some(hi)) = (lo, hi) {
                    period_pts.push((t, lo | (hi << 8)));
                }
            }
            let (a, b, c) = super::util::_macro_if_moving(
                &period_pts,
                e.sample,
                ticks,
                tick_samples,
                super::util::_AY_MERGE_GAP as i32,
            );
            env_period = a;
            env_period_speed = b;
            env_period_loop = c;
        }
        if e.tone_chip == 5 && noise.is_empty() {
            // Every PCE instrument carries the noise mode explicitly.
            noise = vec![if e.tone_noise { 1 } else { 0 }];
            noise_speed = 1;
            noise_loop = 255;
        }
        if (e.tone_chip == 14 || e.tone_chip == 15) && !vol_vals.is_empty() {
            // QSound's volume macro is the raw 14-bit register level.
            vol_vals = vol_vals
                .iter()
                .map(|&v| {
                    std::cmp::min(
                        16383,
                        py_int(py_round(v as f64 * 16383.0 / 255.0)) as i32,
                    )
                })
                .collect();
        }
        let spec = ToneIns {
            wave: std::cmp::max(0, e.tone_wave),
            sample: e.tone_sample,
            sample_kind: e.tone_sample_kind.clone(),
            fds_mod: e.tone_fds_mod,
            fds_mod_depth: e.tone_fds_depth,
            fds_mod_speed: e.tone_fds_speed,
            fds_mod_pos: e.tone_fds_pos,
            pitch,
            pitch_speed,
            pitch_loop,
            pitch_delay,
            waves: wave_vals,
            wave_speed,
            wave_loop,
            vol: vol_vals,
            vol_speed,
            vol_loop,
            vol_delay,
            noise,
            noise_speed,
            noise_loop,
            mode,
            mode_speed,
            mode_loop,
            env_shape,
            env_shape_speed,
            env_shape_loop,
            env_hw,
            env_period,
            env_period_speed,
            env_period_loop,
            panl,
            panl_speed,
            panl_loop,
            panl_delay,
            panr,
            panr_speed,
            panr_loop,
            panr_delay,
            env,
            env_speed,
            env_loop,
            env_delay,
            duty,
            duty_speed,
            duty_loop,
            lfsr,
            lfsr_speed,
            lfsr_loop,
            gb_soft_env: e.gb_soft_env,
            gb_env_vol: e.gb_env_vol,
            gb_env_dir: e.gb_env_dir,
            gb_env_len: e.gb_env_len,
            gb_sound_len: e.gb_sound_len,
            gb_sound_len_inf: e.gb_sound_len_inf,
            gb_wave: e.tone_gb_wave,
            vsu_mod: e.tone_mod,
        };
        let spec0 = spec.clone();
        let chip = e.tone_chip;
        let (idx, kept) = if chip == 1 {
            let others = ay_ins.len();
            take(spec, &mut scc_map, scc_ins, others, early_cap, 1)
        } else if chip == 3 {
            let others = scc_ins.len() + ay_ins.len();
            take(spec, &mut psg_map, psg_ins, others, early_cap, 3)
        } else if chip == 4 {
            let others = scc_ins.len() + ay_ins.len() + psg_ins.len();
            take(spec, &mut nes_map, nes_ins, others, early_cap, 4)
        } else if chip == 5 {
            let others = scc_ins.len()
                + ay_ins.len()
                + psg_ins.len()
                + nes_ins.len();
            take(spec, &mut huc6280_map, huc6280_ins, others, early_cap, 5)
        } else if chip == 6 {
            let others = scc_ins.len()
                + ay_ins.len()
                + psg_ins.len()
                + nes_ins.len()
                + huc6280_ins.len();
            take(spec, &mut gb_map, gb_ins, others, ins_cap, 6)
        } else if chip == 7 {
            let others = scc_ins.len()
                + ay_ins.len()
                + psg_ins.len()
                + nes_ins.len()
                + huc6280_ins.len()
                + gb_ins.len();
            take(spec, &mut vsu_map, vsu_ins, others, ins_cap, 7)
        } else if chip == 8 {
            // The SAA amplitude nibbles are only four bits wide, so a
            // one-step near-identical merge is an audible level change.
            let others = scc_ins.len()
                + ay_ins.len()
                + psg_ins.len()
                + nes_ins.len()
                + huc6280_ins.len()
                + gb_ins.len()
                + vsu_ins.len();
            let (i, k) = super::util::_take_tone(
                spec,
                &mut saa_map,
                saa_ins,
                others,
                ins_cap as usize,
                8,
                0,
            );
            (i, k)
        } else if chip == 9 {
            let others = scc_ins.len()
                + ay_ins.len()
                + psg_ins.len()
                + nes_ins.len()
                + huc6280_ins.len()
                + gb_ins.len()
                + vsu_ins.len()
                + saa_ins.len();
            take(spec, &mut pokey_map, pokey_ins, others, ins_cap, 9)
        } else if chip == 10 {
            let others = scc_ins.len()
                + ay_ins.len()
                + psg_ins.len()
                + nes_ins.len()
                + huc6280_ins.len()
                + gb_ins.len()
                + vsu_ins.len()
                + saa_ins.len()
                + pokey_ins.len();
            take(spec, &mut mikey_map, mikey_ins, others, ins_cap, 10)
        } else if chip == 11 {
            let others = scc_ins.len()
                + ay_ins.len()
                + psg_ins.len()
                + nes_ins.len()
                + huc6280_ins.len()
                + gb_ins.len()
                + vsu_ins.len()
                + saa_ins.len()
                + pokey_ins.len()
                + mikey_ins.len();
            take(spec, &mut x1_map, x1_ins, others, ins_cap, 11)
        } else if chip == 12 {
            let others = scc_ins.len()
                + ay_ins.len()
                + psg_ins.len()
                + nes_ins.len()
                + huc6280_ins.len()
                + gb_ins.len()
                + vsu_ins.len()
                + saa_ins.len()
                + pokey_ins.len()
                + mikey_ins.len()
                + x1_ins.len();
            take(spec, &mut fds_map, fds_ins, others, ins_cap, 12)
        } else if chip == 13 {
            let others = scc_ins.len()
                + ay_ins.len()
                + psg_ins.len()
                + nes_ins.len()
                + huc6280_ins.len()
                + gb_ins.len()
                + vsu_ins.len()
                + saa_ins.len()
                + pokey_ins.len()
                + mikey_ins.len()
                + x1_ins.len()
                + fds_ins.len();
            take(
                spec,
                &mut wonderswan_map,
                wonderswan_ins,
                others,
                ins_cap,
                13,
            )
        } else if chip == 14 || chip == 15 {
            let others = scc_ins.len()
                + ay_ins.len()
                + psg_ins.len()
                + nes_ins.len()
                + huc6280_ins.len()
                + gb_ins.len()
                + vsu_ins.len()
                + saa_ins.len()
                + pokey_ins.len()
                + mikey_ins.len()
                + x1_ins.len()
                + fds_ins.len()
                + wonderswan_ins.len();
            take(
                spec,
                &mut qsound_map,
                qsound_ins,
                others,
                ins_cap,
                chip,
            )
        } else if chip == 16 {
            let others = scc_ins.len()
                + ay_ins.len()
                + psg_ins.len()
                + nes_ins.len()
                + huc6280_ins.len()
                + gb_ins.len()
                + vsu_ins.len()
                + saa_ins.len()
                + pokey_ins.len()
                + mikey_ins.len()
                + x1_ins.len()
                + fds_ins.len()
                + wonderswan_ins.len()
                + qsound_ins.len();
            take(
                spec,
                &mut k005289_map,
                k005289_ins,
                others,
                ins_cap,
                16,
            )
        } else {
            // AY runs first, so the SCC list is still empty and `others` is 0.
            take(spec, &mut ay_map, ay_ins, 0, early_cap, 2)
        };
        let changed = kept != spec0;
        bound.push((ei, idx));
        if matches!(chip, 3 | 7 | 9 | 10 | 11)
            && !e.tone_noise
            && changed
            && kept.pitch.is_empty()
            && !e.tone_pitch_pts.is_empty()
        {
            // The tone instrument list is full, so this note fell back to the
            // plain instrument. Put the period trajectory on E5 row steps.
            let mut steps: Vec<(i64, i32)> = e
                .tone_pitch_pts
                .iter()
                .map(|&(t, per)| {
                    (
                        t,
                        py_int(py_round(
                            (tbl.offset_for(
                                e.note,
                                per + e.tone_period_offset,
                            ) - base_off) as f64,
                        )) as i32,
                    )
                })
                .collect();
            steps.retain(|p| p.1 != 0);
            if !steps.is_empty() {
                e.pitch_steps = steps;
            }
        }
        if chip == 4 && !kept.pitch.is_empty() {
            // The instrument now carries the pitch trajectory; the row ramp
            // would double it.
            e.sweep.clear();
        }
        if chip == 5 {
            // When the envelope became an instrument macro, the column is
            // the macro's full-scale reference (31).
            if !kept.vol.is_empty() {
                e.vol_pts.clear();
                e.vol = if pce_peak != 0 { pce_peak } else { 31 };
                e.vol_slide = 0;
            } else {
                e.vol = column_vol;
            }
        } else if chip == 8 {
            // The SAA volume macro is normalized to its segment peak.
            e.vol_pts.clear();
            e.vol = if !kept.vol.is_empty() && saa_peak != 0 {
                saa_peak
            } else {
                column_vol
            };
            e.vol_slide = 0;
        } else if chip == 3 || (chip == 6 && e.tone_gb_wave) {
            // The SMS column is the raw 4-bit register; keep vol_pts as
            // pattern steps rather than folding them into a volume macro.
        } else if chip == 10 {
            // Lynx computes outVol = (column * macro) / 127.
            if !kept.vol.is_empty() {
                e.vol = 127;
                e.vol_pts.clear();
                e.vol_slide = 0;
            } else {
                e.vol = column_vol;
            }
        } else if chip == 11 {
            if !kept.vol.is_empty() {
                e.vol = 15;
                e.vol_pts.clear();
                e.vol_slide = 0;
            } else {
                e.vol = column_vol;
                e.vol_slide = 0;
            }
        } else if chip == 12 {
            // FDS volume column tops out at 32 and the macro is the register.
            if !kept.vol.is_empty() {
                e.vol = 32;
                e.vol_pts.clear();
                e.vol_slide = 0;
            } else {
                e.vol = column_vol;
            }
        } else if chip == 13 {
            // WonderSwan carries volume and pan together in the pan macros.
            e.vol = 15;
            e.vol_pts.clear();
            e.vol_slide = 0;
        } else if chip == 14 || chip == 15 {
            if !kept.vol.is_empty() {
                e.vol = 255;
                e.vol_pts.clear();
                e.vol_slide = 0;
            } else {
                e.vol = column_vol;
            }
        } else if !kept.vol.is_empty() {
            // The column that makes the macro the register.
            e.vol_pts.clear();
            e.vol = if chip == 5 { 31 } else { 15 };
            e.vol_slide = 0;
        } else if e.vol_slide != 0
            || (!spec0.vol.is_empty() && !series.is_empty()
                && _unit_saw(&spec0.vol))
        {
            // No volume macro on the instrument the note actually got, so the
            // column is the register. A one-level saw keeps moving one step
            // per tick from there.
            if e.vol_slide == 0 {
                e.vol = series[0];
                e.vol_slide = _slide_open(&series);
            }
            e.vol_pts = _saw_anchors(&series, e.sample, tick_samples)
                .into_iter()
                .map(|p| (p.0, p.1, Some(p.2)))
                .collect();
        } else if !spec0.vol.is_empty() {
            // The decay did not fit in the instrument list.
            e.vol = column_vol;
        } else {
            e.vol = column_vol;
            e.vol_pts.clear();
        }
    }
    drop(take);
    // Re-apply the leading-delay compression to every final tone macro
    // before grouping.
    for bucket in [
        &mut *scc_ins,
        &mut *ay_ins,
        &mut *psg_ins,
        &mut *nes_ins,
        &mut *huc6280_ins,
        &mut *gb_ins,
        &mut *vsu_ins,
        &mut *saa_ins,
        &mut *x1_ins,
        &mut *fds_ins,
        &mut *wonderswan_ins,
        &mut *qsound_ins,
        &mut *k005289_ins,
    ] {
        super::util::_compress_tone_delays(bucket);
    }
    // AY/SSG volume envelopes that differ by a constant 1 or 2 can share the
    // loudest instrument; the quieter notes ride the volume column.
    let ay_notes: Vec<(usize, usize)> = bound
        .iter()
        .filter(|&&(ei, _)| events[ei].tone_chip == 2)
        .copied()
        .collect();
    let ay_vol_map =
        super::util::_merge_ay_volume_offsets(ay_ins, &ay_notes, events);
    if ay_vol_map.iter().any(|(old, new)| old != new) {
        for i in 0..bound.len() {
            let (ei, idx) = bound[i];
            if events[ei].tone_chip == 2 {
                bound[i] = (ei, *ay_vol_map.get(&idx).unwrap_or(&idx));
            }
        }
    }
    // Merge truncated macro copies before fixing the absolute indices.
    let mut family_notes: IndexMap<i32, IndexMap<usize, Vec<(i64, i64)>>> =
        IndexMap::new();
    for &(ei, idx) in &bound {
        let event = &events[ei];
        family_notes
            .entry(event.tone_chip)
            .or_default()
            .entry(idx)
            .or_default()
            .push((
                event.sample,
                if event.end != 0 { event.end } else { track_end },
            ));
    }
    let mut tone_maps: IndexMap<i32, IndexMap<usize, usize>> = IndexMap::new();
    tone_maps.insert(
        1,
        super::util::_merge_tone_prefixes(
            scc_ins,
            family_notes.get(&1),
            tick_samples,
            use_speed,
        ),
    );
    tone_maps.insert(
        2,
        super::util::_merge_tone_prefixes(
            ay_ins,
            family_notes.get(&2),
            tick_samples,
            use_speed,
        ),
    );
    tone_maps.insert(
        3,
        super::util::_merge_tone_prefixes(
            psg_ins,
            family_notes.get(&3),
            tick_samples,
            use_speed,
        ),
    );
    tone_maps.insert(
        4,
        super::util::_merge_tone_prefixes(
            nes_ins,
            family_notes.get(&4),
            tick_samples,
            use_speed,
        ),
    );
    tone_maps.insert(
        5,
        super::util::_merge_tone_prefixes(
            huc6280_ins,
            family_notes.get(&5),
            tick_samples,
            use_speed,
        ),
    );
    tone_maps.insert(
        6,
        super::util::_merge_tone_prefixes(
            gb_ins,
            family_notes.get(&6),
            tick_samples,
            use_speed,
        ),
    );
    tone_maps.insert(
        7,
        super::util::_merge_tone_prefixes(
            vsu_ins,
            family_notes.get(&7),
            tick_samples,
            use_speed,
        ),
    );
    tone_maps.insert(
        8,
        super::util::_merge_tone_prefixes(
            saa_ins,
            family_notes.get(&8),
            tick_samples,
            use_speed,
        ),
    );
    tone_maps.insert(
        12,
        super::util::_merge_tone_prefixes(
            fds_ins,
            family_notes.get(&12),
            tick_samples,
            use_speed,
        ),
    );
    tone_maps.insert(
        13,
        super::util::_merge_tone_prefixes(
            wonderswan_ins,
            family_notes.get(&13),
            tick_samples,
            use_speed,
        ),
    );
    tone_maps.insert(
        16,
        super::util::_merge_tone_prefixes(
            k005289_ins,
            family_notes.get(&16),
            tick_samples,
            use_speed,
        ),
    );
    for i in 0..bound.len() {
        let (ei, idx) = bound[i];
        if let Some(mapping) = tone_maps.get(&events[ei].tone_chip) {
            let new_idx = *mapping.get(&idx).unwrap_or(&idx);
            bound[i] = (ei, new_idx);
        }
    }
    super::util::_loop_saa_pitch_macros(saa_ins, lfo_loop);
    let n_scc = scc_ins.len() as i32;
    let n_ay = ay_ins.len() as i32;
    let n_psg = psg_ins.len() as i32;
    let n_nes = nes_ins.len() as i32;
    let n_huc = huc6280_ins.len() as i32;
    let n_gb = gb_ins.len() as i32;
    let n_vsu = vsu_ins.len() as i32;
    let n_saa = saa_ins.len() as i32;
    let n_pokey = pokey_ins.len() as i32;
    let n_mikey = mikey_ins.len() as i32;
    let n_x1 = x1_ins.len() as i32;
    let n_fds = fds_ins.len() as i32;
    let n_ws = wonderswan_ins.len() as i32;
    for &(ei, idx) in &bound {
        let idx = idx as i32;
        let event = &mut events[ei];
        if event.tone_chip == 1 {
            event.ins = tone_ins_base + idx;
        } else if event.tone_chip == 3 {
            event.ins = tone_ins_base + n_scc + n_ay + idx;
        } else if event.tone_chip == 4 {
            event.ins = tone_ins_base + n_scc + n_ay + n_psg + idx;
        } else if event.tone_chip == 5 {
            event.ins = tone_ins_base + n_scc + n_ay + n_psg + n_nes + idx;
        } else if event.tone_chip == 6 {
            event.ins =
                tone_ins_base + n_scc + n_ay + n_psg + n_nes + n_huc + idx;
        } else if event.tone_chip == 7 {
            event.ins = tone_ins_base
                + n_scc
                + n_ay
                + n_psg
                + n_nes
                + n_huc
                + n_gb
                + idx;
        } else if event.tone_chip == 8 {
            event.ins = tone_ins_base
                + n_scc
                + n_ay
                + n_psg
                + n_nes
                + n_huc
                + n_gb
                + n_vsu
                + idx;
        } else if event.tone_chip == 9 {
            event.ins = tone_ins_base
                + n_scc
                + n_ay
                + n_psg
                + n_nes
                + n_huc
                + n_gb
                + n_vsu
                + n_saa
                + idx;
        } else if event.tone_chip == 10 {
            event.ins = tone_ins_base
                + n_scc
                + n_ay
                + n_psg
                + n_nes
                + n_huc
                + n_gb
                + n_vsu
                + n_saa
                + n_pokey
                + idx;
        } else if event.tone_chip == 11 {
            event.ins = tone_ins_base
                + n_scc
                + n_ay
                + n_psg
                + n_nes
                + n_huc
                + n_gb
                + n_vsu
                + n_saa
                + n_pokey
                + n_mikey
                + idx;
        } else if event.tone_chip == 12 {
            event.ins = tone_ins_base
                + n_scc
                + n_ay
                + n_psg
                + n_nes
                + n_huc
                + n_gb
                + n_vsu
                + n_saa
                + n_pokey
                + n_mikey
                + n_x1
                + idx;
        } else if event.tone_chip == 13 {
            event.ins = tone_ins_base
                + n_scc
                + n_ay
                + n_psg
                + n_nes
                + n_huc
                + n_gb
                + n_vsu
                + n_saa
                + n_pokey
                + n_mikey
                + n_x1
                + n_fds
                + idx;
        } else if event.tone_chip == 14 || event.tone_chip == 15 {
            event.ins = tone_ins_base
                + n_scc
                + n_ay
                + n_psg
                + n_nes
                + n_huc
                + n_gb
                + n_vsu
                + n_saa
                + n_pokey
                + n_mikey
                + n_x1
                + n_fds
                + n_ws
                + idx;
        } else if event.tone_chip == 16 {
            event.ins = tone_ins_base
                + n_scc
                + n_ay
                + n_psg
                + n_nes
                + n_huc
                + n_gb
                + n_vsu
                + n_saa
                + n_pokey
                + n_mikey
                + n_x1
                + n_fds
                + n_ws
                + qsound_ins.len() as i32
                + idx;
        } else {
            // AY-8910 (tone_chip 2) binds first, so its list starts at SCC end.
            event.ins = tone_ins_base + n_scc + idx;
        }
    }
    merged
}

enum PitchTableRef {
    Period(Arc<PeriodPitchTable>),
    Frequency(Arc<FrequencyPitchTable>),
}

impl PitchTableRef {
    fn offset_for(&self, note: i32, target: i32) -> i32 {
        match self {
            PitchTableRef::Period(t) => t.offset_for(note, target),
            PitchTableRef::Frequency(t) => t.offset_for(note, target),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cycle_finds_exact_period() {
        assert_eq!(_cycle(&[1, 2, 1, 2, 1, 2], 128), Some(2));
        assert_eq!(_cycle(&[4, 5, 6, 7], 128), None);
        assert_eq!(_cycle(&[3, 3, 3, 3], 128), Some(2));
    }

    #[test]
    fn step_compress_raises_speed_for_runs() {
        let mut series = Vec::new();
        for level in [5, 6, 7, 8, 9, 10, 11, 12] {
            for _ in 0..8 {
                series.push(level);
            }
        }
        let (out, speed) = _step_compress(&series, 32, 0.03);
        assert!(speed > 1, "speed {speed}");
        assert!(out.len() < series.len());
        assert!(out.len() <= MACRO_MAX);
    }

    #[test]
    fn unit_saw_and_fold() {
        assert!(_unit_saw(&[5, 4, 3, 2, 1]));
        assert!(!_unit_saw(&[1, 2, 3, 4, 5]));
        assert_eq!(
            _fold_saw(&[5, 4, 3, 2, 1, 5, 4, 3, 2, 1]),
            Some((vec![5, 4, 3, 2, 1], 1, 0))
        );
        assert_eq!(_fold_saw(&[1, 2, 1, 2]), None);
    }

    #[test]
    fn macro_delay_holds_leading_run() {
        let (values, speed, loop_, delay) =
            _macro_series_delay(&[7, 7, 3, 2, 1], false, false);
        assert_eq!(delay, 1);
        assert_eq!(speed, 1);
        assert_eq!(loop_, 255);
        assert_eq!(values, vec![7, 3, 2, 1]);
    }

    #[test]
    fn suggest_speed_prefers_three_to_eight_frames() {
        let frame = (VGM_RATE as f64 / 60.0) as i64;
        let keyons: Vec<i64> = (0..20).map(|i| i * 4 * frame).collect();
        assert_eq!(suggest_speed(&keyons, 60.0), 4);
        assert_eq!(suggest_speed(&keyons[..4], 60.0), 1);
    }

    #[test]
    fn classify_legato_splits_held_step() {
        let mut held = NoteEvent {
            sample: 0,
            on: true,
            note: 60,
            e5: E5_CENTER,
            end: 4000,
            sweep: vec![(1000, 64)],
            ..NoteEvent::default()
        };
        let mut small = NoteEvent {
            sample: 0,
            on: true,
            note: 60,
            e5: E5_CENTER,
            end: 4000,
            sweep: vec![(1000, 16)],
            ..NoteEvent::default()
        };
        let mut events = vec![held, small];
        let (steps, ramps) = _classify_legato(&mut events, 735.0, None);
        assert_eq!((steps, ramps), (1, 1));
        assert_eq!(events[0].pitch_steps, vec![(1000, 64)]);
        assert!(events[0].sweep.is_empty());
        assert_eq!(events[1].pitch_steps, Vec::<(i64, i32)>::new());
    }

    #[test]
    fn pce_volume_peak_splits_gate() {
        let ev = NoteEvent {
            sample: 0,
            on: true,
            note: 60,
            vol: 20,
            end: 2000,
            tone_chip: 5,
            tone_period: 1000,
            tone_wave: 0,
            vol_pts: vec![(1000, 10, None)],
            ..NoteEvent::default()
        };
        let out = _split_pce_volume_peaks(&[ev], 3_579_545, 0);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].sample, 0);
        assert_eq!(out[1].sample, 1000);
        assert_eq!(out[1].vol, 10);
    }
}

