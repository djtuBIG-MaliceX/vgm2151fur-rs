//! Tone and wave-table emitters for the analyzer.
//!
//! Each function turns one chip's collected hits/offs into `NoteEvent`s on the
//! song and, where the chip has row effects, appends them to `channel_fx`.
//! Translated from `analyze.py` lines 4223-5041.

use std::collections::HashSet;

use indexmap::IndexMap;

use crate::model::{Song, NoteEvent, pcm_pan_fx};
use crate::pitch::{frequency_pitch_table, furnace_frequency_note, furnace_period_note};
use crate::c140::merge_step_writes;
use crate::pokey::{FX_AUDCTL, FX_WAVE};
use crate::x1_010::{X1_FREQ_DIV, X1_MAX_FREQ};

use crate::ay8910::{AyHit, AyOff, FX_ENV_SET, FX_MODE, FX_NOISE, ay_note};
use crate::mikey::{MIKEY_DIVIDER, MIKEY_MAX_PERIOD, MikeyHit, MikeyOff};
use crate::saa1099::{SAA_DIVIDER, SaaHit, SaaOff, saa_period_note};
use crate::ssgpcm::SsgPcmHit;
use crate::x1_010::{X1Hit, X1Off};

use super::util::{
    _AY_MERGE_GAP, _AY_NOTE_HOLD, _AY_OPEN_WINDOW, _NOTE_HOLD_SAMPLES,
    _NOTE_STEP_SEMITONES, _tone_steps, _value_at, segment_periods,
};

/// Append notes and key-offs for one AY-compatible register stream.
///
/// Used for both a stand-alone AY-8910 (divider 8) and the YM2203's SSG
/// (divider 16).  The channel base map decides where each chip instance
/// lands; the pitch macros use the matching Furnace pitch table.
pub fn _emit_ay_notes(
    song: &mut Song,
    hits: &[AyHit],
    offs: &[AyOff],
    at: &IndexMap<i32, i32>,
    clock: u32,
    divider: f64,
    ins_base: i32,
    total_samples: i64,
) -> i32 {
    let events = &mut song.events;
    let channel_fx = &mut song.channel_fx;
    let mut count = 0;
    for h in hits {
        let Some(&base) = at.get(&h.chip_id) else {
            continue;
        };
        let ch = base + h.voice;
        let st = h.sample_time;
        let gate_end = if h.off_sample != 0 {
            h.off_sample
        } else if total_samples != 0 {
            total_samples
        } else {
            st
        };
        // A hardware envelope is a continuous volume contour. Splitting the
        // hit on a pitch step would trigger a new note and restart that
        // envelope (the phase-reset macro), turning one percussion decay into
        // several attacks. Keep the whole hit as one note and put the pitch
        // motion on its macro instead.
        let modes: Vec<(i64, i32)> = h
            .fx_pts
            .iter()
            .filter(|p| p.1 == FX_MODE)
            .map(|(t, _, val)| (*t, *val))
            .collect();
        // The state on the note row is the last mode write in the opening
        // burst, not the first. A driver can open a note while the previous
        // envelope bit is still set and clear it a few samples later; the
        // pattern can represent only the settled state.
        let mut opening: Option<i32> = None;
        for (t, val) in &modes {
            if *t <= st + _AY_OPEN_WINDOW {
                opening = Some(*val);
            }
        }
        if opening.is_none() && !modes.is_empty() {
            opening = Some(modes[0].1);
        }
        let env_open =
            h.env || opening.is_some_and(|op| ((op + 1) & 4) != 0);
        let env_hit = env_open
            || modes
                .iter()
                .any(|(t, val)| *t > st + _AY_OPEN_WINDOW && ((val + 1) & 4) != 0);
        let segs: Vec<(i64, i32, Vec<(i64, i32)>)> =
            if (h.opened_tone || !h.freq_pts.is_empty()) && !env_hit {
                let mut points: Vec<(i64, i32)> =
                    Vec::with_capacity(1 + h.freq_pts.len());
                points.push((h.sample_time, h.period));
                points.extend_from_slice(&h.freq_pts);
                segment_periods(
                    &points,
                    gate_end,
                    clock as f64,
                    divider,
                    0,
                    _AY_MERGE_GAP,
                    _AY_NOTE_HOLD,
                    false,
                    0xFFF,
                )
            } else {
                let intra = if env_hit {
                    h.freq_pts.clone()
                } else {
                    Vec::new()
                };
                vec![(h.sample_time, h.period, intra)]
            };
        let mut buckets: Vec<Vec<(i64, i32, i32)>> =
            vec![Vec::new(); segs.len()];
        for &(t, fx, val) in &h.fx_pts {
            let mut idx = 0usize;
            for (i, (sst, _per, _intra)) in segs.iter().enumerate() {
                if *sst <= t {
                    idx = i;
                } else {
                    break;
                }
            }
            buckets[idx].push((t, fx, val));
        }
        for i in 0..segs.len() {
            let (sst, period, intra) = &segs[i];
            let (sst, period) = (*sst, *period);
            let nxt = if i + 1 < segs.len() { segs[i + 1].0 } else { 0 };
            let seg_end = if nxt != 0 { nxt } else { h.off_sample };
            let bound = if nxt != 0 { nxt } else { gate_end };
            let (note, e5) = ay_note(clock, period, divider);
            let steps = _tone_steps(intra, sst, 0, _AY_MERGE_GAP);
            let mut vol = _value_at(&h.vol_pts, sst, h.vol);
            if env_open {
                // The envelope generator drives the level; the register's low
                // nibble is ignored. Writing that nibble (usually 0) as the
                // column makes the pattern look like a silent note. The column
                // is 15 whenever a volume macro carries the level.
                vol = 15;
            }
            let vol_pts: Vec<(i64, i32, Option<i32>)> = h
                .vol_pts
                .iter()
                .filter(|p| sst < p.0 && p.0 < bound)
                .map(|(t, v)| (*t, *v, None))
                .collect();
            let prev_st = if i > 0 { segs[i - 1].0 } else { -1 };
            // A pitch split starts the next note in whatever tone/noise mode
            // the register already holds. Its own fx list only has the writes
            // after the split, so a later tone-to-noise handover became the
            // macro's *first and only* value and the note stayed on tone.
            // Seed the settled mode and noise period at the segment start.
            let mut latest: IndexMap<i32, i32> = IndexMap::new();
            for &(t, fx, val) in &h.fx_pts {
                if t <= sst && (fx == FX_MODE || fx == FX_NOISE) {
                    latest.insert(fx, val);
                }
            }
            let mut later: Vec<(i64, i32, i32)> = Vec::new();
            for &(t, fx, val) in &buckets[i] {
                // A source register burst often lands a sample or two after
                // the gate. It belongs to this note's opening state, not the
                // next tick, or a snare would start on the previous hit's
                // noise period.
                if t > sst + _AY_OPEN_WINDOW {
                    later.push((t, fx, val));
                    continue;
                }
                if fx == FX_ENV_SET && i > 0 && t <= prev_st {
                    continue;
                }
                latest.insert(fx, val);
            }
            for (&fx, &val) in &latest {
                channel_fx.push((sst, ch, fx, val));
            }
            // The row effect carries the settled opening value. Keep the same
            // value in the note's effect list so the instrument macro cannot
            // replay a stale earlier value (a shape-8 write followed by shape
            // 9 within the burst used to restart the envelope at shape 8 for a
            // whole tick).
            let mut settled_fx: Vec<(i64, i32, i32)> = latest
                .iter()
                .map(|(&fx, &val)| (sst, fx, val))
                .collect();
            settled_fx.extend_from_slice(&later);
            let mut ev = NoteEvent::new(sst, ch, true, note, ins_base, vol, e5, 0);
            ev.end = seg_end;
            ev.vol_pts = vol_pts;
            ev.tone_pitch_pts = steps;
            ev.tone_chip = 2;
            ev.tone_clock = clock;
            ev.tone_div = divider as i32;
            ev.tone_fx_pts = settled_fx;
            events.push(ev);
            count += 1;
        }
    }
    for off in offs {
        let Some(&base) = at.get(&off.chip_id) else {
            continue;
        };
        events.push(NoteEvent::new(
            off.sample_time,
            base + off.voice,
            false,
            180,
            -1,
            -1,
            0x80,
            0,
        ));
    }
    count
}

