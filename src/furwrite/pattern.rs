//! Pattern build: row placement, pitch trajectories, chip effects.
//!
//! Port of `furwrite.py` lines 2449-4511: `_traj_at`, `_emit_op_fx`,
//! `_emit_steps`, `_emit_jump_row`, `_emit_sweep`, `_assign_pcm_rows`,
//! `_pattern_len`, `_phrase_pattern_len`, `_cell_ed`, `_place_chip_fx`,
//! `_opn_module_channels`, `_build_patterns`, `_fit_fx_cols`.

use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};

use indexmap::IndexMap;

use crate::model::{FmSystem, NoteEvent, Song};
use crate::parity::{py_int, py_round};

use super::buf::{EMPTY, NOTE_OFF, PAT_LEN_MAX, PAT_TAIL_ROWS};
use super::row::{_add_fx, _add_quad_fx, _blank_row, _bump, _fx_keep, _slide_level, Row};

/// The row grid: one sparse column map per module channel.
pub type Grid = Vec<IndexMap<i32, Row>>;

/// Test seam replacing the module-level `_emit_sweep` patch: a hook receives
/// the real grid, so it can add effect columns exactly like
/// `tests/_synth.py:inject_rows`.
pub struct EmitHooks<'a> {
    pub sweep_hook: Option<
        &'a mut dyn FnMut(
            &mut Grid,
            &Song,
            &NoteEvent,
            usize,
            i32,
            &mut IndexMap<(i32, i32), i32>,
            &IndexMap<i32, Vec<i32>>,
            Option<i32>,
            Option<&mut IndexMap<String, i64>>,
        ),
    >,
}

impl<'a> Default for EmitHooks<'a> {
    fn default() -> Self {
        EmitHooks { sweep_hook: None }
    }
}

// ---------------------------------------------------------------------------
// Small helpers mirroring Python's builtins and module-private utilities.

fn fx_slice(cell: Option<&Row>) -> &[(i32, i32)] {
    match cell {
        Some(row) => row.fx.as_deref().unwrap_or(&[]),
        None => &[],
    }
}

fn any_cmd(cell: Option<&Row>, cmd: i32) -> bool {
    fx_slice(cell).iter().any(|&(c, _)| c == cmd)
}

fn any_cmd_val(cell: Option<&Row>, cmd: i32, val: i32) -> bool {
    fx_slice(cell).iter().any(|&(c, v)| c == cmd && v == val)
}

fn bisect_left_i64(xs: &[i64], v: f64) -> usize {
    let mut lo = 0usize;
    let mut hi = xs.len();
    while lo < hi {
        let mid = (lo + hi) / 2;
        if (xs[mid] as f64) < v {
            lo = mid + 1;
        } else {
            hi = mid;
        }
    }
    lo
}

fn bisect_left_f64(xs: &[f64], v: f64) -> usize {
    let mut lo = 0usize;
    let mut hi = xs.len();
    while lo < hi {
        let mid = (lo + hi) / 2;
        if xs[mid] < v {
            lo = mid + 1;
        } else {
            hi = mid;
        }
    }
    lo
}

fn bisect_right_i32(xs: &[i32], v: i32) -> usize {
    let mut lo = 0usize;
    let mut hi = xs.len();
    while lo < hi {
        let mid = (lo + hi) / 2;
        if v < xs[mid] {
            hi = mid;
        } else {
            lo = mid + 1;
        }
    }
    lo
}

/// `_system_span`: module columns one FM system occupies.
fn _system_span(system: &FmSystem) -> i32 {
    system.channels()
}

fn _channel_vol_max(song: &Song, n_ch: i32, default: i32, ch_offset: i32) -> Vec<i32> {
    let mut table = vec![default; n_ch.max(0) as usize];
    let systems = &song.fm_systems;
    let kinds: BTreeSet<&str> = systems.iter().map(|s| s.kind.as_str()).collect();
    if ch_offset != 0 || kinds.len() < 2 {
        return table;
    }
    let mut ch: i32 = 0;
    for system in systems {
        let top = if system.kind == "opll" {
            15
        } else if matches!(
            system.kind.as_str(),
            "opl" | "opl2" | "opl3" | "y8950" | "opl4"
        ) {
            63
        } else {
            127
        };
        for _ in 0..system.fm_channels {
            if ch < n_ch {
                table[ch as usize] = top;
            }
            ch += 1;
        }
        ch += _system_span(system) - system.fm_channels;
    }
    table
}

fn fx_filtered_or_default(cell: Option<&Row>, fx_cols: usize, drop: &[i32]) -> Vec<(i32, i32)> {
    match cell.and_then(|c| c.fx.as_ref()) {
        Some(fx) if !fx.is_empty() => fx
            .iter()
            .copied()
            .filter(|(c, _)| !drop.contains(c))
            .collect(),
        _ => vec![(EMPTY, EMPTY); fx_cols],
    }
}

// ---------------------------------------------------------------------------
// Pitch trajectories.

pub fn _traj_at(pts: &[(i64, i32)], t: i64) -> i32 {
    let mut v = 0;
    for &(s, d) in pts {
        if s > t {
            break;
        }
        v = d;
    }
    v
}

pub fn _emit_op_fx(
    grid: &mut Grid,
    song: &Song,
    ev: &NoteEvent,
    points: &[(i64, i32, i32)],
    code: i32,
    fx_cols: usize,
    total_rows: i32,
    chan: Option<i32>,
    mut stats: Option<&mut IndexMap<String, i64>>,
    note_row: Option<i32>,
) {
    let ch = chan.unwrap_or(ev.ch);
    if ch < 0 || ch as usize >= grid.len() || points.is_empty() {
        return;
    }
    let note_row = note_row
        .unwrap_or_else(|| song.place(ev.sample, stats.as_deref_mut()).0);
    let mut limit_row = total_rows;
    if ev.end != 0 {
        limit_row = total_rows.min(
            (note_row + 1).max(song.place(ev.end, stats.as_deref_mut()).0 + 1),
        );
    }
    let mut by_row: BTreeMap<(i32, i32), (i32, i32)> = BTreeMap::new();
    for &(sample, op, value) in points {
        let (mut row, delay) = song.place(sample, stats.as_deref_mut());
        if row < note_row {
            row = note_row;
        }
        if !(0 <= row && row < limit_row) {
            continue;
        }
        by_row.insert((row, op), (delay, value));
    }
    let effect_ops = [0i32, 2, 1, 3];
    let cu = ch as usize;
    for (&(row, op), &(mut delay, value)) in by_row.iter() {
        let cell = grid[cu].entry(row).or_insert_with(_blank_row);
        if 0 <= cell.note && cell.note < 180 && row != note_row {
            continue;
        }
        if row == note_row {
            delay = 0;
        }
        if delay > 0 && !any_cmd(Some(&*cell), 0xED) {
            _add_fx(cell, 0xED, delay, fx_cols, stats.as_deref_mut());
        }
        let x = if matches!(code, 0x16 | 0x53 | 0x55) {
            effect_ops[(op & 3) as usize]
        } else {
            op
        };
        _add_fx(
            cell,
            code,
            ((x + 1) & 0x0F) << 4 | (value & 0x0F),
            fx_cols,
            stats.as_deref_mut(),
        );
    }
}

fn free_row(grid: &Grid, ch: usize, mut r: i32, total_rows: i32) -> Option<i32> {
    while r < total_rows {
        let free = match grid[ch].get(&r) {
            None => true,
            Some(cell) => !(0 <= cell.note && cell.note < 180),
        };
        if free {
            return Some(r);
        }
        r += 1;
    }
    None
}

fn cell_at<'b>(grid: &'b mut Grid, ch: usize, r: i32) -> &'b mut Row {
    grid[ch].entry(r).or_insert_with(_blank_row)
}

pub fn _emit_steps(
    grid: &mut Grid,
    song: &Song,
    ev: &NoteEvent,
    fx_cols: usize,
    total_rows: i32,
    e5_writes: &mut IndexMap<(i32, i32), i32>,
    chan: Option<i32>,
    mut stats: Option<&mut IndexMap<String, i64>>,
    note_row: Option<i32>,
) {
    let ch = chan.unwrap_or(ev.ch);
    if ch < 0 || ch as usize >= grid.len() {
        return;
    }
    let cu = ch as usize;
    let div = if ev.pcm || ev.sweep_div >= 2.0 {
        2.0
    } else {
        1.0
    };
    let note_row = note_row
        .unwrap_or_else(|| song.place(ev.sample, stats.as_deref_mut()).0);
    let mut by_row: BTreeMap<i32, (i32, i32)> = BTreeMap::new();
    for &(t, delta) in &ev.pitch_steps {
        let (mut row, delay) = song.place(t, stats.as_deref_mut());
        if row <= note_row {
            row = note_row + 1;
        }
        if 0 <= row && row < total_rows {
            by_row.insert(row, (delta, delay));
        }
    }
    let mut e5w = ev.e5;
    let mut xpose: i32 = 0;

    let targets: Vec<i32> = by_row.keys().copied().collect();
    let mut e8_rows: BTreeSet<i32> = BTreeSet::new();
    let mut limit_row = total_rows;
    if ev.end != 0 {
        limit_row = total_rows.min(
            (note_row + 1).max(song.place(ev.end, stats.as_deref_mut()).0 + 1),
        );
    }
    for (idx, &row) in targets.iter().enumerate() {
        if row >= limit_row {
            break;
        }
        let (delta, delay) = by_row[&row];
        let mut next_row = if idx + 1 < targets.len() {
            targets[idx + 1]
        } else {
            total_rows
        };
        next_row = next_row.min(limit_row);
        if let Some(cell) = grid[cu].get(&row) {
            if 0 <= cell.note && cell.note < 180 {
                continue;
            }
        }
        if delay > 0 && !any_cmd(grid[cu].get(&row), 0xED) {
            _add_fx(
                cell_at(grid, cu, row),
                0xED,
                delay,
                fx_cols,
                stats.as_deref_mut(),
            );
        }
        let mut cur_row = row;
        while cur_row < next_row {
            let need = delta as f64 - (xpose as f64 + (e5w - ev.e5) as f64 / div);
            if need.abs() <= 1024.0 {
                break;
            }
            let y = if need > 0.0 { 15 } else { -15 };
            _add_fx(
                cell_at(grid, cu, cur_row),
                if y > 0 { 0xE8 } else { 0xE9 },
                15,
                fx_cols,
                stats.as_deref_mut(),
            );
            e8_rows.insert(cur_row);
            xpose += 64 * y;
            let nxt = free_row(grid, cu, cur_row + 1, total_rows);
            match nxt {
                None => {
                    cur_row = next_row;
                    break;
                }
                Some(nxt) if nxt >= next_row => {
                    cur_row = next_row;
                    break;
                }
                Some(nxt) => cur_row = nxt,
            }
        }
        if cur_row >= next_row {
            continue;
        }
        let need = delta as f64 - (xpose as f64 + (e5w - ev.e5) as f64 / div);
        if need.abs() < 1e-6 {
            continue;
        }
        let desired = delta - xpose;
        let hi_units = (ev.e5_hi - ev.e5) as f64 / div;
        let lo_units = (ev.e5_lo - ev.e5) as f64 / div;
        let mut semis: i32 = if desired as f64 > hi_units {
            ((desired as f64 - hi_units) / 64.0).ceil() as i32
        } else if (desired as f64) < lo_units {
            ((desired as f64 - lo_units) / 64.0).floor() as i32
        } else {
            0
        };
        let rest = desired - 64 * semis;
        if rest != 0 && rest % 64 == 0 {
            let extra = rest / 64;
            if (semis + extra).abs() <= 15 {
                semis += extra;
            }
        }
        if semis != 0 {
            if e8_rows.contains(&cur_row) {
                let nxt = free_row(grid, cu, cur_row + 1, total_rows);
                match nxt {
                    None => continue,
                    Some(nxt) if nxt >= next_row => continue,
                    Some(nxt) => cur_row = nxt,
                }
            }
            let y = semis.clamp(-15, 15);
            _add_fx(
                cell_at(grid, cu, cur_row),
                if y > 0 { 0xE8 } else { 0xE9 },
                y.abs(),
                fx_cols,
                stats.as_deref_mut(),
            );
            e8_rows.insert(cur_row);
            xpose += 64 * y;
        }
        let desired = delta - xpose;
        let new_e5 = ev
            .e5_lo
            .max(ev.e5_hi.min(py_int(py_round(ev.e5 as f64 + desired as f64 * div)) as i32));
        if new_e5 != e5w {
            e5_writes.insert((ch, cur_row), new_e5);
            e5w = new_e5;
        }
    }
}

