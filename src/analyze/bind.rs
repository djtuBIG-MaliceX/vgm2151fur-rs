//! Instrument-copy binding passes for the analyzer.
//!
//! These are the post-collection passes that turn sub-row automation into
//! instrument macros and copies, sharing one copy between notes when the
//! played prefixes agree. Translated from `analyze.py` lines 5042-6362.

use std::collections::{BTreeSet, HashSet};

use indexmap::IndexMap;

use crate::fm::FMPatch;
use crate::model::NoteEvent;
use crate::parity::{py_floor_div, py_int, py_mod, py_round};
use crate::timing::place_tick_floor;
use crate::vgm::VGM_RATE;
use crate::ym2413::OPLL_SETTLE;

/// The format stores a macro length in one byte, so 255 is the longest macro
/// that can be declared. 256 wraps to 0 and the reader then never consumes the
/// data block, reporting "invalid macro code" on the bytes after it.
const MACRO_MAX: i64 = 255;

/// Key-construction operations the binding passes need from the analyzer's
/// interned-patch key type.
///
/// `state.rs` owns the concrete `PatchKey`; macro copies extend a base key
/// with a tag and payload. Deriving through the trait keeps the Python
/// equality semantics (same base plus same payload is the same copy) without
/// imposing a second key type on the analyzer.
pub trait PatchKeyOps: Clone + Eq + std::hash::Hash {
    fn _derive_key(tag: u8, base: &Self, payload: &[i64]) -> Self;
}

impl PatchKeyOps for super::state::PatchKey {
    fn _derive_key(tag: u8, base: &Self, payload: &[i64]) -> Self {
        // The leading i64::MIN cannot collide with an OPM/OPN identity tuple,
        // whose elements are register values and masks.
        let mut out: Vec<i64> = vec![i64::MIN, i64::from(tag)];
        match base {
            super::state::PatchKey::Plain(values) => {
                out.push(0);
                out.push(values.len() as i64);
                out.extend(values);
            }
            super::state::PatchKey::Ext { cid, key0 } => {
                out.push(1);
                out.push(i64::from(*cid));
                out.push(key0.len() as i64);
                out.extend(key0);
            }
        }
        out.push(payload.len() as i64);
        out.extend(payload);
        super::state::PatchKey::Plain(out)
    }
}

fn _push_ints(out: &mut Vec<i64>, values: &[i32]) {
    out.push(values.len() as i64);
    out.extend(values.iter().map(|v| i64::from(*v)));
}

fn _push_option_macro(out: &mut Vec<i64>, macro_: &Option<(Vec<i32>, i32, i32)>) {
    match macro_ {
        None => out.push(0),
        Some((values, speed, loop_)) => {
            out.push(1);
            _push_ints(out, values);
            out.push(i64::from(*speed));
            out.push(i64::from(*loop_));
        }
    }
}

fn _push_op_macro(out: &mut Vec<i64>, entries: &[(i32, Vec<i32>, i32)]) {
    out.push(entries.len() as i64);
    for (op, values, speed) in entries {
        out.push(i64::from(*op));
        _push_ints(out, values);
        out.push(i64::from(*speed));
    }
}

/// Move sample/tone instrument indices after FM instruments are appended.
pub fn _rebase_non_fm_ins(events: &mut [NoteEvent], old_count: i32, new_count: i32) {
    let delta = new_count - old_count;
    if delta == 0 {
        return;
    }
    for e in events {
        if (e.dac || e.pcm || e.tone_chip != 0) && e.ins >= old_count {
            e.ins += delta;
        }
    }
}

/// True when `a` is `b` plus a (possibly empty) extra tail.
pub fn _macro_extends(a: &[i32], b: &[i32]) -> bool {
    if a.is_empty() != b.is_empty() || a.len() < b.len() {
        return false;
    }
    &a[..b.len()] == b
}

/// Compare per-operator macros: `a` must extend `b` for every operator.
pub fn _op_macro_extends(a: &[(i32, Vec<i32>, i32)], b: &[(i32, Vec<i32>, i32)]) -> bool {
    let da: IndexMap<i32, (&[i32], i32)> =
        a.iter().map(|(op, values, speed)| (*op, (values.as_slice(), *speed))).collect();
    let db: IndexMap<i32, (&[i32], i32)> =
        b.iter().map(|(op, values, speed)| (*op, (values.as_slice(), *speed))).collect();
    if da.keys().copied().collect::<BTreeSet<i32>>()
        != db.keys().copied().collect::<BTreeSet<i32>>()
    {
        return false;
    }
    for (op, (values_a, speed_a)) in &da {
        let (values_b, speed_b) = db[op];
        if *speed_a != speed_b || !_macro_extends(values_a, values_b) {
            return false;
        }
    }
    true
}

/// Whether `a` carries every macro of `b` plus a longer tail.
pub fn _fm_macro_extends(a: &FMPatch, b: &FMPatch) -> bool {
    if !_macro_extends(&a.fms_macro, &b.fms_macro)
        || !_macro_extends(&a.ams_macro, &b.ams_macro)
        || !_macro_extends(&a.pitch_macro, &b.pitch_macro)
        || !_macro_extends(&a.wave_macro, &b.wave_macro)
        || !_macro_extends(&a.vol_macro, &b.vol_macro)
        || !_macro_extends(&a.phase_macro, &b.phase_macro)
        || !_op_macro_extends(&a.op_tl_macro, &b.op_tl_macro)
        || !_op_macro_extends(&a.op_dt_macro, &b.op_dt_macro)
        || !_op_macro_extends(&a.op_dt2_macro, &b.op_dt2_macro)
        || !_op_macro_extends(&a.op_mul_macro, &b.op_mul_macro)
    {
        return false;
    }
    for (nonempty, speed_a, speed_b) in [
        (
            !a.fms_macro.is_empty() || !b.fms_macro.is_empty(),
            a.macro_speed,
            b.macro_speed,
        ),
        (
            !a.pitch_macro.is_empty() || !b.pitch_macro.is_empty(),
            a.pitch_speed,
            b.pitch_speed,
        ),
        (
            !a.wave_macro.is_empty() || !b.wave_macro.is_empty(),
            a.wave_speed,
            b.wave_speed,
        ),
        (
            !a.vol_macro.is_empty() || !b.vol_macro.is_empty(),
            a.vol_speed,
            b.vol_speed,
        ),
        (
            !a.phase_macro.is_empty() || !b.phase_macro.is_empty(),
            a.phase_speed,
            b.phase_speed,
        ),
    ] {
        if nonempty && speed_a != speed_b {
            return false;
        }
    }
    for (nonempty, loop_a, loop_b) in [
        (
            !a.pitch_macro.is_empty() || !b.pitch_macro.is_empty(),
            a.pitch_loop,
            b.pitch_loop,
        ),
        (
            !a.wave_macro.is_empty() || !b.wave_macro.is_empty(),
            a.wave_loop,
            b.wave_loop,
        ),
        (
            !a.vol_macro.is_empty() || !b.vol_macro.is_empty(),
            a.vol_loop,
            b.vol_loop,
        ),
        (
            !a.phase_macro.is_empty() || !b.phase_macro.is_empty(),
            a.phase_loop,
            b.phase_loop,
        ),
    ] {
        if nonempty && loop_a != loop_b {
            return false;
        }
    }
    true
}

pub fn _macro_ticks(start: i64, end: i64, tick_samples: f64) -> i64 {
    if end <= start {
        return 1;
    }
    i64::max(1, ((end - start) as f64 / tick_samples).ceil() as i64)
}

/// The value a macro plays on each tick, including delay/hold/loop tails.
pub fn _macro_effective(
    values: &[i32],
    speed: i32,
    loop_: i32,
    ticks: i64,
    delay: i32,
) -> Vec<i32> {
    if values.is_empty() {
        return Vec::new();
    }
    let speed = i32::max(1, speed);
    let hold = i64::from(i32::max(0, delay)) + 1;
    let mut out: Vec<i32> = Vec::new();
    for t in 0..ticks {
        if t < hold {
            out.push(values[0]);
            continue;
        }
        let pos = 1 + py_floor_div(t - hold, i64::from(speed));
        if pos < values.len() as i64 {
            out.push(values[pos as usize]);
        } else if loop_ >= 0 && (loop_ as usize) < values.len() {
            let span = i64::max(1, values.len() as i64 - i64::from(loop_));
            out.push(values[(i64::from(loop_) + py_mod(pos - values.len() as i64, span)) as usize]);
        } else {
            out.push(*values.last().unwrap());
        }
    }
    out
}