/// Append Lynx LFSR notes/key-offs.
///
/// Pitch is the timer divider ``(backup+1)<<shift``; Furnace's period table
/// turns it back into note+E5.  Duty/int, LFSR seed and pan ride the
/// instrument macros or 08xx effects.  A large held pitch step becomes a new
/// note via `segment_periods`, exactly like POKEY.
pub fn _emit_mikey_notes(
    song: &mut Song,
    hits: &[MikeyHit],
    offs: &[MikeyOff],
    at: &IndexMap<i32, i32>,
    total_samples: i64,
) -> i32 {
    let events = &mut song.events;
    let channel_fx = &mut song.channel_fx;
    let mut count = 0;
    for h in hits {
        let Some(&base) = at.get(&h.chip_id) else {
            continue;
        };
        let ch = base + h.voice;
        let st = h.sample_time;
        let gate_end = if h.off_sample != 0 {
            h.off_sample
        } else if total_samples != 0 {
            total_samples
        } else {
            st
        };
        let mut points: Vec<(i64, i32)> = Vec::with_capacity(1 + h.freq_pts.len());
        points.push((st, h.value));
        points.extend_from_slice(&h.freq_pts);
        let segs = segment_periods(
            &points,
            gate_end,
            h.clock as f64,
            MIKEY_DIVIDER as f64,
            0,
            _AY_MERGE_GAP,
            _AY_NOTE_HOLD,
            false,
            MIKEY_MAX_PERIOD,
        );
        for i in 0..segs.len() {
            let (sst, value, intra) = &segs[i];
            let (sst, value) = (*sst, *value);
            let nxt = if i + 1 < segs.len() { segs[i + 1].0 } else { 0 };
            let seg_end = if nxt != 0 { nxt } else { h.off_sample };
            let bound = if nxt != 0 { nxt } else { gate_end };
            let (note, e5) = furnace_period_note(
                h.clock as f64,
                MIKEY_DIVIDER as f64,
                value.max(1),
                0,
                MIKEY_MAX_PERIOD,
            );
            let steps = _tone_steps(intra, sst, 0, _AY_MERGE_GAP);
            let vol = _value_at(&h.vol_pts, sst, h.vol);
            let vol_pts: Vec<(i64, i32, Option<i32>)> = h
                .vol_pts
                .iter()
                .filter(|p| sst < p.0 && p.0 < bound)
                .map(|(t, v)| (*t, *v, None))
                .collect();
            let duty_at = _value_at(&h.duty_pts, sst, h.duty);
            let duty_steps: Vec<(i64, i32)> = h
                .duty_pts
                .iter()
                .filter(|p| sst < p.0 && p.0 < bound)
                .copied()
                .collect();
            let lfsr_at = _value_at(&h.lfsr_pts, sst, h.lfsr);
            let lfsr_steps: Vec<(i64, i32)> = h
                .lfsr_pts
                .iter()
                .filter(|p| sst < p.0 && p.0 < bound)
                .copied()
                .collect();
            let mut ev = NoteEvent::new(
                sst,
                ch,
                true,
                note,
                -1,
                vol,
                e5,
                _value_at(&h.pan_pts, sst, h.pan),
            );
            ev.end = seg_end;
            ev.vol_pts = vol_pts;
            ev.tone_pitch_pts = steps;
            ev.tone_chip = 10;
            ev.tone_clock = h.clock;
            ev.tone_div = MIKEY_DIVIDER;
            ev.tone_period_offset = 0;
            ev.tone_period = value;
            ev.tone_duty = duty_at;
            ev.tone_duty_pts = duty_steps;
            ev.tone_lfsr = lfsr_at;
            ev.tone_lfsr_pts = lfsr_steps;
            events.push(ev);
            count += 1;
            for &(t, pan) in &h.pan_pts {
                if sst < t && t < bound {
                    channel_fx.push((t, ch, 0x08, pan));
                }
            }
        }
    }
    for off in offs {
        let Some(&base) = at.get(&off.chip_id) else {
            continue;
        };
        events.push(NoteEvent::new(
            off.sample_time,
            base + off.voice,
            false,
            180,
            -1,
            -1,
            0x80,
            0,
        ));
    }
    count
}