/// A step of this many 1/64-semitone units between two held KC/KF values is a
/// hard pitch change (a legato note), not the start of a slide.
const _SWEEP_JUMP_UNITS: f64 = 56.0;

#[allow(clippy::too_many_arguments)]
pub fn _emit_jump_row(
    grid: &mut Grid,
    ch: i32,
    fx_cols: usize,
    total_rows: i32,
    row: i32,
    note_row: i32,
    target_abs: f64,
    div: f64,
    e5_note: i32,
    chunks: &mut IndexMap<i32, i32>,
    e5_lo: i32,
    e5_hi: i32,
    e5_writes: &mut IndexMap<(i32, i32), i32>,
    note_rows: Option<&[i32]>,
    mut stats: Option<&mut IndexMap<String, i64>>,
    last_rate: i32,
    max_rows: Option<i32>,
) -> Option<(f64, i32)> {
    if row < 0 || row >= total_rows {
        return None;
    }
    let cu = ch as usize;
    let held: i32 = chunks
        .iter()
        .filter(|(r, _)| **r < row)
        .map(|(_, v)| *v)
        .sum();
    let off = target_abs - 64.0 * held as f64;
    let mut w = py_int(py_round(off / 64.0)) as i32;
    let lo_u = (e5_lo - e5_note) as f64 / div;
    let hi_u = (e5_hi - e5_note) as f64 / div;
    let mut fine = off - 64.0 * w as f64;
    if fine > hi_u {
        w += ((fine - hi_u) / 64.0).ceil() as i32;
    } else if fine < lo_u {
        w -= ((lo_u - fine) / 64.0).ceil() as i32;
    }
    fine = off - 64.0 * w as f64;
    let limit = match note_rows {
        Some(rows) => {
            let mut sorted: Vec<i32> = rows.to_vec();
            sorted.sort();
            sorted
                .into_iter()
                .find(|&r| r > row)
                .unwrap_or(total_rows)
        }
        None => total_rows,
    };
    let mut plan: Vec<(i32, i32)> = Vec::new();
    let mut cur_row = row;
    let mut rem = w;
    loop {
        let step = if rem != 0 { rem.clamp(-15, 15) } else { 0 };
        plan.push((cur_row, step));
        rem -= step;
        if rem == 0 {
            break;
        }
        cur_row += 1;
    }
    for &(jrow, _step) in &plan {
        if jrow < 0 || jrow >= total_rows || jrow >= limit {
            return None;
        }
        if let Some(max_rows) = max_rows {
            if jrow - row + 1 > max_rows {
                return None;
            }
        }
        if jrow != note_row {
            if let Some(cell) = grid[cu].get(&jrow) {
                if 0 <= cell.note && cell.note < 180 {
                    return None;
                }
            }
        }
    }
    let drop_rows: Vec<i32> = chunks.keys().copied().filter(|&r| r >= row).collect();
    for jrow in drop_rows {
        chunks.shift_remove(&jrow);
        if let Some(cell) = grid[cu].get_mut(&jrow) {
            if let Some(fx) = cell.fx.as_mut() {
                fx.retain(|&(c, _)| c != 0xE8 && c != 0xE9);
            }
        }
    }
    let new_e5 = e5_lo.max(e5_hi.min(py_int(py_round(e5_note as f64 + fine * div)) as i32));
    for (idx, &(jrow, step)) in plan.iter().enumerate() {
        let cell = grid[cu].entry(jrow).or_insert_with(_blank_row);
        if idx == 0 && last_rate != 0 {
            _fx_keep(cell, &[0x01, 0x02]);
            _add_fx(
                cell,
                if last_rate > 0 { 0x01 } else { 0x02 },
                0,
                fx_cols,
                stats.as_deref_mut(),
            );
        }
        if step != 0 {
            chunks.insert(jrow, step);
            _add_fx(
                cell,
                if step > 0 { 0xE8 } else { 0xE9 },
                step.abs(),
                fx_cols,
                stats.as_deref_mut(),
            );
        }
    }
    e5_writes.insert((ch, row), new_e5);
    Some((64.0 * chunks.values().map(|&v| v as f64).sum::<f64>(), new_e5))
}

#[allow(clippy::too_many_arguments)]
pub fn _emit_sweep(
    grid: &mut Grid,
    song: &Song,
    ev: &NoteEvent,
    fx_cols: usize,
    total_rows: i32,
    e5_writes: &mut IndexMap<(i32, i32), i32>,
    note_rows: &IndexMap<i32, Vec<i32>>,
    chan: Option<i32>,
    mut stats: Option<&mut IndexMap<String, i64>>,
) {
    let ch = chan.unwrap_or(ev.ch);
    let pts = &ev.sweep;
    if pts.is_empty() || ch < 0 || ch as usize >= grid.len() {
        return;
    }
    let cu = ch as usize;
    let row_len = song.samples_per_row();
    let tick = row_len / song.speed.max(1) as f64;
    let start = ev.sample;
    let mut end = pts[pts.len() - 1].0;
    let div = if ev.pcm || ev.sweep_div >= 2.0 {
        2.0
    } else {
        1.0
    };
    let e5_byte = |offset: f64| -> i32 {
        ev.e5_lo.max(
            ev.e5_hi
                .min(py_int(py_round(ev.e5 as f64 + offset * div)) as i32),
        )
    };
    let (row0, _d0) = song.place(start, stats.as_deref_mut());
    if ((end - start) as f64) < tick {
        let final_ = _traj_at(pts, end) as f64;
        let rows = note_rows.get(&ch).map(|v| v.as_slice());
        if _emit_jump_row(
            grid,
            ch,
            fx_cols,
            total_rows,
            row0 + 1,
            row0,
            final_,
            div,
            ev.e5,
            &mut IndexMap::new(),
            ev.e5_lo,
            ev.e5_hi,
            e5_writes,
            rows,
            stats.as_deref_mut(),
            0,
            None,
        )
        .is_none()
        {
            _bump(stats.as_deref_mut(), "legato_skipped", 1);
        }
        return;
    }
    if grid[cu].get(&row0).is_none() {
        return;
    }
    let base = song.t0 as f64 + row0 as f64 * row_len;
    let mut crossed = false;
    let after: &[i32] = note_rows.get(&ch).map(|v| v.as_slice()).unwrap_or(&[]);
    let i = bisect_right_i32(after, row0);
    if i < after.len() {
        let limit = py_int(song.t0 as f64 + after[i] as f64 * row_len);
        if limit < end {
            end = limit;
            crossed = true;
            _bump(stats.as_deref_mut(), "sweep_clipped", 1);
        }
    }

    let mut per_row: Vec<[i32; 3]> = Vec::new();
    for (idx, &(s, _d)) in pts.iter().enumerate() {
        let r = song.place(s, stats.as_deref_mut()).0;
        if let Some(last) = per_row.last_mut() {
            if last[0] == r {
                last[2] = idx as i32;
                continue;
            }
        }
        per_row.push([r, idx as i32, idx as i32]);
    }

    let val = |e: i32| -> f64 {
        if 0 <= e && (e as usize) < per_row.len() {
            pts[per_row[e as usize][2] as usize].1 as f64
        } else {
            0.0
        }
    };

    // Segment: (is_jump, first per_row index, last, pitch held before).
    let mut segs: Vec<(bool, i32, i32, f64)> = Vec::new();
    let mut prev_held = 0.0;
    let mut run_first: i32 = -1;
    let mut anchor = 0.0;
    for e in 0..per_row.len() as i32 {
        let row = per_row[e as usize][0];
        let step = val(e) - val(e - 1);
        let ahead = if (e + 1) < per_row.len() as i32
            && per_row[(e + 1) as usize][0] == row + 1
        {
            val(e + 1) - val(e)
        } else {
            0.0
        };
        let behind = if e >= 1 && per_row[(e - 1) as usize][0] == row - 1 {
            val(e - 1) - val(e - 2)
        } else {
            0.0
        };
        let big = step.abs() >= _SWEEP_JUMP_UNITS;
        let chain = (ahead.abs() >= _SWEEP_JUMP_UNITS && ahead * step > 0.0)
            || (behind.abs() >= _SWEEP_JUMP_UNITS && behind * step > 0.0);
        let hard = big && !chain;
        let d = val(e);
        if hard {
            if run_first >= 0 {
                segs.push((false, run_first, e - 1, anchor));
                run_first = -1;
            }
            segs.push((true, e, e, prev_held));
        } else if run_first < 0 {
            run_first = e;
            anchor = prev_held;
        }
        prev_held = d;
    }
    if run_first >= 0 {
        segs.push((false, run_first, per_row.len() as i32 - 1, anchor));
    }

    let mut e5w = ev.e5;
    let mut xpose = 0.0;
    let mut slide = 0.0;
    let mut last_rate = 0;
    let mut chunks: IndexMap<i32, i32> = IndexMap::new();
    let mut jump_last_row = row0 - 1;

    for seg_i in 0..segs.len() {
        let (is_jump, a, b, seg_anchor) = segs[seg_i];
        if is_jump {
            let jrow = per_row[a as usize][0];
            let s = pts[per_row[a as usize][1] as usize].0;
            let d = pts[per_row[a as usize][2] as usize].1 as f64;
            if crossed && s >= end {
                continue;
            }
            if jrow <= row0 {
                continue;
            }
            if jrow >= total_rows {
                _bump(stats.as_deref_mut(), "legato_skipped", 1);
                continue;
            }
            if let Some(c) = grid[cu].get(&jrow) {
                if 0 <= c.note && c.note < 180 {
                    continue;
                }
            }
            let nxt = if seg_i + 1 < segs.len() {
                per_row[segs[seg_i + 1].1 as usize][0]
            } else {
                total_rows
            };
            let rows = note_rows.get(&ch).map(|v| v.as_slice());
            let max_rows = 1.max(nxt - jrow);
            match _emit_jump_row(
                grid,
                ch,
                fx_cols,
                total_rows,
                jrow,
                row0,
                d,
                div,
                ev.e5,
                &mut chunks,
                ev.e5_lo,
                ev.e5_hi,
                e5_writes,
                rows,
                stats.as_deref_mut(),
                last_rate,
                Some(max_rows),
            ) {
                None => {
                    _bump(stats.as_deref_mut(), "legato_skipped", 1);
                    continue;
                }
                Some((xp, e5)) => {
                    xpose = xp;
                    e5w = e5;
                }
            }
            slide = 0.0;
            last_rate = 0;
            let mut jmax = jump_last_row.max(jrow);
            for &r in chunks.keys() {
                jmax = jmax.max(r);
            }
            jump_last_row = jmax;
            continue;
        }

        // A slide run: ramp from the held anchor through this run's points.
        let i_from = per_row[a as usize][1] as usize;
        let i_to = per_row[b as usize][2] as usize;
        let first_s = pts[i_from].0;
        if crossed && first_s >= end {
            continue;
        }
        let run_pts: Vec<(i64, f64)> = (i_from..=i_to)
            .map(|j| (pts[j].0, pts[j].1 as f64))
            .collect();
        let step_times: Vec<i64> = run_pts.iter().map(|&(s, _)| s).collect();
        let traj_run = |t: i64| -> f64 {
            let mut v = seg_anchor;
            for &(s, d) in &run_pts {
                if s > t {
                    break;
                }
                v = d;
            }
            v
        };
        let mut k = per_row[a as usize][0] - row0;
        k = k.max(jump_last_row - row0 + 1).max(0);
        let mut k_last = per_row[b as usize][0] - row0;
        if (b as usize + 1) < per_row.len() {
            k_last = k_last.min(per_row[b as usize + 1][0] - row0 - 1);
        }
        if k_last < k {
            continue;
        }
        let mut run_last: Option<i32> = None;
        while k <= k_last {
            let rs = base + k as f64 * row_len;
            if rs >= end as f64 {
                break;
            }
            let re_full = base + (k + 1) as f64 * row_len;
            let re = re_full.min(end as f64);
            let merged = bisect_left_i64(&step_times, re_full) as i32
                - bisect_left_i64(&step_times, rs) as i32
                - 1;
            _bump(stats.as_deref_mut(), "sweep_rate_merged", merged.max(0) as i64);
            let first_row = seg_i == 0 && k == 0;
            let cell_k = grid[cu].get(&(row0 + k));
            let ed = if last_rate == 0 {
                _cell_ed(cell_k)
            } else {
                0
            };
            let t_eff = if first_row { start as f64 } else { rs };
            let want = if first_row {
                0.0
            } else {
                traj_run(py_int(rs + tick))
            };
            let mut cur = (e5w - ev.e5) as f64 / div + xpose + slide;
            if (want - cur).abs() >= 6.0 {
                let blocked = match cell_k {
                    None => false,
                    Some(c) => 0 <= c.note && c.note < 180,
                };
                if !blocked {
                    e5w = e5_byte(want - xpose - slide);
                    e5_writes.insert((ch, row0 + k), e5w);
                    cur = (e5w - ev.e5) as f64 / div + xpose + slide;
                } else {
                    _bump(stats.as_deref_mut(), "e5_skipped", 1);
                }
            }
            let is_last = k == k_last;
            let mut span = if is_last {
                (re_full - t_eff) / tick
            } else {
                (re - t_eff) / tick
            };
            if ed != 0 {
                span = (song.speed - ed) as f64;
            }
            let want_end = if first_row && ev.pcm {
                0.0
            } else {
                traj_run(py_int(if is_last { re_full } else { re }))
            };
            let mut rate = 0.0;
            if span > 0.0 {
                let limit = 255.0 / div;
                rate = ((want_end - cur) / span).clamp(-limit, limit);
            }
            let param = (-255).max(255.min(py_int(py_round(rate * div)) as i32));
            if param != last_rate || param != 0 {
                let cell = grid[cu].entry(row0 + k).or_insert_with(_blank_row);
                let mut fx = fx_filtered_or_default(Some(&*cell), fx_cols, &[0x01, 0x02]);
                if fx.is_empty() {
                    cell.fx = None;
                } else {
                    cell.fx = Some(std::mem::take(&mut fx));
                }
                if param != 0 {
                    _add_fx(
                        cell,
                        if param > 0 { 0x01 } else { 0x02 },
                        255.min(param.abs()),
                        fx_cols,
                        stats.as_deref_mut(),
                    );
                } else {
                    _add_fx(
                        cell,
                        if last_rate > 0 { 0x01 } else { 0x02 },
                        0,
                        fx_cols,
                        stats.as_deref_mut(),
                    );
                }
                last_rate = param;
            }
            let engine_rate = param as f64 / div;
            if stats.is_some() && span > 0.0 {
                let mut bad_here = false;
                for j in 0..4 {
                    let t = py_int(rs + (re_full - rs) * (2 * j + 1) as f64 / 8.0);
                    if t <= start {
                        continue;
                    }
                    let actual = traj_run(t);
                    let model = cur + engine_rate * span * (2 * j + 1) as f64 / 8.0;
                    let dev = (model - actual).abs();
                    if dev > 6.0 {
                        bad_here = true;
                        _bump(stats.as_deref_mut(), "sweep_dev_bad", 1);
                    }
                    let prev = stats
                        .as_ref()
                        .and_then(|s| s.get("sweep_dev_max").copied())
                        .unwrap_or(0);
                    if dev > prev as f64 {
                        stats
                            .as_mut()
                            .unwrap()
                            .insert("sweep_dev_max".to_string(), py_int(py_round(dev)));
                        // (insert returns the old value; discard it)
                    }
                }
                if bad_here {
                    _bump(stats.as_deref_mut(), "sweep_dev_rows", 1);
                }
            }
            slide += engine_rate * span;
            run_last = Some(k);
            k += 1;
        }

        if run_last.is_none() {
            continue;
        }
        let stop_row = row0 + run_last.unwrap() + 1;
        let mut foreign = false;
        if 0 <= stop_row && stop_row < total_rows {
            if let Some(cell) = grid[cu].get(&stop_row) {
                foreign = 0 <= cell.note && cell.note < 180;
            }
        }
        if last_rate != 0 && 0 <= stop_row && stop_row < total_rows {
            let cell = grid[cu].entry(stop_row).or_insert_with(_blank_row);
            if !any_cmd(Some(&*cell), 0x01) && !any_cmd(Some(&*cell), 0x02) {
                _add_fx(
                    cell,
                    if last_rate > 0 { 0x01 } else { 0x02 },
                    0,
                    fx_cols,
                    stats.as_deref_mut(),
                );
            }
            last_rate = 0;
        }
        if seg_i != segs.len() - 1 {
            continue;
        }
        if foreign || !(0 <= stop_row && stop_row < total_rows) {
            continue;
        }
        let final_ = pts[per_row[b as usize][2] as usize].1 as f64;
        let pin = e5_byte(final_ - xpose - slide);
        if pin == e5w {
            continue;
        }
        grid[cu].entry(stop_row).or_insert_with(_blank_row);
        e5_writes.insert((ch, stop_row), pin);
        e5w = pin;
    }
}

