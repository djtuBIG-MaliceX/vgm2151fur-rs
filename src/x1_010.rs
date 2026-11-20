//! Seta/Allumer X1-010: wavetable voices and 8-bit PCM bodies.
//!
//! VGM 1.71 carries the clock at header 0xD8 and writes the chip with `0xC8`:
//! a 16-bit big-endian register/memory offset followed by one byte. The first
//! 0x80 bytes are the sixteen channel register blocks, 0x80-0xFFF is envelope
//! shape RAM (32 shapes of 128 bytes), 0x1000-0x1FFF is waveform RAM (32 shapes
//! of 128 bytes), and data block 0x91 supplies the 1 MB sample ROM.
//!
//! Each voice is either an LFSR-style wavetable voice or an 8-bit PCM voice.
//! Wave voices read a 128-byte signed waveform selected by register 1 and a
//! 128-byte hardware envelope selected by register 5. PCM voices read
//! `start*0x1000 .. (0x100-end)*0x1000` from the ROM at
//! `clock/8192 * frequency` Hz.
//!
//! Literal port of `vgm2151fur/x1_010.py` from vgm2151fur 0.11.0.

use std::collections::{BTreeMap, BTreeSet};

use crate::parity::{py_int, py_round};
use crate::pitch::frequency_pitch_table;
use crate::vgm::{ChipWrite, VgmFile, VGM_RATE, assemble_rom};
pub use crate::vgm::X1_010_DEFAULT_CLOCK;

pub const X1_CHANNELS: usize = 16;
/// Furnace's DivPlatformX1_010 uses this as the pitch-table frequency base.
pub const X1_FREQ_DIV: i32 = 4_194_304;
pub const X1_MAX_FREQ: i32 = 0xFFFF;
pub const X1_WAVE_BYTES: usize = 128;
pub const X1_ENV_BYTES: usize = 128;
pub const X1_ROM_SIZE: usize = 0x100000;

pub const REG_STATUS: usize = 0;
pub const REG_VOL_WAVE: usize = 1;
pub const REG_FREQ_LO: usize = 2;
pub const REG_FREQ_HI: usize = 3;
pub const REG_START_ENVPERIOD: usize = 4;
pub const REG_END_ENVSHAPE: usize = 5;

/// Defaults of `_AY_MERGE_GAP`, `_AY_NOTE_HOLD` and `_NOTE_STEP_SEMITONES` in
/// analyze.py, used by `segment_x1_freq`.
pub const X1_MERGE_GAP: i64 = 16;
pub const X1_NOTE_HOLD: i64 = 1470;
pub const X1_NOTE_STEP_SEMITONES: f64 = 0.75;

#[allow(dead_code)]
fn s8(value: i32) -> i32 {
    let value = value & 0xFF;
    if value >= 0x80 { value - 0x100 } else { value }
}

/// Wave-mode register value after the status bit-7 divider.
pub fn x1_wave_reg(freq: i32) -> i32 {
    i32::max(1, freq)
}

/// PCM frequency register. The core substitutes 4 for zero.
pub fn x1_pcm_freq(freq: i32) -> i32 {
    if freq == 0 { 4 } else { freq }
}

pub fn x1_pcm_rate(clock: u32, freq: i32) -> i32 {
    i32::max(
        1,
        py_int(py_round(clock as f64 / 8192.0 * x1_pcm_freq(freq) as f64)) as i32,
    )
}

/// One wavetable voice note.
#[derive(Clone, Debug, PartialEq)]
pub struct X1Hit {
    pub sample_time: i64,
    pub chip_id: i32,
    pub voice: usize,
    /// 16-bit pitch register at the gate.
    pub freq: i32,
    pub env_period: i32,
    /// Source envelope number (0..31).
    pub env_shape: i32,
    pub env_oneshot: bool,
    /// Source waveform number (0..31).
    pub wave: i32,
    pub clock: u32,
    pub wave_data: Vec<u8>,
    pub env_data: Vec<u8>,
    pub off_sample: i64,
    pub freq_pts: Vec<(i64, i32)>,
    pub wave_pts: Vec<(i64, Vec<u8>)>,
    pub env_pts: Vec<(i64, Vec<u8>)>,
    pub env_period_pts: Vec<(i64, i32)>,
    pub env_mode_pts: Vec<(i64, i32)>,
    /// Period-0 envelopes hold envelope byte 0. Some drivers rewrite that byte
    /// while a note sounds, so keep the resulting mono level steps.
    pub env_level_pts: Vec<(i64, i32)>,
    /// Live left/right levels of that byte (left, right, 0-15). Period-0 shape
    /// switches also land here because they select another constant byte pair.
    pub env_lr_pts: Vec<(i64, i32, i32)>,
}