pub fn _x1_semitones(clock: u32, value: i32) -> f64 {
    frequency_pitch_table(clock as f64, X1_FREQ_DIV as f64, X1_MAX_FREQ)
        .note_fraction(value.max(1))
}

/// Split an X1 frequency timeline into notes (frequency mode, not period).
pub fn _segment_x1_freq(
    points: &[(i64, i32)],
    end: i64,
    clock: u32,
    merge_gap: i64,
    min_hold: i64,
) -> Vec<(i64, i32, Vec<(i64, i32)>)> {
    if points.is_empty() {
        return Vec::new();
    }
    let merged = merge_step_writes(points, merge_gap);
    let (mut cur_t, mut cur_v) = merged[0];
    let mut cur_s = _x1_semitones(clock, cur_v);
    let mut intra: Vec<(i64, i32)> = Vec::new();
    let mut segs: Vec<(i64, i32, Vec<(i64, i32)>)> = Vec::new();
    for i in 1..merged.len() {
        let (t, value) = merged[i];
        let hold_until = if i + 1 < merged.len() {
            merged[i + 1].0
        } else {
            end
        };
        let held = hold_until - t;
        let new_s = _x1_semitones(clock, value);
        let step = (new_s - cur_s).abs();
        if step >= _NOTE_STEP_SEMITONES && held >= min_hold {
            segs.push((cur_t, cur_v, intra));
            cur_t = t;
            cur_v = value;
            cur_s = new_s;
            intra = Vec::new();
        } else {
            intra.push((t, value));
            cur_s = new_s;
        }
    }
    segs.push((cur_t, cur_v, intra));
    segs
}