// ---------------------------------------------------------------------------
// PCM placement and pattern window.

pub fn _assign_pcm_rows(
    song: &Song,
    total_rows: i32,
    last_sample: i64,
    mut stats: Option<&mut IndexMap<String, i64>>,
    clip_row: Option<i32>,
    clip_cut: Option<i64>,
) -> IndexMap<usize, (i32, i32)> {
    let mut by_ch: IndexMap<i32, Vec<usize>> = IndexMap::new();
    for (idx, ev) in song.events.iter().enumerate() {
        if ev.pcm && ev.ch >= 0 {
            by_ch.entry(ev.ch).or_default().push(idx);
        }
    }
    let limit = total_rows.min(song.place(last_sample, stats.as_deref_mut()).0 + 1);
    let mut out: IndexMap<usize, (i32, i32)> = IndexMap::new();

    let wrapped = |row: i32, delay: i32, sample: i64| -> (i32, i32) {
        if let (Some(clip_row), Some(clip_cut)) = (clip_row, clip_cut) {
            if row >= total_rows && row <= clip_row + 1 && sample < clip_cut {
                return (clip_row, 0);
            }
        }
        (row, delay)
    };

    for evs in by_ch.values_mut() {
        evs.sort_by(|&a, &b| {
            let ea = &song.events[a];
            let eb = &song.events[b];
            (ea.sample, if ea.on { 0 } else { 1 })
                .cmp(&(eb.sample, if eb.on { 0 } else { 1 }))
        });
        let ons: Vec<usize> = evs.iter().copied().filter(|&i| song.events[i].on).collect();
        let ideals: Vec<(i32, i32)> = ons
            .iter()
            .map(|&i| song.place(song.events[i].sample, stats.as_deref_mut()))
            .collect();
        let mut placed: IndexMap<i32, usize> = IndexMap::new();
        for (i, &eidx) in ons.iter().enumerate() {
            let (mut row, mut delay) = ideals[i];
            let sample = song.events[eidx].sample;
            let (wrow, wdelay) = wrapped(row, delay, sample);
            row = wrow;
            delay = wdelay;
            if row < 0 || row >= total_rows {
                continue;
            }
            if let Some(&prev_i) = placed.get(&row) {
                let prev = ons[prev_i];
                if song.events[prev].sample == sample {
                    out.shift_remove(&prev);
                    _bump(stats.as_deref_mut(), "pcm_merged", 1);
                    placed.insert(row, i);
                    out.insert(eidx, (row, delay));
                    continue;
                }
                let mut dest = row + 1;
                while dest < total_rows && placed.contains_key(&dest) {
                    dest += 1;
                }
                if dest < total_rows {
                    placed.insert(dest, i);
                    out.insert(eidx, (dest, 0));
                    _bump(stats.as_deref_mut(), "pcm_shifted", 1);
                    continue;
                }
                out.shift_remove(&prev);
                _bump(stats.as_deref_mut(), "pcm_evicted", 1);
                placed.insert(row, i);
                out.insert(eidx, (row, delay));
                continue;
            }
            if row >= limit {
                continue;
            }
            placed.insert(row, i);
            out.insert(eidx, (row, delay));
        }
        let played: Vec<usize> = ons.iter().copied().filter(|i| out.contains_key(i)).collect();
        for &off_idx in evs.iter().filter(|&&i| !song.events[i].on) {
            let off = &song.events[off_idx];
            let before: Vec<usize> = played
                .iter()
                .copied()
                .filter(|&i| song.events[i].sample < off.sample)
                .collect();
            let on = match before.last() {
                None => continue,
                Some(&on) => on,
            };
            let on_row = out[&on].0;
            let nxt = played
                .iter()
                .copied()
                .find(|&i| song.events[i].sample > song.events[on].sample);
            let next_row = match nxt {
                Some(n) => out[&n].0,
                None => total_rows,
            };
            let (mut off_row, mut off_delay) =
                song.place(off.sample, stats.as_deref_mut());
            let (wrow, wdelay) = wrapped(off_row, off_delay, off.sample);
            off_row = wrow;
            off_delay = wdelay;
            if off_row >= next_row {
                if off.cut_before && next_row > on_row + 1 {
                    off_row = next_row - 1;
                    off_delay = 0;
                } else {
                    continue;
                }
            }
            if off_row <= on_row {
                off_row = on_row + 1;
                off_delay = 0;
            }
            if off_row >= next_row || off_row >= total_rows {
                continue;
            }
            if placed.contains_key(&off_row) {
                continue;
            }
            out.insert(off_idx, (off_row, off_delay));
        }
    }
    out
}

pub fn _pattern_len(last_row: i32, cap: i32) -> i32 {
    (last_row + 1 + PAT_TAIL_ROWS).min(cap).max(1)
}

pub fn _phrase_pattern_len(song: &Song, last_row: i32, total_rows: i32) -> i32 {
    let pulse = {
        let v = song.row_subdiv;
        if v == 0.0 {
            1.0
        } else {
            v
        }
    }
    .max(1.0);
    let base = PAT_LEN_MAX.min(py_int(py_round(32.0 * pulse)) as i32);
    if total_rows <= 0 {
        return _pattern_len(last_row, PAT_LEN_MAX);
    }
    for mult in 1..9 {
        let cand = PAT_LEN_MAX.min(base * mult);
        let orders = (total_rows + cand - 1) / cand;
        if orders <= 250 {
            return _pattern_len(last_row, cand);
        }
        if cand == PAT_LEN_MAX {
            break;
        }
    }
    _pattern_len(last_row, PAT_LEN_MAX)
}