/// Whether `a` and `b` play the same macros for `ticks` ticks.
pub fn _fm_macro_same_for(a: &FMPatch, b: &FMPatch, ticks: i64) -> bool {
    for (va, vb, speed, loop_) in [
        (
            &a.fms_macro,
            &b.fms_macro,
            1,
            255,
        ),
        (
            &a.ams_macro,
            &b.ams_macro,
            1,
            255,
        ),
        (
            &a.pitch_macro,
            &b.pitch_macro,
            b.pitch_speed,
            b.pitch_loop,
        ),
        (
            &a.wave_macro,
            &b.wave_macro,
            b.wave_speed,
            b.wave_loop,
        ),
        (
            &a.vol_macro,
            &b.vol_macro,
            b.vol_speed,
            b.vol_loop,
        ),
        (
            &a.phase_macro,
            &b.phase_macro,
            b.phase_speed,
            b.phase_loop,
        ),
    ] {
        if _macro_effective(va, speed, loop_, ticks, 0)
            != _macro_effective(vb, speed, loop_, ticks, 0)
        {
            return false;
        }
    }
    for (va, vb) in [
        (&a.op_tl_macro, &b.op_tl_macro),
        (&a.op_dt_macro, &b.op_dt_macro),
        (&a.op_dt2_macro, &b.op_dt2_macro),
        (&a.op_mul_macro, &b.op_mul_macro),
    ] {
        let da: IndexMap<i32, (&[i32], i32)> =
            va.iter().map(|(op, values, speed)| (*op, (values.as_slice(), *speed))).collect();
        let db: IndexMap<i32, (&[i32], i32)> =
            vb.iter().map(|(op, values, speed)| (*op, (values.as_slice(), *speed))).collect();
        for (op, (values_b, speed_b)) in &db {
            let Some((values_a, speed_a)) = da.get(op) else {
                return false;
            };
            if _macro_effective(values_a, *speed_a, 255, ticks, 0)
                != _macro_effective(values_b, *speed_b, 255, ticks, 0)
            {
                return false;
            }
        }
    }
    true
}

pub fn _fm_macro_size(patch: &FMPatch) -> i64 {
    let mut size: i64 = 0;
    size += patch.fms_macro.len() as i64;
    size += patch.ams_macro.len() as i64;
    size += patch.pitch_macro.len() as i64;
    size += patch.wave_macro.len() as i64;
    size += patch.vol_macro.len() as i64;
    size += patch.phase_macro.len() as i64;
    for entries in [
        &patch.op_tl_macro,
        &patch.op_dt_macro,
        &patch.op_dt2_macro,
        &patch.op_mul_macro,
    ] {
        size += entries.iter().map(|(_op, values, _speed)| values.len() as i64).sum::<i64>();
    }
    size
}

/// Map each index to the longest compatible extension in its group.
///
/// Prefix chains are resolved to the final (largest) member. Equal-size
/// identical macros pick the lower index so the mapping is stable.
pub fn _prefix_targets(
    indexes: &[usize],
    size_of: impl Fn(usize) -> i64,
    prefix_of: impl Fn(usize, usize) -> bool,
) -> IndexMap<usize, usize> {
    let mut target: IndexMap<usize, usize> = indexes.iter().map(|i| (*i, *i)).collect();
    for i in indexes {
        let mut best: Option<usize> = None;
        for j in indexes {
            if j == i || !prefix_of(*j, *i) {
                continue;
            }
            let sj = size_of(*j);
            let si = size_of(*i);
            if sj < si || (sj == si && j > i) {
                continue;
            }
            if best.is_none()
                || sj > size_of(best.unwrap())
                || (sj == size_of(best.unwrap()) && j < best.as_ref().unwrap())
            {
                best = Some(*j);
            }
        }
        if let Some(best) = best {
            target.insert(*i, best);
        }
    }
    for i in indexes {
        let mut cur = *i;
        let mut seen: HashSet<usize> = HashSet::new();
        while target[&cur] != cur && !seen.contains(&cur) {
            seen.insert(cur);
            cur = target[&cur];
        }
        target.insert(*i, cur);
    }
    target
}

// ---------------------------------------------------------------------------
// Shared macro helpers from util.rs (ported there; thin wrappers keep the
// Python keyword-only defaults in one place).
// ---------------------------------------------------------------------------

fn _mseries(series: &[i32], tail_loop: bool) -> (Vec<i32>, i32, i32) {
    super::tone::_macro_series(series, tail_loop, 1, 0, 16)
}

fn _tvals(pts: &[(i64, i32)], start: i64, ticks: i64, tick_samples: f64) -> Vec<i32> {
    super::tone::_tick_values(pts, start, ticks, tick_samples)
}

/// `_tick_values` for a float-valued point list (the OPN pitch trajectory).
fn _tick_values_float(
    pts: &[(i64, f64)],
    start: i64,
    ticks: i64,
    tick_samples: f64,
) -> Vec<f64> {
    let mut out: Vec<f64> = Vec::new();
    let mut i = 0usize;
    let mut cur = if pts.is_empty() { 0.0 } else { pts[0].1 };
    for k in 0..ticks {
        let t = start as f64 + k as f64 * tick_samples;
        while i < pts.len() && pts[i].0 as f64 <= t {
            cur = pts[i].1;
            i += 1;
        }
        out.push(cur);
    }
    out
}

/// Pitch in 1/64 semitones; only differences are meaningful.
///
/// Ported here because `ym2203.py` has no Rust module yet; identical to the
/// Python function the analysis path normally imports.
pub fn opn_pitch_units(block: i32, fnum: i32) -> f64 {
    let fnum = if fnum <= 0 { 1 } else { fnum };
    768.0 * ((fnum as f64).log2() + block as f64)
}

/// Octave of the loudest audible carrier, relative to the channel f-num.
pub fn opn_carrier_octave(patch: &FMPatch) -> i32 {
    if !patch.opn {
        return 0;
    }
    let mut best_tl: i32 = 0x80;
    let mut best_octave: Option<i32> = None;
    for i in crate::fm::carriers_for(patch.alg & 7, true, false, false) {
        let op = &patch.ops[i as usize];
        let tl = i32::from(op.tl & 0x7F);
        if tl >= 0x7F {
            continue;
        }
        let mult = op.dt_mul & 0x0F;
        let ratio = if mult == 0 { 0.5 } else { f64::from(mult) };
        let octave = if ratio == 0.5 || ratio == 1.0 || ratio == 2.0 || ratio == 4.0 || ratio == 8.0
        {
            Some(py_int(py_round(ratio.log2())) as i32)
        } else {
            None
        };
        if tl < best_tl {
            best_tl = tl;
            best_octave = octave;
        } else if tl == best_tl && octave.is_some() {
            if best_octave.is_none() || octave.unwrap() > best_octave.unwrap() {
                best_octave = octave;
            }
        }
    }
    best_octave.unwrap_or(0)
}