/// Append X1-010 wavetable notes and key-offs.
///
/// Wave pitch is the source 16-bit frequency register, inverted through
/// Furnace's frequency-mode pitch table.  Envelope mode/shape/period and
/// waveform changes become instrument macros.
pub fn _emit_x1_wave_notes(
    song: &mut Song,
    hits: &[X1Hit],
    offs: &[X1Off],
    at: &IndexMap<i32, i32>,
    wave_map: &IndexMap<Vec<u8>, i32>,
    env_map: &IndexMap<Vec<u8>, i32>,
    total_samples: i64,
) -> i32 {
    let events = &mut song.events;
    let mut count = 0;
    for h in hits {
        let Some(&base) = at.get(&h.chip_id) else {
            continue;
        };
        let ch = base + h.voice as i32;
        let st = h.sample_time;
        let gate_end = if h.off_sample != 0 {
            h.off_sample
        } else if total_samples != 0 {
            total_samples
        } else {
            st
        };
        let mut points: Vec<(i64, i32)> = Vec::with_capacity(1 + h.freq_pts.len());
        points.push((st, h.freq));
        points.extend_from_slice(&h.freq_pts);
        let segs = _segment_x1_freq(&points, gate_end, h.clock, _AY_MERGE_GAP, _AY_NOTE_HOLD);
        let wave_idx = *wave_map.get(&h.wave_data).unwrap_or(&0);
        let env_idx = *env_map.get(&h.env_data).unwrap_or(&0);
        let const_key = vec![0xFFu8; 128];
        let const_env = *env_map.get(&const_key).unwrap_or(&env_idx);
        let period_zero = h.env_period == 0;
        let first_byte = h.env_data[0];
        let initial_lr: (i32, i32) = if period_zero {
            (((first_byte >> 4) & 0x0F) as i32, (first_byte & 0x0F) as i32)
        } else {
            (15, 15)
        };
        // A period-0 envelope holds the first byte of whichever shape is
        // selected. Drivers use that byte as a stereo level and switch shapes
        // or rewrite the byte to ramp or pan the note, so keep the left/right
        // pair rather than a mono average.
        let mut lr_by_time: IndexMap<i64, (i32, i32)> = IndexMap::new();
        if period_zero {
            for &(t, left, right) in &h.env_lr_pts {
                lr_by_time.insert(t, (left, right));
            }
        }
        let mut lr_pts: Vec<(i64, i32, i32)> = lr_by_time
            .iter()
            .map(|(&t, &(left, right))| (t, left, right))
            .collect();
        lr_pts.sort_by_key(|p| p.0);
        for i in 0..segs.len() {
            let (sst, value, intra) = &segs[i];
            let (sst, value) = (*sst, *value);
            let nxt = if i + 1 < segs.len() { segs[i + 1].0 } else { 0 };
            let seg_end = if nxt != 0 { nxt } else { h.off_sample };
            let bound = if nxt != 0 { nxt } else { gate_end };
            let (note, e5) = furnace_frequency_note(
                h.clock as f64,
                X1_FREQ_DIV as f64,
                value.max(1),
                X1_MAX_FREQ,
            );
            let mut wave_pts: Vec<(i64, i32)> = Vec::new();
            for (t, body) in &h.wave_pts {
                let mapped = *wave_map.get(body).unwrap_or(&wave_idx);
                if mapped != wave_idx {
                    wave_pts.push((*t, mapped));
                }
            }
            let env_pts: Vec<(i64, i32)> = if period_zero {
                Vec::new()
            } else {
                let mut out = Vec::new();
                for (t, body) in &h.env_pts {
                    let mapped = *env_map.get(body).unwrap_or(&env_idx);
                    if mapped != env_idx {
                        out.push((*t, mapped));
                    }
                }
                out
            };
            let (mut left, mut right) = initial_lr;
            for &(t, l, r) in &lr_pts {
                if t <= sst {
                    left = l;
                    right = r;
                }
            }
            let level = if period_zero { left.max(right) } else { 15 };
            let pan = if period_zero { pcm_pan_fx(left, right) } else { 0 };
            let vol_pts: Vec<(i64, i32, Option<i32>)> = if period_zero {
                lr_pts
                    .iter()
                    .filter(|p| sst < p.0 && p.0 < bound)
                    .map(|(t, l, r)| (*t, (*l).max(*r), Some(pcm_pan_fx(*l, *r))))
                    .collect()
            } else {
                Vec::new()
            };
            let mode = 1 | if h.env_oneshot { 2 } else { 0 };
            let mut ev = NoteEvent::new(sst, ch, true, note, -1, level, e5, pan);
            ev.end = seg_end;
            ev.vol_pts = vol_pts;
            ev.tone_pitch_pts = intra.clone();
            ev.tone_chip = 11;
            ev.tone_clock = h.clock;
            ev.tone_div = X1_FREQ_DIV;
            ev.tone_period = value;
            ev.tone_wave = wave_idx;
            ev.x1_wave_pts = wave_pts;
            ev.tone_env_shape = if period_zero { const_env } else { env_idx };
            ev.x1_env_shape_pts = env_pts;
            ev.tone_env_period = h.env_period;
            ev.x1_env_period_pts = h
                .env_period_pts
                .iter()
                .filter(|p| sst < p.0 && p.0 < bound)
                .copied()
                .collect();
            ev.tone_env_mode = mode;
            ev.x1_env_mode_pts = h
                .env_mode_pts
                .iter()
                .filter(|p| sst < p.0 && p.0 < bound)
                .copied()
                .collect();
            events.push(ev);
            count += 1;
        }
    }
    for off in offs {
        let Some(&base) = at.get(&off.chip_id) else {
            continue;
        };
        events.push(NoteEvent::new(
            off.sample_time,
            base + off.voice as i32,
            false,
            180,
            -1,
            -1,
            0x80,
            0,
        ));
    }
    count
}