pub fn _cell_ed(cell: Option<&Row>) -> i32 {
    for &(c, v) in fx_slice(cell) {
        if c == 0xED {
            return v;
        }
    }
    0
}

pub fn _place_chip_fx(
    grid: &mut Grid,
    song: &Song,
    entries: &[(i64, i32, i32, i32, i32)],
    fx_cols: usize,
    total_rows: i32,
    content_cut: Option<i64>,
    mut stats: Option<&mut IndexMap<String, i64>>,
    max_ch: Option<i32>,
) {
    if entries.is_empty() {
        return;
    }
    let max_ch = match max_ch {
        None => grid.len() as i32,
        Some(v) => v.min(grid.len() as i32),
    };
    let row_len = song.samples_per_row();
    let has_speed = entries.iter().any(|entry| entry.1 == 0x17);
    let mut by_tick: IndexMap<(i32, i32), Vec<i32>> = IndexMap::new();
    let mut notes: Vec<(f64, i32, i32, i32)> = Vec::new();
    let mut times: Vec<f64> = Vec::new();
    if has_speed {
        let tick = row_len / song.speed.max(1) as f64;
        for ch in 0..max_ch {
            for (&row, cell) in grid[ch as usize].iter() {
                if 0 <= cell.note && cell.note < 180 {
                    by_tick
                        .entry((row, _cell_ed(Some(cell))))
                        .or_default()
                        .push(ch);
                }
            }
        }
        for (&(row, delay), chans) in by_tick.iter() {
            for &ch in chans {
                notes.push((
                    song.t0 as f64 + row as f64 * row_len + delay as f64 * tick,
                    ch,
                    row,
                    delay,
                ));
            }
        }
        notes.sort_by(|a, b| {
            a.0.partial_cmp(&b.0)
                .unwrap_or(Ordering::Equal)
                .then(a.1.cmp(&b.1))
                .then(a.2.cmp(&b.2))
                .then(a.3.cmp(&b.3))
        });
        times = notes.iter().map(|n| n.0).collect();
    }

    for entry in entries {
        let sample = entry.0;
        let cmd = entry.1;
        let val = entry.2;
        let tick_off = entry.3;
        let target_ch = entry.4;
        let target_ch = 0.max(target_ch.min(grid.len() as i32 - 1));
        let ch_lo = target_ch;
        let ch_hi = (grid.len() as i32).min(target_ch + 8);
        if let Some(cut) = content_cut {
            if sample >= cut {
                continue;
            }
        }
        if cmd != 0x17 {
            let row = song.row_of(sample);
            if row < 0 || row >= total_rows {
                continue;
            }
            let cell = grid[target_ch as usize].entry(row).or_insert_with(_blank_row);
            _add_fx(cell, cmd, val, fx_cols, stats.as_deref_mut());
            continue;
        }
        let mut target: Option<(f64, i32, i32, i32)> = None;
        let i = bisect_left_f64(&times, sample as f64);
        for j in [i as i64 - 1, i as i64] {
            if !(0 <= j && (j as usize) < notes.len()) {
                continue;
            }
            let cand = notes[j as usize];
            if !(ch_lo <= cand.1 && cand.1 < ch_hi) {
                continue;
            }
            if (cand.0 - sample as f64).abs() > row_len {
                continue;
            }
            if target.is_none()
                || (cand.0 - sample as f64).abs() < (target.unwrap().0 - sample as f64).abs()
            {
                target = Some(cand);
            }
        }

        let mut chosen: Option<(i32, i32)> = None;
        let mut delay = 0;
        let mut row;
        if let Some(t) = target {
            row = t.2;
            delay = t.3;
        } else {
            row = song.row_of(sample);
            if row < 0 || row >= total_rows {
                continue;
            }
        }
        if tick_off != 0 {
            let speed = song.speed.max(1);
            delay += tick_off;
            row += crate::parity::py_floor_div(delay as i64, speed as i64) as i32;
            delay = crate::parity::py_mod(delay as i64, speed as i64) as i32;
            if row < 0 || row >= total_rows {
                continue;
            }
        }
        if target.is_some() {
            if let Some(chans) = by_tick.get(&(row, delay)) {
                for &cand_ch in chans {
                    if !(ch_lo <= cand_ch && cand_ch < ch_hi) {
                        continue;
                    }
                    if !any_cmd(grid[cand_ch as usize].get(&row), cmd) {
                        chosen = Some((cand_ch, row));
                        break;
                    }
                }
            }
            if chosen.is_none() {
                for cand_ch in ch_lo..ch_hi {
                    let cell = grid[cand_ch as usize].get(&row);
                    if _cell_ed(cell) == delay && !any_cmd(cell, cmd) {
                        chosen = Some((cand_ch, row));
                        break;
                    }
                }
            }
        }
        if chosen.is_none() {
            for cand_ch in ch_lo..ch_hi {
                let cell = grid[cand_ch as usize].get(&row);
                let free = match cell {
                    None => true,
                    Some(c) => !(0 <= c.note && c.note < 180),
                };
                let ed = _cell_ed(cell);
                if free && (ed == 0 || ed == delay) && !any_cmd(cell, cmd) {
                    chosen = Some((cand_ch, row));
                    break;
                }
            }
        }
        let (ch, row) = chosen.unwrap_or((target_ch, row));
        let cell = grid[ch as usize].entry(row).or_insert_with(_blank_row);
        if any_cmd_val(Some(&*cell), cmd, val) {
            continue;
        }
        _add_fx(cell, cmd, val, fx_cols, stats.as_deref_mut());
        if delay != 0 && _cell_ed(Some(&*cell)) == 0 {
            _add_fx(cell, 0xED, delay, fx_cols, stats.as_deref_mut());
        }
    }
}

/// The OPN systems with a real hard-pan register. The YM2203 is mono.
const _OPN_KINDS: [&str; 3] = ["ym2612", "ym2608", "ym2610"];

pub fn _opn_module_channels(song: &Song, n_ch: i32, ch_offset: i32) -> BTreeSet<i32> {
    let mut out: BTreeSet<i32> = BTreeSet::new();
    if ch_offset != 0 {
        return out;
    }
    let mut base = 0;
    for system in &song.fm_systems {
        let span = _system_span(system);
        if _OPN_KINDS.contains(&system.kind.as_str()) {
            for ch in base..n_ch.min(base + system.fm_channels) {
                out.insert(ch);
            }
        }
        base += span;
    }
    out
}

pub fn _fit_fx_cols(patterns: &[(i32, i32, Vec<Option<Row>>)], n_ch: i32) -> Vec<i32> {
    let mut cols = vec![1i32; n_ch.max(0) as usize];
    for (ch, _idx, rows) in patterns {
        if !(0 <= *ch && *ch < n_ch) {
            continue;
        }
        let mut used = 0usize;
        for row in rows.iter().flatten() {
            let n = fx_slice(Some(row)).iter().filter(|&&(c, _)| c != EMPTY).count();
            if n > used {
                used = n;
            }
        }
        if used as i32 > cols[*ch as usize] {
            cols[*ch as usize] = used as i32;
        }
    }
    cols.into_iter().map(|c| c.min(8)).collect()
}

// ---------------------------------------------------------------------------
// Pattern assembly.

fn opl_reset_patch(patches: &[crate::fm::FMPatch], ins: i32) -> Option<&crate::fm::FMPatch> {
    if !(0 <= ins && (ins as usize) < patches.len()) {
        return None;
    }
    let p = &patches[ins as usize];
    if !p.opl || p.drum {
        return None;
    }
    Some(p)
}

/// Working state for `_build_patterns`. The Python nested closures are methods
/// so the grid and the event list can be borrowed independently.
struct PatternCtx<'a, 'h> {
    song: &'a Song,
    n_ch: i32,
    pat_len_max: i32,
    fx_cols: usize,
    ch_offset: i32,
    stats: Option<&'a mut IndexMap<String, i64>>,
    vsu_wave_base: i32,
    warnings: &'a mut Vec<String>,
    hooks: &'a mut EmitHooks<'h>,
    grid: Grid,
    total_rows: i32,
    content_end: Option<i32>,
    content_cut: Option<i64>,
    loop_row: i32,
    note_row_shift: IndexMap<usize, (i32, i32)>,
    fx_row_shift: IndexMap<(i32, i64), (i32, i32)>,
    shifted_rows: BTreeSet<(i32, i32)>,
    note_e5: IndexMap<(i32, i32), i32>,
    pcm_pos: IndexMap<usize, (i32, i32)>,
    note_ev: IndexMap<(i32, i32), usize>,
}

impl<'a, 'h> PatternCtx<'a, 'h> {
    /// Keep a pre-wrap event on the last loop row when its ED would roll.
    fn loop_clip(&self, row: i32, delay: i32, sample: i64) -> (i32, i32) {
        if let (Some(cut), Some(content_end)) = (self.content_cut, self.content_end) {
            if sample < cut && row > content_end && row <= content_end + 1 {
                let speed = self.song.speed.max(1);
                let tick = self.song.samples_per_row() / speed as f64;
                let offset = (sample - self.song.t0) as f64 / tick
                    - (content_end * speed) as f64;
                let d = py_int(py_round(offset)) as i32;
                return (content_end, (speed - 1).min(0.max(d)));
            }
        }
        (row, delay)
    }

    /// Keep a post-marker event on the loop's first row.
    fn loop_start(&self, row: i32, delay: i32, sample: i64) -> (i32, i32) {
        if let (Some(cut), Some(marker)) = (self.content_cut, self.song.loop_marker_sample) {
            let _ = cut;
            if sample >= marker && row < self.loop_row && row >= self.loop_row - 1 {
                return (self.loop_row, 0);
            }
        }
        (row, delay)
    }

    fn placed_note_row(&mut self, idx: usize) -> i32 {
        if let Some(&(row, _)) = self.note_row_shift.get(&idx) {
            return row;
        }
        let (pcm, sample) = {
            let ev = &self.song.events[idx];
            (ev.pcm, ev.sample)
        };
        let (row, delay);
        if pcm {
            match self.pcm_pos.get(&idx) {
                Some(&(r, d)) => {
                    row = r;
                    delay = d;
                }
                None => return -1,
            }
        } else {
            let p = self.song.place(sample, self.stats.as_deref_mut());
            row = p.0;
            delay = p.1;
        }
        let (row2, delay2) = self.loop_clip(row, delay, sample);
        let (row3, _d3) = self.loop_start(row2, delay2, sample);
        row3
    }

