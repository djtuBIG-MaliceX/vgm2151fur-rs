//! Nintendo Virtual Boy VSU (VSU-VUE) -> notes, wavetables and macros.
//!
//! The VSU has six voices: five 32-entry, 6-bit wavetable channels and one
//! noise channel sharing an LFSR. The five wavetables are shared; a voice
//! chooses one through its `RAM` register.
//!
//! VGM command `0xC7` carries a 16-bit **big-endian register word index**
//! (the hardware byte address divided by four), matching Furnace's
//! `writeS_BE(addr >> 2)`. Register map:
//!
//! ```text
//! 0x000-0x09F  wave RAM: five tables of 32 6-bit samples
//! 0x0A0-0x0BF  modulation table: 32 signed bytes
//! 0x100 + ch*0x10 + r   channel registers
//!     0 INTL   key-on bit 7, interval enable bit 5, interval 0-4
//!     1 LRV    left level (high nibble), right level (low nibble)
//!     2 FQL    frequency low
//!     3 FQH    frequency high (3 bits)
//!     4 EV0    envelope value (high nibble), speed/direction (low)
//!     5 EV1    bit 0 enable, bit 1 loop, bits 4-6 tap/sweep/mod
//!     6 RAM    wavetable index (0-4; >4 is silence)
//!     7 SCW    channel 4 sweep/modulation control
//! 0x160        reset: bit 0 stops every voice
//! ```
//!
//! Playback rate: the hardware advances one wave step every
//! `2048 - Frequency` VSU clocks. Furnace's VBOY platform writes
//! `2048 - pitchTableValue`, so a note is built from the period table value
//! `2048 - Frequency` with the same divider (16) the platform uses.
//!
//! Envelopes: EV1 bit 0 enables a hardware envelope. Its value steps every
//! `(EV0 & 7) + 1` envelope ticks, one tick being 76,800 VSU clocks. The
//! converter replays that state machine into a volume timeline; Furnace then
//! drives EV0 from a volume macro. EV1 bits 4-6 are the noise tap (channel 5)
//! or the sweep/modulation enables (channel 4).
//!
//! Sweep/modulation on channel 4 is translated to Furnace's native VBOY
//! effects (`13xx` sweep, `14xy` modulation) and the instrument's FD
//! modulation table, so the same Mednafen core runs it at its own rate.
//!
//! A driver may rewrite wave RAM while voices are playing (the `a64` and
//! Galactic Pinball logs show both static and per-tick cases). Bursts of wave
//! writes are settled into images; active voices get a wave point at the burst
//! start. The analyzer carries those to the pattern as `10xx` effects.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use crate::parity::{py_int, py_round};
use crate::pitch::furnace_period_note;
use crate::vgm::{ChipWrite, VgmFile, VGM_RATE, VSU_DEFAULT_CLOCK};

pub const VSU_CHANNELS: usize = 6;
pub const VSU_WAVE_CHANNELS: usize = 5;
pub const VSU_WAVE_LEN: usize = 32;
pub const VSU_WAVE_MAX: i32 = 63;
pub const VSU_PITCH_DIVIDER: i32 = 16;
pub const VSU_MAX_PERIOD: i32 = 0x7FF;
// One 60 Hz frame: drivers write the note's own registers a few ms after the
// gate, exactly like the SCC/C140 paths.
pub const VSU_REFIT_SAMPLES: i64 = 735;
// Shorter than this is a driver init/glitch gate, not music.
pub const VSU_MIN_NOTE: i64 = 100;
// Core timing, in VSU clocks.
pub const VSU_EFFECT_CLOCKS: i64 = 4800;
pub const VSU_INTERVAL_CLOCKS: i64 = VSU_EFFECT_CLOCKS * 4;
pub const VSU_ENVELOPE_CLOCKS: i64 = VSU_EFFECT_CLOCKS * 16;
// A wave rewrite is a burst of byte writes; writes within this window belong
// to one settled image.
pub const VSU_WAVE_BURST: i64 = 64;
// `10xx` carries one byte. Keep the palette inside that limit.
pub const VSU_WAVE_LIMIT: usize = 256;

/// Clock for one VSU instance.
pub fn vsu_clock(vgm: &VgmFile, chip_id: i32) -> u32 {
    if chip_id == 1 && vgm.vsu_clock_2 != 0 {
        return vgm.vsu_clock_2;
    }
    if vgm.vsu_clock != 0 {
        vgm.vsu_clock
    } else {
        VSU_DEFAULT_CLOCK
    }
}

/// Period-table value (2048 - Frequency) -> (Furnace note, E5).
pub fn vsu_note(clock: u32, period: i32) -> (i32, i32) {
    let period = period.clamp(0, VSU_MAX_PERIOD);
    let clock = if clock != 0 {
        clock
    } else {
        VSU_DEFAULT_CLOCK
    };
    furnace_period_note(
        clock as f64,
        VSU_PITCH_DIVIDER as f64,
        period,
        0,
        VSU_MAX_PERIOD,
    )
}

fn _env_tick_samples(clock: u32) -> f64 {
    let clock = if clock != 0 {
        clock
    } else {
        VSU_DEFAULT_CLOCK
    };
    VSU_ENVELOPE_CLOCKS as f64 * VGM_RATE as f64 / clock as f64
}