/// Append notes/key-offs for one POKEY register stream.
///
/// AUDCTL is chip-wide, so one 0x11 effect per change goes on the chip's first
/// column.  Waveform changes are per column (0x10).  The instrument itself
/// carries the per-frame volume contour and the pitch trajectory.
pub fn _emit_pokey_notes(
    song: &mut Song,
    hits: &[crate::pokey::PokeyHit],
    offs: &[crate::pokey::PokeyOff],
    at: &IndexMap<i32, i32>,
    audctl_pts: &[(i64, i32, i32)],
    wave_pts: &[(i64, i32, i32, i32)],
    ins_base: i32,
    total_samples: i64,
) -> i32 {
    let events = &mut song.events;
    let channel_fx = &mut song.channel_fx;
    let mut count = 0;
    let mut wave_by_ch: IndexMap<(i32, i32), Vec<(i64, i32)>> = IndexMap::new();
    let mut sorted_waves: Vec<(i64, i32, i32, i32)> = wave_pts.to_vec();
    sorted_waves.sort_by_key(|p| p.0);
    for &(sample, cid, voice, val) in &sorted_waves {
        wave_by_ch.entry((cid, voice)).or_default().push((sample, val));
    }
    let mut audctl_by_cid: IndexMap<i32, Vec<(i64, i32)>> = IndexMap::new();
    let mut sorted_audctl: Vec<(i64, i32, i32)> = audctl_pts.to_vec();
    sorted_audctl.sort_by_key(|p| p.0);
    for &(sample, cid, val) in &sorted_audctl {
        audctl_by_cid.entry(cid).or_default().push((sample, val));
    }
    for h in hits {
        let Some(&base) = at.get(&h.chip_id) else {
            continue;
        };
        let ch = base + h.voice;
        let st = h.sample_time;
        let gate_end = if h.off_sample != 0 {
            h.off_sample
        } else if total_samples != 0 {
            total_samples
        } else {
            st
        };
        let empty_wave: Vec<(i64, i32)> = Vec::new();
        let wpts: &[(i64, i32)] = wave_by_ch
            .get(&(h.chip_id, h.voice))
            .map_or(&empty_wave[..], |v| &v[..]);
        let empty_audctl: Vec<(i64, i32)> = Vec::new();
        let apts: &[(i64, i32)] = audctl_by_cid
            .get(&h.chip_id)
            .map_or(&empty_audctl[..], |v| &v[..]);
        let mut points: Vec<(i64, i32)> = Vec::with_capacity(1 + h.freq_pts.len());
        points.push((st, h.value));
        points.extend_from_slice(&h.freq_pts);
        let segs = segment_periods(
            &points,
            gate_end,
            h.clock as f64,
            1.0,
            0,
            _AY_MERGE_GAP,
            _AY_NOTE_HOLD,
            false,
            crate::pokey::POKEY_PITCH_MAX,
        );
        for i in 0..segs.len() {
            let (sst, value, intra) = &segs[i];
            let (sst, value) = (*sst, *value);
            let nxt = if i + 1 < segs.len() { segs[i + 1].0 } else { 0 };
            let seg_end = if nxt != 0 { nxt } else { h.off_sample };
            let bound = if nxt != 0 { nxt } else { gate_end };
            let (note, e5) = crate::pokey::pokey_note(h.clock, value);
            let steps = _tone_steps(intra, sst, 0, _AY_MERGE_GAP);
            let vol = _value_at(&h.vol_pts, sst, h.vol);
            let vol_pts: Vec<(i64, i32, Option<i32>)> = h
                .vol_pts
                .iter()
                .filter(|p| sst < p.0 && p.0 < bound)
                .map(|(t, v)| (*t, *v, None))
                .collect();
            // AUDC's distortion and AUDCTL live in the POKEY instrument's
            // wave and duty macros, so each note carries the timbre state at
            // its own tick instead of depending only on a row effect.
            let wave_at = _value_at(wpts, sst, 5);
            let mut wave_steps: Vec<(i64, i32)> = vec![(sst, wave_at)];
            wave_steps.extend(wpts.iter().filter(|p| sst < p.0 && p.0 < bound).copied());
            let audctl_at = _value_at(apts, sst, 0);
            let mut audctl_steps: Vec<(i64, i32, i32)> =
                vec![(sst, FX_AUDCTL, audctl_at)];
            audctl_steps.extend(
                apts.iter()
                    .filter(|p| sst < p.0 && p.0 < bound)
                    .map(|(t, v)| (*t, FX_AUDCTL, *v)),
            );
            let mut ev = NoteEvent::new(sst, ch, true, note, ins_base, vol, e5, 0);
            ev.end = seg_end;
            ev.vol_pts = vol_pts;
            ev.tone_pitch_pts = steps;
            ev.tone_chip = 9;
            ev.tone_clock = h.clock;
            ev.tone_div = 1;
            ev.tone_period_offset = 0;
            ev.tone_wave = wave_at;
            ev.tone_wave_pts = wave_steps;
            ev.tone_fx_pts = audctl_steps;
            events.push(ev);
            count += 1;
        }
    }
    for off in offs {
        let Some(&base) = at.get(&off.chip_id) else {
            continue;
        };
        events.push(NoteEvent::new(
            off.sample_time,
            base + off.voice,
            false,
            180,
            -1,
            -1,
            0x80,
            0,
        ));
    }
    for &(sample, cid, val) in audctl_pts {
        let Some(&base) = at.get(&cid) else {
            continue;
        };
        channel_fx.push((sample, base, FX_AUDCTL, val));
    }
    let mut last_wave_val: IndexMap<i32, i32> = IndexMap::new();
    for &(sample, cid, voice, val) in &sorted_waves {
        let Some(&base) = at.get(&cid) else {
            continue;
        };
        let ch = base + voice;
        if last_wave_val.get(&ch) == Some(&val) {
            continue;
        }
        last_wave_val.insert(ch, val);
        channel_fx.push((sample, ch, FX_WAVE, val));
    }
    count
}

pub fn _saa_amp_at(h: &SaaHit, t: i64) -> (i32, i32) {
    let (mut left, mut right) = (h.left, h.right);
    for pt in &h.amp_pts {
        if pt.0 <= t {
            left = pt.1;
            right = pt.2;
        } else {
            break;
        }
    }
    (left, right)
}

