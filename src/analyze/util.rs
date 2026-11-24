//! Pure helpers shared by the analyzer: pitch codes, tone period
//! segmentation, the PSG write lattice and the tone-instrument merge passes.
//!
//! Everything here is a literal port of `analyze.py` lines 419-2345. The
//! handful of symbols that already live in `model.rs` are re-exported (not
//! duplicated) under the Python spellings, so the rest of the analyzer can
//! keep calling `super::util::<name>`.

use std::collections::HashSet;
use std::hash::{Hash, Hasher};

use indexmap::IndexMap;

use crate::c140::merge_step_writes;
use crate::model::{NoteEvent, ToneIns};
use crate::parity::{format_f, fmt_g, py_floor_div, py_int, py_round};
use crate::pitch::{furnace_period_note, module_clock, E5_CENTER, NOTE_MAX};
use crate::timing::Grid;
use crate::vgm::{VgmFile, VSU_DEFAULT_CLOCK, VGM_RATE};
use crate::vsu::{VSU_ENVELOPE_CLOCKS, VSU_WAVE_BURST};

// Constants and one-line helpers that were already ported into `model.rs`.
// Re-export both the model spelling and the Python spelling, so callers keep
// working whichever one they use.
pub use crate::model::{
    furnace_note_name, kc_kf_units, kc_to_furnace_note, kf_to_e5, multipcm_pan_fx,
    opm_pitch_offset, pcm_pan_fx, pcm_rate_note, shift_note_e5, transpose_opm_events,
    vol15, KC_SEMI, ONSET_MERGE_SAMPLES, OPM_BURST_LOAD_SAMPLES, OPM_DEFAULT_CLOCK,
    OPN_PATCH_SETTLE, OPN_TL_FOLD,
};
pub use crate::model::{
    pcm_rate_note as _pcm_rate_note, shift_note_e5 as _shift_note_e5,
    KC_SEMI as _KC_SEMI, ONSET_MERGE_SAMPLES as _ONSET_MERGE_SAMPLES,
    OPM_BURST_LOAD_SAMPLES as _OPM_BURST_LOAD_SAMPLES,
};

/// A period step of this many semitones, held for `min_hold` samples, is a
/// new note. SCC and AY drivers retune with the gate open.
pub const _NOTE_STEP_SEMITONES: f64 = 0.75;
/// One 60 Hz frame; SCC merge gap and minimum hold.
pub const _NOTE_HOLD_SAMPLES: i64 = 735;
/// Low/high byte pair, and a second write in the same tick.
pub const _AY_MERGE_GAP: i64 = 16;
/// A note's opening register burst can trail the gate by more than the
/// byte-pair gap.
pub const _AY_OPEN_WINDOW: i64 = 64;
/// Two frames: a one-frame AY step is the pitch macro.
pub const _AY_NOTE_HOLD: i64 = 1470;

/// Slide and vibrato inside one note, as (sample, period).
///
/// `merge_gap` collapses a low/high byte pair. The SCC default also
/// collapses writes up to 600 samples apart.
pub fn _tone_steps(
    freq_pts: &[(i64, i32)],
    gate: i64,
    refit: i64,
    merge_gap: i64,
) -> Vec<(i64, i32)> {
    merge_step_writes(freq_pts, merge_gap)
        .into_iter()
        .filter(|p| p.1 != 0 && p.0 >= gate + refit)
        .collect()
}

/// A period's pitch in semitones from the module's C-(-5).
pub fn _period_semitones(
    clock: f64,
    divider: f64,
    period: i32,
    offset: i32,
    maximum: i32,
) -> f64 {
    let (note, e5) = furnace_period_note(
        module_clock(clock),
        divider,
        period.max(1),
        offset,
        maximum,
    );
    note as f64 + (e5 - E5_CENTER) as f64 / 128.0
}

/// Split a period timeline into notes.
///
/// Returns (start_sample, period, intra) per note. A step of at least
/// 0.75 semitone held for `min_hold` starts a new note. Smaller steps stay
/// in `intra`.
pub fn segment_periods(
    points: &[(i64, i32)],
    end: i64,
    clock: f64,
    divider: f64,
    offset: i32,
    merge_gap: i64,
    min_hold: i64,
    drop_short: bool,
    maximum: i32,
) -> Vec<(i64, i32, Vec<(i64, i32)>)> {
    if points.is_empty() {
        return Vec::new();
    }
    let merged = merge_step_writes(points, merge_gap);
    let (mut cur_t, mut cur_p) = merged[0];
    let mut cur_s = _period_semitones(clock, divider, cur_p, offset, maximum);
    let mut intra: Vec<(i64, i32)> = Vec::new();
    let mut segs: Vec<(i64, i32, Vec<(i64, i32)>)> = Vec::new();
    for i in 1..merged.len() {
        let (t, period) = merged[i];
        let hold_until = if i + 1 < merged.len() {
            merged[i + 1].0
        } else {
            end
        };
        let held = hold_until - t;
        let new_s = _period_semitones(clock, divider, period, offset, maximum);
        let step = (new_s - cur_s).abs();
        if step >= _NOTE_STEP_SEMITONES && held >= min_hold {
            segs.push((cur_t, cur_p, intra));
            cur_t = t;
            cur_p = period;
            cur_s = new_s;
            intra = Vec::new();
        } else if step >= _NOTE_STEP_SEMITONES && drop_short {
            continue;
        } else {
            intra.push((t, period));
            cur_s = new_s;
        }
    }
    segs.push((cur_t, cur_p, intra));
    segs
}