impl Default for X1Hit {
    fn default() -> Self {
        Self {
            sample_time: 0,
            chip_id: 0,
            voice: 0,
            freq: 0,
            env_period: 0,
            env_shape: 0,
            env_oneshot: false,
            wave: 0,
            clock: 0,
            wave_data: Vec::new(),
            env_data: Vec::new(),
            off_sample: 0,
            freq_pts: Vec::new(),
            wave_pts: Vec::new(),
            env_pts: Vec::new(),
            env_period_pts: Vec::new(),
            env_mode_pts: Vec::new(),
            env_level_pts: Vec::new(),
            env_lr_pts: Vec::new(),
        }
    }
}

/// One PCM voice note with its exact ROM range.
#[derive(Clone, Debug, PartialEq)]
pub struct X1PcmHit {
    pub sample_time: i64,
    pub chip_id: i32,
    pub voice: usize,
    pub start: i64,
    pub end: i64,
    pub freq: i32,
    /// Mono average of the two register nibbles, 0..15.
    pub volume: i32,
    /// Raw register byte, for diagnostics.
    pub pan: i32,
    pub data: Vec<u8>,
    pub clock: u32,
    pub off_sample: i64,
    pub freq_pts: Vec<(i64, i32)>,
    pub vol_pts: Vec<(i64, i32)>,
    /// Live left/right volume nibbles, (sample, left, right).
    pub lr_pts: Vec<(i64, i32, i32)>,
}

impl Default for X1PcmHit {
    fn default() -> Self {
        Self {
            sample_time: 0,
            chip_id: 0,
            voice: 0,
            start: 0,
            end: 0,
            freq: 0,
            volume: 0,
            pan: 0,
            data: Vec::new(),
            clock: 0,
            off_sample: 0,
            freq_pts: Vec::new(),
            vol_pts: Vec::new(),
            lr_pts: Vec::new(),
        }
    }
}

impl X1PcmHit {
    pub fn rate(&self) -> i32 {
        x1_pcm_rate(self.clock, self.freq)
    }