pub fn _saa_mode_at(h: &SaaHit, t: i64) -> (bool, bool) {
    let (mut tone, mut noise) = (h.tone, h.noise);
    for pt in &h.mode_pts {
        if pt.0 <= t {
            tone = pt.1;
            noise = pt.2;
        } else {
            break;
        }
    }
    (tone, noise)
}

pub fn _saa_noise_at(h: &SaaHit, t: i64) -> i32 {
    let mut value = h.noise_freq;
    for pt in &h.noise_pts {
        if pt.0 <= t {
            value = pt.1;
        } else {
            break;
        }
    }
    value
}

pub fn _saa_env_at(h: &SaaHit, t: i64) -> i32 {
    let mut value = h.env;
    for pt in &h.env_pts {
        if pt.0 <= t {
            value = pt.1;
        } else {
            break;
        }
    }
    value
}

/// Furnace 10xy: high nibble toggles noise, low nibble tone.
pub fn _saa_mode_effect(tone: bool, noise: bool) -> i32 {
    (if noise { 0x10 } else { 0 }) | (if tone { 0x01 } else { 0 })
}

/// Platform psgMode: bit 0 tone, bit 1 noise.
pub fn _saa_mode_macro(tone: bool, noise: bool) -> i32 {
    (if tone { 0x01 } else { 0 }) | (if noise { 0x02 } else { 0 })
}

/// Append notes and key-offs for one dual-chip SAA1099 stream.
///
/// The amplitude gate is the note delimiter.  Pitch writes inside a gate are
/// the source's pseudo-duty detunes, so they stay on the note's pitch macro
/// instead of becoming new notes.  The raw left/right amplitude nibbles go to
/// the panning macros; the note column is 15 when those macros are kept.
pub fn _emit_saa_notes(
    song: &mut Song,
    hits: &[SaaHit],
    offs: &[SaaOff],
    at: &IndexMap<i32, i32>,
    clock: f64,
    total_samples: i64,
) -> i32 {
    let events = &mut song.events;
    let channel_fx = &mut song.channel_fx;
    let mut count = 0;
    for h in hits {
        let chip_id = h.chip_id as i32;
        let Some(&base) = at.get(&chip_id) else {
            continue;
        };
        let ch = base + h.voice;
        let st = h.sample_time;
        let gate_end = if h.off_sample != 0 {
            h.off_sample
        } else if total_samples != 0 {
            total_samples
        } else {
            st
        };
        if h.hidden {
            // Silent source notes for an envelope generator.  Split at every
            // period change so the note/E5 carries the source voice's exact
            // frequency; one long macro would be thinned and would alias the
            // envelope rate.  Volume 0 keeps these notes inaudible.
            let mut starts: Vec<(i64, i32)> = Vec::with_capacity(1 + h.freq_pts.len());
            starts.push((st, h.period));
            starts.extend_from_slice(&h.freq_pts);
            for i in 0..starts.len() {
                let (when, period) = starts[i];
                let next_t = if i + 1 < starts.len() {
                    starts[i + 1].0
                } else {
                    h.off_sample
                };
                let (src_note, src_e5) = saa_period_note(clock, period);
                let mut ev = NoteEvent::new(when, ch, true, src_note, -1, 0, src_e5, 0);
                ev.end = next_t;
                ev.tone_amp_pts = Vec::new();
                ev.tone_fx_pts = Vec::new();
                ev.tone_chip = 8;
                ev.tone_clock = clock as u32;
                ev.tone_div = SAA_DIVIDER;
                ev.tone_period_offset = 0;
                events.push(ev);
                count += 1;
            }
            continue;
        }
        // A large pitch step held for more than half a frame is a legato note,
        // not a macro: split it so the instrument is shared across the notes
        // of a descent.  Fast pseudo-duty detunes are small and stay on the
        // pitch macro.  The amplitude/pan, mode, noise and envelope state at
        // each split is carried into that note, the same way volume is.
        let mut points: Vec<(i64, i32)> = Vec::with_capacity(1 + h.freq_pts.len());
        points.push((h.sample_time, h.period));
        points.extend_from_slice(&h.freq_pts);
        let segs = segment_periods(
            &points,
            gate_end,
            clock,
            SAA_DIVIDER as f64,
            0,
            _AY_MERGE_GAP,
            i64::max(1, _NOTE_HOLD_SAMPLES / 2),
            false,
            0xFFF,
        );
        for i in 0..segs.len() {
            let (sst, period, intra) = &segs[i];
            let (sst, period) = (*sst, *period);
            let nxt = if i + 1 < segs.len() { segs[i + 1].0 } else { 0 };
            let seg_end = if nxt != 0 { nxt } else { h.off_sample };
            let bound = if nxt != 0 { nxt } else { gate_end };
            let (note, e5) = saa_period_note(clock, period);
            let steps = _tone_steps(intra, sst, 0, _AY_MERGE_GAP);
            let (left, right) = _saa_amp_at(h, sst);
            let vol = left.max(right);
            let pan = pcm_pan_fx(left, right);
            // Volume is the macro (as on the other PSGs); the left/right
            // balance is note/row state.  Keeping balance out of the
            // instrument lets notes with the same level shape share one.
            let mut amp_pts: Vec<(i64, i32, i32)> = vec![(sst, left, right)];
            for pt in &h.amp_pts {
                if sst < pt.0 && pt.0 < bound {
                    amp_pts.push(*pt);
                }
            }
            let vol_pts: Vec<(i64, i32, Option<i32>)> = amp_pts[1..]
                .iter()
                .map(|&(t, l, r)| (t, l.max(r), None))
                .collect();
            let mut last_pan = pan;
            for &(t, l, r) in &amp_pts[1..] {
                let next_pan = pcm_pan_fx(l, r);
                if next_pan != last_pan {
                    channel_fx.push((t, ch, 0x08, next_pan));
                    last_pan = next_pan;
                }
            }
            let (tone, noise) = _saa_mode_at(h, sst);
            let mut mode_pts: Vec<(i64, bool, bool)> = vec![(sst, tone, noise)];
            for pt in &h.mode_pts {
                if sst < pt.0 && pt.0 < bound {
                    mode_pts.push(*pt);
                }
            }
            let nf = _saa_noise_at(h, sst);
            let mut noise_pts: Vec<(i64, i32)> = vec![(sst, nf)];
            for pt in &h.noise_pts {
                if sst < pt.0 && pt.0 < bound {
                    noise_pts.push(*pt);
                }
            }
            let env = _saa_env_at(h, sst);
            let mut env_pts: Vec<(i64, i32)> = vec![(sst, env)];
            for pt in &h.env_pts {
                if sst < pt.0 && pt.0 < bound {
                    env_pts.push(*pt);
                }
            }

            // Row effects for the opening state; the instrument macros carry
            // any later motion.  The initial effects also keep a constant
            // state when the budget falls back to a plain voice.
            channel_fx.push((sst, ch, 0x10, _saa_mode_effect(tone, noise)));
            channel_fx.push((sst, ch, 0x11, nf));
            for &(t, to, no) in &mode_pts[1..] {
                channel_fx.push((t, ch, 0x10, _saa_mode_effect(to, no)));
            }
            for &(t, value) in &noise_pts[1..] {
                channel_fx.push((t, ch, 0x11, value));
            }
            if h.voice % 3 == 2 {
                channel_fx.push((sst, ch, 0x12, env));
                for &(t, value) in &env_pts[1..] {
                    channel_fx.push((t, ch, 0x12, value));
                }
            }

            let mut fx: Vec<(i64, i32, i32)> = mode_pts
                .iter()
                .map(|&(t, to, no)| (t, 0x10, _saa_mode_macro(to, no)))
                .collect();
            fx.extend(noise_pts.iter().map(|&(t, value)| (t, 0x11, value)));
            if h.voice % 3 == 2 {
                fx.extend(env_pts.iter().map(|&(t, value)| (t, 0x12, value)));
            }

            let mut ev = NoteEvent::new(sst, ch, true, note, -1, vol, e5, pan);
            ev.end = seg_end;
            ev.vol_pts = vol_pts;
            ev.tone_pitch_pts = steps;
            ev.tone_fx_pts = fx;
            ev.tone_chip = 8;
            ev.tone_clock = clock as u32;
            ev.tone_div = SAA_DIVIDER;
            ev.tone_period_offset = 0;
            events.push(ev);
            count += 1;
        }
    }
    for off in offs {
        let chip_id = off.chip_id as i32;
        let Some(&base) = at.get(&chip_id) else {
            continue;
        };
        events.push(NoteEvent::new(
            off.sample_time,
            base + off.voice,
            false,
            180,
            -1,
            -1,
            0x80,
            0,
        ));
    }
    count
}