fn _interval_tick_samples(clock: u32) -> f64 {
    let clock = if clock != 0 {
        clock
    } else {
        VSU_DEFAULT_CLOCK
    };
    VSU_INTERVAL_CLOCKS as f64 * VGM_RATE as f64 / clock as f64
}

fn _wave_bytes(raw: &[u8]) -> Vec<u8> {
    raw.iter().map(|v| v & 0x3F).collect()
}

/// The raw modulation bytes; Furnace stores them as signed chars.
fn _signed32(raw: &[u8]) -> Vec<u8> {
    raw.iter().map(|v| v & 0xFF).collect()
}

#[derive(Clone, Debug, PartialEq)]
pub struct VsuHit {
    pub sample_time: i64,
    pub chip_id: i32,
    pub voice: usize,
    /// 2048 - Frequency register (the pitch-table form Furnace writes).
    pub period: i32,
    pub vol: i32,
    pub pan: i32,
    pub wave: i32,
    pub tap: i32,
    pub mod_: i32,
    pub sweep: i32,
    pub ev1: i32,
    pub off_sample: i64,
    // Mid-note values, all in source sample time.
    pub vol_pts: Vec<(i64, i32)>,
    pub pan_pts: Vec<(i64, i32)>,
    pub pitch_pts: Vec<(i64, i32)>,
    pub wave_pts: Vec<(i64, i32)>,
    pub tap_pts: Vec<(i64, i32)>,
    /// Channel 4: (sample, SweepControl, EV1).
    pub sw_pts: Vec<(i64, i32, i32)>,
}