    fn build(mut self) -> (Vec<Vec<i32>>, Vec<(i32, i32, Vec<Option<Row>>)>, i32, i32) {
        let song = self.song;
        let mut last_sample = song.events.last().map(|e| e.sample).unwrap_or(0);
        if song
            .events
            .iter()
            .any(|ev| ev.on && !ev.pcm && !ev.dac && ev.tone_chip == 0 && ev.end == 0)
        {
            last_sample = last_sample.max(song.vgm.total_samples);
        }
        for ev in &song.events {
            if let Some(&(s, _, _)) = ev.vol_pts.last() {
                last_sample = last_sample.max(s);
            }
            if let Some(&(s, _, _, _, _)) = ev.quad_pts.last() {
                last_sample = last_sample.max(s);
            }
            if ev.on && ev.end != 0 {
                last_sample = last_sample.max(ev.end);
            }
        }
        for entry in &song.channel_fx {
            last_sample = last_sample.max(entry.0);
        }
        for entry in &song.chip_fx {
            last_sample = last_sample.max(entry.0);
        }
        let adjusted = song.loop_sample.is_some() && song.loop_end_sample.is_some();
        let mut loop_row: i32 = if let Some(r) = song.loop_start_row {
            r
        } else if song.loop_sample.is_none() {
            0
        } else if adjusted {
            song.place(song.loop_sample.unwrap(), None).0
        } else {
            song.row_of(song.loop_sample.unwrap())
        };
        if loop_row < 0 {
            loop_row = 0;
        }
        let mut content_end: Option<i32> = None;
        let mut content_cut: Option<i64> = None;
        if adjusted {
            let ce = match song.loop_end_row {
                Some(r) => r,
                None => song.place(song.loop_end_sample.unwrap(), None).0,
            };
            if ce < loop_row {
                content_end = None;
            } else {
                content_end = Some(ce);
                content_cut = song.loop_cut_sample;
            }
        }
        let total_rows: i32;
        if let Some(ce) = content_end {
            total_rows = ce + 1;
        } else {
            let ce = song.place(last_sample, self.stats.as_deref_mut()).0;
            content_end = Some(ce);
            total_rows = (ce + 2).max(1);
        }
        self.total_rows = total_rows;
        self.content_end = content_end;
        self.content_cut = content_cut;
        self.loop_row = loop_row;
        let n_ch = self.n_ch;
        self.grid = (0..n_ch.max(0)).map(|_| IndexMap::new()).collect();
        if self.stats.is_some() {
            let cur = self
                .stats
                .as_ref()
                .unwrap()
                .get("rows")
                .copied()
                .unwrap_or(0);
            self.stats
                .as_mut()
                .unwrap()
                .insert("rows".to_string(), cur.max(total_rows as i64));
        }

        let row_samples = song.samples_per_row();
        // A condensed row can catch two tone notes at once. Pin the earlier
        // one to the end of the previous row.
        let mut tone_rows: IndexMap<i32, IndexMap<i32, Vec<(usize, i32)>>> = IndexMap::new();
        for idx in 0..song.events.len() {
            let ev = &song.events[idx];
            if !(ev.on && ev.tone_chip != 0) {
                continue;
            }
            let ev_ch = ev.ch + self.ch_offset;
            if ev_ch < 0 || ev_ch >= n_ch {
                continue;
            }
            if let Some(cut) = content_cut {
                if ev.sample >= cut {
                    continue;
                }
            }
            let p = song.place(ev.sample, self.stats.as_deref_mut());
            let (row, delay) = self.loop_clip(p.0, p.1, ev.sample);
            let (row, delay) = self.loop_start(row, delay, ev.sample);
            if 0 <= row && row < total_rows {
                tone_rows
                    .entry(ev_ch)
                    .or_default()
                    .entry(row)
                    .or_default()
                    .push((idx, delay));
            }
        }
        let mut used_rows: BTreeSet<(i32, i32)> = BTreeSet::new();
        for (ch, rows) in &tone_rows {
            for &row in rows.keys() {
                used_rows.insert((*ch, row));
            }
        }
        let shift_delay = 255.min(1.max(song.speed - 1));
        let tone_chs: Vec<i32> = tone_rows.keys().copied().collect();
        for ev_ch in tone_chs {
            let mut row_keys: Vec<i32> = tone_rows[&ev_ch].keys().copied().collect();
            row_keys.sort();
            for row in row_keys {
                let group = tone_rows.get_mut(&ev_ch).unwrap().get_mut(&row).unwrap();
                if group.len() < 2 {
                    continue;
                }
                group.sort_by_key(|&(i, _)| song.events[i].sample);
                let mut i = 0usize;
                while i + 1 < group.len() {
                    let (prev_idx, _pdelay) = group[i];
                    let (ev_idx, _edelay) = group[i + 1];
                    i += 1;
                    let prev = &song.events[prev_idx];
                    if !matches!(prev.tone_chip, 1 | 2 | 3 | 4 | 5 | 8)
                        || !prev.vol_pts.is_empty()
                        || prev.end == 0
                        || prev.end > song.events[ev_idx].sample
                        || (row_samples != 0.0
                            && (prev.end - prev.sample) as f64 >= row_samples)
                    {
                        continue;
                    }
                    let dest = row - 1;
                    if dest >= 0 && !used_rows.contains(&(ev_ch, dest)) {
                        self.note_row_shift
                            .insert(prev_idx, (dest, shift_delay));
                        self.fx_row_shift.insert(
                            (ev_ch, prev.sample),
                            (dest, shift_delay),
                        );
                        self.shifted_rows.insert((ev_ch, dest));
                        used_rows.insert((ev_ch, dest));
                        _bump(self.stats.as_deref_mut(), "note_shifted", 1);
                        continue;
                    }
                    if group.len() != 2 {
                        continue;
                    }
                    let dest = row + 1;
                    if dest >= total_rows || used_rows.contains(&(ev_ch, dest)) {
                        continue;
                    }
                    self.note_row_shift.insert(ev_idx, (dest, 0));
                    self.fx_row_shift
                        .insert((ev_ch, song.events[ev_idx].sample), (dest, 0));
                    used_rows.insert((ev_ch, dest));
                    _bump(self.stats.as_deref_mut(), "note_shifted", 1);
                }
            }
        }

        // Chip-wide effects are placed after the notes.
        let mut note_delay: IndexMap<(i32, i32), i32> = IndexMap::new();
        for (ev_ch, rows) in &tone_rows {
            for (&row, group) in rows {
                for &(ev_idx, delay) in group {
                    let shifted = self.note_row_shift.get(&ev_idx);
                    let (dest, dest_delay) = match shifted {
                        Some(&(d, dd)) => (d, dd),
                        None => (row, delay),
                    };
                    let key = (*ev_ch, dest);
                    match note_delay.get(&key) {
                        Some(&prev) => {
                            note_delay.insert(key, prev.min(dest_delay));
                        }
                        None => {
                            note_delay.insert(key, dest_delay);
                        }
                    }
                }
            }
        }
        let mut last_fx: IndexMap<(i64, i32, i32), i32> = IndexMap::new();
        for &(sample, ch, cmd, val) in &song.channel_fx {
            last_fx.insert((sample, ch, cmd), val);
        }
        let mut emitted_fx: BTreeSet<(i64, i32, i32)> = BTreeSet::new();
        let mut channel_fx: Vec<(i64, i32, i32, i32)> = Vec::new();
        for &(sample, ch, cmd, _val) in &song.channel_fx {
            let key = (sample, ch, cmd);
            if emitted_fx.contains(&key) {
                continue;
            }
            emitted_fx.insert(key);
            channel_fx.push((sample, ch, cmd, last_fx[&key]));
        }
        for &(sample, ch0, cmd, mut val) in &channel_fx {
            if cmd == 0x10 && val >= 0x1000 {
                let idx = self.vsu_wave_base + (val & 0x0FFF);
                if idx > 0xFF {
                    _bump(self.stats.as_deref_mut(), "vsu_wave_dropped", 1);
                    continue;
                }
                val = idx;
            }
            let ch = ch0 + self.ch_offset;
            if ch < 0 || ch >= n_ch {
                continue;
            }
            if let Some(cut) = content_cut {
                if sample >= cut {
                    continue;
                }
            }
            let (mut row, mut delay) = match self.fx_row_shift.get(&(ch, sample)) {
                Some(&(r, d)) => (r, d),
                None => {
                    let p = song.place(sample, self.stats.as_deref_mut());
                    let (r0, d0) = self.loop_clip(p.0, p.1, sample);
                    self.loop_start(r0, d0, sample)
                }
            };
            if row < 0 || row >= total_rows {
                continue;
            }
            if let Some(&note_tick) = note_delay.get(&(ch, row)) {
                if delay > note_tick && row + 1 < total_rows {
                    row += 1;
                    delay = 0;
                }
            }
            let cell = self.grid[ch as usize].entry(row).or_insert_with(_blank_row);
            let conflict = fx_slice(Some(&*cell))
                .iter()
                .any(|&(c, v)| c == cmd && v != val && c != EMPTY);
            if conflict {
                let nrow = row + 1;
                if nrow >= total_rows {
                    continue;
                }
                row = nrow;
                delay = 0;
            }
            let cell = self.grid[ch as usize].entry(row).or_insert_with(_blank_row);
            _add_fx(cell, cmd, val, self.fx_cols, self.stats.as_deref_mut());
            if delay != 0 && _cell_ed(Some(&*cell)) == 0 {
                _add_fx(cell, 0xED, delay, self.fx_cols, self.stats.as_deref_mut());
            }
        }

        let opn_pan_ch = _opn_module_channels(song, n_ch, self.ch_offset);
        let fm_vol_max = song.fm_vol_max.max(1);
        let vol_max = _channel_vol_max(song, n_ch, fm_vol_max, self.ch_offset);
        let fm_tl = song.events.iter().any(|ev| {
            ev.on
                && !ev.pcm
                && !ev.tl_pts.is_empty()
                && 0 <= ev.ch + self.ch_offset
                && ev.ch + self.ch_offset < n_ch
        });
        self.pcm_pos = _assign_pcm_rows(
            song,
            total_rows,
            last_sample,
            self.stats.as_deref_mut(),
            self.content_end,
            self.content_cut,
        );
        let pcm_pos = self.pcm_pos.clone();

        let mut legato_on: IndexMap<i32, bool> = IndexMap::new();
        let mut next_on: IndexMap<i32, Vec<(i64, usize)>> = IndexMap::new();
        for (idx, ev) in song.events.iter().enumerate() {
            if ev.on && !ev.pcm {
                let ev_ch = ev.ch + self.ch_offset;
                if 0 <= ev_ch && ev_ch < n_ch {
                    next_on.entry(ev_ch).or_default().push((ev.sample, idx));
                }
            }
        }
        for entries in next_on.values_mut() {
            entries.sort_by_key(|item| item.0);
        }

        for idx in 0..song.events.len() {
            let ev_ch = song.events[idx].ch + self.ch_offset;
            if ev_ch < 0 || ev_ch >= n_ch {
                continue;
            }
            if let Some(cut) = content_cut {
                if song.events[idx].sample >= cut {
                    continue;
                }
            }
            let (on, pcm) = (song.events[idx].on, song.events[idx].pcm);
            if on {
                _bump(self.stats.as_deref_mut(), "keyons", 1);
            }
            let (mut row, mut delay);
            if pcm {
                match pcm_pos.get(&idx) {
                    None => {
                        _bump(
                            self.stats.as_deref_mut(),
                            if on { "pcm_unplaced" } else { "pcm_off_skipped" },
                            1,
                        );
                        continue;
                    }
                    Some(&(r, d)) => {
                        row = r;
                        delay = d;
                    }
                }
            } else {
                let p = song.place(song.events[idx].sample, self.stats.as_deref_mut());
                row = p.0;
                delay = p.1;
            }
            let (r, d) = self.loop_clip(row, delay, song.events[idx].sample);
            let (r, d) = self.loop_start(r, d, song.events[idx].sample);
            row = r;
            delay = d;
            if on {
                if let Some(&(r, d)) = self.note_row_shift.get(&idx) {
                    row = r;
                    delay = d;
                }
            }
            if row < 0 || row >= total_rows {
                continue;
            }
            let cell = self.grid[ev_ch as usize].entry(row).or_insert_with(_blank_row);
            if !on {
                if pcm {
                    if cell.note >= 0 && cell.note != NOTE_OFF {
                        _bump(self.stats.as_deref_mut(), "pcm_off_collision", 1);
                        continue;
                    }
                    cell.note = NOTE_OFF;
                    if delay != 0 {
                        _add_fx(
                            cell,
                            0xED,
                            delay,
                            self.fx_cols,
                            self.stats.as_deref_mut(),
                        );
                    }
                    continue;
                }
                if self.shifted_rows.contains(&(ev_ch, row))
                    && cell.note >= 0
                    && cell.note != NOTE_OFF
                {
                    _bump(self.stats.as_deref_mut(), "off_swallowed", 1);
                    continue;
                }
                let mut started = 0;
                for &(cmd, val) in fx_slice(Some(&*cell)) {
                    if cmd == 0xED {
                        started = val;
                        break;
                    }
                }
                let started_ev = self
                    .note_ev
                    .get(&(ev_ch, row))
                    .map(|&i| &song.events[i]);
                if cell.note < 0 || cell.note == NOTE_OFF {
                    cell.note = NOTE_OFF;
                    if delay != 0 {
                        _add_fx(
                            cell,
                            0xED,
                            delay,
                            self.fx_cols,
                            self.stats.as_deref_mut(),
                        );
                    }
                } else if let Some(sev) = started_ev {
                    if song.events[idx].sample > sev.sample {
                        // Python: max(started + 1, delay) for SN76489/T6W28,
                        // else max(1, delay - started).
                        let ec = if sev.tone_chip == 3 {
                            (started + 1).max(delay)
                        } else {
                            1.max(delay - started)
                        };
                        let speed_now = song.speed.max(1);
                        let nxt = next_on
                            .get(&ev_ch)
                            .and_then(|v| v.iter().copied().find(|n| n.0 >= song.events[idx].sample));
                        if let Some(nxt) = nxt {
                            let p = song.place(nxt.0, self.stats.as_deref_mut());
                            let cut_tick = row * speed_now + started + ec;
                            let next_tick = p.0 * speed_now + p.1;
                            if cut_tick >= next_tick {
                                _bump(self.stats.as_deref_mut(), "off_swallowed", 1);
                                continue;
                            }
                        }
                        _add_fx(
                            cell,
                            0xEC,
                            ec,
                            self.fx_cols,
                            self.stats.as_deref_mut(),
                        );
                    } else if delay > started {
                        _bump(self.stats.as_deref_mut(), "off_swallowed", 1);
                    }
                } else if delay > started {
                    _bump(self.stats.as_deref_mut(), "off_swallowed", 1);
                }
                continue;
            }

            if 0 <= cell.note && cell.note < 180 {
                let prev = self.note_ev.get(&(ev_ch, row)).copied();
                let move_back = match prev {
                    Some(pi) => {
                        let p = &song.events[pi];
                        matches!(p.tone_chip, 2 | 8)
                            && p.vol_pts.is_empty()
                            && p.end != 0
                            && p.end <= song.events[idx].sample
                            && (row_samples == 0.0
                                || ((p.end - p.sample) as f64) < row_samples)
                            && row > 0
                            && !self.grid[ev_ch as usize].contains_key(&(row - 1))
                    }
                    None => false,
                };
                if move_back {
                    let pi = prev.unwrap();
                    let old = self.grid[ev_ch as usize].get(&row).unwrap().clone();
                    self.grid[ev_ch as usize].insert(row - 1, old);
                    self.note_ev.insert((ev_ch, row - 1), pi);
                    self.note_ev.shift_remove(&(ev_ch, row));
                    if let Some(v) = self.note_e5.shift_remove(&(ev_ch, row)) {
                        self.note_e5.insert((ev_ch, row - 1), v);
                    }
                    self.grid[ev_ch as usize].insert(row, _blank_row());
                    _bump(self.stats.as_deref_mut(), "note_shifted", 1);
                } else {
                    _bump(self.stats.as_deref_mut(), "note_collision", 1);
                }
            }
            let cell = self.grid[ev_ch as usize].get_mut(&row).unwrap();
            cell.note = if song.events[idx].note != 180 {
                song.events[idx].note
            } else {
                NOTE_OFF
            };
            let ev = &song.events[idx];
            if ev.legato {
                _add_fx(cell, 0xEA, 1, self.fx_cols, self.stats.as_deref_mut());
                legato_on.insert(ev_ch, true);
            } else if legato_on.shift_remove(&ev_ch).unwrap_or(false) {
                _add_fx(cell, 0xEA, 0, self.fx_cols, self.stats.as_deref_mut());
            }
            if ev.ins >= 0 {
                cell.ins = ev.ins;
            }
            if ev.vol >= 0 {
                if ev.vol_slide != 0 {
                    if ev.vol_slide == 1 {
                        cell.vol = _slide_level(ev.vol);
                        _add_fx(cell, 0xFA, 0x01, self.fx_cols, self.stats.as_deref_mut());
                    } else {
                        cell.vol = ev.vol;
                        _add_fx(cell, 0xFA, 0x00, self.fx_cols, self.stats.as_deref_mut());
                    }
                } else {
                    cell.vol = ev.vol;
                }
            } else if fm_tl && cell.vol == EMPTY {
                cell.vol = 127;
            }
            if delay != 0 {
                _add_fx(cell, 0xED, delay, self.fx_cols, self.stats.as_deref_mut());
            }
            self.note_e5.insert((ev_ch, row), ev.e5);
            self.note_ev.insert((ev_ch, row), idx);
            if let Some(quad) = ev.quad {
                _add_quad_fx(
                    cell,
                    quad,
                    self.fx_cols,
                    self.stats.as_deref_mut(),
                    None,
                );
            } else if !ev.no_pan
                && (ev.pcm
                    || ev.pan != 0
                    || matches!(ev.tone_chip, 7 | 10)
                    || opn_pan_ch.contains(&ev_ch))
            {
                _add_fx(
                    cell,
                    0x08,
                    ev.pan & 0xFF,
                    self.fx_cols,
                    self.stats.as_deref_mut(),
                );
            }
            if ev.c219_noise {
                _add_fx(cell, 0x11, 1, self.fx_cols, self.stats.as_deref_mut());
            }
            if ev.c219_mode != 0 {
                _add_fx(
                    cell,
                    0x12,
                    ev.c219_mode & 0xFF,
                    self.fx_cols,
                    self.stats.as_deref_mut(),
                );
            }
        }

        // A PCM note still sounding at the last content row gets an explicit off.
        for ch in 0..n_ch {
            let mut last: Option<usize> = None;
            for (idx, ev) in song.events.iter().enumerate() {
                if ev.on
                    && ev.pcm
                    && ev.ch + self.ch_offset == ch
                    && pcm_pos.contains_key(&idx)
                {
                    if let Some(cut) = content_cut {
                        if ev.sample >= cut {
                            continue;
                        }
                    }
                    last = Some(idx);
                }
            }
            let last = match last {
                None => continue,
                Some(v) => v,
            };
            if let Some(cut) = content_cut {
                let lev = &song.events[last];
                if lev.end == 0 || lev.end >= cut {
                    continue;
                }
            }
            let on_row = self.pcm_pos[&last].0;
            let has_off = song.events.iter().enumerate().any(|(idx, ev)| {
                !ev.on
                    && ev.pcm
                    && ev.ch + self.ch_offset == ch
                    && ev.sample > song.events[last].sample
                    && content_cut.map_or(true, |c| ev.sample < c)
                    && self.pcm_pos.contains_key(&idx)
                    && self.pcm_pos[&idx].0 > on_row
            });
            if has_off {
                continue;
            }
            let mut target = self.content_end.unwrap_or(total_rows - 1);
            if song.events[last].end != 0 {
                target = target.min(
                    song
                        .place(song.events[last].end, self.stats.as_deref_mut())
                        .0,
                );
            }
            if target <= on_row {
                target = on_row + 1;
            }
            if target >= total_rows {
                continue;
            }
            let cell = self.grid[ch as usize].entry(target).or_insert_with(_blank_row);
            if cell.note >= 0 && cell.note != NOTE_OFF {
                continue;
            }
            cell.note = NOTE_OFF;
        }

        _place_chip_fx(
            &mut self.grid,
            song,
            &song.chip_fx,
            self.fx_cols,
            self.total_rows,
            self.content_cut,
            self.stats.as_deref_mut(),
            None,
        );

        // A held note's carrier fade steps land on their own rows.
        if fm_tl {
            for idx in 0..song.events.len() {
                let ev_ch = song.events[idx].ch + self.ch_offset;
                {
                    let ev = &song.events[idx];
                    if !ev.on || ev.pcm || ev.tl_pts.is_empty() || !(0 <= ev_ch && ev_ch < n_ch) {
                        continue;
                    }
                }
                let t0 = {
                    let ev = &song.events[idx];
                    let mut t0 = ev.vol_tl_base;
                    if t0.is_none() {
                        t0 = if 0 <= ev.ins && (ev.ins as usize) < song.fm_patches.len() {
                            Some(song.fm_patches[ev.ins as usize].carrier_tl())
                        } else {
                            Some(0x7F)
                        };
                    }
                    t0.unwrap()
                };
                for &(sample, tl) in &song.events[idx].tl_pts {
                    if let Some(cut) = content_cut {
                        if sample >= cut {
                            continue;
                        }
                    }
                    let (row, _delay) = song.place(sample, self.stats.as_deref_mut());
                    if row < 0 || row >= total_rows {
                        continue;
                    }
                    let cell = self.grid[ev_ch as usize].entry(row).or_insert_with(_blank_row);
                    if cell.note >= 0 {
                        _bump(self.stats.as_deref_mut(), "vol_step_dropped", 1);
                        continue;
                    }
                    let top = if (ev_ch as usize) < vol_max.len() {
                        vol_max[ev_ch as usize]
                    } else {
                        fm_vol_max
                    };
                    cell.vol = (top - (tl - t0)).clamp(0, top);
                }
            }
        }

        // Pitch sweeps run after every event is placed.
        let mut note_rows: IndexMap<i32, Vec<i32>> = IndexMap::new();
        for ch in 0..n_ch {
            let mut rows: Vec<i32> = self.grid[ch as usize]
                .iter()
                .filter(|(_, cell)| 0 <= cell.note && cell.note < 180)
                .map(|(&r, _)| r)
                .collect();
            rows.sort();
            note_rows.insert(ch, rows);
        }
        let mut sweep_e5: IndexMap<(i32, i32), i32> = IndexMap::new();
        for idx in 0..song.events.len() {
            let (on, has_steps, ev_ch, sample) = {
                let ev = &song.events[idx];
                (
                    ev.on,
                    !ev.pitch_steps.is_empty(),
                    ev.ch + self.ch_offset,
                    ev.sample,
                )
            };
            if !(on
                && has_steps
                && 0 <= ev_ch
                && ev_ch < n_ch
                && content_cut.map_or(true, |c| sample < c))
            {
                continue;
            }
            let note_row = self.placed_note_row(idx);
            let ev = &song.events[idx];
            if self.ch_offset != 0 {
                _emit_steps(
                    &mut self.grid,
                    song,
                    ev,
                    self.fx_cols,
                    total_rows,
                    &mut sweep_e5,
                    Some(ev.ch + self.ch_offset),
                    self.stats.as_deref_mut(),
                    Some(note_row),
                );
            } else {
                _emit_steps(
                    &mut self.grid,
                    song,
                    ev,
                    self.fx_cols,
                    total_rows,
                    &mut sweep_e5,
                    None,
                    self.stats.as_deref_mut(),
                    Some(note_row),
                );
            }
        }
        for idx in 0..song.events.len() {
            {
                let ev = &song.events[idx];
                let ev_ch = ev.ch + self.ch_offset;
                if !(ev.on
                    && 0 <= ev_ch
                    && ev_ch < n_ch
                    && (!ev.op_mul_fx.is_empty()
                        || !ev.op_dt_fx.is_empty()
                        || !ev.op_dt2_fx.is_empty())
                    && content_cut.map_or(true, |c| ev.sample < c))
                {
                    continue;
                }
            }
            let note_row = self.placed_note_row(idx);
            if note_row < 0 {
                continue;
            }
            let chan = if self.ch_offset != 0 {
                Some(song.events[idx].ch + self.ch_offset)
            } else {
                None
            };
            for code in [0x16, 0x53, 0x55] {
                let points_src = match code {
                    0x16 => &song.events[idx].op_mul_fx,
                    0x53 => &song.events[idx].op_dt_fx,
                    _ => &song.events[idx].op_dt2_fx,
                };
                let points: Vec<(i64, i32, i32)> = match content_cut {
                    Some(cut) => points_src
                        .iter()
                        .copied()
                        .filter(|p| p.0 < cut)
                        .collect(),
                    None => Vec::new(),
                };
                let ev = &song.events[idx];
                let points_ref: &[(i64, i32, i32)] = if content_cut.is_some() {
                    &points
                } else {
                    points_src.as_slice()
                };
                if points_ref.is_empty() {
                    continue;
                }
                _emit_op_fx(
                    &mut self.grid,
                    song,
                    ev,
                    points_ref,
                    code,
                    self.fx_cols,
                    total_rows,
                    chan,
                    self.stats.as_deref_mut(),
                    Some(note_row),
                );
            }
        }
        for idx in 0..song.events.len() {
            {
                let ev = &song.events[idx];
                let ev_ch = ev.ch + self.ch_offset;
                if !(ev.on
                    && !ev.sweep.is_empty()
                    && 0 <= ev_ch
                    && ev_ch < n_ch
                    && content_cut.map_or(true, |c| ev.sample < c))
                {
                    continue;
                }
            }
            let ev = &song.events[idx];
            let chan = if self.ch_offset != 0 {
                Some(ev.ch + self.ch_offset)
            } else {
                None
            };
            if let Some(hook) = self.hooks.sweep_hook.as_mut() {
                hook(
                    &mut self.grid,
                    song,
                    ev,
                    self.fx_cols,
                    total_rows,
                    &mut sweep_e5,
                    &note_rows,
                    chan,
                    self.stats.as_deref_mut(),
                );
            } else {
                _emit_sweep(
                    &mut self.grid,
                    song,
                    ev,
                    self.fx_cols,
                    total_rows,
                    &mut sweep_e5,
                    &note_rows,
                    chan,
                    self.stats.as_deref_mut(),
                );
            }
        }

        // C140 volume steps.
        for idx in 0..song.events.len() {
            let ev_ch = song.events[idx].ch + self.ch_offset;
            {
                let ev = &song.events[idx];
                if !(ev.on && !ev.vol_pts.is_empty()) || ev_ch < 0 || ev_ch >= n_ch {
                    continue;
                }
            }
            let own_row = self.placed_note_row(idx);
            if own_row < 0 {
                continue;
            }
            let next_row = note_rows
                .get(&ev_ch)
                .and_then(|rows| rows.iter().copied().find(|&r| r > own_row))
                .unwrap_or(total_rows);
            let ev = &song.events[idx];
            let mut last_pan: Option<i32> = if ev.no_pan { None } else { Some(ev.pan) };
            let mut note_vol_done = false;
            for &(sample, value, pan) in &ev.vol_pts {
                if let Some(cut) = content_cut {
                    if sample >= cut {
                        continue;
                    }
                }
                let (mut row, mut delay) = song.place(sample, self.stats.as_deref_mut());
                if ev.vol_slide == 0 {
                    row = song.row_of(sample);
                    delay = 0;
                }
                if row < 0 || row >= total_rows || row >= next_row {
                    continue;
                }
                if ev.pcm && row <= own_row {
                    continue;
                }
                let cell = self.grid[ev_ch as usize].entry(row).or_insert_with(_blank_row);
                let first_at_note = cell.note >= 0
                    && ev.tone_chip != 0
                    && note_rows
                        .get(&ev_ch)
                        .map_or(false, |rows| rows.contains(&row))
                    && !note_vol_done;
                if first_at_note {
                    note_vol_done = true;
                    if ev.vol_slide != 0 {
                        let cont = matches!(pan, Some(v) if v != 0);
                        let level = if cont { _slide_level(value) } else { value };
                        cell.vol = level;
                        _add_fx(
                            cell,
                            0xFA,
                            if cont { 0x01 } else { 0x00 },
                            self.fx_cols,
                            self.stats.as_deref_mut(),
                        );
                        if delay != 0 {
                            _add_fx(
                                cell,
                                0xED,
                                delay,
                                self.fx_cols,
                                self.stats.as_deref_mut(),
                            );
                        }
                    } else if value == 0 {
                        continue;
                    } else if value > cell.vol {
                        let target = row + 1;
                        if target < next_row {
                            let tcell = self.grid[ev_ch as usize]
                                .entry(target)
                                .or_insert_with(_blank_row);
                            if !(0 <= tcell.note && tcell.note < 180) {
                                tcell.vol = value;
                                if let Some(p) = pan {
                                    if last_pan.is_none() || Some(p) != last_pan {
                                        _add_fx(
                                            tcell,
                                            0x08,
                                            p & 0xFF,
                                            self.fx_cols,
                                            self.stats.as_deref_mut(),
                                        );
                                        last_pan = Some(p);
                                    }
                                }
                            }
                        }
                    } else {
                        cell.vol = value;
                        if let Some(p) = pan {
                            if last_pan.is_none() || Some(p) != last_pan {
                                _add_fx(
                                    cell,
                                    0x08,
                                    p & 0xFF,
                                    self.fx_cols,
                                    self.stats.as_deref_mut(),
                                );
                                last_pan = Some(p);
                            }
                        }
                    }
                    continue;
                }
                if cell.note >= 0 {
                    _bump(self.stats.as_deref_mut(), "vol_step_dropped", 1);
                    continue;
                }
                if ev.vol_slide != 0 {
                    let cont = matches!(pan, Some(v) if v != 0);
                    let level = if cont { _slide_level(value) } else { value };
                    if cell.vol != EMPTY && cell.vol != level {
                        _bump(self.stats.as_deref_mut(), "vol_step_dropped", 1);
                    }
                    cell.vol = level;
                    _add_fx(
                        cell,
                        0xFA,
                        if cont { 0x01 } else { 0x00 },
                        self.fx_cols,
                        self.stats.as_deref_mut(),
                    );
                    if delay != 0 {
                        _add_fx(cell, 0xED, delay, self.fx_cols, self.stats.as_deref_mut());
                    }
                    continue;
                }
                cell.vol = value;
                if let Some(p) = pan {
                    if last_pan.is_none() || Some(p) != last_pan {
                        _add_fx(
                            cell,
                            0x08,
                            p & 0xFF,
                            self.fx_cols,
                            self.stats.as_deref_mut(),
                        );
                        last_pan = Some(p);
                    }
                }
            }
        }

        // A quad C352 voice carries its level on the four pan slots.
        for idx in 0..song.events.len() {
            let ev_ch = song.events[idx].ch + self.ch_offset;
            {
                let ev = &song.events[idx];
                if !(ev.on && !ev.quad_pts.is_empty()) || ev_ch < 0 || ev_ch >= n_ch {
                    continue;
                }
            }
            let mut prev = song.events[idx].quad;
            for &(sample, fl, fr, rl, rr) in &song.events[idx].quad_pts {
                if let Some(cut) = content_cut {
                    if sample >= cut {
                        continue;
                    }
                }
                let (row, _delay) = song.place(sample, self.stats.as_deref_mut());
                if row < 0 || row >= total_rows {
                    continue;
                }
                let cell = self.grid[ev_ch as usize].entry(row).or_insert_with(_blank_row);
                if cell.note >= 0 {
                    _bump(self.stats.as_deref_mut(), "vol_step_dropped", 1);
                    continue;
                }
                let four = (fl, fr, rl, rr);
                _add_quad_fx(
                    cell,
                    four,
                    self.fx_cols,
                    self.stats.as_deref_mut(),
                    prev,
                );
                prev = Some(four);
            }
        }

        // An OPL modulator level that differs from the shared baseline.
        for idx in 0..song.events.len() {
            let ev_ch = song.events[idx].ch + self.ch_offset;
            {
                let ev = &song.events[idx];
                if !(ev.on && !ev.op_tl_fx.is_empty()) || ev_ch < 0 || ev_ch >= n_ch {
                    continue;
                }
            }
            for &(sample, op, value) in &song.events[idx].op_tl_fx {
                if let Some(cut) = content_cut {
                    if sample >= cut {
                        continue;
                    }
                }
                let (row, delay) = song.place(sample, self.stats.as_deref_mut());
                if row < 0 || row >= total_rows {
                    continue;
                }
                let cell = self.grid[ev_ch as usize].entry(row).or_insert_with(_blank_row);
                _add_fx(
                    cell,
                    0x12 + op,
                    value & 0x3F,
                    self.fx_cols,
                    self.stats.as_deref_mut(),
                );
                if delay != 0 {
                    _add_fx(cell, 0xED, delay, self.fx_cols, self.stats.as_deref_mut());
                }
            }
        }

        // E5 is channel state; replay it per row, back to the neutral 0x80.
        for ch in 0..n_ch {
            let mut cur_e5 = 0x80i32;
            let mut rows: BTreeSet<i32> = BTreeSet::new();
            for &(c, r) in self.note_e5.keys() {
                if c == ch {
                    rows.insert(r);
                }
            }
            for &(c, r) in sweep_e5.keys() {
                if c == ch {
                    rows.insert(r);
                }
            }
            for row in rows {
                let val = match self.note_e5.get(&(ch, row)) {
                    Some(&v) => v,
                    None => match sweep_e5.get(&(ch, row)) {
                        Some(&v) => v,
                        None => continue,
                    },
                };
                if val == cur_e5 {
                    continue;
                }
                let cell = self.grid[ch as usize].entry(row).or_insert_with(_blank_row);
                _add_fx(cell, 0xE5, val, self.fx_cols, self.stats.as_deref_mut());
                cur_e5 = val;
            }
        }

        // On the SN76489/T6W28 path ECxx must land after EDxx.
        for ch in 0..n_ch {
            let keys: Vec<i32> = self.grid[ch as usize].keys().copied().collect();
            for row in keys {
                let note_idx = self.note_ev.get(&(ch, row)).copied();
                let cell = self.grid[ch as usize].get_mut(&row).unwrap();
                if cell.fx.is_none() {
                    continue;
                }
                let tone_chip = match note_idx {
                    Some(i) => song.events[i].tone_chip,
                    None => 0,
                };
                if tone_chip != 3 {
                    continue;
                }
                let ed = fx_slice(Some(&*cell))
                    .iter()
                    .find(|&&(c, _)| c == 0xED)
                    .map(|&(_, v)| v);
                if ed.is_none() {
                    continue;
                }
                let ec = fx_slice(Some(&*cell))
                    .iter()
                    .find(|&&(c, _)| c == 0xEC)
                    .map(|&(_, v)| v);
                if let (Some(ec), Some(ed)) = (ec, ed) {
                    if ec <= ed {
                        if let Some(fx) = cell.fx.as_mut() {
                            fx.retain(|&(c, _)| c != 0xEC);
                        }
                    }
                }
            }
        }

        let last_content = self
            .grid
            .iter()
            .flat_map(|cells| cells.keys().copied())
            .max()
            .unwrap_or(-1);

        let patches = &song.fm_patches;
        if patches.iter().any(|p| p.opl && !p.drum) {
            for ch in 0..n_ch {
                if self.grid[ch as usize].is_empty() {
                    continue;
                }
                let mut carried = -1i32;
                let mut prev_ins = -1i32;
                let mut reset_rows: BTreeSet<i32> = BTreeSet::new();
                let mut keys: Vec<i32> = self.grid[ch as usize].keys().copied().collect();
                keys.sort();
                for row in keys {
                    let cells = &self.grid[ch as usize];
                    let cell = &cells[&row];
                    if cell.ins >= 0 {
                        carried = cell.ins;
                    }
                    if !(0 <= cell.note && cell.note < 180) {
                        continue;
                    }
                    let cur = opl_reset_patch(patches, carried);
                    let prev = opl_reset_patch(patches, prev_ins);
                    if let (Some(cur), Some(prev)) = (cur, prev) {
                        let cur_ops = if cur.four { 4 } else { 2 };
                        let prev_ops = if prev.four { 4 } else { 2 };
                        let mut hit = false;
                        for i in 0..cur_ops.min(prev_ops) {
                            let i = i as usize;
                            if cur.ops[i].ar == 0 && prev.ops[i].rr == 0 {
                                hit = true;
                                break;
                            }
                        }
                        if hit {
                            reset_rows.insert(row);
                        }
                    }
                    prev_ins = carried;
                }
                for row in reset_rows.iter().copied() {
                    {
                        let cells = &self.grid[ch as usize];
                        let cell = match cells.get(&row) {
                            None => continue,
                            Some(c) => c,
                        };
                        let _ = cell;
                    }
                    let cell = self.grid[ch as usize].get_mut(&row).unwrap();
                    _add_fx(cell, 0x30, 1, self.fx_cols, self.stats.as_deref_mut());
                    let nrow = row + 1;
                    if reset_rows.contains(&nrow) || nrow > last_content {
                        continue;
                    }
                    let ncell = self.grid[ch as usize]
                        .entry(nrow)
                        .or_insert_with(_blank_row);
                    _add_fx(ncell, 0x30, 0, self.fx_cols, self.stats.as_deref_mut());
                }
            }
        }

        let pat_len = self
            .pat_len_max
            .min(_phrase_pattern_len(song, last_content, total_rows));

        if loop_row > 0 {
            'grow: while loop_row < total_rows {
                let mut any_note = false;
                for ch in 0..n_ch {
                    if let Some(cell) = self.grid[ch as usize].get(&(loop_row - 1)) {
                        if 0 <= cell.note && cell.note < 180 {
                            any_note = true;
                            break;
                        }
                    }
                }
                if !any_note {
                    break 'grow;
                }
                loop_row += 1;
            }
        }
        self.loop_row = loop_row;
        let mut bounds: Vec<i32> = vec![0];
        let anchor = if 0 < loop_row && loop_row < total_rows {
            Some(loop_row)
        } else {
            None
        };
        let mut pos = 0i32;
        loop {
            let nxt = pos + pat_len;
            if let Some(anchor) = anchor {
                if pos < anchor && anchor < nxt {
                    bounds.push(anchor);
                    pos = anchor;
                    continue;
                }
            }
            if nxt >= total_rows {
                break;
            }
            bounds.push(nxt);
            pos = nxt;
        }
        if *bounds.last().unwrap() < total_rows {
            bounds.push(total_rows);
        }
        let raw_n = bounds.len() as i32 - 1;
        let n_ord = 256.min(raw_n);
        bounds.truncate((n_ord + 1) as usize);
        if raw_n > 256 {
            _bump(self.stats.as_deref_mut(), "orders_truncated", 1);
            self.warnings.push(format!(
                "truncated to 256 orders ({} needed); raise --speed or split the VGM",
                raw_n
            ));
        }
        let mut loop_order = 0i32;
        for oi in 0..n_ord {
            if bounds[oi as usize] == loop_row {
                loop_order = oi;
                break;
            }
        }