/// Emit one sample-playback note per SSG PCM segment.
///
/// Bodies are grouped 120 to a ``DIV_INS_AY`` sample map.  The pattern note
/// is ``60 + slot`` (60..179); the map stores C-4 for every slot, so the body
/// plays at its stored ``c4_rate``.  The caller binds the instrument once the
/// bases are known.
///
/// The returned indices point into `song.events`; Python returned the event
/// objects themselves, and the caller mutates `note`/`ins` on them later.
pub fn _emit_ssg_pcm_notes(
    song: &mut Song,
    hits: &[SsgPcmHit],
    at: &IndexMap<i32, i32>,
    sample_base: i32,
) -> (i32, Vec<(usize, i32)>) {
    let mut count = 0;
    let mut bound: Vec<(usize, i32)> = Vec::new();
    for h in hits {
        let chip_id = h.chip_id as i32;
        let Some(&base) = at.get(&chip_id) else {
            continue;
        };
        let idx = sample_base + h.sample_index;
        let mut ev = NoteEvent::new(
            h.sample_time,
            base + h.voice,
            true,
            60 + crate::parity::py_mod(idx as i64, 120) as i32,
            -1,
            15,
            0x80,
            0,
        );
        ev.pcm = true;
        ev.end = h.end_time;
        ev.no_pan = true;
        song.events.push(ev);
        let event_index = song.events.len() - 1;
        bound.push((event_index, idx));
        count += 1;
    }
    (count, bound)
}