impl Default for VsuHit {
    fn default() -> Self {
        Self {
            sample_time: 0,
            chip_id: 0,
            voice: 0,
            period: 0,
            vol: 0,
            pan: 0,
            wave: 0,
            tap: 0,
            mod_: 0,
            sweep: 0,
            ev1: 0,
            off_sample: 0,
            vol_pts: Vec::new(),
            pan_pts: Vec::new(),
            pitch_pts: Vec::new(),
            wave_pts: Vec::new(),
            tap_pts: Vec::new(),
            sw_pts: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct VsuOff {
    pub sample_time: i64,
    pub chip_id: i32,
    pub voice: usize,
}

#[derive(Clone, Debug, PartialEq)]
struct Voice {
    index: usize,
    keyed: bool,
    intl: i32,
    period: i32,
    vol: i32,
    ev0_low: i32,
    ev1: i32,
    pan: i32,
    ram: i32,
    sweep: i32,
    active: Option<VsuHit>,
    refit_until: i64,
    auto_end: i64,
    env_value: i32,
    env_reload: i32,
    env_counter: i32,
    env_next: f64,
    env_mod_mask: i32,
}

impl Default for Voice {
    fn default() -> Self {
        Self {
            index: 0,
            keyed: false,
            intl: 0,
            period: 0,
            vol: 0,
            ev0_low: 0,
            ev1: 0,
            pan: 0,
            ram: 0,
            sweep: 0,
            active: None,
            refit_until: 0,
            auto_end: 0,
            env_value: 0,
            env_reload: 0,
            env_counter: 1,
            env_next: 0.0,
            env_mod_mask: 0,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
struct Chip {
    waves: Vec<Vec<u8>>,
    dirty: BTreeSet<usize>,
    burst: [i64; VSU_WAVE_CHANNELS],
    settled: [usize; VSU_WAVE_CHANNELS],
    wave_map: HashMap<Vec<u8>, usize>,
    wave_images: Vec<Vec<u8>>,
    mod_: Vec<u8>,
    mod_dirty: bool,
    mod_burst: i64,
    mod_settled: usize,
    mod_map: HashMap<Vec<u8>, usize>,
    mod_images: Vec<Vec<u8>>,
    voices: Vec<Voice>,
    dynamic: bool,
}

fn _new_chip() -> Chip {
    let zero = vec![0u8; VSU_WAVE_LEN];
    let mut wave_map = HashMap::new();
    wave_map.insert(zero.clone(), 0usize);
    let mut mod_map = HashMap::new();
    mod_map.insert(zero.clone(), 0usize);
    Chip {
        waves: (0..VSU_WAVE_CHANNELS)
            .map(|_| vec![0u8; VSU_WAVE_LEN])
            .collect(),
        dirty: BTreeSet::new(),
        burst: [0; VSU_WAVE_CHANNELS],
        settled: [0; VSU_WAVE_CHANNELS],
        wave_map,
        wave_images: vec![zero.clone()],
        mod_: vec![0u8; VSU_WAVE_LEN],
        mod_dirty: false,
        mod_burst: 0,
        mod_settled: 0,
        mod_map,
        mod_images: vec![zero],
        voices: (0..VSU_CHANNELS)
            .map(|i| Voice {
                index: i,
                ..Default::default()
            })
            .collect(),
        dynamic: false,
    }
}

fn _record_vol(hit: Option<&mut VsuHit>, when: i64, value: i32) {
    let Some(hit) = hit else {
        return;
    };
    let value = value.clamp(0, 15);
    if when <= hit.sample_time {
        hit.vol = value;
        return;
    }
    if !hit.vol_pts.is_empty() {
        let (last_t, last_v) = *hit.vol_pts.last().unwrap();
        if last_t == when {
            *hit.vol_pts.last_mut().unwrap() = (when, value);
            return;
        }
        if last_v == value {
            return;
        }
    }
    hit.vol_pts.push((when, value));
}

fn _record_pan(hit: Option<&mut VsuHit>, when: i64, value: i32) {
    let Some(hit) = hit else {
        return;
    };
    let value = value & 0xFF;
    if when <= hit.sample_time {
        hit.pan = value;
        return;
    }
    if !hit.pan_pts.is_empty() {
        let (last_t, last_v) = *hit.pan_pts.last().unwrap();
        if last_t == when {
            *hit.pan_pts.last_mut().unwrap() = (when, value);
            return;
        }
        if last_v == value {
            return;
        }
    }
    hit.pan_pts.push((when, value));
}

fn _record_period(hit: Option<&mut VsuHit>, when: i64, value: i32) {
    let Some(hit) = hit else {
        return;
    };
    let value = value.clamp(0, VSU_MAX_PERIOD);
    if when <= hit.sample_time {
        hit.period = value;
        return;
    }
    if !hit.pitch_pts.is_empty() {
        let (last_t, last_v) = *hit.pitch_pts.last().unwrap();
        if last_t == when {
            *hit.pitch_pts.last_mut().unwrap() = (when, value);
            return;
        }
        if last_v == value {
            return;
        }
    }
    hit.pitch_pts.push((when, value));
}

fn _record_wave(hit: Option<&mut VsuHit>, when: i64, index: i32) {
    let Some(hit) = hit else {
        return;
    };
    if when <= hit.sample_time {
        hit.wave = index;
        return;
    }
    if !hit.wave_pts.is_empty() {
        let (last_t, last_v) = *hit.wave_pts.last().unwrap();
        if last_t == when {
            *hit.wave_pts.last_mut().unwrap() = (when, index);
            return;
        }
        if last_v == index {
            return;
        }
    } else if hit.wave == index {
        return;
    }
    hit.wave_pts.push((when, index));
}

fn _record_tap(hit: Option<&mut VsuHit>, when: i64, value: i32) {
    let Some(hit) = hit else {
        return;
    };
    let value = value & 7;
    if when <= hit.sample_time {
        hit.tap = value;
        return;
    }
    if !hit.tap_pts.is_empty() {
        let (last_t, last_v) = *hit.tap_pts.last().unwrap();
        if last_t == when {
            *hit.tap_pts.last_mut().unwrap() = (when, value);
            return;
        }
        if last_v == value {
            return;
        }
    } else if hit.tap == value {
        return;
    }
    hit.tap_pts.push((when, value));
}

fn _record_sw(hit: Option<&mut VsuHit>, when: i64, sweep: i32, ev1: i32) {
    let Some(hit) = hit else {
        return;
    };
    let sweep = sweep & 0xFF;
    let ev1 = ev1 & 0xFF;
    if when <= hit.sample_time {
        hit.sweep = sweep;
        hit.ev1 = ev1;
        return;
    }
    if !hit.sw_pts.is_empty() {
        let (last_t, last_s, last_e) = *hit.sw_pts.last().unwrap();
        if last_t == when {
            *hit.sw_pts.last_mut().unwrap() = (when, sweep, ev1);
            return;
        }
        if last_s == sweep && last_e == ev1 {
            return;
        }
    } else if hit.sweep == sweep && hit.ev1 == ev1 {
        return;
    }
    hit.sw_pts.push((when, sweep, ev1));
}

/// Turn finished wave/mod bursts into settled images and points.
fn _settle(chip: &mut Chip, _when: i64) {
    let dirty: Vec<usize> = chip.dirty.iter().copied().collect();
    for w in dirty {
        let img = _wave_bytes(&chip.waves[w]);
        let idx = match chip.wave_map.get(&img) {
            Some(&idx) => idx,
            None => {
                let idx = chip.wave_images.len();
                chip.wave_map.insert(img.clone(), idx);
                chip.wave_images.push(img);
                idx
            }
        };
        chip.settled[w] = idx;
        let burst = chip.burst[w];
        let mut saw_dynamic = false;
        for voice in chip.voices.iter_mut() {
            let active_time = match &voice.active {
                Some(h) => h.sample_time,
                None => continue,
            };
            if voice.ram != w as i32 {
                continue;
            }
            if burst < active_time {
                continue;
            }
            if burst > voice.refit_until {
                saw_dynamic = true;
            }
            _record_wave(voice.active.as_mut(), burst, idx as i32);
        }
        if saw_dynamic {
            chip.dynamic = true;
        }
    }
    chip.dirty.clear();
    if chip.mod_dirty {
        let img = _signed32(&chip.mod_);
        let idx = match chip.mod_map.get(&img) {
            Some(&idx) => idx,
            None => {
                let idx = chip.mod_images.len();
                chip.mod_map.insert(img.clone(), idx);
                chip.mod_images.push(img);
                idx
            }
        };
        chip.mod_settled = idx;
        chip.mod_dirty = false;
        let burst = chip.mod_burst;
        let voice = &mut chip.voices[4];
        let refit_until = voice.refit_until;
        if let Some(hit) = voice.active.as_mut() {
            if burst >= hit.sample_time {
                if burst <= refit_until {
                    hit.mod_ = idx as i32;
                } else if hit.mod_ != idx as i32 {
                    // A live mod-table swap cannot ride the instrument. Keep
                    // the first image; the next note will pick up the new one.
                }
            }
        }
    }
}

/// Replay the VSU envelope up to `when` (enabled steps only).
fn _env_advance(voice: &mut Voice, when: i64, env_tick: f64, interval_tick: f64) {
    if voice.active.is_none() {
        return;
    }
    while voice.env_next <= when as f64 + 1e-9 {
        let tick = voice.env_next;
        voice.env_next += env_tick;
        if voice.ev1 & 0x01 == 0 {
            // The core leaves EnvelopeCounter alone while the envelope is
            // disabled, but the clock keeps running.
            continue;
        }
        voice.env_counter -= 1;
        if voice.env_counter > 0 {
            continue;
        }
        voice.env_counter = (voice.ev0_low & 0x07) + 1;
        let mut value = voice.env_value;
        let up = voice.ev0_low & 0x08 != 0;
        if value < 15 && up {
            value += 1;
        } else if value > 0 && !up {
            value -= 1;
        } else if (voice.ev1 & 0x02) != 0 && voice.env_mod_mask != 2 {
            value = voice.env_reload;
            voice.env_mod_mask = 0;
        } else if voice.env_mod_mask == 0 {
            voice.env_mod_mask = 1;
        }
        if value != voice.env_value {
            voice.env_value = value;
            _record_vol(
                voice.active.as_mut(),
                py_int(py_round(tick)),
                value,
            );
        }
    }
    let _ = interval_tick;
}

fn _start(
    settled: &[usize; VSU_WAVE_CHANNELS],
    mod_settled: usize,
    voice: &mut Voice,
    chip_id: i32,
    t: i64,
    env_tick: f64,
    interval_tick: f64,
) {
    if voice.active.is_some() {
        return;
    }
    let wave = if voice.ram <= 4 {
        settled[voice.ram as usize]
    } else {
        0
    };
    let mod_ = if voice.index == 4 { mod_settled } else { 0 };
    let hit = VsuHit {
        sample_time: t,
        chip_id,
        voice: voice.index,
        period: 2048 - voice.period,
        vol: voice.vol,
        pan: voice.pan,
        wave: wave as i32,
        tap: if voice.index == 5 {
            (voice.ev1 >> 4) & 7
        } else {
            0
        },
        mod_: mod_ as i32,
        sweep: voice.sweep,
        ev1: voice.ev1,
        ..Default::default()
    };
    voice.active = Some(hit);
    voice.refit_until = t + VSU_REFIT_SAMPLES;
    voice.auto_end = 0;
    if voice.intl & 0x20 != 0 {
        voice.auto_end = t
            + py_int(py_round(
                ((voice.intl & 0x1F) + 1) as f64 * interval_tick,
            ));
    }
    voice.env_value = voice.vol;
    voice.env_reload = voice.vol;
    voice.env_counter = (voice.ev0_low & 0x07) + 1;
    voice.env_next = t as f64 + env_tick;
    voice.env_mod_mask = 0;
}

fn _close(
    voice: &mut Voice,
    t: i64,
    hits: &mut Vec<VsuHit>,
    offs: &mut Vec<VsuOff>,
    emit_off: bool,
    open_end: bool,
) {
    let Some(mut hit) = voice.active.take() else {
        return;
    };
    let off_sample = if open_end {
        0
    } else if t > hit.sample_time {
        t
    } else {
        0
    };
    hit.off_sample = off_sample;
    if off_sample != 0 && off_sample - hit.sample_time < VSU_MIN_NOTE {
        return;
    }
    if emit_off && off_sample != 0 {
        offs.push(VsuOff {
            sample_time: off_sample,
            chip_id: hit.chip_id,
            voice: hit.voice,
        });
    }
    hits.push(hit);
}

fn _advance(
    voice: &mut Voice,
    t: i64,
    env_tick: f64,
    interval_tick: f64,
    hits: &mut Vec<VsuHit>,
    offs: &mut Vec<VsuOff>,
) {
    if voice.active.is_none() {
        return;
    }
    if voice.auto_end != 0 && voice.auto_end <= t {
        let end = voice.auto_end;
        _env_advance(voice, end, env_tick, interval_tick);
        _close(voice, end, hits, offs, true, false);
        voice.keyed = false;
        voice.auto_end = 0;
        return;
    }
    _env_advance(voice, t, env_tick, interval_tick);
}

/// Keep the most used images and map the rest to the nearest one.
///
/// `10xx` can only name 256 wavetables, and a per-tick wavetable driver can
/// settle more than that. The palette keeps the notes' actual timbres where it
/// can and the closest retained waveform elsewhere.
fn _cap_wave_images(
    images: Vec<Vec<u8>>,
    hits: &mut [VsuHit],
    limit: usize,
) -> (Vec<Vec<u8>>, usize) {
    if images.len() <= limit {
        return (images, 0);
    }
    let mut counts: HashMap<usize, i64> = HashMap::new();
    counts.insert(0, 1);
    for h in hits.iter() {
        *counts.entry(h.wave as usize).or_insert(0) += 1;
        for &(_t, idx) in &h.wave_pts {
            *counts.entry(idx as usize).or_insert(0) += 1;
        }
    }
    let mut keep: Vec<usize> = (0..images.len()).collect();
    keep.sort_by_key(|&i| (-counts.get(&i).copied().unwrap_or(0), i));
    keep.truncate(limit);
    let keep_set: HashSet<usize> = keep.iter().copied().collect();
    let mut remap: HashMap<usize, usize> = HashMap::new();
    for (i, &old) in keep.iter().enumerate() {
        remap.insert(old, i);
    }
    let kept_images: Vec<Vec<u8>> = keep.iter().map(|&i| images[i].clone()).collect();
    let dropped: Vec<usize> = (0..images.len())
        .filter(|i| !keep_set.contains(i))
        .collect();
    let dropped_count = dropped.len();
    for old in dropped {
        let src = &images[old];
        let best = (0..kept_images.len())
            .min_by_key(|&j| {
                let mut sum = 0i32;
                for k in 0..VSU_WAVE_LEN {
                    let d = src[k] as i32 - kept_images[j][k] as i32;
                    sum += d * d;
                }
                sum
            })
            .unwrap_or(0);
        remap.insert(old, best);
    }
    for h in hits.iter_mut() {
        h.wave = *remap.get(&(h.wave as usize)).unwrap_or(&0) as i32;
        if !h.wave_pts.is_empty() {
            h.wave_pts = h
                .wave_pts
                .iter()
                .map(|&(t, i)| (t, *remap.get(&(i as usize)).unwrap_or(&0) as i32))
                .collect();
        }
    }
    (kept_images, dropped_count)
}

/// Collect VSU notes, key-offs, wave images and mod-table images.
///
/// Returns (hits, offs, wave_images, mod_images, saw_dynamic_waves, warnings).
pub fn collect_vsu(
    vgm: &VgmFile,
) -> (
    Vec<VsuHit>,
    Vec<VsuOff>,
    Vec<Vec<u8>>,
    Vec<Vec<u8>>,
    bool,
    Vec<String>,
) {
    let mut warnings: Vec<String> = Vec::new();
    let writes: Vec<&ChipWrite> = vgm.writes.iter().filter(|w| w.chip == "vsu").collect();
    if writes.is_empty() {
        return (Vec::new(), Vec::new(), Vec::new(), Vec::new(), false, warnings);
    }

    let mut chips: BTreeMap<i32, Chip> = BTreeMap::new();
    let mut ids: BTreeSet<i32> = BTreeSet::new();
    for w in &writes {
        ids.insert((w.chip_id & 1) as i32);
    }
    for cid in ids {
        chips.insert(cid, _new_chip());
    }

    let mut hits: Vec<VsuHit> = Vec::new();
    let mut offs: Vec<VsuOff> = Vec::new();
    let mut ram_silent: i64 = 0;
    let mut last_wave_write: i64 = 0;
    let mut last_sample: i64 = 0;

    for w in &writes {
        let cid = (w.chip_id & 1) as i32;
        let Some(chip) = chips.get_mut(&cid) else {
            continue;
        };
        let t = w.sample;
        last_sample = last_sample.max(t);
        let reg = (w.reg & 0x7FFF) as u16;
        let val = (w.val & 0xFF) as i32;
        let clock = vsu_clock(vgm, cid);
        let env_tick = _env_tick_samples(clock);
        let interval_tick = _interval_tick_samples(clock);

        if reg < 0x0A0 {
            let table = (reg >> 5) as usize;
            if table >= VSU_WAVE_CHANNELS {
                continue;
            }
            if !chip.dirty.contains(&table) {
                chip.dirty.insert(table);
                chip.burst[table] = t;
            }
            chip.waves[table][(reg & 0x1F) as usize] = (val & 0x3F) as u8;
            last_wave_write = last_wave_write.max(t);
            continue;
        }
        if reg < 0x100 {
            if !chip.mod_dirty {
                chip.mod_dirty = true;
                chip.mod_burst = t;
            }
            chip.mod_[(reg & 0x1F) as usize] = val as u8;
            last_wave_write = last_wave_write.max(t);
            continue;
        }

        // A non-wave write ends every open wave/mod burst first.
        _settle(chip, t);

        if reg == 0x160 {
            for voice in chip.voices.iter_mut() {
                _advance(
                    voice,
                    t,
                    env_tick,
                    interval_tick,
                    &mut hits,
                    &mut offs,
                );
                voice.keyed = false;
                voice.auto_end = 0;
                _close(voice, t, &mut hits, &mut offs, true, false);
            }
            continue;
        }
        if reg < 0x100 || reg >= 0x160 {
            continue;
        }
        let ch = ((reg - 0x100) >> 4) as usize;
        let r = (reg - 0x100) & 0x0F;
        if ch >= VSU_CHANNELS {
            continue;
        }
        let idx = ch;

        // Split the voice-state borrow around the shared collection vectors.
        _advance(
            &mut chip.voices[ch],
            t,
            env_tick,
            interval_tick,
            &mut hits,
            &mut offs,
        );

        if r == 0x0 {
            let voice = &mut chip.voices[ch];
            let was = voice.keyed;
            let now = val & 0x80 != 0;
            voice.intl = val;
            if now && !was {
                // A key-on while a voice is already keyed resets its phase in
                // the core; treat it as a fresh articulation.
                if voice.active.is_some() {
                    _close(voice, t, &mut hits, &mut offs, false, false);
                }
                voice.keyed = true;
                if voice.ram > 4 && idx != 5 {
                    ram_silent += 1;
                } else {
                    _start(&chip.settled, chip.mod_settled, voice, cid, t, env_tick, interval_tick);
                }
            } else if now && was {
                // Re-key without an intervening key-off: the core resets the
                // phase/counters. Start a fresh note and cut the old one.
                if voice.active.is_some() {
                    _close(voice, t, &mut hits, &mut offs, false, false);
                }
                _start(&chip.settled, chip.mod_settled, voice, cid, t, env_tick, interval_tick);
            } else if !now && was {
                voice.keyed = false;
                voice.auto_end = 0;
                _close(voice, t, &mut hits, &mut offs, true, false);
            }
            continue;
        }

        let voice = &mut chip.voices[ch];
        if !voice.keyed {
            // Volume/frequency/pan writes with the gate off configure the
            // next note. There is no active hit to update.
            if r == 1 {
                voice.pan = val;
            } else if r == 2 {
                voice.period = (voice.period & 0x700) | val;
            } else if r == 3 {
                voice.period = (voice.period & 0xFF) | ((val & 0x07) << 8);
            } else if r == 4 {
                voice.vol = (val >> 4) & 0x0F;
                voice.ev0_low = val & 0x0F;
            } else if r == 5 {
                voice.ev1 = val;
            } else if r == 6 {
                voice.ram = val & 0x0F;
            } else if r == 7 && idx == 4 {
                voice.sweep = val;
            }
            continue;
        }

        let hit_exists = voice.active.is_some();
        if r == 1 {
            voice.pan = val;
            if hit_exists && t <= voice.refit_until {
                if let Some(h) = voice.active.as_mut() {
                    h.pan = val;
                }
            } else {
                _record_pan(voice.active.as_mut(), t, val);
            }
        } else if r == 2 {
            voice.period = (voice.period & 0x700) | val;
            if hit_exists && t <= voice.refit_until {
                if let Some(h) = voice.active.as_mut() {
                    h.period = 2048 - voice.period;
                }
            } else {
                _record_period(voice.active.as_mut(), t, 2048 - voice.period);
            }
        } else if r == 3 {
            voice.period = (voice.period & 0xFF) | ((val & 0x07) << 8);
            if hit_exists && t <= voice.refit_until {
                if let Some(h) = voice.active.as_mut() {
                    h.period = 2048 - voice.period;
                }
            } else {
                _record_period(voice.active.as_mut(), t, 2048 - voice.period);
            }
        } else if r == 4 {
            let new_vol = (val >> 4) & 0x0F;
            let new_low = val & 0x0F;
            let old_vol = voice.vol;
            if hit_exists && t > voice.refit_until && new_vol > old_vol {
                // This driver articulates by raising EV0's value while the
                // gate stays on. It is a new note, not a volume step.
                _close(voice, t, &mut hits, &mut offs, false, false);
                voice.vol = new_vol;
                voice.ev0_low = new_low;
                voice.env_value = new_vol;
                voice.env_reload = new_vol;
                voice.env_counter = (new_low & 0x07) + 1;
                voice.env_next = t as f64 + env_tick;
                voice.env_mod_mask = 0;
                _start(&chip.settled, chip.mod_settled, voice, cid, t, env_tick, interval_tick);
            } else {
                voice.vol = new_vol;
                voice.ev0_low = new_low;
                voice.env_value = new_vol;
                voice.env_reload = new_vol;
                if voice.env_mod_mask == 1 {
                    voice.env_mod_mask = 2;
                }
                if hit_exists && t <= voice.refit_until {
                    if let Some(h) = voice.active.as_mut() {
                        h.vol = new_vol;
                    }
                } else {
                    _record_vol(voice.active.as_mut(), t, new_vol);
                }
            }
        } else if r == 5 {
            let old_tap = (voice.ev1 >> 4) & 7;
            let old_sw = voice.ev1 & 0x70;
            voice.ev1 = val;
            if idx == 5 {
                let tap = (val >> 4) & 7;
                if tap != old_tap {
                    if hit_exists && t <= voice.refit_until {
                        if let Some(h) = voice.active.as_mut() {
                            h.tap = tap;
                        }
                    } else {
                        _record_tap(voice.active.as_mut(), t, tap);
                    }
                }
            } else if idx == 4 && (val & 0x70) != old_sw {
                if hit_exists && t <= voice.refit_until {
                    if let Some(h) = voice.active.as_mut() {
                        h.sweep = voice.sweep;
                        h.ev1 = val;
                    }
                } else {
                    _record_sw(voice.active.as_mut(), t, voice.sweep, val);
                }
            }
        } else if r == 6 {
            let old_ram = voice.ram;
            voice.ram = val & 0x0F;
            if voice.ram != old_ram {
                let new_idx = if voice.ram > 4 {
                    0
                } else {
                    chip.settled[voice.ram as usize]
                };
                if hit_exists && t <= voice.refit_until {
                    if let Some(h) = voice.active.as_mut() {
                        h.wave = new_idx as i32;
                    }
                } else {
                    _record_wave(voice.active.as_mut(), t, new_idx as i32);
                }
            }
        } else if r == 7 && idx == 4 {
            voice.sweep = val;
            if hit_exists && t <= voice.refit_until {
                if let Some(h) = voice.active.as_mut() {
                    h.sweep = val;
                }
            } else {
                _record_sw(voice.active.as_mut(), t, val, voice.ev1);
            }
        }
    }

    // Settle the final burst and close voices still sounding.
    let end = if vgm.total_samples != 0 {
        vgm.total_samples
    } else {
        last_sample
    };
    for (cid, chip) in chips.iter_mut() {
        _settle(chip, if last_wave_write != 0 { last_wave_write } else { end });
        let clock = vsu_clock(vgm, *cid);
        for voice in chip.voices.iter_mut() {
            let env_tick = _env_tick_samples(clock);
            let interval_tick = _interval_tick_samples(clock);
            _advance(voice, end, env_tick, interval_tick, &mut hits, &mut offs);
            if voice.active.is_some() {
                _close(voice, end, &mut hits, &mut offs, false, true);
            }
        }
    }

    // Count dynamic wave activity: a settled image for a table that an active
    // voice uses, beyond the five initial images, means the driver morphs.
    let mut wave_images: Vec<Vec<u8>> = Vec::new();
    let mut wave_index_offset: HashMap<Vec<u8>, usize> = HashMap::new();
    for chip in chips.values() {
        for img in &chip.wave_images {
            if !wave_index_offset.contains_key(img) {
                let idx = wave_images.len();
                wave_index_offset.insert(img.clone(), idx);
                wave_images.push(img.clone());
            }
        }
    }
    // The per-chip images were indexed independently; remap each hit to the
    // combined list.
    for h in hits.iter_mut() {
        let Some(chip) = chips.get(&h.chip_id) else {
            continue;
        };
        let src = if (h.wave as usize) < chip.wave_images.len() {
            chip.wave_images[h.wave as usize].clone()
        } else {
            vec![0u8; VSU_WAVE_LEN]
        };
        h.wave = wave_index_offset[&src] as i32;
        if !h.wave_pts.is_empty() {
            h.wave_pts = h
                .wave_pts
                .iter()
                .map(|&(t, i)| {
                    let idx = if (i as usize) < chip.wave_images.len() {
                        wave_index_offset[&chip.wave_images[i as usize]]
                    } else {
                        0
                    };
                    (t, idx as i32)
                })
                .collect();
        }
    }
    let mut mod_images: Vec<Vec<u8>> = Vec::new();
    let mut mod_map: HashMap<Vec<u8>, usize> = HashMap::new();
    for chip in chips.values() {
        for img in &chip.mod_images {
            if !mod_map.contains_key(img) {
                let idx = mod_images.len();
                mod_map.insert(img.clone(), idx);
                mod_images.push(img.clone());
            }
        }
    }
    for h in hits.iter_mut() {
        let Some(chip) = chips.get(&h.chip_id) else {
            continue;
        };
        let src = if (h.mod_ as usize) < chip.mod_images.len() {
            chip.mod_images[h.mod_ as usize].clone()
        } else {
            vec![0u8; VSU_WAVE_LEN]
        };
        h.mod_ = mod_map[&src] as i32;
    }

    let dynamic = chips.values().any(|chip| chip.dynamic);
    if ram_silent != 0 {
        warnings.push(format!(
            "{ram_silent} VSU note(s) selected wavetable RAM 5-15, which \
             silences the voice; they were not emitted"
        ));
    }
    if dynamic {
        warnings.push(
            "VSU wave RAM is rewritten while voices are active; the module \
             uses live 10xx wave changes and screwThis is set"
                .to_string(),
        );
    }
    let (wave_images, dropped) = _cap_wave_images(wave_images, &mut hits, VSU_WAVE_LIMIT);
    if dropped != 0 {
        warnings.push(format!(
            "{dropped} VSU wave image(s) exceeded the 10xx palette; the \
             closest retained waveform was used"
        ));
    }
    (hits, offs, wave_images, mod_images, dynamic, warnings)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vgm_with(writes: Vec<ChipWrite>) -> VgmFile {
        let mut vgm = VgmFile::default();
        vgm.vsu_clock = 5_000_000;
        vgm.total_samples = 44_100;
        vgm.writes = writes;
        vgm
    }

    #[test]
    fn no_writes_produces_nothing() {
        let vgm = vgm_with(Vec::new());
        let (hits, offs, waves, mods, dynamic, warnings) = collect_vsu(&vgm);
        assert!(hits.is_empty());
        assert!(offs.is_empty());
        assert!(waves.is_empty());
        assert!(mods.is_empty());
        assert!(!dynamic);
        assert!(warnings.is_empty());
    }

    #[test]
    fn keyed_voice_reports_period_volume_pan_and_wave_image() {
        let writes = vec![
            ChipWrite::new(0, "vsu", 0, 0x00, 0x05),
            ChipWrite::new(0, "vsu", 0, 0x1F, 0x40),
            ChipWrite::new(10, "vsu", 0, 0x101, 0x0F),
            ChipWrite::new(10, "vsu", 0, 0x102, 0x10),
            ChipWrite::new(10, "vsu", 0, 0x103, 0x03),
            ChipWrite::new(10, "vsu", 0, 0x104, 0x50),
            ChipWrite::new(10, "vsu", 0, 0x106, 0x00),
            ChipWrite::new(20, "vsu", 0, 0x100, 0x80),
            ChipWrite::new(500, "vsu", 0, 0x100, 0x00),
        ];
        let vgm = vgm_with(writes);
        let (hits, offs, waves, _mods, dynamic, warnings) = collect_vsu(&vgm);
        assert_eq!(hits.len(), 1);
        let hit = &hits[0];
        assert_eq!(hit.sample_time, 20);
        assert_eq!(hit.period, 2048 - 0x310);
        assert_eq!(hit.vol, 5);
        assert_eq!(hit.pan, 0x0F);
        assert_eq!(hit.off_sample, 500);
        assert_eq!(hit.wave, 1);
        assert_eq!(waves.len(), 2);
        assert_eq!(waves[1][0], 5);
        // 0x40 in wave RAM keeps only the low six bits.
        assert_eq!(waves[1][VSU_WAVE_LEN - 1], 0);
        assert_eq!(offs.len(), 1);
        assert_eq!(offs[0].sample_time, 500);
        assert_eq!(offs[0].voice, 0);
        assert!(!dynamic);
        assert!(warnings.is_empty());
    }

    #[test]
    fn envelope_steps_are_replayed_into_volume_points() {
        let writes = vec![
            ChipWrite::new(0, "vsu", 0, 0x101, 0x0F),
            ChipWrite::new(0, "vsu", 0, 0x102, 0x10),
            ChipWrite::new(0, "vsu", 0, 0x103, 0x03),
            ChipWrite::new(0, "vsu", 0, 0x104, 0x50),
            ChipWrite::new(0, "vsu", 0, 0x105, 0x03),
            ChipWrite::new(0, "vsu", 0, 0x100, 0x80),
        ];
        let vgm = vgm_with(writes);
        let (hits, _offs, _waves, _mods, _dynamic, _warnings) = collect_vsu(&vgm);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].vol, 5);
        assert!(!hits[0].vol_pts.is_empty());
        let saw: Vec<i32> = hits[0].vol_pts.iter().take(5).map(|p| p.1).collect();
        assert_eq!(saw, vec![4, 3, 2, 1, 0]);
        assert_eq!(hits[0].vol_pts[5].1, 5);
    }

    #[test]
    fn wave_palette_keeps_the_most_used_image_and_remaps_the_rest() {
        let images = vec![vec![0u8; VSU_WAVE_LEN], vec![1u8; VSU_WAVE_LEN], vec![2u8; VSU_WAVE_LEN]];
        let mut used: Vec<VsuHit> = Vec::new();
        for wave in [1i32, 1, 0, 2] {
            used.push(VsuHit {
                wave,
                ..Default::default()
            });
        }
        used[0].wave_pts = vec![(10, 2)];
        let (kept, dropped) = _cap_wave_images(images, &mut used, 1);
        assert_eq!(dropped, 2);
        assert_eq!(kept, vec![vec![0u8; VSU_WAVE_LEN]]);
        assert!(used.iter().all(|h| h.wave == 0));
        assert_eq!(used[0].wave_pts, vec![(10, 0)]);
    }

    #[test]
    fn clock_selection_and_period_note_clamp() {
        let mut vgm = VgmFile::default();
        assert_eq!(vsu_clock(&vgm, 0), VSU_DEFAULT_CLOCK);
        vgm.vsu_clock = 4_000_000;
        vgm.vsu_clock_2 = 6_000_000;
        assert_eq!(vsu_clock(&vgm, 0), 4_000_000);
        assert_eq!(vsu_clock(&vgm, 1), 6_000_000);
        vgm.vsu_clock_2 = 0;
        assert_eq!(vsu_clock(&vgm, 1), 4_000_000);

        let expected = furnace_period_note(5_000_000.0, 16.0, 0, 0, VSU_MAX_PERIOD);
        assert_eq!(vsu_note(0, 0), expected);
        assert_eq!(vsu_note(5_000_000, -5), expected);
        assert_eq!(
            vsu_note(5_000_000, 0x900),
            furnace_period_note(5_000_000.0, 16.0, VSU_MAX_PERIOD, 0, VSU_MAX_PERIOD)
        );
    }
}