    /// Source sample where the voice keys itself off at the ROM end.
    ///
    /// The X1-010 clears status bit 0 when PCM playback reaches the end
    /// address. A later key-on write is then a real 0 -> 1 edge even when the
    /// register byte still reads 1, which is how drivers retrigger a sample
    /// without writing an explicit off first.
    pub fn natural_end(&self) -> i64 {
        let frames = self.data.len() as i64;
        self.sample_time
            + py_int(py_round(
                frames as f64 * VGM_RATE as f64 / self.rate() as f64,
            ))
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct X1Off {
    pub sample_time: i64,
    pub chip_id: i32,
    pub voice: usize,
}

fn blank_channels() -> Vec<[u8; 8]> {
    vec![[0u8; 8]; X1_CHANNELS]
}

/// Envelope byte as the mono average of its left/right nibbles.
fn level_of_byte(byte: u8) -> i32 {
    (((byte >> 4) & 0x0F) as i32 + (byte & 0x0F) as i32 + 1) / 2
}

/// Merge register steps written within `gap` samples (MSB/LSB pairs).
///
/// Same rule as the other frequency timelines: only the last write of each
/// cluster is kept.
pub fn merge_step_writes(pts: &[(i64, i32)], gap: i64) -> Vec<(i64, i32)> {
    let mut merged: Vec<(i64, i32)> = Vec::new();
    for &p in pts {
        if let Some(last) = merged.last_mut() {
            if p.0 - last.0 <= gap {
                *last = p;
                continue;
            }
        }
        merged.push(p);
    }
    merged
}

/// Note plus fraction a frequency register value stands for.
pub fn x1_semitones(clock: u32, value: i32) -> f64 {
    frequency_pitch_table(clock as f64, X1_FREQ_DIV as f64, X1_MAX_FREQ)
        .note_fraction(i32::max(1, value))
}

/// Split an X1 frequency timeline into notes (frequency mode, not period).
///
/// Returns `(start_sample, register_value, intra_note_steps)`.
pub fn segment_x1_freq(
    points: &[(i64, i32)],
    end: i64,
    clock: u32,
) -> Vec<(i64, i32, Vec<(i64, i32)>)> {
    if points.is_empty() {
        return Vec::new();
    }
    let merged = merge_step_writes(points, X1_MERGE_GAP);
    let (mut cur_t, mut cur_v) = merged[0];
    let mut cur_s = x1_semitones(clock, cur_v);
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
        let new_s = x1_semitones(clock, value);
        let step = (new_s - cur_s).abs();
        if step >= X1_NOTE_STEP_SEMITONES && held >= X1_NOTE_HOLD {
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

enum Active {
    Wave(X1Hit),
    Pcm(X1PcmHit),
}

impl Active {
    fn sample_time(&self) -> i64 {
        match self {
            Active::Wave(hit) => hit.sample_time,
            Active::Pcm(hit) => hit.sample_time,
        }
    }

    fn set_off_sample(&mut self, t: i64) {
        match self {
            Active::Wave(hit) => hit.off_sample = t,
            Active::Pcm(hit) => hit.off_sample = t,
        }
    }
}

#[derive(Clone, Debug, Default)]
struct WarningsSeen {
    short: i64,
    range: i64,
    volume: i64,
    no_rom: i64,
    wave_live: i64,
    env_live: i64,
}

struct X1Collector {
    chips: Vec<i32>,
    clock: u32,
    total_samples: i64,
    regs: BTreeMap<i32, Vec<[u8; 8]>>,
    wave_ram: BTreeMap<i32, Vec<u8>>,
    env_ram: BTreeMap<i32, Vec<u8>>,
    roms: BTreeMap<i32, Vec<u8>>,
    active: BTreeMap<(i32, usize), Option<Active>>,
    hits: Vec<X1Hit>,
    pcm_hits: Vec<X1PcmHit>,
    offs: Vec<X1Off>,
    warnings_seen: WarningsSeen,
}

impl X1Collector {
    fn new(chips: Vec<i32>, clock: u32, total_samples: i64, roms: BTreeMap<i32, Vec<u8>>) -> Self {
        let mut regs = BTreeMap::new();
        let mut wave_ram = BTreeMap::new();
        let mut env_ram = BTreeMap::new();
        let mut active = BTreeMap::new();
        for &cid in &chips {
            regs.insert(cid, blank_channels());
            wave_ram.insert(cid, vec![0u8; 0x1000]);
            env_ram.insert(cid, vec![0u8; 0x1000]);
            for ch in 0..X1_CHANNELS {
                active.insert((cid, ch), None);
            }
        }
        Self {
            chips,
            clock,
            total_samples,
            regs,
            wave_ram,
            env_ram,
            roms,
            active,
            hits: Vec::new(),
            pcm_hits: Vec::new(),
            offs: Vec::new(),
            warnings_seen: WarningsSeen::default(),
        }
    }

    fn close(&mut self, cid: i32, ch: usize, t: i64, emit_off: bool) {
        let key = (cid, ch);
        let Some(slot) = self.active.get_mut(&key) else {
            return;
        };
        let Some(mut hit) = slot.take() else {
            return;
        };
        hit.set_off_sample(t);
        if hit.sample_time() >= t {
            self.warnings_seen.short += 1;
        } else {
            match hit {
                Active::Wave(hit) => self.hits.push(hit),
                Active::Pcm(hit) => self.pcm_hits.push(hit),
            }
            if emit_off && t < self.total_samples {
                self.offs.push(X1Off {
                    sample_time: t,
                    chip_id: cid,
                    voice: ch,
                });
            }
        }
    }

    fn open_hit(&mut self, cid: i32, ch: usize, t: i64, status: u8) {
        let key = (cid, ch);
        let reg = self.regs[&cid][ch];
        let mode_wave = status & 0x02 != 0;
        let div = if status & 0x80 != 0 { 1 } else { 0 };
        if mode_wave {
            let freq = (((reg[REG_FREQ_HI] as i32) << 8) | reg[REG_FREQ_LO] as i32) >> div;
            let wave = (reg[REG_VOL_WAVE] & 0x1F) as i32;
            let shape = (reg[REG_END_ENVSHAPE] & 0x1F) as i32;
            let start = wave as usize * X1_WAVE_BYTES;
            let env_start = shape as usize * X1_ENV_BYTES;
            let wave_data =
                self.wave_ram[&cid][start..start + X1_WAVE_BYTES].to_vec();
            let env_data =
                self.env_ram[&cid][env_start..env_start + X1_ENV_BYTES].to_vec();
            self.active.insert(
                key,
                Some(Active::Wave(X1Hit {
                    sample_time: t,
                    chip_id: cid,
                    voice: ch,
                    freq: i32::max(1, freq),
                    env_period: reg[REG_START_ENVPERIOD] as i32,
                    env_shape: shape,
                    env_oneshot: status & 0x04 != 0,
                    wave,
                    clock: self.clock,
                    wave_data,
                    env_data,
                    ..X1Hit::default()
                })),
            );
            return;
        }
        let start = reg[REG_START_ENVPERIOD] as i64 * 0x1000;
        let end = (0x100 - reg[REG_END_ENVSHAPE] as i64) * 0x1000;
        let rom = self.roms.get(&cid).map(Vec::as_slice).unwrap_or(&[]);
        if rom.is_empty() {
            self.warnings_seen.no_rom += 1;
            self.active.insert(key, None);
            return;
        }
        if start >= end || start >= rom.len() as i64 {
            self.warnings_seen.range += 1;
            self.active.insert(key, None);
            return;
        }
        let end = i64::min(end, rom.len() as i64);
        let body = rom[start as usize..end as usize].to_vec();
        let volume_byte = reg[REG_VOL_WAVE];
        let left = ((volume_byte >> 4) & 0x0F) as i32;
        let right = (volume_byte & 0x0F) as i32;
        if left != right {
            self.warnings_seen.volume += 1;
        }
        self.active.insert(
            key,
            Some(Active::Pcm(X1PcmHit {
                sample_time: t,
                chip_id: cid,
                voice: ch,
                start,
                end,
                freq: x1_pcm_freq((reg[REG_FREQ_LO] >> div) as i32),
                volume: (left + right + 1) / 2,
                pan: volume_byte as i32,
                data: body,
                clock: self.clock,
                ..X1PcmHit::default()
            })),
        );
    }

    fn scan(&mut self, writes: &[ChipWrite]) {
        for w in writes {
            if w.chip != "x1_010" {
                continue;
            }
            let cid = w.chip_id as i32;
            let t = w.sample;
            let addr = w.reg & 0x1FFF;
            let val = (w.val & 0xFF) as u8;
            if addr >= 0x1000 {
                let off = (addr & 0x0FFF) as usize;
                self.wave_ram.get_mut(&cid).unwrap()[off] = val;
                // A waveform rewrite while a note uses it is audible on
                // hardware. Keep the original snapshot for now and report the
                // limitation.
                let mut live = 0i64;
                for ch in 0..X1_CHANNELS {
                    if let Some(Some(Active::Wave(hit))) = self.active.get(&(cid, ch)) {
                        let start = hit.wave as usize * X1_WAVE_BYTES;
                        if start <= off && off < start + X1_WAVE_BYTES {
                            live += 1;
                        }
                    }
                }
                self.warnings_seen.wave_live += live;
                continue;
            }
            if addr >= 0x80 {
                let env_off = (addr & 0x0FFF) as usize;
                self.env_ram.get_mut(&cid).unwrap()[env_off] = val;
                let level = level_of_byte(val);
                let mut live = 0i64;
                for ch in 0..X1_CHANNELS {
                    let key = (cid, ch);
                    if let Some(Some(Active::Wave(hit))) = self.active.get_mut(&key) {
                        let start = hit.env_shape as usize * X1_ENV_BYTES;
                        if start <= env_off && env_off < start + X1_ENV_BYTES {
                            if env_off == start && hit.env_period == 0 {
                                let last = hit
                                    .env_level_pts
                                    .last()
                                    .map(|p| p.1)
                                    .unwrap_or_else(|| level_of_byte(hit.env_data[0]));
                                if level != last {
                                    hit.env_level_pts.push((t, level));
                                }
                                let left = ((val >> 4) & 0x0F) as i32;
                                let right = (val & 0x0F) as i32;
                                let last_lr = hit
                                    .env_lr_pts
                                    .last()
                                    .map(|p| (p.1, p.2))
                                    .unwrap_or_else(|| {
                                        (
                                            ((hit.env_data[0] >> 4) & 0x0F) as i32,
                                            (hit.env_data[0] & 0x0F) as i32,
                                        )
                                    });
                                if (left, right) != last_lr {
                                    hit.env_lr_pts.push((t, left, right));
                                }
                            } else {
                                live += 1;
                            }
                        }
                    }
                }
                self.warnings_seen.env_live += live;
                continue;
            }
            let ch = (addr >> 3) as usize;
            let reg = (addr & 7) as usize;
            let old_status = self.regs[&cid][ch][REG_STATUS];
            self.regs.get_mut(&cid).unwrap()[ch][reg] = val;
            if reg == REG_STATUS {
                let mut was_on = old_status & 0x01 != 0;
                let now_on = val & 0x01 != 0;
                let natural_end = match self.active.get(&(cid, ch)) {
                    Some(Some(Active::Pcm(hit))) if t >= hit.natural_end() => {
                        Some(hit.natural_end())
                    }
                    _ => None,
                };
                if let Some(end) = natural_end {
                    // PCM playback keys itself off at the ROM end address even
                    // though the status byte still reads 1. A later key-on
                    // write is therefore a real 0 -> 1 edge: drivers retrigger
                    // a sample without writing an explicit off first.
                    self.close(cid, ch, end, !now_on);
                    was_on = false;
                }
                if now_on && !was_on {
                    self.close(cid, ch, t, false);
                    self.open_hit(cid, ch, t, val);
                    continue;
                }
                if !now_on && was_on {
                    self.close(cid, ch, t, true);
                    continue;
                }
                if !now_on {
                    continue;
                }
                let div = if val & 0x80 != 0 { 1 } else { 0 };
                let freq = (((self.regs[&cid][ch][REG_FREQ_HI] as i32) << 8)
                    | self.regs[&cid][ch][REG_FREQ_LO] as i32)
                    >> div;
                if let Some(Some(Active::Wave(hit))) = self.active.get_mut(&(cid, ch)) {
                    let one_shot = val & 0x04 != 0;
                    if one_shot != hit.env_oneshot {
                        hit.env_oneshot = one_shot;
                        hit.env_mode_pts
                            .push((t, 1 | (if one_shot { 2 } else { 0 })));
                    }
                    if (val & 0x80 != 0) != (old_status & 0x80 != 0) {
                        // The divider changed mid-note. Recompute from the
                        // registers that are already in force.
                        let last = hit.freq_pts.last().map(|p| p.1).unwrap_or(hit.freq);
                        if freq != last {
                            hit.freq_pts.push((t, i32::max(1, freq)));
                        }
                    }
                }
                continue;
            }
            if self.active.get(&(cid, ch)).is_none_or(Option::is_none) {
                continue;
            }
            let is_wave = matches!(
                self.active.get(&(cid, ch)),
                Some(Some(Active::Wave(_)))
            );
            if is_wave {
                match reg {
                    REG_FREQ_LO | REG_FREQ_HI => {
                        let status = self.regs[&cid][ch][REG_STATUS];
                        let freq = (((self.regs[&cid][ch][REG_FREQ_HI] as i32) << 8)
                            | self.regs[&cid][ch][REG_FREQ_LO] as i32)
                            >> (if status & 0x80 != 0 { 1 } else { 0 });
                        if let Some(Some(Active::Wave(hit))) =
                            self.active.get_mut(&(cid, ch))
                        {
                            let last =
                                hit.freq_pts.last().map(|p| p.1).unwrap_or(hit.freq);
                            if freq != last {
                                hit.freq_pts.push((t, i32::max(1, freq)));
                            }
                        }
                    }
                    REG_VOL_WAVE => {
                        let wave = (val & 0x1F) as i32;
                        let start = wave as usize * X1_WAVE_BYTES;
                        let body = self.wave_ram[&cid][start..start + X1_WAVE_BYTES].to_vec();
                        if let Some(Some(Active::Wave(hit))) =
                            self.active.get_mut(&(cid, ch))
                        {
                            let changed = match hit.wave_pts.last() {
                                Some(p) => p.1 != body,
                                None => hit.wave_data != body,
                            };
                            if changed {
                                hit.wave = wave;
                                hit.wave_pts.push((t, body));
                            }
                        }
                    }
                    REG_END_ENVSHAPE => {
                        let shape = (val & 0x1F) as i32;
                        let start = shape as usize * X1_ENV_BYTES;
                        let body = self.env_ram[&cid][start..start + X1_ENV_BYTES].to_vec();
                        if let Some(Some(Active::Wave(hit))) =
                            self.active.get_mut(&(cid, ch))
                        {
                            let changed = match hit.env_pts.last() {
                                Some(p) => p.1 != body,
                                None => hit.env_data != body,
                            };
                            if changed {
                                hit.env_shape = shape;
                                hit.env_pts.push((t, body.clone()));
                                if hit.env_period == 0 && !body.is_empty() {
                                    let left = ((body[0] >> 4) & 0x0F) as i32;
                                    let right = (body[0] & 0x0F) as i32;
                                    let last_lr = hit
                                        .env_lr_pts
                                        .last()
                                        .map(|p| (p.1, p.2))
                                        .unwrap_or_else(|| {
                                            (
                                                ((hit.env_data[0] >> 4) & 0x0F) as i32,
                                                (hit.env_data[0] & 0x0F) as i32,
                                            )
                                        });
                                    if (left, right) != last_lr {
                                        hit.env_lr_pts.push((t, left, right));
                                    }
                                }
                            }
                        }
                    }
                    REG_START_ENVPERIOD => {
                        if let Some(Some(Active::Wave(hit))) =
                            self.active.get_mut(&(cid, ch))
                        {
                            let last = hit
                                .env_period_pts
                                .last()
                                .map(|p| p.1)
                                .unwrap_or(hit.env_period);
                            if val as i32 != last {
                                hit.env_period_pts.push((t, val as i32));
                            }
                        }
                    }
                    _ => {}
                }
            } else {
                let mut volume_warn = false;
                let mut range_warn = false;
                if let Some(Some(Active::Pcm(hit))) = self.active.get_mut(&(cid, ch)) {
                    match reg {
                        REG_FREQ_LO => {
                            let freq = x1_pcm_freq(val as i32);
                            let last = hit.freq_pts.last().map(|p| p.1).unwrap_or(hit.freq);
                            if freq != last {
                                hit.freq_pts.push((t, freq));
                            }
                        }
                        REG_VOL_WAVE => {
                            let left = ((val >> 4) & 0x0F) as i32;
                            let right = (val & 0x0F) as i32;
                            if left != right {
                                volume_warn = true;
                            }
                            let volume = (left + right + 1) / 2;
                            let last =
                                hit.vol_pts.last().map(|p| p.1).unwrap_or(hit.volume);
                            if volume != last {
                                hit.vol_pts.push((t, volume));
                            }
                            let last_lr = hit
                                .lr_pts
                                .last()
                                .map(|p| (p.1, p.2))
                                .unwrap_or((((hit.pan >> 4) & 0x0F), (hit.pan & 0x0F)));
                            if (left, right) != last_lr {
                                hit.lr_pts.push((t, left, right));
                            }
                        }
                        REG_START_ENVPERIOD | REG_END_ENVSHAPE => {
                            range_warn = true;
                        }
                        _ => {}
                    }
                }
                if volume_warn {
                    self.warnings_seen.volume += 1;
                }
                if range_warn {
                    self.warnings_seen.range += 1;
                }
            }
        }
    }
}

/// Walk an X1-010 register stream.
///
/// Returns `(wave_hits, pcm_hits, offs, warnings)`. Wave/envelope contents are
/// snapshotted when a note starts; the handful of unique bodies is deduplicated
/// by the analyzer. PCM bodies are the exact source address range, so long
/// phrases are never shortened by silence trimming.
pub fn collect_x1_010(
    vgm: &VgmFile,
) -> (Vec<X1Hit>, Vec<X1PcmHit>, Vec<X1Off>, Vec<String>) {
    let mut warnings: Vec<String> = Vec::new();
    let chips: Vec<i32> = vgm
        .writes
        .iter()
        .filter(|w| w.chip == "x1_010")
        .map(|w| w.chip_id as i32)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    if chips.is_empty() {
        return (Vec::new(), Vec::new(), Vec::new(), warnings);
    }
    let clock = if vgm.x1_010_clock != 0 {
        vgm.x1_010_clock
    } else {
        X1_010_DEFAULT_CLOCK
    };
    let roms: BTreeMap<i32, Vec<u8>> = chips
        .iter()
        .map(|&cid| (cid, assemble_rom(vgm, "x1_010", cid as u8, 0, false, false)))
        .collect();
    let mut c = X1Collector::new(chips, clock, vgm.total_samples, roms);
    c.scan(&vgm.writes);
    for &cid in &c.chips.clone() {
        for ch in 0..X1_CHANNELS {
            c.close(cid, ch, c.total_samples, false);
        }
    }
    if c.warnings_seen.no_rom != 0 {
        warnings.push(
            "X1-010 PCM note(s) have no 0x91 sample ROM block; their bodies are missing"
                .to_string(),
        );
    }
    if c.warnings_seen.short != 0 {
        warnings.push(format!(
            "{} zero-length X1-010 gate(s) skipped",
            c.warnings_seen.short
        ));
    }
    if c.warnings_seen.range != 0 {
        warnings.push(format!(
            "{} X1-010 PCM write(s) changed the address range while a note played; the initial range is used",
            c.warnings_seen.range
        ));
    }
    if c.warnings_seen.volume != 0 {
        warnings.push(format!(
            "{} X1-010 PCM volume write(s) have different left/right nibbles; they are kept as stereo pan",
            c.warnings_seen.volume
        ));
    }
    if c.warnings_seen.wave_live != 0 {
        warnings.push(format!(
            "{} X1-010 waveform RAM write(s) changed a waveform while it was playing; the note-start snapshot is used",
            c.warnings_seen.wave_live
        ));
    }
    if c.warnings_seen.env_live != 0 {
        warnings.push(format!(
            "{} X1-010 envelope RAM write(s) changed a shape while it was playing; the note-start snapshot is used",
            c.warnings_seen.env_live
        ));
    }
    (c.hits, c.pcm_hits, c.offs, warnings)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vgm::RomSlice;

    fn write(sample: i64, reg: u32, value: u32) -> ChipWrite {
        ChipWrite::new(sample, "x1_010".into(), 0, reg, value & 0xFF)
    }

    fn vgm(writes: Vec<ChipWrite>, total: i64, clock: u32, rom: &[u8]) -> VgmFile {
        let roms = if rom.is_empty() {
            Vec::new()
        } else {
            vec![RomSlice {
                chip: 0,
                rom_size: i64::max(0x100000, rom.len() as i64),
                start: 0,
                data: rom.to_vec(),
                kind: "x1_010",
                time: 0,
            }]
        };
        VgmFile {
            writes,
            total_samples: total,
            x1_010_clock: clock,
            roms,
            ..Default::default()
        }
    }

    fn wave_setup() -> Vec<ChipWrite> {
        let mut writes: Vec<ChipWrite> = Vec::new();
        // Envelope shape 1 lives at 0x80 on the chip.
        for i in 0..128 {
            writes.push(write(0, 0x80 + i, 0xFF));
        }
        for i in 0..128 {
            writes.push(write(0, 0x1000 + i, (i * 2) & 0xFF));
        }
        for (reg, value) in [
            (0, 0),
            (1, 0),
            (2, 1845 & 0xFF),
            (3, (1845 >> 8) & 0xFF),
            (4, 10),
            (5, 1),
        ] {
            writes.push(write(0, reg, value));
        }
        writes.push(write(0, 0, 3)); // key on, wavetable mode
        writes.push(write(9000, 0, 0));
        writes
    }

    #[test]
    fn helpers_match_the_reference() {
        assert_eq!(s8(0x80), -128);
        assert_eq!(s8(0xFF), -1);
        assert_eq!(s8(0x7F), 127);
        assert_eq!(x1_wave_reg(0), 1);
        assert_eq!(x1_wave_reg(1845), 1845);
        assert_eq!(x1_pcm_freq(0), 4);
        assert_eq!(x1_pcm_freq(16), 16);
        assert_eq!(x1_pcm_rate(16_000_000, 16), 31_250);
        assert_eq!(x1_pcm_rate(0, 16), 1);
    }

    #[test]
    fn wave_note_snapshots_wave_and_envelope_ram() {
        let (hits, pcm, offs, warnings) =
            collect_x1_010(&vgm(wave_setup(), 44_100, 16_000_000, &[]));
        assert_eq!(hits.len(), 1);
        assert!(pcm.is_empty());
        assert_eq!(warnings, Vec::<String>::new());
        assert_eq!(hits[0].wave, 0);
        assert_eq!(hits[0].env_shape, 1);
        assert_eq!(hits[0].freq, 1845);
        assert_eq!(hits[0].wave_data.len(), 128);
        assert_eq!(hits[0].env_data.len(), 128);
        assert_eq!(hits[0].env_data[0], 0xFF);
        assert_eq!(hits[0].wave_data[1], 2);
        assert_eq!(offs.len(), 1);
        assert_eq!(offs[0].sample_time, 9000);
        assert_eq!(offs[0].voice, 0);
    }

    #[test]
    fn pcm_note_body_and_rate() {
        let body: Vec<u8> = (0..0x2000u32).map(|i| (i * 7) as u8).collect();
        let writes = vec![
            write(0, 1, 0xEE),
            write(0, 2, 16),
            write(0, 4, 0),
            write(0, 5, 0xFE),
            write(0, 0, 1),
            write(8000, 0, 0),
        ];
        let (hits, pcm, offs, warnings) =
            collect_x1_010(&vgm(writes, 9000, 16_000_000, &body));
        assert!(hits.is_empty());
        assert_eq!(pcm.len(), 1);
        assert_eq!(pcm[0].start, 0);
        assert_eq!(pcm[0].end, 0x2000);
        assert_eq!(pcm[0].volume, 14);
        assert_eq!(pcm[0].rate(), x1_pcm_rate(16_000_000, 16));
        assert_eq!(pcm[0].data.len(), 0x2000);
        assert_eq!(offs.len(), 1);
        assert_eq!(warnings, Vec::<String>::new());
    }

    #[test]
    fn pcm_key_on_after_natural_end_is_a_new_note() {
        let body: Vec<u8> = (0..0x1000u32).map(|i| (i * 7) as u8).collect();
        let writes = vec![
            write(0, 1, 0xEE),
            write(0, 2, 16),
            write(0, 4, 0),
            write(0, 5, 0xFF),
            write(0, 0, 1),
            write(10_000, 0, 1), // after the first natural end
            write(10_001, 0, 1), // duplicate start burst, same note
            write(20_000, 0, 0),
        ];
        let (hits, pcm, _offs, _warnings) =
            collect_x1_010(&vgm(writes, 21_000, 16_000_000, &body));
        assert!(hits.is_empty());
        assert_eq!(pcm.len(), 2);
        assert_eq!(pcm[0].sample_time, 0);
        assert_eq!(pcm[1].sample_time, 10_000);
        assert!(pcm[0].natural_end() < pcm[1].sample_time);
        assert_eq!(pcm[1].natural_end(), pcm[1].sample_time + 5_780);
    }

    #[test]
    fn frequency_timeline_segments_and_merges() {
        let s = x1_semitones(16_000_000, 1845);
        assert!((s - 104.995_367_480_967_27).abs() < 1e-9);
        assert_eq!(
            segment_x1_freq(&[], 6000, 16_000_000),
            Vec::<(i64, i32, Vec<(i64, i32)>)>::new()
        );
        // A whole-octave jump held for two frames starts a new note.
        let segs = segment_x1_freq(&[(0, 1000), (3000, 2000)], 6000, 16_000_000);
        assert_eq!(segs.len(), 2);
        assert_eq!((segs[0].0, segs[0].1), (0, 1000));
        assert_eq!((segs[1].0, segs[1].1), (3000, 2000));
        // A small step is an intra-note pitch step.
        let segs = segment_x1_freq(&[(0, 1000), (20, 1010)], 6000, 16_000_000);
        assert_eq!(segs.len(), 1);
        assert_eq!(segs[0].2, vec![(20, 1010)]);
    }
}