/// The value in force at `start` (the last write at or before it).
pub fn _value_at(pts: &[(i64, i32)], start: i64, default: i32) -> i32 {
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

/// Most common gap between a pitched chip's register writes, in samples.
///
/// A Furnace export writes its macros once per tick; the CMS/POKEY rips do
/// the same at the driver's frame rate.
pub fn _saa_tick_interval(vgm: &VgmFile, chip: &str) -> Option<f64> {
    let times: Vec<i64> = vgm
        .writes
        .iter()
        .filter(|w| w.chip == chip)
        .map(|w| w.sample)
        .collect();
    if times.len() < 16 {
        return None;
    }
    let mut intervals: Vec<i64> = Vec::new();
    for pair in times.windows(2) {
        let d = pair[1] - pair[0];
        if 64 <= d && d <= 20_000 {
            intervals.push(d);
        }
    }
    if intervals.len() < 8 {
        return None;
    }
    // Counter.most_common keeps the first-seen order on ties.
    let mut counts: IndexMap<i64, i64> = IndexMap::new();
    for &value in &intervals {
        *counts.entry(value).or_insert(0) += 1;
    }
    let mut mode = 0i64;
    let mut hits = 0i64;
    for (&value, &count) in &counts {
        if count > hits {
            mode = value;
            hits = count;
        }
    }
    if hits < 3 {
        return None;
    }
    // Fold near neighbours into the same bucket.
    let width = 1.max(py_floor_div(mode, 64));
    let mut total = 0i64;
    for &value in counts.keys() {
        if (value - mode).abs() <= width {
            total += counts[&value];
        }
    }
    if total < 3 {
        return None;
    }
    let mut weighted = 0i64;
    for (&value, &count) in &counts {
        if (value - mode).abs() <= width {
            weighted += value * count;
        }
    }
    Some(weighted as f64 / total as f64)
}

/// Raise hz and speed until tone pitch macros see every write.
///
/// The row length stays the same and the tick gets finer.
pub fn _tone_pitch_tick_scale(vgm: &VgmFile, hz: f64, speed: i32) -> i32 {
    let mut groups: IndexMap<(&'static str, u8, i32), Vec<i64>> = IndexMap::new();
    let mut vsu_env = 0.0f64;
    for w in &vgm.writes {
        if w.chip == "vsu" {
            let reg = w.reg;
            if reg < 0xA0 {
                // A wave rewrite is a burst; keep one time per settled image.
                let key = (w.chip, w.chip_id, (reg >> 5) as i32);
                let times = groups.entry(key).or_default();
                if times.last().map_or(true, |last| w.sample - *last > VSU_WAVE_BURST) {
                    times.push(w.sample);
                }
            } else if 0x100 <= reg && reg < 0x160 {
                let ch = ((reg - 0x100) >> 4) as i32;
                let r = (reg - 0x100) & 0x0F;
                if r == 2 {
                    groups
                        .entry(("vsu-freq", w.chip_id, ch))
                        .or_default()
                        .push(w.sample);
                } else if r == 4 || r == 5 {
                    let clock = if vgm.vsu_clock != 0 {
                        vgm.vsu_clock
                    } else {
                        VSU_DEFAULT_CLOCK
                    };
                    vsu_env = VSU_ENVELOPE_CLOCKS as f64 * VGM_RATE as f64
                        / clock as f64;
                }
            }
            continue;
        }
        let key = if (w.chip == "ym2203_ssg" || w.chip == "ay8910")
            && w.reg % 2 == 0
            && w.reg < 6
        {
            (w.chip, w.chip_id, (w.reg / 2) as i32)
        } else if w.chip == "saa1099" && 8 <= w.reg && w.reg <= 13 {
            (w.chip, w.chip_id, (w.reg - 8) as i32)
        } else if w.chip == "saa1099" && 0x10 <= w.reg && w.reg <= 0x12 {
            (w.chip, w.chip_id, w.reg as i32)
        } else if w.chip == "k051649" && w.reg % 2 == 0 && w.reg < 10 {
            (w.chip, w.chip_id, (w.reg / 2) as i32)
        } else if w.chip == "k005289" && (w.reg == 4 || w.reg == 5) {
            // TG applies the latched pitch, so the TG writes are the voice's
            // note-change lattice.
            (w.chip, w.chip_id, w.reg as i32 - 4)
        } else if hz < 100.0
            && w.chip == "ym2203"
            && (w.reg == 0xA0 || w.reg == 0xA1 || w.reg == 0xA2)
        {
            // Only lift the 60 Hz fallback; a measured FM lattice is fine.
            (w.chip, w.chip_id, w.reg as i32 - 0xA0)
        } else if hz < 100.0
            && w.chip == "ym2612"
            && ((w.reg & 0xFF & 0xFC) == 0xA0 || (w.reg & 0xFF & 0xFC) == 0xA4)
        {
            // The same for a Genesis driver that rewrites the f-num every
            // frame.
            (
                w.chip,
                w.chip_id,
                ((w.reg & 0xFF & 0xFB) + if w.reg & 0x100 != 0 { 0x100 } else { 0 })
                    as i32,
            )
        } else {
            continue;
        };
        groups.entry(key).or_default().push(w.sample);
    }
    let mut intervals: Vec<i64> = Vec::new();
    for times in groups.values() {
        for pair in times.windows(2) {
            let d = pair[1] - pair[0];
            if d >= 8 {
                intervals.push(d);
            }
        }
    }
    if vsu_env != 0.0 {
        // The hardware envelope can step faster than the note-write lattice.
        let step = 1.max(py_int(py_round(vsu_env)));
        intervals.extend(std::iter::repeat(step).take(8));
    }
    if intervals.len() < 8 {
        return 1;
    }
    // Use the fastest active channel, not the median across every voice.
    let mut group_medians: Vec<i64> = Vec::new();
    for times in groups.values() {
        let mut gaps: Vec<i64> = times
            .windows(2)
            .map(|pair| pair[1] - pair[0])
            .filter(|&d| d >= 8)
            .collect();
        gaps.sort();
        if gaps.len() >= 8 {
            group_medians.push(gaps[gaps.len() / 2]);
        }
    }
    let mut median = if !group_medians.is_empty() {
        *group_medians.iter().min().unwrap()
    } else {
        intervals.sort();
        intervals[intervals.len() / 2]
    };
    if vsu_env != 0.0 {
        median = median.min(1.max(py_int(py_round(vsu_env))));
    }
    if median <= 0 {
        return 1;
    }
    let k = (VGM_RATE as f64 / hz / median as f64).ceil() as i64;
    (1.max(k.min(32).min((255 / speed.max(1)) as i64))) as i32
}

/// Keep a whole number of rows per SN76489 driver write tick.
pub fn _snap_psg_write_lattice(
    vgm: &VgmFile,
    hz: f64,
    speed: i32,
    subdiv: f64,
) -> (f64, i32, f64, String) {
    let tick = match _saa_tick_interval(vgm, "sn76489") {
        Some(t) if t >= 64.0 => t,
        _ => return (hz, speed, subdiv, String::new()),
    };
    let row = VGM_RATE as f64 * speed as f64 / hz;
    if row <= 0.0 {
        return (hz, speed, subdiv, String::new());
    }
    // Keep a whole number of rows per driver write.
    let per_write = 1.max(py_int(py_round(tick / row)));
    let target = tick / per_write as f64;
    let ratio = row / target;
    if !(0.5 <= ratio && ratio <= 2.0) {
        return (hz, speed, subdiv, String::new());
    }
    if vgm.total_samples as f64 / target > 60_000.0 {
        // An audio-rate write loop would need more rows than the order table
        // holds. Leave the existing grid alone.
        return (hz, speed, subdiv, String::new());
    }
    let new_hz = VGM_RATE as f64 * speed as f64 / target;
    let new_subdiv = subdiv * row / target;
    let note = format!(
        "row snapped to the SN76489 write tick ({} samples, {} per write)",
        fmt_g(target, 6),
        fmt_g(per_write as f64, 6)
    );
    (new_hz, speed, new_subdiv, note)
}

/// One row per measured 16th, one tick per 60 Hz frame.
///
/// Furnace's BPM is `60*hz/(highlight*speed)`. Highlight 4 makes this 15
/// times the row rate.
pub fn _musical_row(grid: &Grid) -> (f64, i32, i32, String) {
    let frame = VGM_RATE as f64 / 60.0;
    // `pulse` is the note value. `unit` may have been lengthened so an FM
    // rip stays inside the order budget, and that length is not a 16th.
    let row = if grid.pulse != 0.0 {
        grid.pulse
    } else {
        grid.unit
    };
    let speed = py_int(py_round(row / frame)).clamp(1, 255) as i32;
    let hz = VGM_RATE as f64 * speed as f64 / row;
    let note = format!(
        "{}, {}% within 6%; row = measured 16th, {} ticks/row",
        grid.source,
        format_f(grid.confidence * 100.0, 0),
        speed
    );
    (hz, speed, 1, note)
}

/// The row `Song.place` assigns at t0 = 0.
pub fn _placed_row(sample: i64, spr: f64, speed: i32, hz: f64) -> i32 {
    if speed <= 1 {
        return 0.max((sample as f64 / spr + 0.5).floor() as i64) as i32;
    }
    let mut row = (sample as f64 / spr).floor() as i64;
    let tick = VGM_RATE as f64 / hz;
    let delay = py_int(py_round((sample as f64 - row as f64 * spr) / tick));
    if delay >= speed as i64 {
        row += 1;
    }
    0.max(row) as i32
}

/// Same-channel note-ons that `Song.place` would drop onto one cell.
pub fn _tone_collisions(
    groups: &IndexMap<i32, Vec<i64>>,
    spr: f64,
    speed: i32,
    hz: f64,
) -> i32 {
    let mut n = 0;
    for times in groups.values() {
        let mut seen: HashSet<i32> = HashSet::new();
        for &sample in times {
            let row = _placed_row(sample, spr, speed, hz);
            if seen.contains(&row) {
                n += 1;
            }
            seen.insert(row);
        }
    }
    n
}

/// Drop ticks per row until each note has its own cell.
///
/// Returns the new (hz, speed, subdiv, note) with the largest speed that
/// reduces same-channel collisions.
pub fn _fit_tone_subdivision(
    hz: f64,
    speed: i32,
    events: &[NoteEvent],
    note: &str,
) -> (f64, i32, f64, String) {
    let tick = VGM_RATE as f64 / hz;
    let mut groups: IndexMap<i32, Vec<i64>> = IndexMap::new();
    for event in events {
        if event.on && event.tone_chip != 0 {
            groups.entry(event.ch).or_default().push(event.sample);
        }
    }
    if groups.is_empty() || speed <= 1 {
        return (hz, speed, 1.0, note.to_string());
    }
    let mut best_speed = speed;
    let mut best_hits = _tone_collisions(&groups, tick * speed as f64, speed, hz);
    for spd in (1..speed).rev() {
        if best_hits == 0 {
            break;
        }
        let hits = _tone_collisions(&groups, tick * spd as f64, spd, hz);
        if hits < best_hits {
            best_speed = spd;
            best_hits = hits;
        }
    }
    if best_speed == speed {
        return (hz, speed, 1.0, note.to_string());
    }
    let ratio = speed as f64 / best_speed as f64;
    let head = note.split("; row =").next().unwrap_or(note);
    let fitted = format!(
        "{}; row = {} ticks of the measured 16th's tick ({} per 16th); \
         highlight {} keeps the quarter-note tempo",
        head,
        best_speed,
        speed,
        py_int(py_round(4.0 * ratio))
    );
    (hz, best_speed, ratio, fitted)
}

/// Fold a one-step tuning offset into the note.
///
/// A macro that only settles on a constant detune is the note's pitch.
/// Vibrato stays a macro. Returns the remaining pitch macro.
pub fn _absorb_detune(event: &mut NoteEvent, pitch: &[i32], speed: i32) -> Vec<i32> {
    if pitch.is_empty() || speed != 1 {
        return pitch.to_vec();
    }
    let tail;
    if pitch[0] == 0 {
        let mut i = 0usize;
        while i < pitch.len() && pitch[i] == 0 {
            i += 1;
        }
        if i == pitch.len() {
            return Vec::new();
        }
        // Only a change within the first tick is the note settling.
        if i > 1 {
            return pitch.to_vec();
        }
        // A zero prefix and then one held offset is the note settling.
        if pitch[i..].iter().any(|&v| v != pitch[i]) {
            return pitch.to_vec();
        }
        tail = pitch[i];
    } else {
        if pitch.iter().any(|&v| v != pitch[0]) {
            return pitch.to_vec();
        }
        tail = pitch[0];
    }
    if tail == 0 || tail.abs() > 127 {
        return pitch.to_vec();
    }
    let mut fine = (event.e5 - E5_CENTER) + tail;
    while fine > 127 && event.note < NOTE_MAX {
        event.note += 1;
        fine -= 128;
    }
    while fine < -128 && event.note > 0 {
        event.note -= 1;
        fine += 128;
    }
    event.e5 = E5_CENTER + fine.clamp(-128, 127);
    Vec::new()
}

/// A per-tick macro, or nothing when the register never moves.
pub fn _macro_if_moving(
    pts: &[(i64, i32)],
    start: i64,
    ticks: i64,
    tick_samples: f64,
    merge_gap: i32,
) -> (Vec<i32>, i32, i32) {
    let mut merged: Vec<(i64, i32)> = Vec::new();
    if merge_gap != 0 {
        for &(t, v) in pts {
            if let Some(last) = merged.last_mut() {
                if t - last.0 <= merge_gap as i64 {
                    *last = (last.0, v);
                    continue;
                }
            }
            merged.push((t, v));
        }
    } else {
        merged.extend_from_slice(pts);
    }
    if merged.len() < 2 || merged.iter().all(|p| p.1 == merged[0].1) {
        return (Vec::new(), 1, 255);
    }
    let values = super::tone::_tick_values(&merged, start, ticks, tick_samples);
    super::tone::_macro_series(&values, false, 1, 0, 16)
}

/// Map VSU channel-4 SweepControl/EV1 to a Furnace VBOY effect.
///
/// EV1 bit 4 enables modulation, bit 5 resets its phase, bit 6 enables the
/// sweep/mod block.
pub fn _vsu_sw_effect(sweep: i32, ev1: i32) -> (i32, i32) {
    let mod_ = ev1 & 0x10 != 0;
    let reset = ev1 & 0x20 != 0;
    let sweep_on = ev1 & 0x40 != 0;
    let period = (sweep >> 4) & 0x07;
    if period == 0 {
        return (0x13, 0);
    }
    if mod_ && sweep_on {
        return (
            0x14,
            ((if reset { 3 } else { 1 }) << 4) | ((sweep >> 4) & 0x0F),
        );
    }
    if sweep_on {
        return (0x13, sweep & 0xFF);
    }
    (0x13, 0)
}

/// The same timbre with no per-note motion, so notes can share it.
pub fn _plain_tone(spec: &ToneIns, tone_chip: i32) -> ToneIns {
    if tone_chip == 1 || tone_chip == 5 || tone_chip == 16 {
        // SCC, PCE and the Bubble System WSG are wavetable chips: the wave is
        // the timbre.
        return ToneIns {
            wave: spec.wave,
            ..ToneIns::default()
        };
    }
    if tone_chip == 7 {
        // Keep the starting waveform and modulation table.
        return ToneIns {
            wave: spec.wave,
            vsu_mod: spec.vsu_mod,
            ..ToneIns::default()
        };
    }
    if tone_chip == 6 {
        // Game Boy: a wavetable voice keeps its waveform; a pulse/noise voice
        // keeps the hardware envelope.
        if spec.gb_wave {
            return ToneIns {
                wave: spec.wave,
                gb_wave: true,
                ..ToneIns::default()
            };
        }
        return ToneIns {
            gb_env_vol: spec.gb_env_vol,
            gb_env_dir: spec.gb_env_dir,
            gb_env_len: spec.gb_env_len,
            gb_sound_len: spec.gb_sound_len,
            gb_sound_len_inf: spec.gb_sound_len_inf,
            ..ToneIns::default()
        };
    }
    if tone_chip == 10 {
        // Lynx: keep the starting duty/int and LFSR seed.
        return ToneIns {
            duty: spec.duty.clone(),
            duty_speed: spec.duty_speed,
            duty_loop: spec.duty_loop,
            lfsr: spec.lfsr.clone(),
            lfsr_speed: spec.lfsr_speed,
            lfsr_loop: spec.lfsr_loop,
            ..ToneIns::default()
        };
    }
    if tone_chip == 12 {
        // FDS: keep the waveform and the starting modulation table.
        return ToneIns {
            wave: spec.wave,
            fds_mod: spec.fds_mod,
            fds_mod_depth: spec.fds_mod_depth,
            fds_mod_speed: spec.fds_mod_speed,
            fds_mod_pos: spec.fds_mod_pos,
            ..ToneIns::default()
        };
    }
    if tone_chip == 13 {
        // WonderSwan: the waveform and pan pair are the timbre.
        return ToneIns {
            wave: spec.wave,
            panl: spec.panl.clone(),
            panr: spec.panr.clone(),
            panl_speed: spec.panl_speed,
            panl_loop: spec.panl_loop,
            panr_speed: spec.panr_speed,
            panr_loop: spec.panr_loop,
            ..ToneIns::default()
        };
    }
    if tone_chip == 14 || tone_chip == 15 {
        // QSound: the sample body is the instrument.
        return ToneIns {
            sample: spec.sample,
            sample_kind: spec.sample_kind.clone(),
            ..ToneIns::default()
        };
    }
    if tone_chip == 11 {
        // X1-010: the wave and envelope shape are the timbre.
        return ToneIns {
            wave: spec.wave,
            env: spec.env.clone(),
            env_speed: spec.env_speed,
            env_loop: spec.env_loop,
            env_shape: spec.env_shape.clone(),
            env_shape_speed: spec.env_shape_speed,
            env_shape_loop: spec.env_shape_loop,
            env_period: spec.env_period.clone(),
            env_period_speed: spec.env_period_speed,
            env_period_loop: spec.env_period_loop,
            ..ToneIns::default()
        };
    }
    ToneIns::default()
}

impl Hash for ToneIns {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.wave.hash(state);
        self.pitch.hash(state);
        self.pitch_speed.hash(state);
        self.pitch_loop.hash(state);
        self.pitch_delay.hash(state);
        self.waves.hash(state);
        self.wave_speed.hash(state);
        self.wave_loop.hash(state);
        self.vol.hash(state);
        self.vol_speed.hash(state);
        self.vol_loop.hash(state);
        self.vol_delay.hash(state);
        self.noise.hash(state);
        self.noise_speed.hash(state);
        self.noise_loop.hash(state);
        self.mode.hash(state);
        self.mode_speed.hash(state);
        self.mode_loop.hash(state);
        self.env_shape.hash(state);
        self.env_shape_speed.hash(state);
        self.env_shape_loop.hash(state);
        self.env_hw.hash(state);
        self.env_period.hash(state);
        self.env_period_speed.hash(state);
        self.env_period_loop.hash(state);
        self.duty.hash(state);
        self.duty_speed.hash(state);
        self.duty_loop.hash(state);
        self.lfsr.hash(state);
        self.lfsr_speed.hash(state);
        self.lfsr_loop.hash(state);
        self.panl.hash(state);
        self.panl_speed.hash(state);
        self.panl_loop.hash(state);
        self.panl_delay.hash(state);
        self.panr.hash(state);
        self.panr_speed.hash(state);
        self.panr_loop.hash(state);
        self.panr_delay.hash(state);
        self.env.hash(state);
        self.env_speed.hash(state);
        self.env_loop.hash(state);
        self.env_delay.hash(state);
        self.gb_soft_env.hash(state);
        self.gb_env_vol.hash(state);
        self.gb_env_dir.hash(state);
        self.gb_env_len.hash(state);
        self.gb_sound_len.hash(state);
        self.gb_sound_len_inf.hash(state);
        self.gb_wave.hash(state);
        self.vsu_mod.hash(state);
        self.sample.hash(state);
        self.sample_kind.hash(state);
        self.fds_mod.hash(state);
        self.fds_mod_depth.hash(state);
        self.fds_mod_speed.hash(state);
        self.fds_mod_pos.hash(state);
    }
}

impl Eq for ToneIns {}

/// The macro fields, their speed/loop fields, and their delay field, in the
/// order of `_TONE_MACRO_TIMING`.
fn macro_states(spec: &ToneIns) -> [(&'static str, &[i32], i32, i32, i32); 12] {
    [
        ("pitch", &spec.pitch, spec.pitch_speed, spec.pitch_loop, spec.pitch_delay),
        ("waves", &spec.waves, spec.wave_speed, spec.wave_loop, 0),
        ("vol", &spec.vol, spec.vol_speed, spec.vol_loop, spec.vol_delay),
        ("noise", &spec.noise, spec.noise_speed, spec.noise_loop, 0),
        ("mode", &spec.mode, spec.mode_speed, spec.mode_loop, 0),
        (
            "env_shape",
            &spec.env_shape,
            spec.env_shape_speed,
            spec.env_shape_loop,
            0,
        ),
        (
            "env_period",
            &spec.env_period,
            spec.env_period_speed,
            spec.env_period_loop,
            0,
        ),
        ("panl", &spec.panl, spec.panl_speed, spec.panl_loop, spec.panl_delay),
        ("panr", &spec.panr, spec.panr_speed, spec.panr_loop, spec.panr_delay),
        ("env", &spec.env, spec.env_speed, spec.env_loop, spec.env_delay),
        ("duty", &spec.duty, spec.duty_speed, spec.duty_loop, 0),
        ("lfsr", &spec.lfsr, spec.lfsr_speed, spec.lfsr_loop, 0),
    ]
}

/// True when two per-tick macros stay within `tol` of each other.
///
/// A shorter macro holds its last value after it runs out, so a longer one
/// may share it when its extra tail stays near that final value.
pub fn _tone_macro_close(a: &[i32], b: &[i32], tol: i32) -> bool {
    if a.len() == b.len() {
        return a.iter().zip(b.iter()).all(|(x, y)| (x - y).abs() <= tol);
    }
    if a.is_empty() || b.is_empty() {
        return false;
    }
    let n = a.len().min(b.len());
    if (0..n).any(|i| (a[i] - b[i]).abs() > tol) {
        return false;
    }
    let (longer, shorter) = if a.len() > b.len() { (a, b) } else { (b, a) };
    let last = shorter[shorter.len() - 1];
    longer[n..].iter().all(|&v| (v - last).abs() <= tol)
}

/// Whether two tone instruments can share one macro set at `tol`.
///
/// Everything that is not the two continuous macros must be identical.
pub fn _tone_close(a: &ToneIns, b: &ToneIns, tol: i32) -> bool {
    if a.wave != b.wave
        || a.waves != b.waves
        || a.wave_speed != b.wave_speed
        || a.wave_loop != b.wave_loop
        || a.pitch_speed != b.pitch_speed
        || a.pitch_loop != b.pitch_loop
        || a.noise != b.noise
        || a.noise_speed != b.noise_speed
        || a.noise_loop != b.noise_loop
        || a.vol_speed != b.vol_speed
        || a.vol_loop != b.vol_loop
        || a.mode != b.mode
        || a.mode_speed != b.mode_speed
        || a.mode_loop != b.mode_loop
        || a.env_shape != b.env_shape
        || a.env_shape_speed != b.env_shape_speed
        || a.env_shape_loop != b.env_shape_loop
        || a.env_period != b.env_period
        || a.env_period_speed != b.env_period_speed
        || a.env_period_loop != b.env_period_loop
        || a.panl_speed != b.panl_speed
        || a.panl_loop != b.panl_loop
        || a.panr_speed != b.panr_speed
        || a.panr_loop != b.panr_loop
        || a.env != b.env
        || a.env_speed != b.env_speed
        || a.env_loop != b.env_loop
        || a.gb_soft_env != b.gb_soft_env
        || a.gb_env_vol != b.gb_env_vol
        || a.gb_env_dir != b.gb_env_dir
        || a.gb_env_len != b.gb_env_len
        || a.gb_sound_len != b.gb_sound_len
        || a.gb_sound_len_inf != b.gb_sound_len_inf
        || a.gb_wave != b.gb_wave
        || a.vsu_mod != b.vsu_mod
        || a.sample != b.sample
        || a.sample_kind != b.sample_kind
        || a.fds_mod != b.fds_mod
        || a.fds_mod_depth != b.fds_mod_depth
        || a.fds_mod_speed != b.fds_mod_speed
        || a.fds_mod_pos != b.fds_mod_pos
    {
        return false;
    }
    _tone_macro_close(&a.pitch, &b.pitch, tol)
        && _tone_macro_close(&a.vol, &b.vol, tol)
        && _tone_macro_close(&a.panl, &b.panl, tol)
        && _tone_macro_close(&a.panr, &b.panr, tol)
}

/// Index of `spec` in `bucket`.
///
/// Past the instrument cap, share a plain instrument of the same wave.
/// When the budget is tight, `merge_tol` lets a new note reuse an existing
/// instrument whose continuous macros are within that many register steps.
pub fn _take_tone(
    spec: ToneIns,
    mapping: &mut IndexMap<ToneIns, usize>,
    bucket: &mut Vec<ToneIns>,
    others: usize,
    cap: usize,
    tone_chip: i32,
    merge_tol: i32,
) -> (usize, ToneIns) {
    if let Some(&found) = mapping.get(&spec) {
        return (found, spec);
    }
    if merge_tol > 0 {
        for (i, existing) in bucket.iter().enumerate() {
            if _tone_close(&spec, existing, merge_tol) {
                mapping.insert(spec.clone(), i);
                return (i, existing.clone());
            }
        }
    }
    if bucket.len() + others < cap {
        let idx = bucket.len();
        mapping.insert(spec.clone(), idx);
        bucket.push(spec.clone());
        return (idx, spec);
    }
    let plain = _plain_tone(&spec, tone_chip);
    if let Some(&found) = mapping.get(&plain) {
        return (found, plain);
    }
    for (i, existing) in bucket.iter().enumerate() {
        if *existing == plain {
            return (i, existing.clone());
        }
    }
    if bucket.len() + others < cap {
        let idx = bucket.len();
        mapping.insert(plain.clone(), idx);
        bucket.push(plain.clone());
        return (idx, plain);
    }
    if bucket.is_empty() {
        // The budget is already spent on the earlier families. This family
        // still needs one instrument for its notes to point at.
        mapping.insert(plain.clone(), 0);
        bucket.push(plain.clone());
        return (0, plain);
    }
    (0, bucket[0].clone())
}

/// Everything about a tone instrument except its macro values.
pub fn _tone_sig(spec: &ToneIns) -> String {
    format!(
        "{:?}|{:?}|{:?}|{:?}|{:?}|{:?}|{:?}|{:?}|{:?}|{:?}|{:?}|{:?}|{:?}|{:?}|{:?}|{:?}|{:?}|{:?}|{:?}",
        spec.wave,
        spec.env_hw,
        spec.panl_delay,
        spec.panr_delay,
        spec.env_delay,
        spec.gb_soft_env,
        spec.gb_env_vol,
        spec.gb_env_dir,
        spec.gb_env_len,
        spec.gb_sound_len,
        spec.gb_sound_len_inf,
        spec.gb_wave,
        spec.vsu_mod,
        spec.sample,
        spec.sample_kind,
        spec.fds_mod,
        spec.fds_mod_depth,
        spec.fds_mod_speed,
        spec.fds_mod_pos
    )
}

/// True when every macro of `a` extends the matching macro of `b`.
pub fn _tone_macro_extends(a: &ToneIns, b: &ToneIns) -> bool {
    let am = macro_states(a);
    let bm = macro_states(b);
    for k in 0..am.len() {
        let (_, av, a_speed, a_loop, _) = am[k];
        let (_, bv, b_speed, b_loop, _) = bm[k];
        if av.is_empty() != bv.is_empty() || av.len() < bv.len() {
            return false;
        }
        if &av[..bv.len()] != bv {
            return false;
        }
        if !av.is_empty() || !bv.is_empty() {
            if a_speed != b_speed || a_loop != b_loop {
                return false;
            }
        }
    }
    true
}

/// Whether `a` and `b` play the same tone macros for `ticks` ticks.
///
/// Each side uses its own speed, loop and delay.
pub fn _tone_macro_same_for(a: &ToneIns, b: &ToneIns, ticks: i64) -> bool {
    let am = macro_states(a);
    let bm = macro_states(b);
    for k in 0..am.len() {
        let (_, av, a_speed, a_loop, a_delay) = am[k];
        let (_, bv, b_speed, b_loop, b_delay) = bm[k];
        if super::bind::_macro_effective(av, a_speed, a_loop, ticks, a_delay)
            != super::bind::_macro_effective(bv, b_speed, b_loop, ticks, b_delay)
        {
            return false;
        }
    }
    true
}

pub fn _tone_macro_size(spec: &ToneIns) -> i64 {
    macro_states(spec)
        .iter()
        .map(|&(_, values, _, _, _)| values.len() as i64)
        .sum()
}

/// Collapse a leading run into one value plus a delay in ticks.
pub fn _delay_leading_values(values: &[i32], speed: i32) -> (Vec<i32>, i32) {
    if values.is_empty() {
        return (Vec::new(), 0);
    }
    let mut run = 1usize;
    while run < values.len() && values[run] == values[0] {
        run += 1;
    }
    if run <= 1 {
        return (values.to_vec(), 0);
    }
    let hold = run as i64 * speed.max(1) as i64;
    if hold <= 256 {
        let mut out = Vec::with_capacity(values.len() - run + 1);
        out.push(values[0]);
        out.extend_from_slice(&values[run..]);
        return (out, (hold - 1) as i32);
    }
    let extra = hold - 256;
    let keep = 1 + py_floor_div(extra + speed as i64 - 1, speed.max(1) as i64);
    let mut out = vec![values[0]; keep as usize];
    out.extend_from_slice(&values[run..]);
    (out, 255)
}

/// Make every leading-hold macro use the delay field before grouping.
pub fn _compress_tone_delays(bucket: &mut Vec<ToneIns>) {
    for spec in bucket.iter_mut() {
        if spec.pitch.is_empty()
            && spec.vol.is_empty()
            && spec.panl.is_empty()
            && spec.panr.is_empty()
            && spec.env.is_empty()
        {
            continue;
        }
        let mut updated = spec.clone();
        if updated.pitch_loop == 255 && updated.pitch_delay == 0 && !updated.pitch.is_empty()
        {
            let (short, delay) = _delay_leading_values(&updated.pitch, updated.pitch_speed);
            if delay != 0 {
                updated.pitch = short;
                updated.pitch_delay = delay;
            }
        }
        if updated.vol_loop == 255 && updated.vol_delay == 0 && !updated.vol.is_empty() {
            let (short, delay) = _delay_leading_values(&updated.vol, updated.vol_speed);
            if delay != 0 {
                updated.vol = short;
                updated.vol_delay = delay;
            }
        }
        if updated.panl_loop == 255 && updated.panl_delay == 0 && !updated.panl.is_empty() {
            let (short, delay) = _delay_leading_values(&updated.panl, updated.panl_speed);
            if delay != 0 {
                updated.panl = short;
                updated.panl_delay = delay;
            }
        }
        if updated.panr_loop == 255 && updated.panr_delay == 0 && !updated.panr.is_empty() {
            let (short, delay) = _delay_leading_values(&updated.panr, updated.panr_speed);
            if delay != 0 {
                updated.panr = short;
                updated.panr_delay = delay;
            }
        }
        if updated.env_loop == 255 && updated.env_delay == 0 && !updated.env.is_empty() {
            let (short, delay) = _delay_leading_values(&updated.env, updated.env_speed);
            if delay != 0 {
                updated.env = short;
                updated.env_delay = delay;
            }
        }
        *spec = updated;
    }
}

/// Everything about a tone instrument except its volume macro.
pub fn _tone_vol_sig(spec: &ToneIns) -> String {
    let mut out = String::new();
    macro_rules! add {
        ($v:expr) => {
            out.push_str(&format!("{:?}\x1f", $v));
        };
    }
    add!(spec.wave);
    add!(spec.pitch);
    add!(spec.pitch_speed);
    add!(spec.pitch_loop);
    add!(spec.pitch_delay);
    add!(spec.waves);
    add!(spec.wave_speed);
    add!(spec.wave_loop);
    add!(spec.noise);
    add!(spec.noise_speed);
    add!(spec.noise_loop);
    add!(spec.mode);
    add!(spec.mode_speed);
    add!(spec.mode_loop);
    add!(spec.env_shape);
    add!(spec.env_shape_speed);
    add!(spec.env_shape_loop);
    add!(spec.env_hw);
    add!(spec.env_period);
    add!(spec.env_period_speed);
    add!(spec.env_period_loop);
    add!(spec.duty);
    add!(spec.duty_speed);
    add!(spec.duty_loop);
    add!(spec.lfsr);
    add!(spec.lfsr_speed);
    add!(spec.lfsr_loop);
    add!(spec.panl);
    add!(spec.panl_speed);
    add!(spec.panl_loop);
    add!(spec.panl_delay);
    add!(spec.panr);
    add!(spec.panr_speed);
    add!(spec.panr_loop);
    add!(spec.panr_delay);
    add!(spec.env);
    add!(spec.env_speed);
    add!(spec.env_loop);
    add!(spec.env_delay);
    add!(spec.gb_soft_env);
    add!(spec.gb_env_vol);
    add!(spec.gb_env_dir);
    add!(spec.gb_env_len);
    add!(spec.gb_sound_len);
    add!(spec.gb_sound_len_inf);
    add!(spec.gb_wave);
    add!(spec.vsu_mod);
    add!(spec.sample);
    add!(spec.sample_kind);
    add!(spec.fds_mod);
    add!(spec.fds_mod_depth);
    add!(spec.fds_mod_speed);
    add!(spec.fds_mod_pos);
    out
}

/// A macro level is 0-15, so 15 is the whole range.
pub const _AY_VOL_SPREAD: i32 = 15;

/// Constant 1..15-level difference from `loud` to `quiet`, else None.
pub fn _vol_offset(loud: &ToneIns, quiet: &ToneIns) -> Option<i32> {
    let a = &loud.vol;
    let b = &quiet.vol;
    if a.is_empty()
        || b.is_empty()
        || loud.vol_speed != quiet.vol_speed
        || loud.vol_loop != quiet.vol_loop
    {
        return None;
    }
    if loud.vol_loop != 255 {
        // A loop's phase is not worth reconciling here.
        return None;
    }
    let delay_a = loud.vol_delay;
    let delay_b = quiet.vol_delay;
    let speed = loud.vol_speed.max(1);
    let horizon = a.len().max(b.len()) as i64 * speed as i64
        + delay_a.max(delay_b) as i64
        + 1;
    let ea = super::bind::_macro_effective(a, speed, 255, horizon, delay_a);
    let eb = super::bind::_macro_effective(b, speed, 255, horizon, delay_b);
    let delta = ea[0] - eb[0];
    if delta < 1 || delta > _AY_VOL_SPREAD {
        return None;
    }
    if ea.iter().zip(eb.iter()).any(|(x, y)| x - y != delta) {
        return None;
    }
    Some(delta)
}

/// Share one AY/SSG volume macro when copies differ by a constant offset.
pub fn _merge_ay_volume_offsets(
    bucket: &mut Vec<ToneIns>,
    notes: &[(usize, usize)],
    events: &mut [NoteEvent],
) -> IndexMap<usize, usize> {
    if bucket.len() < 2 {
        return (0..bucket.len()).map(|i| (i, i)).collect();
    }
    let mut groups: IndexMap<String, Vec<usize>> = IndexMap::new();
    for (i, spec) in bucket.iter().enumerate() {
        groups.entry(_tone_vol_sig(spec)).or_default().push(i);
    }
    let mut center_of: Vec<usize> = (0..bucket.len()).collect();
    let mut volume_notes: IndexMap<usize, Vec<usize>> = IndexMap::new();
    for &(event_idx, idx) in notes {
        volume_notes.entry(idx).or_default().push(event_idx);
    }
    for indexes in groups.values() {
        if indexes.len() < 2 {
            continue;
        }
        // Keep the loudest macro: compensating columns only move down.
        let mut pending: Vec<(i64, usize)> = indexes
            .iter()
            .map(|&i| {
                let sum: i64 = bucket[i].vol.iter().map(|&v| v as i64).sum();
                (-sum, i)
            })
            .collect();
        pending.sort();
        while !pending.is_empty() {
            let center = pending.remove(0).1;
            let mut cluster = vec![center];
            let mut rest: Vec<(i64, usize)> = Vec::new();
            for &(weight, i) in &pending {
                if _vol_offset(&bucket[center], &bucket[i]).is_some() {
                    cluster.push(i);
                } else {
                    rest.push((weight, i));
                }
            }
            pending = rest;
            if cluster.len() < 2 {
                continue;
            }
            for &i in &cluster[1..] {
                let delta = _vol_offset(&bucket[center], &bucket[i]).expect("constant offset");
                let event_ids = volume_notes.get(&i).cloned().unwrap_or_default();
                if event_ids.iter().any(|&ei| events[ei].vol < delta) {
                    continue;
                }
                for ei in event_ids {
                    events[ei].vol = (events[ei].vol - delta).clamp(0, 15);
                }
                center_of[i] = center;
            }
        }
    }
    let mut mapping: IndexMap<usize, usize> = IndexMap::new();
    let mut kept: Vec<ToneIns> = Vec::new();
    for i in 0..bucket.len() {
        if center_of[i] == i {
            mapping.insert(i, kept.len());
            kept.push(bucket[i].clone());
        }
    }
    for i in 0..bucket.len() {
        let value = mapping[&center_of[i]];
        mapping.insert(i, value);
    }
    if kept.len() != bucket.len() {
        *bucket = kept;
    }
    mapping
}

/// Share the longest left-to-right-compatible tone macro.
pub fn _merge_tone_prefixes(
    bucket: &mut Vec<ToneIns>,
    notes_by_ins: Option<&IndexMap<usize, Vec<(i64, i64)>>>,
    tick_samples: f64,
    _use_speed: i32,
) -> IndexMap<usize, usize> {
    if bucket.len() < 2 {
        return (0..bucket.len()).map(|i| (i, i)).collect();
    }
    let mut groups: IndexMap<String, Vec<usize>> = IndexMap::new();
    for (i, spec) in bucket.iter().enumerate() {
        groups.entry(_tone_sig(spec)).or_default().push(i);
    }

    let extends = |j: usize, i: usize| -> bool {
        let notes: &[(i64, i64)] = match notes_by_ins.and_then(|m| m.get(&i)) {
            Some(n) => n,
            None => return false,
        };
        if notes.is_empty() {
            return false;
        }
        for &(start, end) in notes {
            if end <= 0 {
                return false;
            }
            let ticks = super::bind::_macro_ticks(start, end, tick_samples);
            if !_tone_macro_same_for(&bucket[j], &bucket[i], ticks) {
                return false;
            }
        }
        true
    };

    let mut target: Vec<usize> = (0..bucket.len()).collect();
    for indexes in groups.values() {
        if indexes.len() > 1 && indexes.iter().any(|&i| _tone_macro_size(&bucket[i]) > 0) {
            let size_of = |i: usize| _tone_macro_size(&bucket[i]);
            let prefix = |j: usize, i: usize| extends(j, i);
            for (k, v) in super::bind::_prefix_targets(indexes, size_of, prefix) {
                target[k] = v;
            }
        }
    }
    let mut mapping: IndexMap<usize, usize> = IndexMap::new();
    let mut kept: Vec<ToneIns> = Vec::new();
    for i in 0..bucket.len() {
        if target[i] == i {
            mapping.insert(i, kept.len());
            kept.push(bucket[i].clone());
        }
    }
    for i in 0..bucket.len() {
        let value = mapping[&target[i]];
        mapping.insert(i, value);
    }
    if kept.len() != bucket.len() {
        *bucket = kept;
    }
    mapping
}

/// Loop a periodic SAA pitch macro independently of its other macros.
pub fn _loop_saa_pitch_macros(bucket: &mut Vec<ToneIns>, lfo_loop: bool) {
    if !lfo_loop {
        return;
    }
    for ins in bucket.iter_mut() {
        if ins.pitch.is_empty() || ins.pitch_loop != 255 {
            continue;
        }
        let (values, speed, loop_) = super::tone::_macro_series(
            &ins.pitch,
            true,
            ins.pitch_speed,
            0,
            16,
        );
        if loop_ != 255 && speed == ins.pitch_speed && values.len() < ins.pitch.len() {
            let mut updated = ins.clone();
            updated.pitch = values;
            updated.pitch_loop = loop_;
            *ins = updated;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kc_codes_match_the_opm_table() {
        // KC 0x4A: octave 4, unused nibble A aliases A# (9 semitones).
        assert_eq!(kc_to_furnace_note(0x4A), 117);
        assert_eq!(furnace_note_name(117), "A-4");
        assert_eq!(furnace_note_name(108), "C-4");
        assert_eq!(furnace_note_name(180), "OFF");
        assert_eq!(kf_to_e5(0x00), 0x80);
        assert_eq!(kf_to_e5(0xFF), 0xBF);
        assert_eq!(kc_kf_units(0x4A, 0x80), 57 * 64 + 32);
    }

    #[test]
    fn note_e5_shifts_stay_inside_the_note_range() {
        assert_eq!(opm_pitch_offset(Some(OPM_DEFAULT_CLOCK)), 0);
        assert_eq!(opm_pitch_offset(None), 0);
        assert_eq!(shift_note_e5(108, 0x80, 32), (108, 0xA0));
        // A downward shift below C-(-5) clamps the note and keeps the fraction.
        assert_eq!(shift_note_e5(108, 0x80, -32), (107, 0xA0));
    }

    #[test]
    fn tone_steps_merge_byte_pairs_and_drop_the_zero_write() {
        let pts = [(0i64, 0x400i32), (10, 0x200), (1000, 0x300), (2000, 0)];
        assert_eq!(
            _tone_steps(&pts, 0, 0, _AY_MERGE_GAP),
            vec![(10, 0x200), (1000, 0x300)]
        );
        assert_eq!(_value_at(&[(0, 5), (100, 7)], 50, 1), 5);
        assert_eq!(_value_at(&[(0, 5), (100, 7)], 100, 1), 7);
        assert_eq!(_value_at(&[(0, 5)], 0, 9), 5);
    }

    #[test]
    fn segment_periods_splits_a_held_octave_step_only() {
        let clock = 3_579_545.0f64;
        let segs = segment_periods(
            &[(0, 0x400), (1000, 0x200)],
            3000,
            clock,
            16.0,
            0,
            _NOTE_HOLD_SAMPLES,
            _NOTE_HOLD_SAMPLES,
            true,
            0xFFF,
        );
        assert_eq!(segs.len(), 2);
        assert_eq!(segs[0].0, 0);
        assert_eq!(segs[0].1, 0x400);
        assert!(segs[0].2.is_empty());
        assert_eq!(segs[1].0, 1000);
        assert_eq!(segs[1].1, 0x200);

        // A tiny retune stays inside the note as an intra-note step.
        let segs = segment_periods(
            &[(0, 0x400), (1000, 0x3F8)],
            1100,
            clock,
            16.0,
            0,
            _NOTE_HOLD_SAMPLES,
            _NOTE_HOLD_SAMPLES,
            true,
            0xFFF,
        );
        assert_eq!(segs.len(), 1);
        assert_eq!(segs[0].2, vec![(1000, 0x3F8)]);
    }

    #[test]
    fn leading_holds_become_delays() {
        assert_eq!(_delay_leading_values(&[5, 5, 5, 3], 1), (vec![5, 3], 2));
        assert_eq!(_delay_leading_values(&[5, 5, 5, 3], 2), (vec![5, 3], 5));
        assert_eq!(_delay_leading_values(&[7], 1), (vec![7], 0));
        // More than 256 held ticks: repeat the value instead of delaying.
        assert_eq!(
            _delay_leading_values(&[0; 300], 1),
            (vec![0; 45], 255)
        );
    }

    #[test]
    fn macro_close_accepts_a_tail_near_the_shorter_last_value() {
        assert!(_tone_macro_close(&[1, 2, 3], &[1, 2], 1));
        assert!(!_tone_macro_close(&[1, 2, 3], &[1, 2], 0));
        assert!(!_tone_macro_close(&[], &[1], 5));
        assert!(_tone_macro_close(&[4, 4], &[4, 4], 0));
    }

    #[test]
    fn plain_tone_keeps_only_the_timbre_of_each_family() {
        let mut spec = ToneIns::default();
        spec.wave = 3;
        spec.pitch = vec![1, 2];
        spec.vol = vec![15, 14];
        let plain = _plain_tone(&spec, 1);
        assert_eq!(plain.wave, 3);
        assert!(plain.pitch.is_empty() && plain.vol.is_empty());

        let mut gb = ToneIns::default();
        gb.wave = 5;
        gb.gb_wave = true;
        let plain = _plain_tone(&gb, 6);
        assert!(plain.gb_wave);
        assert_eq!(plain.wave, 5);

        let mut x1 = ToneIns::default();
        x1.wave = 2;
        x1.env_shape = vec![1, 2];
        x1.env_period = vec![9];
        let plain = _plain_tone(&x1, 11);
        assert_eq!(plain.wave, 2);
        assert_eq!(plain.env_shape, vec![1, 2]);
        assert_eq!(plain.env_period, vec![9]);
        assert!(plain.pitch.is_empty());
    }

    #[test]
    fn take_tone_reuses_the_plain_fallback_past_the_cap() {
        let mut mapping: IndexMap<ToneIns, usize> = IndexMap::new();
        let mut bucket: Vec<ToneIns> = Vec::new();
        let mut spec = ToneIns::default();
        spec.wave = 4;
        spec.pitch = vec![0, 1];
        let (idx, kept) = _take_tone(spec, &mut mapping, &mut bucket, 0, 2, 1, 0);
        assert_eq!(idx, 0);
        assert_eq!(kept.pitch, vec![0, 1]);
        assert_eq!(bucket.len(), 1);

        let mut plain = ToneIns::default();
        plain.wave = 4;
        let (idx, kept) = _take_tone(plain, &mut mapping, &mut bucket, 0, 1, 1, 0);
        assert_eq!(idx, 0);
        assert_eq!(kept, bucket[0]);

        // A different wave and the cap already reached: the writer still
        // needs one instrument, which is the first bucket entry.
        let mut other = ToneIns::default();
        other.wave = 9;
        let (idx, kept) = _take_tone(other, &mut mapping, &mut bucket, 0, 1, 1, 0);
        assert_eq!((idx, kept), (0, bucket[0].clone()));
    }

    #[test]
    fn ay_volume_offsets_share_the_loud_macro() {
        let mut loud = ToneIns::default();
        loud.vol = vec![10, 8];
        let mut quiet = ToneIns::default();
        quiet.vol = vec![7, 5];
        let mut bucket = vec![loud, quiet];
        let mut events = vec![
            NoteEvent::new(0, 0, true, 60, 0, 12, 0x80, 0),
            NoteEvent::new(10, 0, true, 62, 1, 12, 0x80, 0),
        ];
        let notes = vec![(0usize, 0usize), (1, 1)];
        let mapping = _merge_ay_volume_offsets(&mut bucket, &notes, &mut events);
        assert_eq!(bucket.len(), 1);
        assert_eq!(mapping[&0], 0);
        assert_eq!(mapping[&1], 0);
        assert_eq!(events[1].vol, 9);
        assert_eq!(events[0].vol, 12);
    }

    #[test]
    fn merging_volume_offsets_respects_a_low_note_column() {
        let mut loud = ToneIns::default();
        loud.vol = vec![10, 8];
        let mut quiet = ToneIns::default();
        quiet.vol = vec![7, 5];
        let mut bucket = vec![loud, quiet];
        // Too quiet to lower by the full three levels: keep both instruments.
        let mut events = vec![
            NoteEvent::new(0, 0, true, 60, 0, 12, 0x80, 0),
            NoteEvent::new(10, 0, true, 62, 1, 2, 0x80, 0),
        ];
        let notes = vec![(0usize, 0usize), (1, 1)];
        let mapping = _merge_ay_volume_offsets(&mut bucket, &notes, &mut events);
        assert_eq!(bucket.len(), 2);
        assert_eq!(mapping[&1], 1);
        assert_eq!(events[1].vol, 2);
    }

    #[test]
    fn prefix_merge_keeps_the_longer_macro() {
        let mut short = ToneIns::default();
        short.vol = vec![1, 2, 3];
        let mut long = ToneIns::default();
        long.vol = vec![1, 2, 3, 4];
        let mut bucket = vec![short, long];
        let mut notes: IndexMap<usize, Vec<(i64, i64)>> = IndexMap::new();
        notes.insert(0, vec![(0, 300)]);
        let mapping = _merge_tone_prefixes(&mut bucket, Some(&notes), 100.0, 1);
        assert_eq!(bucket.len(), 1);
        assert_eq!(bucket[0].vol, vec![1, 2, 3, 4]);
        assert_eq!(mapping[&0], 0);
        assert_eq!(mapping[&1], 0);
    }

    #[test]
    fn detune_is_folded_into_e5_but_vibrato_stays_a_macro() {
        let mut event = NoteEvent::new(0, 0, true, 100, 0, 10, 0x80, 0);
        assert_eq!(_absorb_detune(&mut event, &[5, 5, 5], 1), Vec::<i32>::new());
        assert_eq!(event.e5, 0x85);

        let mut event = NoteEvent::new(0, 0, true, 100, 0, 10, 0x80, 0);
        assert_eq!(_absorb_detune(&mut event, &[0, 5, 0, 5], 1), vec![0, 5, 0, 5]);

        // Only speed 1 can fold; a faster macro is real motion.
        let mut event = NoteEvent::new(0, 0, true, 100, 0, 10, 0x80, 0);
        assert_eq!(_absorb_detune(&mut event, &[5, 5], 2), vec![5, 5]);
    }
}
