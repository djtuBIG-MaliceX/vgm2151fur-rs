//! Place the source loop in a module's row grid.
//!
//! The VGM header names one iteration: a start sample and a length. The start
//! is the authority. A marker after a pickup means the pickup plays in the
//! intro and again just before each wrap. Opening the loop on the pickup
//! instead strikes it twice per repeat. The end follows the header length,
//! rounded to rows. The row length is calibrated, and a repeat is sample exact.
//! Events just before the wrap that roll past the row end stay on the last loop
//! row. The jump rides a channel with no delay. Delayed notes still land before
//! it.

use std::collections::BTreeSet;

use indexmap::IndexMap;

use crate::model::Song;
use crate::parity::{py_int, py_mod, py_round};

pub const NOTE_OFF: i32 = 180;
pub const PERIODS: [i32; 4] = [4, 8, 12, 16];
pub const MIN_REGULARITY: f64 = 0.75;
// One late note must not hand the clock to a sparser, perfectly even voice.
pub const REG_TOLERANCE: f64 = 0.05;
// Weighted pitch-class agreement through the loop body. Exact equality
// stays near zero when voices sit a row apart. 0.25 still leaves a
// through-composed cue on the VGM marker.
pub const MIN_AUTOCORR: f64 = 0.25;
pub const MIN_AUTOCORR_GAP: f64 = 0.04;
pub const PERIOD_BAND: f64 = 0.02;
pub const MIN_VOTE_MARGIN: f64 = 0.08;
// The winning phase must be clearly heavier than the phase the marker is on.
pub const MIN_PHASE_GAIN: f64 = 0.75;
// The loop beat is an arrival when it brings in this many more voices than
// the candidate behind it. That is a pickup resolving, not a late marker.
pub const TEXTURE_JUMP: i32 = 2;
// The order cut opens this many rows before the first commands: the row
// whose copy the seam search verified. Wider cuts land the seam on rows
// that drift between iterations, and the loop wobbles there.
pub const CUT_ROWS: i32 = 1;