/// Turn sub-row OPLL preset/level/retrigger writes into instrument macros.
#[allow(clippy::too_many_arguments)]
pub fn _bind_opll_macros<K: PatchKeyOps>(
    events: &mut [NoteEvent],
    patches: &mut IndexMap<K, FMPatch>,
    patch_order: &mut Vec<K>,
    channel_fx: &mut Vec<(i64, i32, i32, i32)>,
    hz: f64,
    speed: i32,
    t0: i64,
    total_samples: i64,
    cap: i32,
) -> i32 {
    if events.is_empty() || hz <= 0.0 {
        return 0;
    }
    let tick = VGM_RATE as f64 / hz;
    let row_samples = tick * i64::from(i32::max(1, speed)) as f64;
    let mut by_ch: IndexMap<i32, Vec<usize>> = IndexMap::new();
    for (idx, ev) in events.iter().enumerate() {
        if ev.on {
            by_ch.entry(ev.ch).or_default().push(idx);
        }
    }
    let mut next_on: IndexMap<usize, i64> = IndexMap::new();
    for notes in by_ch.values() {
        for (i, &idx) in notes.iter().enumerate() {
            next_on.insert(
                idx,
                if i + 1 < notes.len() {
                    events[notes[i + 1]].sample
                } else if total_samples != 0 {
                    total_samples
                } else {
                    events[idx].sample
                },
            );
        }
    }
    let mut made = 0i32;
    let mut cache: IndexMap<K, usize> = IndexMap::new();
    for idx in 0..events.len() {
        if !events[idx].on {
            continue;
        }
        if events[idx].tone_chip != 0 || events[idx].pcm || events[idx].dac {
            continue;
        }
        if events[idx].opll_preset_pts.is_empty()
            && events[idx].opll_phase_pts.is_empty()
            && events[idx].vol_pts.is_empty()
        {
            continue;
        }
        if !(0 <= events[idx].ins && (events[idx].ins as usize) < patch_order.len()) {
            continue;
        }
        let base_key = patch_order[events[idx].ins as usize].clone();
        let Some(base) = patches.get(&base_key).cloned() else {
            continue;
        };
        if !base.opll {
            continue;
        }
        let mut times: Vec<i64> =
            events[idx].opll_preset_pts.iter().map(|(t, _p)| *t).collect();
        times.extend(events[idx].opll_phase_pts.iter().copied());
        times.extend(events[idx].vol_pts.iter().map(|(t, _v, _pan)| *t));
        if times.is_empty() {
            continue;
        }
        let mut rows: BTreeSet<i64> = BTreeSet::new();
        for t in &times {
            rows.insert(place_tick_floor(*t, t0, row_samples, speed, tick, true, None).0 as i64);
        }
        let note_row =
            place_tick_floor(events[idx].sample, t0, row_samples, speed, tick, true, None).0
                as i64;
        if rows.len() >= times.len() && !rows.contains(&note_row) {
            continue;
        }
        let mut end = if events[idx].end != 0 {
            events[idx].end
        } else {
            *next_on
                .get(&idx)
                .unwrap_or(&(if total_samples != 0 { total_samples } else { events[idx].sample }))
        };
        if !events[idx].vol_pts.is_empty() {
            end = end.max(events[idx].vol_pts.last().unwrap().0);
        }
        let sample = events[idx].sample;
        let ticks = i64::max(
            1,
            i64::min(py_int((end - sample) as f64 / tick) + 1, 65025),
        );
        let macro_ticks = ticks.min(255);
        let macro_end = sample as f64 + macro_ticks as f64 * tick;
        let mut wave: Option<(Vec<i32>, i32, i32)> = None;
        if !events[idx].opll_preset_pts.is_empty() {
            let mut pts: Vec<(i64, i32)> = vec![(sample, base.preset)];
            pts.extend(events[idx].opll_preset_pts.iter().copied());
            pts.sort_by(|a, b| a.cmp(b));
            let series = _tvals(&pts, sample, macro_ticks, tick);
            if series.iter().any(|v| *v != series[0]) {
                wave = Some(_mseries(&series, false));
            }
        }
        let mut volm: Option<(Vec<i32>, i32, i32)> = None;
        if !events[idx].vol_pts.is_empty() {
            let mut pts: Vec<(i64, i32)> = vec![(sample, i32::max(0, events[idx].vol))];
            pts.extend(
                events[idx]
                    .vol_pts
                    .iter()
                    .map(|(t, v, _pan)| (*t, i32::max(0, *v))),
            );
            pts.sort_by(|a, b| a.cmp(b));
            let series = _tvals(&pts, sample, macro_ticks, tick);
            if series.iter().any(|v| *v != series[0]) {
                volm = Some(_mseries(&series, false));
            }
        }
        let mut phase: Option<(Vec<i32>, i32, i32)> = None;
        if !events[idx].opll_phase_pts.is_empty() {
            let mut series = vec![0i32; macro_ticks as usize];
            let mut hit = false;
            for t in &events[idx].opll_phase_pts {
                let k = py_int(py_round((*t - sample) as f64 / tick));
                if 0 < k && k < macro_ticks {
                    series[k as usize] = 1;
                    hit = true;
                }
            }
            if hit {
                phase = Some(_mseries(&series, false));
            }
        }
        if wave.is_none() && volm.is_none() && phase.is_none() {
            continue;
        }
        if patch_order.len() as i32 >= cap {
            continue;
        }
        let mut payload: Vec<i64> = Vec::new();
        _push_option_macro(&mut payload, &wave);
        _push_option_macro(&mut payload, &volm);
        _push_option_macro(&mut payload, &phase);
        let key = K::_derive_key(1, &base_key, &payload);
        let idx_new = match cache.get(&key) {
            Some(i) => *i,
            None => {
                let mut copy = base.clone();
                copy.wave_macro = wave.as_ref().map(|m| m.0.clone()).unwrap_or_default();
                copy.wave_speed = wave.as_ref().map(|m| m.1).unwrap_or(1);
                copy.wave_loop = wave.as_ref().map(|m| m.2).unwrap_or(255);
                copy.vol_macro = volm.as_ref().map(|m| m.0.clone()).unwrap_or_default();
                copy.vol_speed = volm.as_ref().map(|m| m.1).unwrap_or(1);
                copy.vol_loop = volm.as_ref().map(|m| m.2).unwrap_or(255);
                copy.phase_macro = phase.as_ref().map(|m| m.0.clone()).unwrap_or_default();
                copy.phase_speed = phase.as_ref().map(|m| m.1).unwrap_or(1);
                copy.phase_loop = phase.as_ref().map(|m| m.2).unwrap_or(255);
                copy.uses = 1;
                copy.name = format!("{}+T", base.name);
                copy.base_ins = events[idx].ins;
                patches.insert(key.clone(), copy);
                patch_order.push(key.clone());
                let i = patch_order.len() - 1;
                cache.insert(key, i);
                i
            }
        };
        events[idx].ins = idx_new as i32;
        if volm.is_some() {
            events[idx].vol = 15;
            events[idx].vol_pts.retain(|p| p.0 as f64 >= macro_end);
        }
        if wave.is_some() {
            let ch = events[idx].ch;
            let ev_sample = events[idx].sample;
            channel_fx.retain(|fx| {
                !(fx.1 == ch
                    && fx.2 == 0x10
                    && ev_sample as f64 <= fx.0 as f64
                    && (fx.0 as f64) < macro_end)
            });
        }
        events[idx].opll_preset_pts.clear();
        events[idx].opll_phase_pts.clear();
        made += 1;
    }
    made
}