        let mut jump_ed: BTreeSet<i32> = BTreeSet::new();
        if let (Some(ce), Some(_)) = (self.content_end, song.loop_sample) {
            for ch in 0..n_ch {
                if let Some(cell) = self.grid[ch as usize].get(&ce) {
                    if any_cmd(Some(cell), 0xED) {
                        jump_ed.insert(ch);
                    }
                }
            }
        }
        let jump_carrier: Option<i32> = (0..n_ch).find(|ch| !jump_ed.contains(ch));

        let mut orders: Vec<Vec<i32>> = vec![Vec::new(); n_ch.max(0) as usize];
        let mut patterns: Vec<(i32, i32, Vec<Option<Row>>)> = Vec::new();

        // Standalone volume changes (MSM5232 TA7630 group volume).
        for &(sample, ch0, level) in &song.channel_vol_events {
            let ch = ch0 + self.ch_offset;
            if !(0 <= ch && ch < n_ch) {
                continue;
            }
            if let Some(cut) = content_cut {
                if sample >= cut {
                    continue;
                }
            }
            let (mut row, mut delay) = song.place(sample, self.stats.as_deref_mut());
            if delay != 0 && row + 1 < total_rows {
                row += 1;
                delay = 0;
            }
            if row < 0 || row >= total_rows {
                continue;
            }
            let _ = delay;
            let cell = self.grid[ch as usize].entry(row).or_insert_with(_blank_row);
            cell.vol = level.clamp(0, 255);
        }