/// A replacement loop in pattern rows: `start_row` opens the order,
/// `end_row` carries `Bxx`.
#[derive(Clone, Debug, PartialEq)]
pub struct LoopFix {
    pub start_row: i32,
    pub end_row: i32,
    pub period: i32,
    pub shift_pulses: i32,
    pub step_rows: i32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Clock {
    pub step: i32,
    pub phase: i32,
    pub regularity: f64,
    pub channel: i32,
}

#[derive(Clone, Debug)]
pub(crate) struct ClockRec {
    regularity: f64,
    count: usize,
    step: i32,
    ch: i32,
    ordered: Vec<i32>,
}

fn pymod(a: i32, b: i32) -> i32 {
    py_mod(a as i64, b as i64) as i32
}

/// `Counter(values).most_common(1)[0][0]`: the first value to reach the top
/// count wins a tie.
fn most_common_first(values: &[i32]) -> i32 {
    let mut counts: IndexMap<i32, usize> = IndexMap::new();
    for &value in values {
        *counts.entry(value).or_insert(0) += 1;
    }
    let mut best = 0;
    let mut best_count = 0;
    for (&value, &count) in &counts {
        if count > best_count {
            best = value;
            best_count = count;
        }
    }
    best
}

pub fn clock(notes: &[(i32, i32, i32)]) -> Option<Clock> {
    let mut by: IndexMap<i32, Vec<i32>> = IndexMap::new();
    for &(row, ch, _pitch) in notes {
        by.entry(ch).or_default().push(row);
    }
    let mut best: Option<ClockRec> = None;
    for (&ch, rows) in &by {
        let mut ordered = rows.clone();
        ordered.sort();
        ordered.dedup();
        let gaps: Vec<i32> = ordered
            .windows(2)
            .map(|w| w[1] - w[0])
            .filter(|gap| (3..=96).contains(gap))
            .collect();
        if gaps.len() < 8 {
            continue;
        }
        let top = most_common_first(&gaps);
        let cluster: Vec<i32> = gaps
            .iter()
            .copied()
            .filter(|gap| (gap - top).abs() <= 1)
            .collect();
        let regularity = cluster.len() as f64 / gaps.len() as f64;
        let step = std::cmp::max(
            1,
            py_round(cluster.iter().sum::<i32>() as f64 / cluster.len() as f64) as i32,
        );
        let rec = ClockRec {
            regularity,
            count: cluster.len(),
            step,
            ch,
            ordered,
        };
        let better = match &best {
            None => true,
            Some(prev) => better_clock(&rec, prev),
        };
        if better {
            best = Some(rec);
        }
    }
    let best = match &best {
        None => return None,
        Some(rec) if rec.regularity < MIN_REGULARITY => return None,
        Some(rec) => rec.clone(),
    };
    let mut phase = 0;
    let mut hits = -1;
    for candidate in 0..best.step {
        let count = best
            .ordered
            .iter()
            .filter(|&&row| {
                let d = pymod(row - candidate, best.step);
                std::cmp::min(d, best.step - d) <= 1
            })
            .count();
        if count as i32 > hits {
            hits = count as i32;
            phase = candidate;
        }
    }
    Some(Clock {
        step: best.step,
        phase,
        regularity: best.regularity,
        channel: best.ch,
    })
}

/// Prefer the even voice. A regularity gap inside the tolerance loses to
/// the voice with more on-grid gaps. The pulse is the dense one.
pub(crate) fn better_clock(rec: &ClockRec, best: &ClockRec) -> bool {
    let reg = rec.regularity;
    let count = rec.count;
    let best_reg = best.regularity;
    let best_count = best.count;
    if reg > best_reg + REG_TOLERANCE {
        return true;
    }
    (reg - best_reg).abs() <= REG_TOLERANCE && count > best_count
}

/// beat -> channel -> (pitch, row). Nearest pulse, within half a step.
pub fn grid(notes: &[(i32, i32, i32)], clock: &Clock) -> IndexMap<i32, IndexMap<i32, (i32, i32)>> {
    let mut grid: IndexMap<i32, IndexMap<i32, (i32, i32)>> = IndexMap::new();
    let limit = clock.step as f64 / 2.0;
    for &(row, ch, pitch) in notes {
        let rel = row - clock.phase;
        let beat = py_round(rel as f64 / clock.step as f64) as i32;
        let err = (rel as f64 - beat as f64 * clock.step as f64).abs();
        if err > limit {
            continue;
        }
        let voices = grid.entry(beat).or_default();
        let replace = match voices.get(&ch) {
            None => true,
            Some(prev) => {
                err < ((prev.1 - clock.phase) as f64 - beat as f64 * clock.step as f64).abs()
            }
        };
        if replace {
            voices.insert(ch, (pitch, row));
        }
    }
    grid
}

/// Channels that repeat another channel's pitch class one pulse later.
pub fn echoes(
    grid: &IndexMap<i32, IndexMap<i32, (i32, i32)>>,
    clock_ch: i32,
) -> BTreeSet<i32> {
    let mut channel_set = BTreeSet::new();
    for voices in grid.values() {
        channel_set.extend(voices.keys().copied());
    }
    let channels: Vec<i32> = channel_set.into_iter().collect();
    let mut echoes: BTreeSet<i32> = BTreeSet::new();
    for &src in &channels {
        for &dst in &channels {
            if src == dst || dst == clock_ch || echoes.contains(&dst) {
                continue;
            }
            let mut total = 0;
            let mut hits = 0;
            for (beat, voices) in grid {
                let src_voice = match voices.get(&src) {
                    None => continue,
                    Some(voice) => voice,
                };
                total += 1;
                let later = match grid.get(&(beat + 1)) {
                    None => continue,
                    Some(later) => later,
                };
                if let Some(dst_voice) = later.get(&dst) {
                    if pymod(dst_voice.0, 12) == pymod(src_voice.0, 12) {
                        hits += 1;
                    }
                }
            }
            if total >= 12 && hits >= 8 && hits as f64 / total as f64 >= 0.55 {
                echoes.insert(dst);
            }
        }
    }
    echoes
}

pub fn roles(
    grid: &IndexMap<i32, IndexMap<i32, (i32, i32)>>,
    echoes: &BTreeSet<i32>,
) -> (Option<i32>, Option<i32>) {
    let mut channel_set = BTreeSet::new();
    for voices in grid.values() {
        channel_set.extend(voices.keys().copied());
    }
    let mut stats: Vec<(i32, i32, usize, usize)> = Vec::new();
    for ch in channel_set {
        if echoes.contains(&ch) {
            continue;
        }
        let pitches: Vec<i32> = grid
            .values()
            .filter_map(|voices| voices.get(&ch).map(|voice| voice.0))
            .collect();
        if pitches.len() < 8 {
            continue;
        }
        let mut ordered = pitches.clone();
        ordered.sort();
        let median = ordered[ordered.len() / 2];
        let mut pcs = BTreeSet::new();
        for &pitch in &pitches {
            pcs.insert(pymod(pitch, 12));
        }
        stats.push((ch, median, pcs.len(), pitches.len()));
    }
    if stats.is_empty() {
        return (None, None);
    }
    let mut bass_item = &stats[0];
    for item in &stats[1..] {
        if (item.1, -(item.3 as i64)) < (bass_item.1, -(bass_item.3 as i64)) {
            bass_item = item;
        }
    }
    let bass = bass_item.0;
    let leads: Vec<&(i32, i32, usize, usize)> =
        stats.iter().filter(|item| item.0 != bass).collect();
    if leads.is_empty() {
        return (Some(bass), None);
    }
    let mut lead_item = leads[0];
    for item in &leads[1..] {
        if (item.2, item.1, item.3) > (lead_item.2, lead_item.1, lead_item.3) {
            lead_item = item;
        }
    }
    (Some(bass), Some(lead_item.0))
}

pub fn beat_of(row: i32, clock: &Clock) -> i32 {
    py_round((row - clock.phase) as f64 / clock.step as f64) as i32
}

pub fn slice(
    grid: &IndexMap<i32, IndexMap<i32, (i32, i32)>>,
    beat: i32,
    echoes: &BTreeSet<i32>,
) -> Vec<(i32, i32)> {
    let voices = match grid.get(&beat) {
        None => return Vec::new(),
        Some(voices) => voices,
    };
    let mut out: Vec<(i32, i32)> = voices
        .iter()
        .filter(|(ch, _)| !echoes.contains(*ch))
        .map(|(&ch, &(pitch, _row))| (ch, pymod(pitch, 12)))
        .collect();
    out.sort();
    out
}

/// Pitch classes each channel uses. A pedal weighs less than a melody.
pub fn variety(grid: &IndexMap<i32, IndexMap<i32, (i32, i32)>>) -> IndexMap<i32, i32> {
    let mut seen: IndexMap<i32, BTreeSet<i32>> = IndexMap::new();
    for voices in grid.values() {
        for (&ch, &(pitch, _row)) in voices {
            seen.entry(ch).or_default().insert(pymod(pitch, 12));
        }
    }
    seen.into_iter().map(|(ch, pcs)| (ch, pcs.len() as i32)).collect()
}

/// Share of pitch classes that match, weighted by how much each voice moves.
///
/// Empty against empty is skipped. A voice on only one side counts as a miss.
pub fn agreement(
    left: &IndexMap<i32, i32>,
    right: &IndexMap<i32, i32>,
    weight: &IndexMap<i32, i32>,
) -> Option<f64> {
    if left.is_empty() && right.is_empty() {
        return None;
    }
    let mut keys: BTreeSet<i32> = BTreeSet::new();
    keys.extend(left.keys().copied());
    keys.extend(right.keys().copied());
    if keys.is_empty() {
        return None;
    }
    let mut hit = 0.0;
    let mut total = 0.0;
    for ch in keys {
        let w = *weight.get(&ch).unwrap_or(&1) as f64;
        total += w;
        if left.get(&ch).is_some() && right.get(&ch).is_some() && left[&ch] == right[&ch] {
            hit += w;
        }
    }
    if total != 0.0 { Some(hit / total) } else { None }
}

/// The repeating cell length, in pulses, and its agreement.
pub fn period(
    grid: &IndexMap<i32, IndexMap<i32, (i32, i32)>>,
    echoes: &BTreeSet<i32>,
    loop_beat: i32,
    end_beat: i32,
) -> Option<(i32, f64)> {
    let weight = variety(grid);
    let span = end_beat - loop_beat;
    let mut scored: Vec<(f64, i32)> = Vec::new();
    for &period in &PERIODS {
        if span < period * 2 {
            continue;
        }
        let mut total = 0;
        let mut acc = 0.0;
        for beat in loop_beat..(end_beat - period) {
            let left: IndexMap<i32, i32> = slice(grid, beat, echoes).into_iter().collect();
            let right: IndexMap<i32, i32> =
                slice(grid, beat + period, echoes).into_iter().collect();
            let part = agreement(&left, &right, &weight);
            if part.is_none() {
                continue;
            }
            total += 1;
            acc += part.unwrap();
        }
        if total < 16 {
            continue;
        }
        scored.push((acc / total as f64, period));
    }
    if scored.is_empty() {
        return None;
    }
    // A tie keeps the shorter cell. Two identical bars are an 8, not a 16.
    let mut ranked = scored.clone();
    ranked.sort_by(|a, b| {
        b.0.partial_cmp(&a.0)
            .unwrap()
            .then_with(|| a.1.cmp(&b.1))
    });
    let (mut best_ac, mut best_period) = ranked[0];
    if best_ac < MIN_AUTOCORR {
        return None;
    }
    let mut by_period = scored.clone();
    by_period.sort_by_key(|item| item.1);
    for &(ac, period) in &by_period {
        if period <= best_period || ac < best_ac - PERIOD_BAND {
            continue;
        }
        // A multiple of a cell that already matches is the same cell again.
        if scored.iter().any(|&(other_ac, other)| {
            0 < other && other < period && pymod(period, other) == 0 && other_ac >= ac - PERIOD_BAND
        }) {
            continue;
        }
        best_ac = ac;
        best_period = period;
    }
    let rivals: Vec<f64> = scored
        .iter()
        .filter(|&&(_, period)| {
            period != best_period
                && pymod(period, best_period) != 0
                && pymod(best_period, period) != 0
        })
        .map(|&(ac, _)| ac)
        .collect();
    if !rivals.is_empty() {
        let top_rival = rivals.iter().copied().fold(f64::NEG_INFINITY, f64::max);
        if best_ac - top_rival < MIN_AUTOCORR_GAP {
            return None;
        }
    }
    Some((best_period, best_ac))
}

/// True when the loop length is within one pulse of a whole cell count.
pub fn whole_bars(length_rows: i32, step: i32, period: i32) -> bool {
    let pulses = length_rows as f64 / step as f64;
    let bars = pulses / period as f64;
    (bars - py_round(bars)).abs() * period as f64 <= 1.0 + 1e-6
}

pub fn ensemble(
    grid: &IndexMap<i32, IndexMap<i32, (i32, i32)>>,
    beat: i32,
    echoes: &BTreeSet<i32>,
) -> i32 {
    match grid.get(&beat) {
        None => 0,
        Some(voices) => voices.keys().filter(|ch| !echoes.contains(*ch)).count() as i32,
    }
}

/// Pulses to add so the loop beat lands on the voted downbeat.
///
/// The second value is the lead over the runner-up. The third is how far
/// the winner sits above the phase the marker is already on. The dict is
/// every phase's score.
pub fn phase_shift(
    grid: &IndexMap<i32, IndexMap<i32, (i32, i32)>>,
    echoes: &BTreeSet<i32>,
    lead: Option<i32>,
    loop_beat: i32,
    end_beat: i32,
    period: i32,
) -> (i32, f64, f64, IndexMap<i32, f64>) {
    let mut scores: Vec<(f64, i32)> = Vec::new();
    for phase in 0..period {
        let mut ensemble_count = 0;
        let mut entries = 0;
        let mut count = 0;
        for beat in loop_beat..=end_beat {
            if pymod(beat, period) != phase {
                continue;
            }
            count += 1;
            ensemble_count += ensemble(grid, beat, echoes);
            if let Some(lead) = lead {
                let here = grid.get(&beat).map_or(false, |v| v.contains_key(&lead));
                let before = grid.get(&(beat - 1)).map_or(false, |v| v.contains_key(&lead));
                if here && !before {
                    entries += 1;
                }
            }
        }
        if count != 0 {
            scores.push((
                ensemble_count as f64 / count as f64 + 2.5 * (entries as f64 / count as f64),
                phase,
            ));
        }
    }
    scores.sort_by(|a, b| {
        b.0.partial_cmp(&a.0)
            .unwrap()
            .then_with(|| b.1.cmp(&a.1))
    });
    if scores.is_empty() {
        return (0, 0.0, 0.0, IndexMap::new());
    }
    let by_phase: IndexMap<i32, f64> = scores.iter().map(|&(score, phase)| (phase, score)).collect();
    let current = pymod(loop_beat, period);
    let current_score = *by_phase.get(&current).unwrap_or(&0.0);
    let margin = if scores.len() > 1 {
        scores[0].0 - scores[1].0
    } else {
        scores[0].0
    };
    let mut delta = pymod(scores[0].1 - current, period);
    if delta as f64 > period as f64 / 2.0 {
        delta -= period;
    }
    (
        delta,
        margin,
        scores[0].0 - current_score,
        by_phase,
    )
}

/// Pulses back to the lead's entry, when that entry is inside this cell.
///
/// The entry is a lead note with the lead silent on the pulse before it.
/// The nearest such note at or before the marker, and no more than half a
/// cell back, is the start of the phrase the marker fell into.
pub fn entry_shift(
    grid: &IndexMap<i32, IndexMap<i32, (i32, i32)>>,
    lead: Option<i32>,
    loop_beat: i32,
    period: i32,
) -> Option<i32> {
    let lead = lead?;
    let mut found: Option<i32> = None;
    for beat in (loop_beat - period / 2)..=loop_beat {
        let here = grid.get(&beat).map_or(false, |v| v.contains_key(&lead));
        if !here {
            continue;
        }
        let before = grid.get(&(beat - 1)).map_or(false, |v| v.contains_key(&lead));
        if before {
            continue;
        }
        found = Some(beat);
    }
    match found {
        None => None,
        Some(beat) if beat == loop_beat => None,
        Some(beat) => Some(beat - loop_beat),
    }
}

/// The row most of the beat's own notes sit on.
///
/// A delayed double is ignored. A tie goes to the earlier row, which is
/// where a sample keyed a row early already landed.
pub fn command_row(
    grid: &IndexMap<i32, IndexMap<i32, (i32, i32)>>,
    beat: i32,
    echoes: &BTreeSet<i32>,
) -> Option<i32> {
    let voices = match grid.get(&beat) {
        None => return None,
        Some(voices) => voices,
    };
    let mut rows: Vec<i32> = voices
        .iter()
        .filter(|(ch, _)| !echoes.contains(*ch))
        .map(|(_, &(_pitch, row))| row)
        .collect();
    if rows.is_empty() {
        rows = voices.values().map(|&(_pitch, row)| row).collect();
    }
    if rows.is_empty() {
        return None;
    }
    let mut counts: IndexMap<i32, usize> = IndexMap::new();
    for row in rows {
        *counts.entry(row).or_insert(0) += 1;
    }
    let best = counts.values().copied().max().unwrap_or(0);
    counts
        .iter()
        .filter(|&(_, &count)| count == best)
        .map(|(&row, _)| row)
        .min()
}

/// The marker fell on the empty row after a chord. Land before that chord.
pub fn snap_empty_marker(
    notes: &[(i32, i32, i32)],
    loop_row: i32,
    end_row: i32,
    step: i32,
) -> Option<LoopFix> {
    let at_marker = notes.iter().filter(|&&(row, _ch, _pitch)| row == loop_row).count();
    if at_marker != 0 {
        return None;
    }
    let chord = loop_row - 1;
    let hits = notes.iter().filter(|&&(row, _ch, _pitch)| row == chord).count();
    if hits < 2 || chord < 1 {
        return None;
    }
    // A row before the commands, unless one row is a whole pulse. The end
    // moves with the start, and the played span keeps its length.
    let mut start = if step > 1 { chord - CUT_ROWS } else { chord };
    if start < 0 {
        start = 0;
    }
    let delta = start - loop_row;
    let end = end_row + delta;
    if end < start {
        return None;
    }
    Some(LoopFix {
        start_row: start,
        end_row: end,
        period: 0,
        shift_pulses: 0,
        step_rows: step,
    })
}

/// Rotate a whole-bar loop onto its downbeat, or None to keep the marker.
///
/// `notes` is `(row, channel, pitch)` for note-ons. `end_row` is the row
/// the jump is on today.
pub fn phase_loop(
    notes: &[(i32, i32, i32)],
    loop_row: i32,
    end_row: i32,
) -> Option<LoopFix> {
    if end_row < loop_row || notes.is_empty() {
        return None;
    }
    let clock = clock(notes)?;
    let grid = grid(notes, &clock);
    let echoes = echoes(&grid, clock.channel);
    let (_bass, lead) = roles(&grid, &echoes);
    let loop_beat = beat_of(loop_row, &clock);
    let end_beat = beat_of(end_row, &clock);
    let chosen = period(&grid, &echoes, loop_beat, end_beat);
    let length_rows = end_row - loop_row + 1;
    let chosen = match chosen {
        Some(chosen) if whole_bars(length_rows, clock.step, chosen.0) => chosen,
        _ => return snap_empty_marker(notes, loop_row, end_row, clock.step),
    };

    let (period, _ac) = chosen;
    let (vote, margin, gain, phase_scores) =
        phase_shift(&grid, &echoes, lead, loop_beat, end_beat, period);
    // A non-zero vote must clear the runner-up and the phase the marker is
    // on. Confirming the current phase does not, and a lead entry can still
    // move the loop.
    let vote_ok = vote != 0 && margin >= MIN_VOTE_MARGIN && gain >= MIN_PHASE_GAIN;
    let mut shift = entry_shift(&grid, lead, loop_beat, period);
    if let Some(shift_value) = shift {
        if ensemble(&grid, loop_beat, &echoes)
            >= ensemble(&grid, loop_beat + shift_value, &echoes) + TEXTURE_JUMP
        {
            shift = None;
        }
    }
    // One pulse back is the late-marker case (a double arriving on the
    // marker's own beat). A longer walk back must land on a heavier phase,
    // and loses to a vote that already names a downbeat.
    if let Some(shift_value) = shift {
        if shift_value.abs() > 1 {
            let dest = *phase_scores
                .get(&pymod(loop_beat + shift_value, period))
                .unwrap_or(&0.0);
            let here = *phase_scores
                .get(&pymod(loop_beat, period))
                .unwrap_or(&0.0);
            if dest < here + MIN_PHASE_GAIN {
                shift = None;
            }
        }
    }
    if let Some(shift_value) = shift {
        if vote_ok && shift_value != vote {
            shift = None;
        }
    }
    // Shift 0 is a confirmed downbeat, and the order still opens a few rows
    // early. A vote that did not clear the bar is not that confirmation: the
    // marker stays where the VGM put it.
    let confirmed = vote_ok || (vote == 0 && margin >= MIN_VOTE_MARGIN);
    let shift = match shift {
        None => {
            if !confirmed {
                return snap_empty_marker(notes, loop_row, end_row, clock.step);
            }
            vote
        }
        Some(shift_value) => shift_value,
    };
    if shift.abs() > period / 2 {
        return snap_empty_marker(notes, loop_row, end_row, clock.step);
    }

    let command = match command_row(&grid, loop_beat + shift, &echoes) {
        None => return snap_empty_marker(notes, loop_row, end_row, clock.step),
        Some(command) => command,
    };
    if (command - loop_row).abs() > (period * clock.step) / 2 + clock.step {
        return None;
    }
    // A row before the commands. A one-row clock keeps the cut on the same
    // pulse, or it would land mid-bar. The end moves with the start.
    let start = if clock.step > 1 {
        std::cmp::max(0, command - CUT_ROWS)
    } else {
        command
    };
    let delta = start - loop_row;
    let end = end_row + delta;
    // The VGM jump can sit a few rows past the last note; both ends move
    // together and stay inside that span.
    let last_row = notes.iter().map(|&(row, _ch, _pitch)| row).max().unwrap();
    if end < start || end > std::cmp::max(end_row, last_row) || start > std::cmp::max(end_row, last_row)
    {
        return None;
    }
    let (start, end) = _seat_seam(
        &_exact_rows(notes),
        start,
        end,
        0,
        std::cmp::max(end_row, last_row),
        end_row,
    );
    if start == loop_row && end == end_row {
        return None;
    }
    Some(LoopFix {
        start_row: start,
        end_row: end,
        period,
        shift_pulses: shift,
        step_rows: clock.step,
    })
}

// A phrase match has to be this close before the seam search will move the
// loop. A short coincidence (an inner repetition that agrees for under a
// second) does not.
pub const _SEAM_SCORE: f64 = 0.95;
pub const _SEAM_TOL: i32 = 2;

/// Channels that sit on one pitch class. They agree on every bar.
pub fn pedals(notes: &[(i32, i32, i32)]) -> BTreeSet<i32> {
    let mut by: IndexMap<i32, Vec<i32>> = IndexMap::new();
    for &(_row, ch, pitch) in notes {
        by.entry(ch).or_default().push(pymod(pitch, 12));
    }
    let mut pedals = BTreeSet::new();
    for (&ch, pcs) in &by {
        if pcs.len() < 12 {
            continue;
        }
        let top = most_common_first(pcs);
        let top_count = pcs.iter().filter(|&&pc| pc == top).count();
        if top_count as f64 / pcs.len() as f64 >= 0.8 {
            pedals.insert(ch);
        }
    }
    pedals
}

type NoteSet = BTreeSet<(i32, i32)>;
type NoteMaps = (
    IndexMap<i32, NoteSet>,
    IndexMap<i32, NoteSet>,
    IndexMap<i32, NoteSet>,
    IndexMap<i32, i32>,
);

/// `(row -> {(ch, pc)})`, `(row -> note bits)`, and notes per row.
///
/// A row's bits are one bit per (channel, pitch class); the widened map ORs
/// the rows within `_SEAM_TOL` so a seam match can sit a couple of rows off.
pub fn note_maps(notes: &[(i32, i32, i32)]) -> NoteMaps {
    let mut by: IndexMap<i32, NoteSet> = IndexMap::new();
    let mut bits: IndexMap<i32, NoteSet> = IndexMap::new();
    let mut weight: IndexMap<i32, i32> = IndexMap::new();
    for &(row, ch, pitch) in notes {
        let pc = pymod(pitch, 12);
        by.entry(row).or_default().insert((ch, pc));
        bits.entry(row).or_default().insert((ch, pc));
        *weight.entry(row).or_insert(0) += 1;
    }
    let mut wide: IndexMap<i32, NoteSet> = IndexMap::new();
    if !bits.is_empty() {
        let lo = *bits.keys().min().unwrap();
        let hi = *bits.keys().max().unwrap();
        for row in (lo - _SEAM_TOL)..=(hi + _SEAM_TOL) {
            let mut acc: NoteSet = BTreeSet::new();
            for delta in -_SEAM_TOL..=_SEAM_TOL {
                if let Some(set) = bits.get(&(row + delta)) {
                    acc.extend(set.iter().copied());
                }
            }
            wide.insert(row, acc);
        }
    }
    (by, bits, wide, weight)
}

/// How much of each side's notes show up on the other, a couple of rows off.
///
/// Both directions count. A sparse slice that is merely contained in a
/// busier one does not score as a match.
pub fn seam_score(
    bits: &IndexMap<i32, NoteSet>,
    wide: &IndexMap<i32, NoteSet>,
    start: i32,
    other: i32,
    width: i32,
) -> f64 {
    let mut left = 0;
    let mut right = 0;
    let mut hit_left = 0;
    let mut hit_right = 0;
    for offset in 0..width {
        let here = bits.get(&(start + offset));
        let there = bits.get(&(other + offset));
        if let Some(here) = here {
            if !here.is_empty() {
                left += here.len();
                if let Some(wide_there) = wide.get(&(other + offset)) {
                    hit_left += here.intersection(wide_there).count();
                }
            }
        }
        if let Some(there) = there {
            if !there.is_empty() {
                right += there.len();
                if let Some(wide_here) = wide.get(&(start + offset)) {
                    hit_right += there.intersection(wide_here).count();
                }
            }
        }
    }
    if left < 8 || right < 8 {
        return 0.0;
    }
    (hit_left as f64 / left as f64).min(hit_right as f64 / right as f64)
}

/// Best copy of `[start, start+width)` in `[lo, hi]`, or `(0.0, None)`.
///
/// The comparison window has to fit completely inside the track: a copy
/// clipped by the end of the log would be scored over fewer rows and can
/// beat a real seam. A module already cut at its loop end has no complete
/// copy inside the track, and its loop is left alone.
pub fn best_return(
    bits: &IndexMap<i32, NoteSet>,
    wide: &IndexMap<i32, NoteSet>,
    start: i32,
    end_row: i32,
    lo: i32,
    hi: i32,
    width: i32,
    step: i32,
) -> (f64, Option<i32>) {
    let mut best_score = 0.0;
    let mut best_at = None;
    let mut other = lo;
    while other <= hi {
        if end_row - other < width {
            other += step;
            continue;
        }
        let score = seam_score(bits, wide, start, other, width);
        if score > best_score {
            best_score = score;
            best_at = Some(other);
        }
        other += step;
    }
    (best_score, best_at)
}

/// Non-pedal channels that play the same pitch class within a few rows.
pub fn attack_agrees(
    by: &IndexMap<i32, NoteSet>,
    command: i32,
    ret: i32,
    pedals: &BTreeSet<i32>,
) -> i32 {
    let mut left: IndexMap<i32, BTreeSet<i32>> = IndexMap::new();
    let mut right: IndexMap<i32, BTreeSet<i32>> = IndexMap::new();
    for delta in -3..=3 {
        if let Some(notes) = by.get(&(command + delta)) {
            for &(ch, pc) in notes {
                if !pedals.contains(&ch) {
                    left.entry(ch).or_default().insert(pc);
                }
            }
        }
        if let Some(notes) = by.get(&(ret + delta)) {
            for &(ch, pc) in notes {
                if !pedals.contains(&ch) {
                    right.entry(ch).or_default().insert(pc);
                }
            }
        }
    }
    left.iter()
        .filter(|(ch, pcs)| {
            right
                .get(*ch)
                .map_or(false, |other| pcs.intersection(other).next().is_some())
        })
        .count() as i32
}

// The cut may move this many rows either way when a different seat makes the
// rows across the seam agree exactly. The end moves with the start, and the
// played length never changes.
pub const SEAT_SHIFT: i32 = 2;
// Rows after the seam the seat compares. The attack is what the ear follows,
// and the window must fit inside the log: the tail past the jump is the copy
// the comparison needs, and comparing against the log's end proves nothing.
pub const SEAT_SPAN: i32 = 16;

/// `row -> {(channel, pitch)}` at full pitch, for the seat comparison.
pub fn _exact_rows(notes: &[(i32, i32, i32)]) -> IndexMap<i32, NoteSet> {
    let mut by: IndexMap<i32, NoteSet> = IndexMap::new();
    for &(row, ch, pitch) in notes {
        by.entry(row).or_default().insert((ch, pitch));
    }
    by
}

/// Voices that play on one side of the seam but not the other.
///
/// The jump sits on `end`, and the row after it must be the loop's first
/// row. The rows have to agree at the row here, not a couple of rows off:
/// this measures what the module plays, while the fuzzy match only finds
/// the seam.
pub fn _seat_cost(exact: &IndexMap<i32, NoteSet>, start: i32, end: i32) -> i32 {
    let empty = NoteSet::new();
    let mut cost = 0;
    for offset in 0..SEAT_SPAN {
        let left = exact.get(&(start + offset)).unwrap_or(&empty);
        let right = exact.get(&(end + 1 + offset)).unwrap_or(&empty);
        cost += left.symmetric_difference(right).count() as i32;
    }
    cost
}

/// The nearby seat whose rows agree exactly across the seam, if any.
///
/// Nearest first. A thin improvement means the tail is not a row-exact
/// copy, and moving on that evidence only trades one wobble for another.
/// The window must fit inside the log, and a seat that already agrees
/// never moves.
pub fn _seat_seam(
    exact: &IndexMap<i32, NoteSet>,
    start: i32,
    end: i32,
    lo: i32,
    hi: i32,
    content_end: i32,
) -> (i32, i32) {
    if end + SEAT_SPAN > content_end {
        return (start, end);
    }
    if _seat_cost(exact, start, end) == 0 {
        return (start, end);
    }
    for distance in 1..=SEAT_SHIFT {
        for moved in [distance, -distance] {
            if start + moved < lo || end + moved > hi {
                continue;
            }
            if end + moved + SEAT_SPAN > content_end {
                continue;
            }
            if _seat_cost(exact, start + moved, end + moved) == 0 {
                return (start + moved, end + moved);
            }
        }
    }
    (start, end)
}

/// Move the loop onto the phrase start the end of the track repeats.
///
/// The VGM loop length is the invariant: a candidate start's copy sits one
/// loop length later, with the comparison window completely inside the
/// track. A shorter copy (an inner repetition) is not a seam. Candidates
/// run a window on either side of the marker; the earliest long run of
/// near-perfect matches wins, and its heaviest command row opens the loop.
pub fn phrase_seam(
    notes: &[(i32, i32, i32)],
    loop_row: i32,
    end_row: i32,
    forward_rows: i32,
) -> Option<LoopFix> {
    let span = end_row - loop_row;
    let window = std::cmp::min(forward_rows, span);
    if window < 16 || span < 16 {
        return None;
    }
    let (by, bits, wide, weight) = note_maps(notes);
    if bits.is_empty() {
        return None;
    }
    let compare = std::cmp::max(16, py_int(forward_rows as f64 * 0.375) as i32);
    let tail = std::cmp::max(12, py_int(forward_rows as f64 * 0.125) as i32);
    let end_lo = end_row - window;
    let end_hi = end_row - tail;
    // One loop length later is where a candidate's copy has to sit; the
    // slack covers the marker's own rounding and per-row alignment.
    let loop_rows = span + 1;
    let slack = std::cmp::max(4, loop_rows / 200);
    let first = std::cmp::max(0, loop_row - window);
    let last = loop_row + window;

    let scan = |first: i32, last: i32| -> IndexMap<i32, (f64, i32)> {
        let mut found: IndexMap<i32, (f64, i32)> = IndexMap::new();
        let mut start = first;
        while start <= last {
            let near = start + loop_rows;
            let lo = std::cmp::max(end_lo, std::cmp::max(start + tail, near - slack));
            let hi = std::cmp::min(end_hi, std::cmp::min(near + slack, end_row - compare));
            if hi < lo {
                start += 2;
                continue;
            }
            let (score, ret) = best_return(&bits, &wide, start, end_row, lo, hi, compare, 2);
            if let Some(ret) = ret {
                found.insert(start, (score, ret));
            }
            start += 2;
        }
        found
    };

    let scores = scan(first, last);
    if scores.is_empty() {
        return None;
    }
    let peak = scores
        .values()
        .map(|item| item.0)
        .fold(f64::NEG_INFINITY, f64::max);
    if peak < _SEAM_SCORE {
        return None;
    }
    let mut high: Vec<i32> = scores
        .iter()
        .filter(|&(_, (score, _ret))| *score >= f64::max(_SEAM_SCORE, peak - 0.03))
        .map(|(&row, _)| row)
        .collect();
    high.sort();
    if high.is_empty() {
        return None;
    }
    let need = py_int(forward_rows as f64 * 0.15) as i32;
    let mut runs: Vec<Vec<i32>> = Vec::new();
    for row in high {
        let starts_new = match runs.last() {
            None => true,
            Some(run) => row - run[run.len() - 1] > 4,
        };
        if starts_new {
            runs.push(vec![row]);
        } else {
            runs.last_mut().unwrap().push(row);
        }
    }
    // The earliest run that reaches the peak band is the phrase start; a
    // later copy is the same phrase again and drops everything before it.
    let band = runs
        .iter()
        .find(|run| run[run.len() - 1] - run[0] >= need)?;
    let band_start = band[0];
    let lag = scores[&band_start].1 - band_start;
    let pedals = pedals(notes);
    let clock = clock(notes);
    let step = match &clock {
        Some(clock) => clock.step,
        None => 2,
    };
    let snap_end = band_start + std::cmp::max(8, forward_rows / 8);
    let mut command: Option<i32> = None;
    let mut command_n = -1;
    let mut ret: Option<i32> = None;
    for row in std::cmp::max(0, band_start - 4)..=snap_end {
        let row_weight = *weight.get(&row).unwrap_or(&0);
        if row_weight < 3 || row_weight < command_n {
            continue;
        }
        let near = row + lag;
        let command_notes: NoteSet = by
            .get(&row)
            .map(|notes| {
                notes
                    .iter()
                    .filter(|(ch, _pc)| !pedals.contains(ch))
                    .copied()
                    .collect()
            })
            .unwrap_or_default();
        let mut agreed = 0;
        let mut agreed_at: Option<i32> = None;
        let mut agreed_share = -1;
        let mut agreed_n = -1;
        for candidate in (near - 4)..=(near + 4) {
            let channels = attack_agrees(&by, row, candidate, &pedals);
            let candidate_notes: NoteSet = by
                .get(&candidate)
                .map(|notes| {
                    notes
                        .iter()
                        .filter(|(ch, _pc)| !pedals.contains(ch))
                        .copied()
                        .collect()
                })
                .unwrap_or_default();
            let share = command_notes.intersection(&candidate_notes).count() as i32;
            if channels < 2 && share < 2 {
                continue;
            }
            // Prefer the row that actually holds the same notes. A neighbour
            // can "agree" only because the chord sits a row or two away.
            if (share, *weight.get(&candidate).unwrap_or(&0), channels)
                > (agreed_share, agreed_n, agreed)
            {
                agreed = channels;
                agreed_share = share;
                agreed_n = *weight.get(&candidate).unwrap_or(&0);
                agreed_at = Some(candidate);
            }
        }
        let agreed_at = match agreed_at {
            None => continue,
            Some(agreed_at) => agreed_at,
        };
        if row_weight > command_n || command.is_none() {
            command_n = row_weight;
            command = Some(row);
            ret = Some(agreed_at);
        }
    }
    let command = command?;
    let ret = match ret {
        None => return None,
        Some(ret) if ret <= command => return None,
        Some(ret) => ret,
    };
    let start = if step > 1 {
        std::cmp::max(0, command - CUT_ROWS)
    } else {
        command
    };
    // Played length equals the distance between the two copies of the chord,
    // and the jump lands on the early row, where the chord is not struck twice.
    // The end follows the start: both ends move by the same amount. The seam
    // stays continuous (the row after the end copies the loop's first
    // row) and the played span keeps its length.
    let end = start + (ret - command) - 1;
    if end <= start || end > end_row || start > end_row {
        return None;
    }
    let (start, end) = _seat_seam(&_exact_rows(notes), start, end, 0, end_row, end_row);
    if start == loop_row && end == end_row {
        return None;
    }
    Some(LoopFix {
        start_row: start,
        end_row: end,
        period: 0,
        shift_pulses: 0,
        step_rows: step,
    })
}

/// Return a new loop, or None to keep the VGM marker.
///
/// `forward_rows` is the window around the marker a phrase start may sit
/// in. The converter passes four seconds of rows, and a shorter loop passes
/// its own length. Omitted, only the within-bar downbeat is considered.
pub fn find_loop(
    notes: &[(i32, i32, i32)],
    loop_row: i32,
    end_row: i32,
    forward_rows: Option<i32>,
) -> Option<LoopFix> {
    let fix = phase_loop(notes, loop_row, end_row);
    let forward_rows = match forward_rows {
        None => return fix,
        Some(rows) => rows,
    };
    if fix.is_some() || forward_rows < 8 {
        return fix;
    }
    phrase_seam(notes, loop_row, end_row, forward_rows)
}

/// One short clause for the convert line.
pub fn describe_fix(fix: &LoopFix, loop_row: i32) -> String {
    let delta = fix.start_row - loop_row;
    if fix.period != 0 && fix.shift_pulses != 0 {
        return format!(
            "loop {:+}/{} ({:+} rows)",
            fix.shift_pulses, fix.period, delta
        );
    }
    if delta != 0 {
        let ahead = if delta < 0 { "early" } else { "late" };
        let n = delta.abs();
        let unit = if n == 1 { "row" } else { "rows" };
        return format!("loop {n} {unit} {ahead}");
    }
    "loop kept".to_string()
}

pub fn last_content_row(song: &Song) -> i32 {
    let mut last = 0i64;
    for ev in &song.events {
        last = last.max(ev.sample);
        if let Some(&(sample, _level, _balance)) = ev.vol_pts.last() {
            last = last.max(sample);
        }
    }
    for &(sample, _ch, _cmd, _val) in &song.channel_fx {
        last = last.max(sample);
    }
    song.row_of(last)
}

/// A sample `Song.row_of` maps back to `row`.
pub fn row_to_sample(song: &Song, row: i32) -> i64 {
    song.t0 + py_int((row as f64 * song.samples_per_row() + 1e-6).floor())
}

// The wrap is checked against the recording's own copy of the loop head,
// and the copy can drift a row or two. `_COPY_DRIFT_WIDE` is the drift that
// still counts as the same music when a start is judged on its own.
pub const _COPY_DRIFT: i32 = 3;
pub const _COPY_DRIFT_WIDE: i32 = 8;
// Comparison window in rows, the agreement share, and the thin-window floor:
// a handful of notes is not evidence.
pub const _COPY_SPAN: i32 = 96;
pub const _COPY_SHARE: f64 = 0.95;
pub const _COPY_SHARE_WIDE: f64 = 0.90;
pub const _COPY_MIN_EVENTS: i32 = 4;
// A pick this far past the last recorded copy row has no copy of its own; a
// few rows of overshoot still reads the stretch behind it.
pub const _COPY_EDGE: i32 = 3;

/// (channel, pitch) -> rows, for the copy comparison.
pub fn copy_index(notes: &[(i32, i32, i32)]) -> IndexMap<(i32, i32), Vec<i32>> {
    let mut by: IndexMap<(i32, i32), Vec<i32>> = IndexMap::new();
    for &(row, ch, pitch) in notes {
        by.entry((ch, pitch)).or_default().push(row);
    }
    for rows in by.values_mut() {
        rows.sort();
    }
    by
}

fn bisect_left(rows: &[i32], value: i32) -> usize {
    rows.partition_point(|&row| row < value)
}

/// Matched and total notes each way over the recorded copy.
///
/// Forward counts the start's own notes that come back one loop length
/// later. Backward counts the copy's notes that the start accounts for. The
/// comparison stops where the recording does: past the declared end there is
/// no copy to check against.
pub fn copy_shares(
    by: &IndexMap<(i32, i32), Vec<i32>>,
    start: i32,
    length: i32,
    end_row: i32,
    drift: i32,
) -> (i32, i32, i32, i32) {
    let head_end = std::cmp::min(start + _COPY_SPAN, end_row - length + 1);
    let copy_end = std::cmp::min(start + length + _COPY_SPAN, end_row + 1);
    let mut fwd = 0;
    let mut total_fwd = 0;
    for rows in by.values() {
        let mut i = bisect_left(rows, start);
        while i < rows.len() && rows[i] < head_end {
            total_fwd += 1;
            let target = rows[i] + length;
            let j = bisect_left(rows, target - drift);
            if j < rows.len() && rows[j] <= target + drift {
                fwd += 1;
            }
            i += 1;
        }
    }
    let mut back = 0;
    let mut total_back = 0;
    for rows in by.values() {
        let mut i = bisect_left(rows, start + length);
        while i < rows.len() && rows[i] < copy_end {
            total_back += 1;
            let target = rows[i] - length;
            let j = bisect_left(rows, target - drift);
            if j < rows.len() && rows[j] <= target + drift {
                back += 1;
            }
            i += 1;
        }
    }
    (fwd, total_fwd, back, total_back)
}

pub fn copy_faithful_share(
    by: &IndexMap<(i32, i32), Vec<i32>>,
    start: i32,
    length: i32,
    end_row: i32,
    drift: i32,
    share: f64,
) -> bool {
    let (fwd, total_fwd, back, total_back) = copy_shares(by, start, length, end_row, drift);
    if total_fwd < _COPY_MIN_EVENTS || total_back < _COPY_MIN_EVENTS {
        return false;
    }
    fwd as f64 >= total_fwd as f64 * share && back as f64 >= total_back as f64 * share
}

pub fn copy_faithful(
    by: &IndexMap<(i32, i32), Vec<i32>>,
    start: i32,
    length: i32,
    end_row: i32,
    drift: i32,
) -> bool {
    copy_faithful_share(by, start, length, end_row, drift, _COPY_SHARE)
}

/// The row displacement the recording's copy of the head measures.
///
/// Every matched pair contributes its own displacement. The copy's period is
/// the one most pairs agree on. None when there are too few pairs to tell.
pub fn copy_period(
    by: &IndexMap<(i32, i32), Vec<i32>>,
    start: i32,
    length: i32,
    end_row: i32,
    drift: i32,
) -> Option<i32> {
    let mut counts: IndexMap<i32, i32> = IndexMap::new();
    let head_end = std::cmp::min(start + _COPY_SPAN, end_row - length + 1);
    for rows in by.values() {
        let mut i = bisect_left(rows, start);
        while i < rows.len() && rows[i] < head_end {
            let target = rows[i] + length;
            let j = bisect_left(rows, target - drift);
            if j < rows.len() && rows[j] <= target + drift {
                *counts.entry(rows[j] - rows[i]).or_insert(0) += 1;
            }
            i += 1;
        }
    }
    if counts.is_empty() {
        return None;
    }
    let mut items: Vec<(i32, i32)> = counts.iter().map(|(&period, &pairs)| (period, pairs)).collect();
    items.sort_by_key(|kv| kv.0);
    let mut best: Option<(i32, i32)> = None;
    for &(period, pairs) in &items {
        if best.map_or(true, |(_, best_pairs)| pairs > best_pairs) {
            best = Some((period, pairs));
        }
    }
    let (period, pairs) = best.unwrap();
    if pairs >= _COPY_MIN_EVENTS { Some(period) } else { None }
}

/// The start the recording's own copy puts the wrap on.
///
/// A pick below the verified stretch wraps into rows the copy does not
/// answer; a few rows past the last copy row still reads the stretch
/// behind it, and further out the pick stands.
pub fn copy_aligned(
    notes: &[(i32, i32, i32)],
    picked: i32,
    end_row: i32,
    length: Option<i32>,
    window: i32,
) -> i32 {
    let length = match length {
        None => return picked,
        Some(length) if length < 16 => return picked,
        Some(length) => length,
    };
    let by = copy_index(notes);
    let top = end_row - length;
    let mut stretch: Option<i32> = None;
    let mut start = top;
    let stop = std::cmp::max(-1, top - window);
    while start > stop {
        if start < 0 {
            break;
        }
        if copy_faithful(&by, start, length, end_row, _COPY_DRIFT) {
            stretch = Some(start);
        }
        start -= 1;
    }
    let stretch = match stretch {
        None => return picked,
        Some(stretch) => stretch,
    };
    if picked <= top {
        if picked < stretch
            && !copy_faithful_share(
                &by,
                picked,
                length,
                end_row,
                _COPY_DRIFT_WIDE,
                _COPY_SHARE_WIDE,
            )
        {
            return stretch;
        }
        return picked;
    }
    if picked <= top + _COPY_EDGE {
        return stretch;
    }
    picked
}

/// The VGM header's loop length in rows.
pub fn declared_length(song: &Song) -> Option<i32> {
    if song.vgm.loop_samples == 0 || song.samples_per_row() <= 0.0 {
        return None;
    }
    let length = py_round(song.vgm.loop_samples as f64 / song.samples_per_row()) as i32;
    if length >= 2 { Some(length) } else { None }
}

/// Store one loop and make the played iteration match the header length.
pub fn write_loop(
    song: &mut Song,
    start_row: i32,
    end_row: i32,
    text: &str,
    marker: Option<i64>,
) -> String {
    let rows = end_row - start_row + 1;
    if song.vgm.loop_samples != 0 && rows > 0 {
        // The loop is a whole number of rows, and the measured row length can
        // miss the header's length by a fraction of a sample. Over a long loop
        // that fraction becomes milliseconds of drift per repeat. The row is
        // re-derived from the loop. The correction stays under 1%.
        let target = song.vgm.loop_samples as f64 / rows as f64;
        let old = song.samples_per_row();
        if old > 0.0 && (target / old - 1.0).abs() <= 0.01 {
            song.hz *= old / target;
        }
    }
    // A tenth of a row in. Every row rate reads the point as the row the
    // loop opens on, not the row below.
    let nudge = py_int(song.samples_per_row() / 10.0);
    song.loop_start_row = Some(start_row);
    song.loop_end_row = Some(end_row);
    song.loop_marker_sample = Some(match marker {
        Some(marker) => marker,
        None => row_to_sample(song, start_row),
    });
    song.loop_sample = Some(row_to_sample(song, start_row) + nudge);
    song.loop_end_sample = Some(row_to_sample(song, end_row) + nudge);
    // The wrap sample: content at or past it is the copy of the head and
    // stays unwritten at every row rate. A copy that drifts a hair early
    // still lands past it.
    let mut cut = row_to_sample(song, end_row + 1);
    if song.vgm.loop_samples != 0 {
        cut = std::cmp::min(cut, song.loop_sample.unwrap() + song.vgm.loop_samples);
    }
    song.loop_cut_sample = Some(cut);
    song.warnings.push(text.to_string());
    text.to_string()
}

/// Set `song`'s loop start and end. None when the VGM has no loop.
///
/// The header's loop length is the source's own statement of one iteration.
/// The end is always written from it. The start stays on the marker: a
/// marker placed after a pickup means the pickup plays once in the intro and
/// again at the end of each iteration. Moving the start back would strike it
/// twice per loop.
pub fn apply_loop(song: &mut Song) -> Option<String> {
    let loop_sample = song.loop_sample?;
    let loop_row = std::cmp::max(0, song.row_of(loop_sample));
    let length = declared_length(song);
    let notes: Vec<(i32, i32, i32)> = song
        .events
        .iter()
        .filter(|ev| ev.on && 0 <= ev.note && ev.note < NOTE_OFF)
        .map(|ev| (song.row_of(ev.sample), ev.ch, ev.note))
        .collect();
    if let Some(length) = length {
        let mut rows = length;
        // The recording's copy can measure the exact row the head returns on.
        // Use it only for a small correction. A long tail is the next
        // iteration's own head, not a different period.
        if !notes.is_empty() {
            let last_row = last_content_row(song);
            let by = copy_index(&notes);
            if copy_faithful(&by, loop_row, length, last_row, _COPY_DRIFT) {
                if let Some(period) = copy_period(&by, loop_row, length, last_row, _COPY_DRIFT) {
                    if (period - length).abs() <= _COPY_DRIFT {
                        rows = period;
                    }
                }
            }
        }
        // Anchor the marker to its nearest row in the final grid. The row
        // length is about to be derived from the loop. Reading the marker row
        // with the old length could put the start one row off.
        let start_row = if song.vgm.loop_samples > 0 && rows > 0 {
            py_int(py_round(
                (loop_sample - song.t0) as f64 * rows as f64 / song.vgm.loop_samples as f64,
            )) as i32
        } else {
            loop_row
        };
        let start_row = std::cmp::max(0, start_row);
        let end_row = start_row + rows - 1;
        return Some(write_loop(
            song,
            start_row,
            end_row,
            "loop kept",
            Some(loop_sample),
        ));
    }

    // A rip with no declared length keeps the finder's own estimate.
    let end_row = last_content_row(song);
    if notes.is_empty() || end_row < loop_row {
        return Some("loop kept".to_string());
    }
    // Four seconds either side of the marker, or the whole loop when it is
    // shorter. The seam search applies that cap itself.
    let mut forward = 0;
    if song.hz > 0.0 && song.speed > 0 {
        forward = py_int(py_round(4.0 * song.hz / song.speed as f64)) as i32;
    }
    let fix = find_loop(&notes, loop_row, end_row, Some(forward));
    let picked = std::cmp::max(
        fix.as_ref().map_or(loop_row, |fix| fix.start_row),
        loop_row,
    );
    let start_row = std::cmp::max(
        copy_aligned(&notes, picked, end_row, None, forward),
        loop_row,
    );
    let mut fix = match fix {
        None => {
            // A loop marker with no declared length and no repeating body: the
            // whole remaining track is one iteration. Never invent a one-row
            // loop, which would collapse the module to a single order.
            return Some(write_loop(
                song,
                start_row,
                end_row,
                "loop kept; no repeat found, using the whole track",
                None,
            ));
        }
        Some(fix) => fix,
    };
    if start_row != fix.start_row {
        fix = LoopFix {
            start_row,
            end_row: fix.end_row,
            period: 0,
            shift_pulses: 0,
            step_rows: 1,
        };
    }
    let text = describe_fix(&fix, loop_row);
    Some(write_loop(song, start_row, fix.end_row, &text, None))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::NoteEvent;

    fn test_song() -> Song {
        Song {
            vgm: crate::vgm::VgmFile::default(),
            t0: 0,
            loop_sample: None,
            speed: 0,
            hz: 0.0,
            fm_patches: Default::default(),
            pcm_samples: Default::default(),
            events: Default::default(),
            warnings: Default::default(),
            oki_samples: Default::default(),
            msm6258_samples: Default::default(),
            segapcm_samples: Default::default(),
            segapcm_ids: Default::default(),
            rf5c68_samples: Default::default(),
            rf5c164_samples: Default::default(),
            c140_samples: Default::default(),
            c219_samples: Default::default(),
            c219_ids: Default::default(),
            c219_count: 0,
            c352_samples: Default::default(),
            k053260_samples: Default::default(),
            k054539_samples: Default::default(),
            k054539_ids: Default::default(),
            k054539_count: 0,
            ymz280b_samples: Default::default(),
            ymz280b_ids: Default::default(),
            ymz280b_count: 0,
            multipcm_samples: Default::default(),
            multipcm_ids: Default::default(),
            multipcm_count: 0,
            upd7759_samples: Default::default(),
            upd7759_ids: Default::default(),
            upd7759_count: 0,
            pwm_samples: Default::default(),
            pwm_ids: Default::default(),
            pwm_count: 0,
            msm5205_samples: Default::default(),
            msm5205_ids: Default::default(),
            msm5205_count: 0,
            ga20_samples: Default::default(),
            ga20_ids: Default::default(),
            ga20_count: 0,
            msm5232_ids: Default::default(),
            msm5232_controls: Default::default(),
            msm5232_count: 0,
            es5503_samples: Default::default(),
            es5503_ids: Default::default(),
            es5503_oscs: Default::default(),
            es5503_rate: 0,
            es5503_count: 0,
            channel_vol_events: Default::default(),
            mikey_ids: Default::default(),
            mikey_ins: Default::default(),
            mikey_samples: Default::default(),
            mikey_count: 0,
            x1_010_ids: Default::default(),
            x1_010_ins: Default::default(),
            x1_010_samples: Default::default(),
            x1_010_count: 0,
            x1_waves: Default::default(),
            x1_envs: Default::default(),
            fds_ids: Default::default(),
            fds_ins: Default::default(),
            fds_waves: Default::default(),
            fds_mod_waves: Default::default(),
            fds_count: 0,
            wonderswan_ids: Default::default(),
            wonderswan_ins: Default::default(),
            wonderswan_waves: Default::default(),
            wonderswan_pcm_samples: Default::default(),
            wonderswan_count: 0,
            wonderswan_pcm_count: 0,
            qsound_ids: Default::default(),
            qsound_ins: Default::default(),
            qsound_samples: Default::default(),
            qsound_count: 0,
            qsound_adpcm_count: 0,
            channel_fx: Default::default(),
            chip_fx: Default::default(),
            grid_note: String::new(),
            legato_steps: 0,
            legato_ramps: 0,
            row_subdiv: 0.0,
            hilight_a: 0,
            hilight_b: 0,
            fm_chip: String::new(),
            fm_systems: Default::default(),
            has_fm_source: false,
            ym2151_ids: Default::default(),
            ym2203_ids: Default::default(),
            ym2203_ext: false,
            ym2203_ssg_count: 0,
            ym2612_ids: Default::default(),
            ym2612_ext: false,
            ym2612_csm: false,
            opl_ids: Default::default(),
            opl_drums: false,
            opll_ids: Default::default(),
            opll_drums: false,
            opll_vrc7: false,
            fm_vol_max: 0,
            fm_notes: 0,
            drum_notes: 0,
            ym2610_ids: Default::default(),
            ym2610_b: false,
            ym2610_ssg_count: 0,
            ym2610_a_samples: Default::default(),
            ym2610_b_samples: Default::default(),
            ym2610_a_count: 0,
            ym2610_b_count: 0,
            ym2608_ids: Default::default(),
            ym2608_ext: false,
            ym2608_ssg_count: 0,
            ym2608_rhythm_count: 0,
            ym2608_rhythm_notes: 0,
            ym2608_b_samples: Default::default(),
            ym2608_b_count: 0,
            y8950_b_samples: Default::default(),
            y8950_b_count: 0,
            opl4_wave_samples: Default::default(),
            opl4_wave_count: 0,
            k007232_volume: 0.0,
            loop_end_sample: None,
            loop_start_row: None,
            loop_end_row: None,
            loop_marker_sample: None,
            loop_cut_sample: None,
            c352_quad: false,
            scc_ids: Default::default(),
            scc_waves: Default::default(),
            scc_ins: Default::default(),
            scc_count: 0,
            k005289_ids: Default::default(),
            k005289_waves: Default::default(),
            k005289_ins: Default::default(),
            k005289_count: 0,
            ay_ids: Default::default(),
            ay_ins: Default::default(),
            ay_count: 0,
            pokey_ids: Default::default(),
            pokey_ins: Default::default(),
            pokey_count: 0,
            ssg_pcm_samples: Default::default(),
            ssg_pcm_count: 0,
            nes_ids: Default::default(),
            nes_present: false,
            nes_ins: Default::default(),
            nes_dpcm_samples: Default::default(),
            nes_pcm_samples: Default::default(),
            nes_count: 0,
            nes_pcm_count: 0,
            huc6280_ids: Default::default(),
            huc6280_waves: Default::default(),
            huc6280_ins: Default::default(),
            huc6280_pcm_samples: Default::default(),
            huc6280_count: 0,
            huc6280_pcm_count: 0,
            gb_ids: Default::default(),
            gb_waves: Default::default(),
            gb_ins: Default::default(),
            gb_count: 0,
            vsu_ids: Default::default(),
            vsu_waves: Default::default(),
            vsu_mod_waves: Default::default(),
            vsu_ins: Default::default(),
            vsu_count: 0,
            vsu_dynamic: false,
            saa_ids: Default::default(),
            saa_ins: Default::default(),
            saa_count: 0,
            psg_ids: Default::default(),
            psg_ins: Default::default(),
            psg_count: 0,
            psg_stereo: false,
            psg_t6w28: false,
            psg_dac_samples: Default::default(),
            psg_dac_plays: Default::default(),
            psg_dac_voices: Default::default(),
            psg_dac_rate: 0,
            psg_dac_count: 0,
            psg_dac_stats: Default::default(),
            dac_samples: Default::default(),
            dac_count: 0,
            dac_stats: Default::default(),
        }
    }

    #[test]
    fn clock_finds_the_dense_voice() {
        // One voice on every fourth row for 128 rows: step 4, phase 0.
        let notes: Vec<(i32, i32, i32)> = (0..=32)
            .map(|i| (i * 4, 0, (i % 4) as i32))
            .collect();
        let found = clock(&notes).expect("clock");
        assert_eq!(found.step, 4);
        assert_eq!(found.phase, 0);
        assert_eq!(found.channel, 0);
        assert!((found.regularity - 1.0).abs() < 1e-12);
    }

    #[test]
    fn period_prefers_the_shorter_identical_cell() {
        // Beats 0..32 repeat every 4; 8/12/16 also match, ties keep 4.
        let notes: Vec<(i32, i32, i32)> = (0..=32)
            .map(|beat| (beat * 4, 0, (beat % 4) as i32))
            .collect();
        let found = clock(&notes).unwrap();
        let grid = grid(&notes, &found);
        let echoes = echoes(&grid, found.channel);
        let (period, ac) = period(&grid, &echoes, 0, 32).expect("period");
        assert_eq!(period, 4);
        assert!((ac - 1.0).abs() < 1e-12);
    }

    #[test]
    fn empty_marker_snaps_before_the_chord() {
        let notes = vec![(7, 0, 60), (7, 1, 64), (7, 2, 67)];
        let fix = snap_empty_marker(&notes, 8, 20, 4).expect("fix");
        assert_eq!(fix.start_row, 6);
        assert_eq!(fix.end_row, 18);
        assert_eq!(fix.step_rows, 4);
        assert_eq!(fix.period, 0);
        assert_eq!(fix.shift_pulses, 0);
    }

    #[test]
    fn describe_fix_matches_the_convert_clause() {
        let rotated = LoopFix {
            start_row: 10,
            end_row: 40,
            period: 8,
            shift_pulses: 2,
            step_rows: 4,
        };
        assert_eq!(describe_fix(&rotated, 12), "loop +2/8 (-2 rows)");
        let early = LoopFix {
            start_row: 10,
            end_row: 40,
            period: 0,
            shift_pulses: 0,
            step_rows: 4,
        };
        assert_eq!(describe_fix(&early, 12), "loop 2 rows early");
        assert_eq!(describe_fix(&early, 11), "loop 1 row early");
        assert_eq!(
            describe_fix(
                &LoopFix {
                    start_row: 12,
                    end_row: 40,
                    period: 0,
                    shift_pulses: 0,
                    step_rows: 4,
                },
                12
            ),
            "loop kept"
        );
    }

    #[test]
    fn apply_loop_writes_the_declared_length() {
        let mut song = test_song();
        song.hz = 50.0;
        song.speed = 1;
        song.t0 = 0;
        song.loop_sample = Some(1764);
        song.vgm.loop_samples = 3528;
        let text = apply_loop(&mut song).expect("loop");
        assert_eq!(text, "loop kept");
        assert_eq!(song.loop_start_row, Some(2));
        assert_eq!(song.loop_end_row, Some(5));
        assert_eq!(song.loop_marker_sample, Some(1764));
        // 882 samples per row; one tenth of a row is 88.
        assert_eq!(song.loop_sample, Some(1852));
        assert_eq!(song.loop_end_sample, Some(4498));
        assert_eq!(song.loop_cut_sample, Some(5292));
        assert_eq!(song.warnings, vec!["loop kept".to_string()]);
        assert!((song.hz - 50.0).abs() < 1e-12);
    }

    #[test]
    fn apply_loop_without_a_marker_is_none() {
        let mut song = test_song();
        song.loop_sample = None;
        assert_eq!(apply_loop(&mut song), None);
        assert_eq!(song.warnings.len(), 0);
    }

    #[test]
    fn note_event_is_still_default_constructible() {
        // Keep the test helper honest about the event fields loopfind reads.
        let ev = NoteEvent::default();
        assert!(!ev.on);
        assert_eq!(ev.note, 0);
    }
}