/// Turn YM2203/YM2612 f-num trajectories into instrument pitch macros.
#[allow(clippy::too_many_arguments)]
pub fn _bind_fm_pitch<K: PatchKeyOps>(
    events: &mut [NoteEvent],
    patches: &mut IndexMap<K, FMPatch>,
    patch_order: &mut Vec<K>,
    clock: f64,
    hz: f64,
    total_samples: i64,
    cap: i32,
    allow_steps: bool,
    row_ticks: i32,
    quant: i32,
    opll: bool,
) -> (i32, i32) {
    let moving = events.iter().any(|e| e.on && !e.fm_pitch_pts.is_empty());
    if !moving {
        return (0, 0);
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
    let mut by_ch: IndexMap<i32, Vec<usize>> = IndexMap::new();
    for (idx, e) in events.iter().enumerate() {
        if e.on {
            by_ch.entry(e.ch).or_default().push(idx);
        }
    }
    let mut made = 0i32;
    let mut fallbacks = 0i32;
    let mut idx_map: IndexMap<K, usize> = IndexMap::new();
    for notes in by_ch.values() {
        for i in 0..notes.len() {
            let e_idx = notes[i];
            if events[e_idx].fm_pitch_pts.is_empty() {
                continue;
            }
            let next_start = if i + 1 < notes.len() {
                events[notes[i + 1]].sample
            } else {
                track_end
            };
            let mut end = if events[e_idx].end != 0 {
                events[e_idx].end
            } else {
                next_start
            };
            let gate_end = end;
            let mut pts: Vec<(i64, i32, i32)> = events[e_idx].fm_pitch_pts.clone();
            if i + 1 < notes.len() {
                let cutoff = next_start - OPLL_SETTLE;
                let kept: Vec<(i64, i32, i32)> =
                    pts.iter().filter(|p| p.0 < cutoff).copied().collect();
                if !kept.is_empty() {
                    pts = kept;
                }
            }
            if events[e_idx].end != 0 && pts.len() > 1 {
                let first = pts[0];
                let mut kept = vec![first];
                kept.extend(pts[1..].iter().filter(|p| {
                    (p.0 - events[e_idx].end).abs() > 2
                }).copied());
                pts = kept;
            }
            if !pts.is_empty() {
                end = end.max(pts.last().unwrap().0);
            }
            let sample = events[e_idx].sample;
            let ticks = i64::max(
                1,
                i64::min(py_int((end - sample) as f64 / tick_samples) + 1, 65025),
            );
            let ref_units = opn_pitch_units(pts[0].1, pts[0].2);
            if opll && pts.len() <= 2 && pts.last().unwrap().0 < gate_end {
                let last = *pts.last().unwrap();
                let (note, e5) = crate::opl::opll_note_for(last.1, last.2, clock);
                events[e_idx].note = note;
                events[e_idx].e5 = e5;
                continue;
            }
            let row_ok = super::emit::_row_steppable(&pts, sample, tick_samples, row_ticks);
            if (allow_steps
                && (pts.len() <= 4
                    || super::emit::_steppable_pitch(&pts, 8, 2.0, 400.0)
                    || (ticks > MACRO_MAX && row_ok)))
                || (opll && row_ok)
            {
                let deltas: Vec<(i64, i32)> = pts[1..]
                    .iter()
                    .map(|(t, b, f)| {
                        (*t, py_int(py_round(opn_pitch_units(*b, *f) - ref_units)) as i32)
                    })
                    .collect();
                if !deltas.is_empty() && deltas.iter().any(|(_t, d)| *d != 0) {
                    events[e_idx].pitch_steps = deltas;
                    continue;
                }
            }
            let mut steps: Vec<(i64, f64)> = vec![(sample, 0.0)];
            steps.extend(
                pts.iter()
                    .map(|(t, b, f)| (*t, opn_pitch_units(*b, *f) - ref_units)),
            );
            let series = _tick_values_float(&steps, sample, ticks, tick_samples);
            let mut vals: Vec<i32> = series
                .iter()
                .map(|d| {
                    i32::max(
                        -32768,
                        i32::min(32767, py_int(py_round(2.0 * d)) as i32),
                    )
                })
                .collect();
            if quant > 1 {
                vals = vals
                    .iter()
                    .map(|v| py_int(py_round(*v as f64 / quant as f64)) as i32 * quant)
                    .collect();
            }
            if vals.windows(2).all(|w| w[0] == w[1]) {
                continue;
            }
            if patch_order.len() as i32 >= cap {
                let deltas: Vec<(i64, i32)> = pts[1..]
                    .iter()
                    .map(|(t, b, f)| {
                        (*t, py_int(py_round(opn_pitch_units(*b, *f) - ref_units)) as i32)
                    })
                    .collect();
                if !deltas.is_empty() {
                    let mut spread: Vec<i32> = vec![0];
                    spread.extend(deltas.iter().map(|(_t, d)| *d));
                    if spread.iter().max().unwrap() - spread.iter().min().unwrap() >= 4 {
                        events[e_idx].sweep = deltas;
                        fallbacks += 1;
                    }
                }
                continue;
            }
            let (macro_, speed, loop_) = _mseries(&vals, true);
            if macro_.is_empty() {
                continue;
            }
            let base_key = patch_order[events[e_idx].ins as usize].clone();
            let mut payload: Vec<i64> = Vec::new();
            _push_ints(&mut payload, &macro_);
            payload.push(i64::from(speed));
            payload.push(i64::from(loop_));
            let key = K::_derive_key(2, &base_key, &payload);
            let idx = match idx_map.get(&key) {
                Some(i) => {
                    patches
                        .get_mut(&patch_order[*i])
                        .unwrap()
                        .uses += 1;
                    *i
                }
                None => {
                    let base = patches.get(&base_key).unwrap().clone();
                    let mut copy = base.clone();
                    copy.pitch_macro = macro_;
                    copy.pitch_speed = speed;
                    copy.pitch_loop = loop_;
                    copy.uses = 1;
                    copy.name = format!("{}+P", base.name);
                    copy.base_ins = events[e_idx].ins;
                    patches.insert(key.clone(), copy);
                    patch_order.push(key.clone());
                    let i = patch_order.len() - 1;
                    idx_map.insert(key, i);
                    i
                }
            };
            events[e_idx].ins = idx as i32;
            made += 1;
        }
    }
    (made, fallbacks)
}

/// Turn a scripted modulator level into row effects.
pub fn _bind_op_macros<K: PatchKeyOps>(
    events: &mut [NoteEvent],
    patches: &mut IndexMap<K, FMPatch>,
    patch_order: &mut Vec<K>,
    tick_samples: f64,
    cap: i32,
) -> (i32, i32) {
    let mut made = 0i32;
    let mut fallbacks = 0i32;
    let mut idx_map: IndexMap<K, usize> = IndexMap::new();
    if tick_samples <= 0.0 {
        return (0, 0);
    }
    for idx in 0..events.len() {
        if !events[idx].on
            || events[idx].opl_op_tl.is_empty()
            || events[idx].drum
            || events[idx].pcm
        {
            continue;
        }
        if !(0 <= events[idx].ins && (events[idx].ins as usize) < patch_order.len()) {
            continue;
        }
        let base_key = patch_order[events[idx].ins as usize].clone();
        let Some(base) = patches.get(&base_key).cloned() else {
            continue;
        };
        let carriers: HashSet<i32> = base.carriers().into_iter().collect();
        if events[idx]
            .opl_op_tl
            .iter()
            .any(|(oi, pts)| !carriers.contains(oi) && pts.len() > 2)
        {
            let mut entries: Vec<(i32, Vec<i32>, i32)> = Vec::new();
            for (oi, pts) in &events[idx].opl_op_tl {
                if carriers.contains(oi) || pts.is_empty() {
                    continue;
                }
                let last_s = pts.last().unwrap().0;
                let span_end = if events[idx].end != 0 && events[idx].end > last_s {
                    events[idx].end
                } else {
                    last_s
                };
                let span_ticks = f64::max(
                    1.0,
                    (span_end - events[idx].sample) as f64 / tick_samples,
                );
                let speed = i32::max(
                    1,
                    i32::min(254, (span_ticks / 240.0).floor() as i32 + 1),
                );
                let mut marks: Vec<(i64, i32)> = Vec::new();
                for (s, val) in pts {
                    let slot = i64::max(
                        0,
                        py_floor_div(
                            ((*s - events[idx].sample) as f64 / tick_samples).floor()
                                as i64,
                            i64::from(speed),
                        ),
                    );
                    if slot > 254 {
                        break;
                    }
                    if !marks.is_empty() && marks.last().unwrap().0 == slot {
                        *marks.last_mut().unwrap() = (slot, *val);
                    } else {
                        marks.push((slot, *val));
                    }
                }
                if marks.is_empty() {
                    continue;
                }
                let start = i32::from(base.ops[*oi as usize].tl & 0x3F);
                let mut values: Vec<i32> = Vec::new();
                let mut mi = 0usize;
                let mut cur = start;
                for slot in 0..=marks.last().unwrap().0 {
                    while mi < marks.len() && marks[mi].0 <= slot {
                        cur = marks[mi].1;
                        mi += 1;
                    }
                    values.push(cur);
                }
                if values.iter().all(|v| *v == values[0]) && values[0] == start {
                    continue;
                }
                entries.push((*oi, values, speed));
            }
            if entries.is_empty() {
                continue;
            }
            let mut payload: Vec<i64> = Vec::new();
            _push_op_macro(&mut payload, &entries);
            let key = K::_derive_key(3, &base_key, &payload);
            let idx_new = match idx_map.get(&key) {
                Some(i) => {
                    patches.get_mut(&patch_order[*i]).unwrap().uses += 1;
                    *i
                }
                None => {
                    if patch_order.len() as i32 >= cap {
                        fallbacks += 1;
                        continue;
                    }
                    let mut copy = base.clone();
                    copy.op_tl_macro = entries;
                    copy.uses = 1;
                    copy.name = format!("{}+lvl", base.name);
                    copy.base_ins = events[idx].ins;
                    patches.insert(key.clone(), copy);
                    patch_order.push(key.clone());
                    let i = patch_order.len() - 1;
                    idx_map.insert(key, i);
                    i
                }
            };
            events[idx].ins = idx_new as i32;
            made += 1;
            continue;
        }

        let mut fx: Vec<(i64, i32, i32)> = Vec::new();
        for (oi, pts) in &events[idx].opl_op_tl {
            if carriers.contains(oi) || pts.is_empty() {
                continue;
            }
            let baseline = i32::from(base.ops[*oi as usize].tl & 0x3F);
            for (sample, value) in pts {
                let value = *value & 0x3F;
                if value == baseline {
                    continue;
                }
                let item = (*sample, *oi, value);
                if !fx.is_empty() && fx.last().unwrap().0 == *sample && fx.last().unwrap().1 == *oi
                {
                    *fx.last_mut().unwrap() = item;
                } else {
                    fx.push(item);
                }
            }
        }
        if fx.is_empty() {
            continue;
        }
        events[idx].op_tl_fx = fx;
        made += 1;
    }
    (made, fallbacks)
}

/// Per-note PMS/AMS macro when the instrument's common ramp does not fit.
pub fn _bind_pms_macros<K: PatchKeyOps>(
    events: &mut [NoteEvent],
    patches: &mut IndexMap<K, FMPatch>,
    patch_order: &mut Vec<K>,
    hz: f64,
    total_samples: i64,
    cap: i32,
) -> (i32, i32) {
    let mut made = 0i32;
    let mut fallbacks = 0i32;
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
    let mut by_ch: IndexMap<i32, Vec<usize>> = IndexMap::new();
    for (idx, e) in events.iter().enumerate() {
        if e.on {
            by_ch.entry(e.ch).or_default().push(idx);
        }
    }
    let mut idx_map: IndexMap<K, usize> = IndexMap::new();
    let ch_notes: Vec<Vec<usize>> = by_ch.values().cloned().collect();
    let mut work: Vec<(usize, usize, usize)> = Vec::new();
    for (ci, notes) in ch_notes.iter().enumerate() {
        for (i, &e_idx) in notes.iter().enumerate() {
            let e = &events[e_idx];
            if e.ins >= 0
                && !e.pcm
                && !e.dac
                && e.tone_chip == 0
                && !e.opm_pms_pts.is_empty()
            {
                work.push((ci, i, e_idx));
            }
        }
    }
    work.sort_by_key(|item| events[item.2].opm_pms_pts.len() > 1);
    for (ci, i, e_idx) in work {
        let notes = &ch_notes[ci];
        let pts: Vec<(i64, i32, i32)> = events[e_idx].opm_pms_pts.clone();
        let pms_vals;
        let ams_vals;
        let speed;
        if pts.len() <= 1 {
            pms_vals = vec![if pts.is_empty() { 0 } else { pts[0].1 }];
            ams_vals = vec![if pts.is_empty() { 0 } else { pts[0].2 }];
            speed = 1i32;
        } else {
            let end = if events[e_idx].end != 0 {
                events[e_idx].end
            } else if i + 1 < notes.len() {
                events[notes[i + 1]].sample
            } else {
                track_end
            };
            let span = i64::max(1, end - events[e_idx].sample);
            let mut speed_i = i32::max(
                1,
                i32::min(
                    255,
                    (span as f64 / tick_samples / 255.0).ceil() as i32,
                ),
            );
            let mut pms_series: Vec<i32>;
            let mut ams_series: Vec<i32>;
            loop {
                let steps = i64::max(
                    1,
                    i64::min(
                        py_int(
                            (end - events[e_idx].sample) as f64
                                / (f64::from(speed_i) * tick_samples),
                        ) + 1,
                        65025,
                    ),
                );
                pms_series = Vec::new();
                ams_series = Vec::new();
                let mut j = 0usize;
                let (mut cur_p, mut cur_a) = (pts[0].1, pts[0].2);
                for k in 0..steps {
                    let t = events[e_idx].sample as f64
                        + k as f64 * f64::from(speed_i) * tick_samples;
                    while j < pts.len() && pts[j].0 as f64 <= t {
                        cur_p = pts[j].1;
                        cur_a = pts[j].2;
                        j += 1;
                    }
                    pms_series.push(cur_p);
                    ams_series.push(cur_a);
                }
                while pms_series.len() > 1
                    && pms_series[pms_series.len() - 1] == pms_series[pms_series.len() - 2]
                    && ams_series[ams_series.len() - 1] == ams_series[ams_series.len() - 2]
                {
                    pms_series.pop();
                    ams_series.pop();
                }
                if pms_series.len() <= 255 || speed_i >= 255 {
                    break;
                }
                speed_i = i32::min(255, speed_i * 2);
            }
            let single_p = pms_series.iter().all(|v| *v == pms_series[0]);
            let single_a = ams_series.iter().all(|v| *v == ams_series[0]);
            if single_p && single_a {
                pms_series.truncate(1);
                ams_series.truncate(1);
                speed_i = 1;
            }
            pms_vals = pms_series;
            ams_vals = ams_series;
            speed = speed_i;
        }
        let base_key = patch_order[events[e_idx].ins as usize].clone();
        let Some(base) = patches.get(&base_key).cloned() else {
            continue;
        };
        if base.fms_macro.is_empty() {
            if pms_vals.len() == 1
                && ams_vals.len() == 1
                && pms_vals[0] == base.pms
                && ams_vals[0] == base.ams
            {
                continue;
            }
        } else if (pms_vals.clone(), ams_vals.clone(), speed)
            == (base.fms_macro.clone(), base.ams_macro.clone(), base.macro_speed)
        {
            continue;
        }
        let mut payload: Vec<i64> = Vec::new();
        _push_ints(&mut payload, &pms_vals);
        _push_ints(&mut payload, &ams_vals);
        payload.push(i64::from(speed));
        let key = K::_derive_key(4, &base_key, &payload);
        let idx = match idx_map.get(&key) {
            Some(i) => {
                patches.get_mut(&patch_order[*i]).unwrap().uses += 1;
                *i
            }
            None => {
                if patch_order.len() as i32 >= cap {
                    fallbacks += 1;
                    continue;
                }
                let mut copy = base.clone();
                copy.fms_macro = pms_vals;
                copy.ams_macro = ams_vals;
                copy.macro_speed = speed;
                copy.uses = 1;
                copy.name = format!("{}+pms", base.name);
                copy.base_ins = events[e_idx].ins;
                patches.insert(key.clone(), copy);
                patch_order.push(key.clone());
                let i = patch_order.len() - 1;
                idx_map.insert(key, i);
                i
            }
        };
        events[e_idx].ins = idx as i32;
        made += 1;
    }
    (made, fallbacks)
}

/// Share the longest left-to-right-compatible macro between instruments.
pub fn _merge_fm_prefix_macros(
    events: &mut [NoteEvent],
    fm_patches: &mut Vec<FMPatch>,
    tick_samples: f64,
) -> i32 {
    let old_count = fm_patches.len();
    if old_count < 2 {
        return 0;
    }
    let mut notes_by_ins: IndexMap<i32, Vec<(i64, i64)>> = IndexMap::new();
    for e in events.iter() {
        if e.on && 0 <= e.ins && (e.ins as usize) < old_count {
            notes_by_ins.entry(e.ins).or_default().push((e.sample, e.end));
        }
    }
    let mut groups: IndexMap<crate::fm::FmIdentity, Vec<usize>> = IndexMap::new();
    for (i, patch) in fm_patches.iter().enumerate() {
        groups.entry(patch.identity()).or_default().push(i);
    }

    let extends = |j: usize, i: usize| -> bool {
        if !_fm_macro_extends(&fm_patches[j], &fm_patches[i]) {
            return false;
        }
        if let Some(notes) = notes_by_ins.get(&(i as i32)) {
            for (start, end) in notes {
                if *end <= 0 {
                    return false;
                }
                let ticks = _macro_ticks(*start, *end, tick_samples);
                if !_fm_macro_same_for(&fm_patches[j], &fm_patches[i], ticks) {
                    return false;
                }
            }
        }
        true
    };

    let mut target: IndexMap<usize, usize> = (0..old_count).map(|i| (i, i)).collect();
    for indexes in groups.values() {
        if indexes.len() > 1
            && indexes
                .iter()
                .any(|i| _fm_macro_size(&fm_patches[*i]) > 0)
        {
            let mapping = _prefix_targets(
                indexes,
                |i| _fm_macro_size(&fm_patches[i]),
                &extends,
            );
            for (k, v) in mapping {
                target.insert(k, v);
            }
        }
    }
    if (0..old_count).all(|i| target[&i] == i) {
        return 0;
    }
    let mut new_index: IndexMap<usize, usize> = IndexMap::new();
    let mut kept: Vec<FMPatch> = Vec::new();
    for i in 0..old_count {
        if target[&i] == i {
            new_index.insert(i, kept.len());
            kept.push(fm_patches[i].clone());
        }
    }
    for i in 0..old_count {
        new_index.insert(i, new_index[&target[&i]]);
    }
    let mut merged_patches: Vec<FMPatch> = Vec::new();
    for (idx, patch) in kept.iter().enumerate() {
        let mut base: i32 = if patch.base_ins >= 0 {
            new_index
                .get(&(patch.base_ins as usize))
                .map(|v| *v as i32)
                .unwrap_or(-1)
        } else {
            -1
        };
        if base == idx as i32 {
            base = -1; // the copy's base merged into this slot
        }
        let mut next = patch.clone();
        next.base_ins = base;
        merged_patches.push(next);
    }
    *fm_patches = merged_patches;
    for e in events.iter_mut() {
        if 0 <= e.ins && (e.ins as usize) < old_count {
            if let Some(&mapped) = new_index.get(&(e.ins as usize)) {
                e.ins = mapped as i32;
            }
        }
    }
    _rebase_non_fm_ins(events, old_count as i32, fm_patches.len() as i32);
    (old_count - fm_patches.len()) as i32
}

/// Fold trailing FM macro copies back onto their base voices.
pub fn _trim_fm_macro_copies(
    events: &mut [NoteEvent],
    fm_patches: &mut Vec<FMPatch>,
    limit: usize,
) -> i32 {
    let mut removed: IndexMap<usize, usize> = IndexMap::new();
    while fm_patches.len() > usize::max(1, limit) {
        let idx = fm_patches.len() - 1;
        let patch = &fm_patches[idx];
        let base = patch.base_ins;
        if base < 0 || base as usize >= idx {
            break;
        }
        removed.insert(idx, base as usize);
        fm_patches.pop();
    }
    if removed.is_empty() {
        return 0;
    }
    for idx in removed.keys().copied().collect::<Vec<usize>>() {
        let mut base = removed[&idx];
        let mut seen: HashSet<usize> = HashSet::new();
        while removed.contains_key(&base) && !seen.contains(&base) {
            seen.insert(base);
            base = removed[&base];
        }
        removed.insert(idx, base);
    }
    for e in events.iter_mut() {
        if e.ins >= 0 {
            if let Some(&mapped) = removed.get(&(e.ins as usize)) {
                e.ins = mapped as i32;
            }
        }
    }
    removed.len() as i32
}

/// Fold excess base FM timbres onto an earlier patch.
pub fn _trim_fm_base_patches(
    events: &mut [NoteEvent],
    fm_patches: &mut Vec<FMPatch>,
    limit: usize,
) -> i32 {
    let mut removed: IndexMap<usize, usize> = IndexMap::new();
    while fm_patches.len() > usize::max(1, limit) {
        let idx = fm_patches.len() - 1;
        let alg = fm_patches[idx].alg & 7;
        let mut base = 0usize;
        for j in 0..idx {
            if (fm_patches[j].alg & 7) == alg {
                base = j;
                break;
            }
        }
        removed.insert(idx, base);
        fm_patches.pop();
    }
    if removed.is_empty() {
        return 0;
    }
    for e in events.iter_mut() {
        if e.ins >= 0 {
            if let Some(&mapped) = removed.get(&(e.ins as usize)) {
                e.ins = mapped as i32;
            }
        }
    }
    removed.len() as i32
}

/// Relabel notes at their loudest carrier's octave, either FM family.
pub fn _apply_opn_carrier_octave(
    events: &mut [NoteEvent],
    fm_patches: &mut Vec<FMPatch>,
    skip_channels: Option<&HashSet<i32>>,
) -> i32 {
    let mut by_ins: IndexMap<i32, Vec<usize>> = IndexMap::new();
    let mut skipped_ins: HashSet<i32> = HashSet::new();
    for (idx, e) in events.iter().enumerate() {
        if !e.on || e.pcm || e.dac || e.tone_chip != 0 {
            continue;
        }
        if let Some(skip) = skip_channels {
            if skip.contains(&e.ch) {
                if 0 <= e.ins && (e.ins as usize) < fm_patches.len() {
                    skipped_ins.insert(e.ins);
                }
                continue;
            }
        }
        if 0 <= e.ins && (e.ins as usize) < fm_patches.len() {
            by_ins.entry(e.ins).or_default().push(idx);
        }
    }
    let mut shifted = 0i32;
    let ins_list: Vec<i32> = by_ins.keys().copied().collect();
    for ins_key in ins_list {
        let notes = by_ins[&ins_key].clone();
        let mut patch = fm_patches[ins_key as usize].clone();
        let octave = if patch.opl {
            crate::opl::opl_carrier_octave(&patch)
        } else {
            opn_carrier_octave(&patch)
        };
        if octave == 0 {
            continue;
        }
        if notes
            .iter()
            .any(|i| !(0 <= events[*i].note + 12 * octave && events[*i].note + 12 * octave <= 179))
        {
            continue;
        }
        let mut ins = ins_key;
        if skipped_ins.contains(&ins_key) {
            patch.name = format!("{} L", patch.name);
            patch.base_ins = ins_key;
            ins = fm_patches.len() as i32;
            fm_patches.push(patch.clone());
            for i in &notes {
                events[*i].ins = ins;
            }
        }
        for i in &notes {
            events[*i].note += 12 * octave;
        }
        let shift = -1536 * octave;
        let (macro_, speed, loop_) = if !patch.pitch_macro.is_empty() {
            (
                patch.pitch_macro.iter().map(|v| v + shift).collect::<Vec<i32>>(),
                patch.pitch_speed,
                patch.pitch_loop,
            )
        } else {
            (vec![shift], 1, 255)
        };
        patch.pitch_macro = macro_;
        patch.pitch_speed = speed;
        patch.pitch_loop = loop_;
        fm_patches[ins as usize] = patch;
        shifted += 1;
    }
    shifted
}

/// Common fields of the two ADPCM hit records used by `_pcm_levels`.
pub trait PcmHitLike {
    fn vol(&self) -> i32;
    fn pan(&self) -> i32;
    fn vol_pts(&self) -> &[(i64, i32)];
    fn pan_pts(&self) -> &[(i64, i32)];
}

impl PcmHitLike for crate::ym2610::AdpcmAHit {
    fn vol(&self) -> i32 {
        self.vol
    }
    fn pan(&self) -> i32 {
        self.pan
    }
    fn vol_pts(&self) -> &[(i64, i32)] {
        &self.vol_pts
    }
    fn pan_pts(&self) -> &[(i64, i32)] {
        &self.pan_pts
    }
}

impl PcmHitLike for crate::ym2610::AdpcmBHit {
    fn vol(&self) -> i32 {
        self.vol
    }
    fn pan(&self) -> i32 {
        self.pan
    }
    fn vol_pts(&self) -> &[(i64, i32)] {
        &self.vol_pts
    }
    fn pan_pts(&self) -> &[(i64, i32)] {
        &self.pan_pts
    }
}

impl PcmHitLike for crate::ym2608::RhythmHit {
    fn vol(&self) -> i32 {
        self.vol
    }
    fn pan(&self) -> i32 {
        self.pan
    }
    fn vol_pts(&self) -> &[(i64, i32)] {
        &self.vol_pts
    }
    fn pan_pts(&self) -> &[(i64, i32)] {
        &self.pan_pts
    }
}

/// Late level and pan writes as (sample, volume, pan) points.
pub fn _pcm_levels(hit: &impl PcmHitLike) -> Vec<(i64, i32, Option<i32>)> {
    let mut pts: Vec<(i64, i32, i32)> = hit
        .vol_pts()
        .iter()
        .map(|(t, v)| (*t, 0, *v))
        .chain(hit.pan_pts().iter().map(|(t, p)| (*t, 1, *p)))
        .collect();
    pts.sort_by(|a, b| a.cmp(b));
    let mut vol = hit.vol();
    let mut pan = hit.pan();
    let mut last = (if pan == 0 { 0 } else { vol }, pan);
    let mut levels: Vec<(i64, i32, Option<i32>)> = Vec::new();
    let mut i = 0usize;
    while i < pts.len() {
        let t = pts[i].0;
        while i < pts.len() && pts[i].0 == t {
            let (_t, kind, value) = pts[i];
            if kind != 0 {
                pan = value;
            } else {
                vol = value;
            }
            i += 1;
        }
        let level = (if pan == 0 { 0 } else { vol }, pan);
        if level == last {
            continue;
        }
        levels.push((t, level.0, Some(level.1)));
        last = level;
    }
    levels
}

/// Late B writes as pitch steps (1/64 semitones) and volume/pan points.
pub fn _adpcm_b_motion(
    hit: &crate::ym2610::AdpcmBHit,
) -> (Vec<(i64, i32)>, Vec<(i64, i32, Option<i32>)>) {
    let base = hit.rate & 0xFFFF;
    let mut steps: Vec<(i64, i32)> = Vec::new();
    if base != 0 {
        for (t, rate) in &hit.rate_pts {
            let delta = py_int(py_round(
                64.0 * 12.0 * ((*rate & 0xFFFF) as f64 / base as f64).log2(),
            )) as i32;
            if delta != 0 && (steps.is_empty() || steps.last().unwrap().1 != delta) {
                steps.push((*t, delta));
            }
        }
    }
    (steps, _pcm_levels(hit))
}

/// Make a held note's *rising* level reachable, not just its falling one.
pub fn _bind_opm_levels<K: Clone + Eq + std::hash::Hash>(
    events: &mut [NoteEvent],
    patches: &mut IndexMap<K, FMPatch>,
    patch_order: &[K],
) -> (i32, i32) {
    let mut floor: IndexMap<i32, i32> = IndexMap::new();
    let mut tl_keys: BTreeSet<i32> = BTreeSet::new();
    for ev in events.iter() {
        if !ev.on || ev.pcm || !(0 <= ev.ins && (ev.ins as usize) < patch_order.len()) {
            continue;
        }
        if ev.tl_pts.is_empty() {
            continue;
        }
        let Some(patch) = patches.get(&patch_order[ev.ins as usize]) else {
            continue;
        };
        let key = ev.ins;
        let want = ev
            .tl_pts
            .iter()
            .map(|(_t, tl)| *tl)
            .chain(std::iter::once(patch.carrier_tl()))
            .min()
            .unwrap();
        let prev = floor.get(&key).copied();
        floor.insert(key, prev.map_or(want, |p| i32::min(p, want)));
        tl_keys.insert(key);
    }
    let mut opened: IndexMap<i32, i32> = IndexMap::new();
    for key in &tl_keys {
        let key = *key;
        let Some(patch) = patches.get_mut(&patch_order[key as usize]) else {
            continue;
        };
        let base = patch.carrier_tl();
        let low = floor[&key];
        if low >= base {
            continue;
        }
        let delta = base - low;
        let carriers: HashSet<i32> = patch.carriers().into_iter().collect();
        for i in 0..4usize {
            if carriers.contains(&(i as i32)) && (patch.ops[i].tl & 0x7F) < 0x7F {
                let tl = i32::from(patch.ops[i].tl & 0x7F) - delta;
                patch.ops[i].tl = tl as u8;
            }
        }
        opened.insert(key, base);
    }
    let mut rebased = 0i32;
    if !opened.is_empty() {
        for ev in events.iter_mut() {
            if !ev.on || ev.pcm || !(0 <= ev.ins && (ev.ins as usize) < patch_order.len()) {
                continue;
            }
            let Some(&base) = opened.get(&ev.ins) else {
                continue;
            };
            let low = floor[&ev.ins];
            ev.vol_tl_base = Some(low);
            ev.vol = i32::max(0, i32::min(127, 127 - (base - low)));
            rebased += 1;
        }
    }
    (opened.len() as i32, rebased)
}

// YM2151 DT1 index -> Furnace's engine index (same table as the writer).
const _YM_DT_TO_FURNACE: [i32; 8] = [3, 4, 5, 6, 7, 2, 1, 0];

/// Place a note's mid-note DT1/MULT/DT2 writes.
pub fn _bind_opm_dtmul_macros<K: PatchKeyOps>(
    events: &mut [NoteEvent],
    patches: &mut IndexMap<K, FMPatch>,
    patch_order: &mut Vec<K>,
    tick_samples: f64,
    t0: i64,
    use_speed: i32,
    cap: i32,
) -> (i32, i32) {
    fn expand(marks: &[(i64, i32)], start: i32) -> Vec<i32> {
        let mut cur = start;
        let mut mi = 0usize;
        let mut values: Vec<i32> = Vec::new();
        for slot in 0..=marks.last().unwrap().0 {
            while mi < marks.len() && marks[mi].0 <= slot {
                cur = marks[mi].1;
                mi += 1;
            }
            values.push(cur);
        }
        values
    }

    fn row_marks(
        pts: &[(i64, i32)],
        start: i32,
        end: i64,
        t0: i64,
        row_samples: f64,
    ) -> (Vec<(i64, i32)>, bool) {
        let mut groups: std::collections::BTreeMap<i64, Vec<(i64, i32)>> =
            std::collections::BTreeMap::new();
        for (s, value) in pts {
            if end != 0 && *s >= end {
                continue;
            }
            let row = i64::max(0, ((*s - t0) as f64 / row_samples).floor() as i64);
            groups.entry(row).or_default().push((*s, *value));
        }
        let mut carried = start;
        let mut out: Vec<(i64, i32)> = Vec::new();
        let mut lossless = true;
        for (_row, vals) in groups {
            let mut distinct: Vec<i32> = Vec::new();
            for (_s, value) in &vals {
                if distinct.is_empty() || *distinct.last().unwrap() != *value {
                    distinct.push(*value);
                }
            }
            if distinct.len() > 1 && distinct[0] != carried {
                lossless = false;
            }
            if *distinct.last().unwrap() != carried {
                out.push((vals.last().unwrap().0, *distinct.last().unwrap()));
                carried = *distinct.last().unwrap();
            }
        }
        (out, lossless)
    }

    fn macro_values(
        pts: &[(i64, i32)],
        start: i32,
        end: i64,
        sample: i64,
        tick_samples: f64,
    ) -> (Vec<i32>, i32) {
        let last_s = pts.last().unwrap().0;
        let span_end = if end != 0 && end > last_s { end } else { last_s };
        let span_ticks = f64::max(1.0, (span_end - sample) as f64 / tick_samples);
        let speed = i32::max(
            1,
            i32::min(254, (span_ticks / 240.0).floor() as i32 + 1),
        );
        let mut marks: Vec<(i64, i32)> = Vec::new();
        for (s, value) in pts {
            let slot = i64::max(
                0,
                py_floor_div(
                    ((*s - sample) as f64 / tick_samples).floor() as i64,
                    i64::from(speed),
                ),
            );
            if slot > 254 {
                break;
            }
            if !marks.is_empty() && marks.last().unwrap().0 == slot {
                *marks.last_mut().unwrap() = (slot, *value);
            } else {
                marks.push((slot, *value));
            }
        }
        let values = if marks.is_empty() { Vec::new() } else { expand(&marks, start) };
        (values, speed)
    }

    let mut made = 0i32;
    let mut fallbacks = 0i32;
    let mut idx_map: IndexMap<K, usize> = IndexMap::new();
    if tick_samples <= 0.0 {
        return (0, 0);
    }
    let row_samples = tick_samples * i64::from(i32::max(1, use_speed)) as f64;
    for e_idx in 0..events.len() {
        if !events[e_idx].on
            || (events[e_idx].op_dtmul_pts.is_empty() && events[e_idx].op_dt2_pts.is_empty())
            || events[e_idx].pcm
        {
            continue;
        }
        if !(0 <= events[e_idx].ins && (events[e_idx].ins as usize) < patch_order.len()) {
            continue;
        }
        let base_key = patch_order[events[e_idx].ins as usize].clone();
        let Some(base) = patches.get(&base_key).cloned() else {
            continue;
        };
        let mut by_op: IndexMap<i32, Vec<(i64, i32)>> = IndexMap::new();
        for (s, oi, raw) in &events[e_idx].op_dtmul_pts {
            by_op.entry(*oi).or_default().push((*s, *raw));
        }
        let mut dt2_by_op: IndexMap<i32, Vec<(i64, i32)>> = IndexMap::new();
        for (s, oi, dt2) in &events[e_idx].op_dt2_pts {
            dt2_by_op.entry(*oi).or_default().push((*s, *dt2));
        }
        let mut mul_entries: Vec<(i32, Vec<i32>, i32)> = Vec::new();
        let mut dt_entries: Vec<(i32, Vec<i32>, i32)> = Vec::new();
        let mut dt2_entries: Vec<(i32, Vec<i32>, i32)> = Vec::new();
        let mut plans: Vec<(i32, &'static str, Vec<(i64, i32)>, Vec<i32>, i32, i32)> =
            Vec::new();
        let mut row_load: std::collections::BTreeMap<i64, i32> =
            std::collections::BTreeMap::new();
        for oi in 0..4i32 {
            let raw_pts: &[(i64, i32)] =
                by_op.get(&oi).map(|v| v.as_slice()).unwrap_or(&[]);
            let dt2_pts: &[(i64, i32)] =
                dt2_by_op.get(&oi).map(|v| v.as_slice()).unwrap_or(&[]);
            if raw_pts.is_empty() && dt2_pts.is_empty() {
                continue;
            }
            let start_raw = base.ops[oi as usize].dt_mul;
            let start_mul = i32::from(start_raw & 0x0F);
            let start_dt = _YM_DT_TO_FURNACE[usize::from((start_raw >> 4) & 7)];
            let start_dt2 = i32::from((base.ops[oi as usize].dt2_d2r >> 6) & 3);
            let mut series: Vec<(&'static str, Vec<(i64, i32)>, i32)> = Vec::new();
            if !raw_pts.is_empty() {
                series.push((
                    "mul",
                    raw_pts.iter().map(|(s, raw)| (*s, raw & 0x0F)).collect(),
                    start_mul,
                ));
                series.push((
                    "dt",
                    raw_pts
                        .iter()
                        .map(|(s, raw)| (*s, _YM_DT_TO_FURNACE[((raw >> 4) & 7) as usize]))
                        .collect(),
                    start_dt,
                ));
            }
            if !dt2_pts.is_empty() {
                series.push(("dt2", dt2_pts.to_vec(), start_dt2));
            }
            for (kind, pts, start) in series {
                let (marks, ok) =
                    row_marks(&pts, start, events[e_idx].end, t0, row_samples);
                let (values, speed) = macro_values(
                    &pts,
                    start,
                    events[e_idx].end,
                    events[e_idx].sample,
                    tick_samples,
                );
                if ok && !marks.is_empty() {
                    for (s, _v) in &marks {
                        let row = i64::max(
                            0,
                            ((*s - t0) as f64 / row_samples).floor() as i64,
                        );
                        *row_load.entry(row).or_insert(0) += 1;
                    }
                }
                plans.push((oi, kind, if ok { marks } else { Vec::new() }, values, speed, start));
            }
        }
        let force_macro = row_load.values().copied().max().unwrap_or(0) + 2 > 8;
        for (oi, kind, marks, values, speed, start) in plans {
            if !marks.is_empty() && !force_macro {
                for (s, value) in marks {
                    match kind {
                        "mul" => events[e_idx].op_mul_fx.push((s, oi, value)),
                        "dt" => events[e_idx].op_dt_fx.push((s, oi, value)),
                        _ => events[e_idx].op_dt2_fx.push((s, oi, value)),
                    }
                }
                continue;
            }
            if values.is_empty()
                || (values.iter().all(|v| *v == values[0]) && values[0] == start)
            {
                continue;
            }
            let entry = (oi, values, speed);
            match kind {
                "mul" => mul_entries.push(entry),
                "dt" => dt_entries.push(entry),
                _ => dt2_entries.push(entry),
            }
        }
        if mul_entries.is_empty() && dt_entries.is_empty() && dt2_entries.is_empty() {
            continue;
        }
        let mut payload: Vec<i64> = Vec::new();
        _push_op_macro(&mut payload, &mul_entries);
        _push_op_macro(&mut payload, &dt_entries);
        _push_op_macro(&mut payload, &dt2_entries);
        let key = K::_derive_key(5, &base_key, &payload);
        let idx = match idx_map.get(&key) {
            Some(i) => {
                patches.get_mut(&patch_order[*i]).unwrap().uses += 1;
                *i
            }
            None => {
                if patch_order.len() as i32 >= cap {
                    fallbacks += 1;
                    continue;
                }
                let mut copy = base.clone();
                copy.uses = 1;
                copy.name = format!("{}+mul", base.name);
                copy.base_ins = events[e_idx].ins;
                if !mul_entries.is_empty() {
                    copy.op_mul_macro = mul_entries;
                }
                if !dt_entries.is_empty() {
                    copy.op_dt_macro = dt_entries;
                }
                if !dt2_entries.is_empty() {
                    copy.op_dt2_macro = dt2_entries;
                }
                patches.insert(key.clone(), copy);
                patch_order.push(key.clone());
                let i = patch_order.len() - 1;
                idx_map.insert(key, i);
                i
            }
        };
        events[e_idx].ins = idx as i32;
        made += 1;
    }
    (made, fallbacks)
}

/// Give each Ext channel-3 column back the level range it was scripted with.
pub fn _bind_ext_levels<K: Clone + Eq + std::hash::Hash>(
    events: &mut [NoteEvent],
    patches: &mut IndexMap<K, FMPatch>,
    patch_order: &[K],
    notes: &[(usize, i32, i32, i32)],
    floors: &std::collections::BTreeMap<i32, [i32; 4]>,
) -> (i32, i32) {
    for (ins_idx, per_op) in floors {
        if !(0 <= *ins_idx && (*ins_idx as usize) < patch_order.len()) {
            continue;
        }
        let Some(patch) = patches.get_mut(&patch_order[*ins_idx as usize]) else {
            continue;
        };
        for (oi, floor) in per_op.iter().enumerate() {
            if *floor <= 0x7F && i32::from(patch.ops[oi].tl & 0x7F) != *floor {
                patch.ops[oi].tl = *floor as u8;
            }
        }
    }
    let mut levelled = 0i32;
    let mut skipped = 0i32;
    for (ev_i, ins_idx, oi, tl) in notes {
        if *ev_i >= events.len()
            || !(0 <= *ins_idx && (*ins_idx as usize) < patch_order.len())
        {
            skipped += 1;
            continue;
        }
        let Some(patch) = patches.get(&patch_order[*ins_idx as usize]).cloned() else {
            skipped += 1;
            continue;
        };
        if *oi >= patch.ops.len() as i32 {
            skipped += 1;
            continue;
        }
        let floor = floors
            .get(ins_idx)
            .map(|v| v[*oi as usize])
            .unwrap_or(0x7F);
        let ev = &mut events[*ev_i];
        ev.vol_tl_base = Some(floor);
        ev.vol = i32::max(0, i32::min(127, 127 - (*tl - floor)));
        levelled += 1;
    }
    (levelled, skipped)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fm::FMPatch;

    #[test]
    fn macro_effective_holds_and_loops() {
        // Python: values=(3,5,7), speed=2, loop=1, ticks=6.
        assert_eq!(
            _macro_effective(&[3, 5, 7], 2, 1, 6, 0),
            vec![3, 5, 5, 7, 7, 5]
        );
        // delay holds the first value for delay+1 ticks, then speed applies.
        assert_eq!(_macro_effective(&[3, 9], 1, 255, 4, 1), vec![3, 3, 9, 9]);
        // Empty macro plays nothing.
        assert!(_macro_effective(&[], 1, 255, 4, 0).is_empty());
    }

    #[test]
    fn macro_extends_requires_same_emptiness_and_prefix() {
        assert!(_macro_extends(&[1, 2, 3], &[1, 2]));
        assert!(_macro_extends(&[1, 2], &[1, 2]));
        assert!(_macro_extends(&[], &[]));
        assert!(!_macro_extends(&[1], &[]));
        assert!(!_macro_extends(&[1, 3], &[1, 2]));
        assert!(!_macro_extends(&[1], &[1, 2]));
    }

    #[test]
    fn prefix_targets_resolves_chains_to_the_longest() {
        let mapping = _prefix_targets(
            &[0, 1, 2],
            |i| (i + 1) as i64,
            |j, i| j > i,
        );
        assert_eq!(mapping[&0], 2);
        assert_eq!(mapping[&1], 2);
        assert_eq!(mapping[&2], 2);
        // Equal-size identical macros keep the lower index.
        let same = _prefix_targets(&[3, 4], |_i| 1, |_j, _i| false);
        assert_eq!(same[&3], 3);
        assert_eq!(same[&4], 4);
    }

    #[test]
    fn trim_fm_macro_copies_folds_copy_chains() {
        let mut patches = vec![FMPatch::default(); 3];
        patches[1].base_ins = 0;
        patches[2].base_ins = 1;
        let mut events = vec![NoteEvent::new(0, 0, true, 60, 2, 100, 0x80, 0)];
        let removed = _trim_fm_macro_copies(&mut events, &mut patches, 1);
        assert_eq!(removed, 2);
        assert_eq!(patches.len(), 1);
        assert_eq!(events[0].ins, 0);
    }

    #[test]
    fn rebase_non_fm_ins_moves_only_sample_and_tone_notes() {
        let mut events = vec![
            NoteEvent::new(0, 0, true, 60, 5, 100, 0x80, 0),
            NoteEvent::new(0, 1, true, 60, 4, 100, 0x80, 0),
        ];
        events[0].tone_chip = 1;
        events[1].pcm = true;
        _rebase_non_fm_ins(&mut events, 5, 7);
        assert_eq!(events[0].ins, 7);
        assert_eq!(events[1].ins, 4);
        let mut fm = vec![NoteEvent::new(0, 0, true, 60, 5, 100, 0x80, 0)];
        _rebase_non_fm_ins(&mut fm, 5, 7);
        assert_eq!(fm[0].ins, 5);
    }
}