        // Furnace's Genesis/OPN platforms read 08 00 as "center".
        for ch in opn_pan_ch.iter().copied() {
            let mut intended = EMPTY;
            let mut muted = false;
            let mut keys: Vec<i32> = self.grid[ch as usize].keys().copied().collect();
            keys.sort();
            for row in keys {
                let cell = self.grid[ch as usize].get_mut(&row).unwrap();
                let pan_vals: Vec<i32> = fx_slice(Some(&*cell))
                    .iter()
                    .filter(|&&(c, _)| c == 0x08)
                    .map(|&(_, v)| v)
                    .collect();
                let pan_val = pan_vals.last().copied();
                if cell.vol != EMPTY && !muted {
                    intended = cell.vol;
                }
                match pan_val {
                    None => {
                        if muted {
                            if cell.vol != EMPTY {
                                intended = cell.vol;
                            }
                            cell.vol = 0;
                        }
                    }
                    Some(0) => {
                        if let Some(fx) = cell.fx.as_mut() {
                            fx.retain(|&(c, _)| c != 0x08);
                        }
                        if cell.vol != EMPTY {
                            intended = cell.vol;
                        }
                        if intended == EMPTY {
                            intended = 127;
                        }
                        cell.vol = 0;
                        muted = true;
                    }
                    Some(_) => {
                        if muted {
                            muted = false;
                            if cell.vol != EMPTY {
                                intended = cell.vol;
                            } else {
                                cell.vol = if intended != EMPTY { intended } else { 127 };
                            }
                        }
                    }
                }
            }
        }