/// True when a pattern row can follow this trajectory.
///
/// Glides and slow curves have few direction changes. A vibrato that
/// reverses within a row needs a macro.
pub fn _steppable_pitch(
    pts: &[(i64, i32, i32)],
    max_reversals: i32,
    tolerance: f64,
    min_segment_samples: f64,
) -> bool {
    if pts.len() < 3 {
        return true;
    }
    let mut reversals = 0;
    let mut direction = 0;
    let mut last_change = pts[0].0;
    let mut prev = opn_pitch_units(pts[0].1, pts[0].2);
    for &(t, b, f) in &pts[1..] {
        let cur = opn_pitch_units(b, f);
        let d = cur - prev;
        prev = cur;
        let new = if d > tolerance {
            1
        } else if d < -tolerance {
            -1
        } else {
            continue;
        };
        if direction != 0 && new != direction {
            if ((t - last_change) as f64) < min_segment_samples {
                return false;
            }
            last_change = t;
            reversals += 1;
            if reversals > max_reversals {
                return false;
            }
        }
        direction = new;
    }
    true
}

fn opn_pitch_units(block: i32, fnum: i32) -> f64 {
    let fnum = if fnum <= 0 { 1 } else { fnum };
    768.0 * ((fnum as f64).log2() + block as f64)
}

/// True when the trajectory has at most about one write per pattern row.
///
/// A vibrato written once per driver tick at speed 1 can ride the row's E5/E8
/// effects exactly.  A macro would have to thin it to fit 255 entries, which
/// turns a 60 Hz wobble into a slow staircase.  Source and row rates can be
/// close enough that one write per row on average still collides now and
/// then; losing less than a row of timing is fine, while an aliased macro is
/// not.  Require roughly two thirds of the writes to have their own row.
pub fn _row_steppable(
    pts: &[(i64, i32, i32)],
    start: i64,
    tick_samples: f64,
    row_ticks: i32,
) -> bool {
    if row_ticks <= 0 || tick_samples <= 0.0 {
        return false;
    }
    let row_len = tick_samples * row_ticks as f64;
    let mut rows: HashSet<i64> = HashSet::new();
    for &(t, _b, _f) in pts {
        rows.insert((((t - start) as f64) / row_len).floor() as i64);
    }
    rows.len() * 3 >= pts.len() * 2
}

#[cfg(test)]
mod tests {
    use super::*;

    fn saa_hit() -> SaaHit {
        SaaHit::default()
    }

    #[test]
    fn saa_at_helpers_follow_last_write() {
        let mut h = saa_hit();
        h.left = 3;
        h.right = 4;
        h.tone = true;
        h.noise = false;
        h.noise_freq = 2;
        h.env = 1;
        h.amp_pts = vec![(100, 10, 12), (200, 7, 9)];
        h.mode_pts = vec![(100, false, true), (200, true, false)];
        h.noise_pts = vec![(150, 3)];
        h.env_pts = vec![(150, 5)];
        assert_eq!(_saa_amp_at(&h, 99), (3, 4));
        assert_eq!(_saa_amp_at(&h, 100), (10, 12));
        assert_eq!(_saa_amp_at(&h, 199), (10, 12));
        assert_eq!(_saa_mode_at(&h, 100), (false, true));
        assert_eq!(_saa_mode_at(&h, 250), (true, false));
        assert_eq!(_saa_noise_at(&h, 150), 3);
        assert_eq!(_saa_env_at(&h, 0), 1);
        assert_eq!(_saa_env_at(&h, 150), 5);
    }

    #[test]
    fn saa_effect_and_macro_nibbles() {
        assert_eq!(_saa_mode_effect(true, false), 0x01);
        assert_eq!(_saa_mode_effect(false, true), 0x10);
        assert_eq!(_saa_mode_effect(true, true), 0x11);
        assert_eq!(_saa_mode_macro(true, false), 0x01);
        assert_eq!(_saa_mode_macro(false, true), 0x02);
        assert_eq!(_saa_mode_macro(true, true), 0x03);
    }

    #[test]
    fn steppable_pitch_rejects_fast_reversals() {
        let monotone = [(0, 4, 100), (500, 4, 200), (1000, 4, 300)];
        assert!(_steppable_pitch(&monotone, 8, 2.0, 400.0));
        // Reverses direction every 100 samples: a vibrato, not a row step.
        let vibrato = [
            (0, 4, 100),
            (100, 4, 200),
            (200, 4, 100),
            (300, 4, 200),
            (400, 4, 100),
        ];
        assert!(!_steppable_pitch(&vibrato, 8, 2.0, 400.0));
        // Few samples: no trajectory to judge.
        assert!(_steppable_pitch(&monotone[..2], 8, 2.0, 400.0));
    }

    #[test]
    fn row_steppable_needs_two_thirds_of_writes_on_own_rows() {
        let sparse = [(0, 4, 100), (100, 4, 120), (200, 4, 140)];
        assert!(_row_steppable(&sparse, 0, 100.0, 1));
        let dense = [
            (0, 4, 100),
            (10, 4, 120),
            (20, 4, 140),
            (30, 4, 160),
            (40, 4, 180),
        ];
        assert!(!_row_steppable(&dense, 0, 100.0, 1));
        assert!(!_row_steppable(&sparse, 0, 0.0, 1));
        assert!(!_row_steppable(&sparse, 0, 100.0, 0));
    }

    #[test]
    fn x1_semitones_rises_with_frequency() {
        let low = _x1_semitones(8_000_000, 1000);
        let high = _x1_semitones(8_000_000, 2000);
        assert!(high > low);
        // A zero register is clamped to 1.
        assert_eq!(_x1_semitones(8_000_000, 0), _x1_semitones(8_000_000, 1));
    }
}