        for ch in 0..n_ch {
            for oi in 0..n_ord {
                let start_row = bounds[oi as usize];
                let end_row = bounds[(oi + 1) as usize];
                let mut rows: Vec<Option<Row>> = vec![None; pat_len as usize];
                let span = pat_len.min(end_row - start_row);
                for i in 0..span {
                    if let Some(src) = self.grid[ch as usize].get(&(start_row + i)) {
                        rows[i as usize] = Some(src.clone());
                    }
                }
                if oi != n_ord - 1 && 0 < span && span < pat_len {
                    let idx = (span - 1) as usize;
                    if rows[idx].is_none() {
                        rows[idx] = Some(_blank_row());
                    }
                    let cell = rows[idx].as_mut().unwrap();
                    _add_fx(cell, 0x0D, 0, self.fx_cols, self.stats.as_deref_mut());
                }
                if oi == n_ord - 1 && song.loop_sample.is_some() {
                    let ce = self.content_end.unwrap();
                    let mut jump_row = pat_len.min(0.max(ce - start_row + 1)) - 1;
                    if jump_row < 0 {
                        jump_row = 0;
                    }
                    let carry = match jump_carrier {
                        None => ch == 0,
                        Some(c) => ch == c,
                    };
                    if carry {
                        let idx = jump_row as usize;
                        if rows[idx].is_none() {
                            rows[idx] = Some(_blank_row());
                        }
                        let cell = rows[idx].as_mut().unwrap();
                        if any_cmd(Some(&*cell), 0xED) {
                            _bump(self.stats.as_deref_mut(), "ed_stripped_loop", 1);
                            let mut fx =
                                fx_filtered_or_default(Some(&*cell), self.fx_cols, &[0xED]);
                            if fx.is_empty() {
                                cell.fx = None;
                            } else {
                                cell.fx = Some(std::mem::take(&mut fx));
                            }
                        }
                        _add_fx(
                            cell,
                            0x0B,
                            loop_order,
                            self.fx_cols,
                            self.stats.as_deref_mut(),
                        );
                    }
                }
                orders[ch as usize].push(oi);
                patterns.push((ch, oi, rows));
            }
        }

        (orders, patterns, loop_order, pat_len)
    }
}

pub fn _build_patterns(
    song: &Song,
    n_ch: i32,
    pat_len_max: i32,
    fx_cols: usize,
    ch_offset: i32,
    stats: Option<&mut IndexMap<String, i64>>,
    vsu_wave_base: i32,
    warnings: &mut Vec<String>,
    hooks: &mut EmitHooks<'_>,
) -> (Vec<Vec<i32>>, Vec<(i32, i32, Vec<Option<Row>>)>, i32, i32) {
    let ctx = PatternCtx {
        song,
        n_ch,
        pat_len_max,
        fx_cols,
        ch_offset,
        stats,
        vsu_wave_base,
        warnings,
        hooks,
        grid: Vec::new(),
        total_rows: 0,
        content_end: None,
        content_cut: None,
        loop_row: 0,
        note_row_shift: IndexMap::new(),
        fx_row_shift: IndexMap::new(),
        shifted_rows: BTreeSet::new(),
        note_e5: IndexMap::new(),
        pcm_pos: IndexMap::new(),
        note_ev: IndexMap::new(),
    };
    ctx.build()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::furwrite::buf::{EMPTY, PAT_LEN_MAX};
    use crate::furwrite::row::Row;

    #[test]
    fn traj_holds_until_the_next_write() {
        let pts = [(10i64, 5i32), (20, -3), (30, 7)];
        assert_eq!(_traj_at(&pts, 0), 0);
        assert_eq!(_traj_at(&pts, 10), 5);
        assert_eq!(_traj_at(&pts, 19), 5);
        assert_eq!(_traj_at(&pts, 20), -3);
        assert_eq!(_traj_at(&pts, 99), 7);
    }

    #[test]
    fn pattern_len_adds_a_ten_row_tail() {
        assert_eq!(_pattern_len(-1, PAT_LEN_MAX), 10);
        assert_eq!(_pattern_len(0, PAT_LEN_MAX), 11);
        assert_eq!(_pattern_len(1000, PAT_LEN_MAX), 256);
        assert_eq!(_pattern_len(5, 8), 8);
    }

    #[test]
    fn fit_fx_cols_counts_used_effect_slots() {
        let mut a = Row::default();
        a.fx = Some(vec![(0x01, 1), (EMPTY, EMPTY), (0x02, 2)]);
        let mut b = Row::default();
        b.fx = Some(vec![(0x03, 3), (EMPTY, EMPTY), (EMPTY, EMPTY)]);
        let patterns = vec![(0i32, 0i32, vec![Some(a), None]), (1, 0, vec![Some(b)])];
        assert_eq!(_fit_fx_cols(&patterns, 3), vec![2, 1, 1]);
    }
}
