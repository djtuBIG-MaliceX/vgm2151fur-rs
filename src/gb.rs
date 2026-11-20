//! Game Boy DMG (LR35902 APU) register stream -> notes and wavetables.
//!
//! The DMG has four voices: two pulse channels (channel 1 also has a hardware
//! frequency sweep), one 32-nibble wavetable voice, and a noise channel.
//! Unlike the NES, every voice has its own trigger bit (NRx4/NR44 bit 7), so
//! a note starts on that write. The envelope lives in NRx2 for the pulse/noise
//! voices, while the wave voice's level is NR32.
//!
//! Register map (VGM command `0xB3`, register `0x00` = `FF10`/NR10):
//!
//! ```text
//! 0x00    NR10 sweep (ch1)        0x01-0x04 NR11-14 pulse 1
//! 0x06-0x09 NR21-24 pulse 2       0x0A-0x0E NR30-34 wave
//! 0x10-0x13 NR41-44 noise         0x14 NR50 master volume
//! 0x15    NR51 stereo output      0x16 NR52 sound on/off
//! 0x20-0x2F wave RAM (32 nibbles)
//! ```
//!
//! Two envelope encodings appear in real logs. A driver that writes NRx2 once
//! at the note start is using the *hardware* envelope (initial volume,
//! direction and period); that maps straight onto Furnace's GB envelope
//! fields. A driver that keeps rewriting NRx2 during the note is using the
//! hardware's "zombie" volume bug (Furnace's software envelope): the writes
//! are replayed through the same state machine the emulator uses, and the
//! resulting volume trajectory becomes a volume macro.

use std::collections::{BTreeSet, HashMap};
use std::sync::LazyLock;

use indexmap::IndexMap;

use crate::parity::{py_int, py_round};
use crate::pitch::furnace_period_note;
use crate::vgm::{ChipWrite, VgmFile};

pub const GB_CHANNELS: usize = 4;
pub const GB_DEFAULT_CLOCK: u32 = 4_194_304;
/// Furnace's GB pitch table: clock / 16, an 11-bit period register, and the
/// platform writes `2048 - freq` back to NRx3/NRx4.
pub const GB_PITCH_DIVIDER: u32 = 16;
pub const GB_MAX_PERIOD: i32 = 0x7FF;
/// Shorter than this is a click/housekeeping gate, not a note.
pub const GB_MIN_NOTE: i64 = 100;
/// A pitch write this soon after the trigger is the note's real base, not a
/// bend (drivers often set the frequency just after starting the voice).
pub const GB_REFIT_SAMPLES: i64 = 735;
/// An envelope or NR51 write this close to the trigger belongs to that note
/// (drivers write them at or a sample or two after the trigger). The window
/// is deliberately tiny: a frame-sized one would swallow the next note's
/// setup, which lands ~700 samples later in fast percussion.
pub const GB_ENV_REFIT: i64 = 16;
/// Two mid-note NRx2 writes mean the driver is doing a software volume ramp.
pub const GB_SOFT_ENV_WRITES: usize = 2;
/// A held pitch jump of at least this many 1/64 semitones (0.75 st) is a
/// legato note change, not a slide. The pattern gets a note cell there.
pub const GB_LEGATO_UNITS: i32 = 48;
/// A whole-wave RAM rewrite is a burst of byte writes; writes this close
/// belong to the same morph step.
pub const GB_WAVE_BURST: i64 = 64;

pub const REG_SWEEP: u32 = 0x00;
pub const REG_P1_DUTY: u32 = 0x01;
pub const REG_P1_ENV: u32 = 0x02;
pub const REG_P1_LO: u32 = 0x03;
pub const REG_P1_HI: u32 = 0x04;
pub const REG_P2_DUTY: u32 = 0x06;
pub const REG_P2_ENV: u32 = 0x07;
pub const REG_P2_LO: u32 = 0x08;
pub const REG_P2_HI: u32 = 0x09;
pub const REG_W_ON: u32 = 0x0A;
pub const REG_W_LEN: u32 = 0x0B;
pub const REG_W_VOL: u32 = 0x0C;
pub const REG_W_LO: u32 = 0x0D;
pub const REG_W_HI: u32 = 0x0E;
pub const REG_N_LEN: u32 = 0x10;
pub const REG_N_ENV: u32 = 0x11;
pub const REG_N_FREQ: u32 = 0x12;
pub const REG_N_CTL: u32 = 0x13;
pub const REG_VOL: u32 = 0x14;
pub const REG_PAN: u32 = 0x15;
pub const REG_ON: u32 = 0x16;
pub const REG_WAVE: u32 = 0x20;

/// Furnace's noiseTable (gb.cpp). The index is note-60; the value is the NR43
/// register it writes. Only dividers 4-7 and the shift-0 row are present,
/// which is what the platform can express.
static _NOISE_TABLE: LazyLock<Vec<i32>> = LazyLock::new(|| {
    let mut table: Vec<i32> = vec![0];
    for shift in (0..16).rev() {
        for div in (4..8).rev() {
            table.push((shift << 4) | div);
        }
    }
    table.extend([0x03, 0x02, 0x01, 0x00]);
    table
});

/// 11-bit NRx3/NRx4 value -> (Furnace note, E5).
///
/// Furnace writes `2048 - table_value` to the register, so the table must
/// produce `2048 - period`.
pub fn gb_note(clock: u32, period: i32) -> (i32, i32) {
    let period = period.clamp(0, GB_MAX_PERIOD);
    let clock = if clock == 0 {
        GB_DEFAULT_CLOCK
    } else {
        clock
    };
    furnace_period_note(
        clock as f64,
        GB_PITCH_DIVIDER as f64,
        2048 - period,
        0,
        GB_MAX_PERIOD,
    )
}

/// 1/64-semitone change between two NRx3/NRx4 period values.
pub fn gb_period_delta(base: i32, new: i32) -> i32 {
    let pa = 2048 - base.clamp(0, GB_MAX_PERIOD);
    let pb = 2048 - new.clamp(0, GB_MAX_PERIOD);
    if pa <= 0 || pb <= 0 {
        return 0;
    }
    py_int(py_round(64.0 * 12.0 * (pa as f64 / pb as f64).log2())) as i32
}

/// Furnace's noise column for an NR43 value.
///
/// The platform's noise table is a lookup indexed by `note - 60`; the note
/// name is not a 12-TET pitch. Each chromatic step moves the hardware rate by
/// two to four semitones (and the last few steps by up to an octave), so the
/// converter selects the table entry whose period matches NR43 instead of
/// transposing by note.
///
/// The table also starts with a duplicate of the fastest rate (period 0.5) at
/// C-0, and only covers shift 0-15 with dividers 4-7 (plus the shift-0 row).
/// Furnace renders the entries faster than period 8 as a near-DC rumble, not
/// noise, so the top of the range is folded onto C-5 (period 8), the fastest
/// rate that still reads as noise. This is what makes the noise note appear
/// at C-5 rather than C-0. Slower values keep their relative pitch below it.
pub fn gb_noise_note(nr43: i32) -> i32 {
    let table = &*_NOISE_TABLE;
    let value = nr43 & 0xF7;
    // Skip the leading duplicate so NR43=0 uses the real end-of-table entry
    // rather than the C-0 alias.
    let found = table
        .iter()
        .enumerate()
        .skip(1)
        .find(|&(_, &v)| v == value)
        .map(|(i, _)| i);
    let mut pos = match found {
        Some(pos) => pos,
        None => {
            let want = _noise_period(value);
            let mut best: Option<(f64, usize)> = None;
            for (i, &entry) in table.iter().enumerate() {
                let err = (f64::max(1e-9, _noise_period(entry)).log2()
                    - f64::max(1e-9, want).log2())
                .abs();
                if best.is_none_or(|(b, _)| err < b) {
                    best = Some((err, i));
                }
            }
            best.expect("noise table is never empty").1
        }
    };
    if pos == 0 {
        pos = table.len() - 1;
    }
    60 + i32::min(pos as i32, 60)
}

fn _noise_period(nr43: i32) -> f64 {
    let shift = (nr43 >> 4) & 0x0F;
    let div = nr43 & 7;
    let base = if div != 0 { div as f64 } else { 0.5 };
    base * (1i32 << shift) as f64
}

/// NR32 output level -> Furnace 0-15 column volume.
pub fn wave_volume(nr32: i32) -> i32 {
    [0, 15, 10, 5][((nr32 >> 5) & 3) as usize]
}

/// 16 wave-RAM bytes -> the 32 nibbles Furnace stores, sample order.
pub fn wave_nibbles(image: &[u8]) -> Vec<u8> {
    let mut out = vec![0u8; 32];
    for i in 0..16 {
        out[i * 2] = (image[i] >> 4) & 0x0F;
        out[i * 2 + 1] = image[i] & 0x0F;
    }
    out
}

#[derive(Clone, Debug, PartialEq)]
pub struct GbHit {
    pub sample_time: i64,
    /// 0/1 pulse, 2 wave, 3 noise
    pub voice: usize,
    /// NRx3/NRx4 value, or NR43 for noise
    pub period: i32,
    /// 0-15 at note start
    pub vol: i32,
    /// pulse duty 0-3; noise width 0/1
    pub duty: i32,
    /// wavetable index for voice 2
    pub wave: usize,
    pub env_vol: i32,
    pub env_dir: i32,
    pub env_len: i32,
    /// Furnace field (63 = length disabled)
    pub sound_len: i32,
    pub soft_env: bool,
    pub env_raw: i32,
    pub off_sample: i64,
    pub pitch_pts: Vec<(i64, i32)>,
    pub vol_pts: Vec<(i64, i32)>,
    pub wave_pts: Vec<(i64, usize)>,
    pub duty_pts: Vec<(i64, i32)>,
    pub pan: i32,
    pub pan_pts: Vec<(i64, i32)>,
    pub sweep: i32,
    pub sweep_pts: Vec<(i64, i32)>,
    /// A pitch-only split: the pattern gets a note cell with EAxx legato so
    /// the hardware envelope is not restarted.
    pub legato: bool,
}

impl Default for GbHit {
    fn default() -> Self {
        GbHit {
            sample_time: 0,
            voice: 0,
            period: 0,
            vol: 0,
            duty: 0,
            wave: 0,
            env_vol: 0,
            env_dir: 0,
            env_len: 0,
            sound_len: 63,
            soft_env: false,
            env_raw: 0,
            off_sample: 0,
            pitch_pts: Vec::new(),
            vol_pts: Vec::new(),
            wave_pts: Vec::new(),
            duty_pts: Vec::new(),
            pan: 0xFF,
            pan_pts: Vec::new(),
            sweep: 0,
            sweep_pts: Vec::new(),
            legato: false,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct GbOff {
    pub sample_time: i64,
    pub voice: usize,
}

/// The emulator's NRx2 "zombie" volume state.
#[derive(Clone, Debug, PartialEq)]
struct _EnvState {
    #[allow(dead_code)]
    pub volume: i32,
    pub countdown: i32,
    pub clock: bool,
    pub locked: bool,
}

impl Default for _EnvState {
    fn default() -> Self {
        _EnvState {
            volume: 0,
            countdown: 0,
            clock: false,
            locked: false,
        }
    }
}

/// One NRx2 write, using the CGB-E (single-pass) rule.
///
/// The converter tags the chip as a CGB-E so Furnace's own emulator uses the
/// same rule. The full source lives in `sound/gb/apu.c`.
fn _nrx2_step(vol: i32, value: i32, old_value: i32, st: &mut _EnvState) -> i32 {
    let mut vol = vol;
    if st.clock {
        st.countdown = value & 7;
    }
    let mut should_tick = (value & 7) != 0 && (old_value & 7) == 0 && !st.locked;
    let should_invert = ((value & 8) != 0) ^ ((old_value & 8) != 0);
    if (value & 0xF) == 8 && (old_value & 0xF) == 8 && !st.locked {
        should_tick = true;
    }
    if should_invert {
        if (value & 8) != 0 {
            if (old_value & 7) == 0 && !st.locked {
                vol ^= 0xF;
            } else {
                vol = (0x0E - vol) & 0xF;
            }
            should_tick = false;
        } else {
            vol = (0x10 - vol) & 0xF;
        }
    }
    if should_tick {
        vol = (vol + if (value & 8) != 0 { 1 } else { -1 }) & 0xF;
    } else if (value & 7) == 0 && st.clock {
        if !should_invert {
            if vol == 0xF && (value & 8) != 0 {
                st.locked = true;
            } else if vol == 0 && (value & 8) == 0 {
                st.locked = true;
            }
        } else if vol == 1 && (value & 8) == 0 {
            st.locked = true;
        } else if vol == 0x0E && (value & 8) != 0 {
            st.locked = true;
        }
        st.clock = false;
    }
    vol
}

/// Replay mid-note NRx2 writes into a coalesced volume timeline.
///
/// `writes` is [(sample, value)] for one voice. `env_raw` is the NRx2 value in
/// force at the trigger. Returns the (sample, volume) points strictly inside
/// (start, end) and the write count.
fn _softenv_volumes(
    writes: &[(i64, i32)],
    start: i64,
    end: i64,
    env_vol: i32,
    env_raw: i32,
) -> (Vec<(i64, i32)>, usize) {
    let mut st = _EnvState {
        volume: env_vol,
        countdown: env_vol & 7,
        .._EnvState::default()
    };
    let mut vol = env_vol;
    let mut old = env_raw;
    let mut points: Vec<(i64, i32)> = Vec::new();
    let mut count = 0usize;
    for &(t, value) in writes {
        if !(start < t && t < end) {
            continue;
        }
        vol = _nrx2_step(vol, value, old, &mut st);
        old = value;
        count += 1;
        if points.last().map(|p| p.0) == Some(t) {
            *points.last_mut().unwrap() = (t, vol);
        } else {
            points.push((t, vol));
        }
    }
    (points, count)
}

fn is_trigger(w: &ChipWrite) -> bool {
    let reg = w.reg & 0x7F;
    (w.val & 0x80) != 0
        && (reg == REG_P1_HI || reg == REG_P2_HI || reg == REG_W_HI || reg == REG_N_CTL)
}

struct GbState {
    duty: [i32; 2],
    length_val: [i32; 4],
    length_en: [bool; 4],
    env: [i32; 4],
    period: [i32; 3],
    noise_period: i32,
    noise_width: i32,
    wave_on: bool,
    wave_started: bool,
    wave_vol: i32,
    sweep: i32,
    pan: [i32; 4],
    enabled: bool,
    active: [Option<GbHit>; 4],
    env_writes: [Vec<(i64, i32)>; 4],
    hits: Vec<GbHit>,
    offs: Vec<GbOff>,
    glitches: i64,
    prev_period: [i32; 3],
    wave_image: [u8; 16],
    wave_index_by_key: HashMap<Vec<u8>, usize>,
    waves: Vec<Vec<u8>>,
}

impl GbState {
    fn new() -> Self {
        GbState {
            duty: [0, 0],
            length_val: [0; 4],
            length_en: [false; 4],
            env: [0; 4],
            period: [0; 3],
            noise_period: 0,
            noise_width: 0,
            wave_on: false,
            wave_started: false,
            wave_vol: 0,
            sweep: 0,
            pan: [0xFF; 4],
            enabled: true,
            active: [None, None, None, None],
            env_writes: [Vec::new(), Vec::new(), Vec::new(), Vec::new()],
            hits: Vec::new(),
            offs: Vec::new(),
            glitches: 0,
            prev_period: [0; 3],
            wave_image: [0; 16],
            wave_index_by_key: HashMap::new(),
            waves: Vec::new(),
        }
    }

    fn wave_now(&mut self, t: i64, record: bool) -> usize {
        let nib = wave_nibbles(&self.wave_image);
        let idx = match self.wave_index_by_key.get(&nib) {
            Some(&idx) => idx,
            None => {
                let idx = self.waves.len();
                self.wave_index_by_key.insert(nib.clone(), idx);
                self.waves.push(nib);
                idx
            }
        };
        if record {
            let live = self.active[2]
                .as_ref()
                .is_some_and(|hit| t > hit.sample_time);
            if live {
                let (replace, push) = {
                    let hit = self.active[2].as_ref().unwrap();
                    match hit.wave_pts.last() {
                        Some(last) => {
                            if t - last.0 <= GB_WAVE_BURST {
                                (true, false)
                            } else {
                                (false, last.1 != idx)
                            }
                        }
                        None => (false, true),
                    }
                };
                if replace {
                    // A whole-image rewrite is a burst of byte writes; keep
                    // the first time and the settled image.
                    let hit = self.active[2].as_mut().unwrap();
                    let last_time = hit.wave_pts.last().unwrap().0;
                    *hit.wave_pts.last_mut().unwrap() = (last_time, idx);
                } else if push {
                    self.active[2].as_mut().unwrap().wave_pts.push((t, idx));
                }
            }
        }
        idx
    }

    fn dac_on(&self, voice: usize) -> bool {
        if voice == 2 {
            return self.wave_on;
        }
        (self.env[voice] & 0xF8) != 0
    }

    /// Whether a trigger right now would produce sound.
    ///
    /// Drivers stop a pulse/noise voice by writing NRx2 = 0x08 (DAC on,
    /// volume 0) and re-triggering, or by clearing the DAC. Such a "trigger"
    /// is a note-off, not a new note: the previous note must get an explicit
    /// off. A hardware attack from 0 (direction up, period > 0) does rise, so
    /// it stays audible.
    fn voice_audible(&self, voice: usize) -> bool {
        if voice == 2 {
            // A wave trigger with NR32 = 0 is muted; the note starts when the
            // output level rises (see the NR32 branch). A trigger while the
            // level is up starts a normal note.
            return self.wave_on && wave_volume(self.wave_vol) > 0;
        }
        let e = self.env[voice];
        if (e & 0xF8) == 0 {
            return false;
        }
        (e >> 4) > 0 || ((e & 8) != 0 && (e & 7) != 0)
    }

    fn final_sound_len(&self, voice: usize) -> i32 {
        // Pulses disable length counting at `soundLen == 63`; the noise
        // platform tests `soundLen < 64`, so 64 is its "disabled" value.
        // Without that, every noise note is cut after 64 length ticks even
        // when the source never enabled the counter.
        if self.length_en[voice] && self.length_val[voice] > 0 {
            return 63 - self.length_val[voice];
        }
        if voice == 3 { 64 } else { 63 }
    }

    fn close(&mut self, voice: usize, t: i64, emit_off: bool) {
        let Some(mut hit) = self.active[voice].take() else {
            return;
        };
        hit.off_sample = t;
        let env_end = if t == 0 { 10i64.pow(12) } else { t };
        let (pts, count) = _softenv_volumes(
            &self.env_writes[voice],
            hit.sample_time,
            env_end,
            hit.env_vol,
            hit.env_raw,
        );
        // Only a note whose NRx2 period is zero is using the zombie volume
        // bug. A note with a hardware envelope (period > 0) decays on its
        // own; mid-note decay-rate changes are handled by splitting the note
        // where they happen (see the NRx2 branch).
        if count >= GB_SOFT_ENV_WRITES && hit.env_len == 0 {
            hit.soft_env = true;
            hit.vol_pts = pts;
        }
        self.env_writes[voice].clear();
        // A pulse/noise note with volume 0 and no growth is silent. A hardware
        // attack from 0 (direction up, period > 0) does sound, so keep it.
        let mut silent = voice != 2
            && hit.vol == 0
            && !hit.soft_env
            && !(hit.env_dir != 0 && hit.env_len != 0);
        if voice == 2 && hit.vol == 0 && hit.vol_pts.is_empty() {
            silent = true;
        }
        if t <= 0 {
            if !silent {
                self.hits.push(hit);
            }
        } else if t - hit.sample_time >= GB_MIN_NOTE {
            if !silent {
                self.hits.push(hit);
                if emit_off {
                    self.offs.push(GbOff {
                        sample_time: t,
                        voice,
                    });
                }
            }
        } else {
            self.glitches += 1;
        }
    }

    fn open_hit(&mut self, voice: usize, t: i64, vol_override: Option<i32>) {
        self.env_writes[voice].clear();
        if voice == 2 {
            if !self.wave_on {
                return;
            }
            let index = self.wave_now(t, false);
            self.active[voice] = Some(GbHit {
                sample_time: t,
                voice: 2,
                period: self.period[2],
                vol: wave_volume(self.wave_vol),
                duty: 0,
                wave: index,
                sound_len: 63,
                pan: self.pan[2],
                ..GbHit::default()
            });
            return;
        }
        if !self.dac_on(voice) && vol_override.is_none() {
            return;
        }
        let start_vol = match vol_override {
            Some(v) => v,
            None => self.env[voice] >> 4,
        };
        if voice == 3 {
            self.active[voice] = Some(GbHit {
                sample_time: t,
                voice: 3,
                period: self.noise_period,
                vol: start_vol,
                duty: self.noise_width,
                env_vol: start_vol,
                env_dir: (self.env[3] >> 3) & 1,
                env_len: self.env[3] & 7,
                sound_len: self.final_sound_len(3),
                pan: self.pan[3],
                env_raw: self.env[3],
                ..GbHit::default()
            });
        } else {
            self.active[voice] = Some(GbHit {
                sample_time: t,
                voice,
                period: self.period[voice],
                vol: start_vol,
                duty: self.duty[voice],
                env_vol: start_vol,
                env_dir: (self.env[voice] >> 3) & 1,
                env_len: self.env[voice] & 7,
                sound_len: self.final_sound_len(voice),
                pan: self.pan[voice],
                sweep: if voice == 0 { self.sweep } else { 0 },
                env_raw: self.env[voice],
                ..GbHit::default()
            });
            self.prev_period[voice] = self.period[voice];
        }
    }

    fn trigger(&mut self, voice: usize, t: i64) {
        let audible = self.voice_audible(voice);
        // A silent re-trigger (NRx2 = 0x08 or a cleared wave DAC) is how many
        // drivers stop a voice, so the previous note needs a real off. A real
        // re-trigger replaces the previous note on the same row.
        self.close(voice, t, !audible);
        if audible {
            self.open_hit(voice, t, None);
        }
    }

    fn record_pitch(&mut self, voice: usize, t: i64) {
        let Some(hit) = self.active[voice].as_ref() else {
            return;
        };
        if t < hit.sample_time {
            return;
        }
        if self.period[voice] == self.prev_period[voice] {
            return;
        }
        let old_period = self.prev_period[voice];
        self.prev_period[voice] = self.period[voice];
        let hit_start = self.active[voice].as_ref().unwrap().sample_time;
        let no_pts = self.active[voice]
            .as_ref()
            .unwrap()
            .pitch_pts
            .is_empty();
        // A pitch write just after the trigger is the note's real base, not a
        // bend. This matters for a voice started muted whose frequency is set
        // a few samples before the output level rises (Kirby DL2 wave).
        if t - hit_start <= GB_REFIT_SAMPLES && no_pts {
            self.active[voice].as_mut().unwrap().period = self.period[voice];
            return;
        }
        // A pulse driver that plays a legato melody writes a new NRx3/NRx4
        // without a trigger. A clear jump of about a semitone or more becomes
        // its own note cell, marked legato so Furnace changes pitch without
        // restarting the hardware envelope. Slow slides stay in pitch_pts and
        // become row effects later.
        if voice != 3 && gb_period_delta(old_period, self.period[voice]).abs() >= GB_LEGATO_UNITS {
            self.close(voice, t, false);
            self.open_hit(voice, t, None);
            if let Some(new_hit) = self.active[voice].as_mut() {
                new_hit.legato = true;
            }
            return;
        }
        // A frequency write without a trigger keeps the hardware envelope
        // running. It is a pitch bend, not a new note: splitting here (an
        // arpeggio, for instance) would restart the envelope at the note's
        // initial volume and hold notes the source lets decay.
        let period = self.period[voice];
        let hit = self.active[voice].as_mut().unwrap();
        if let Some(last) = hit.pitch_pts.last_mut() {
            if last.0 == t {
                last.1 = period;
                return;
            }
            if last.1 == period {
                return;
            }
        }
        hit.pitch_pts.push((t, period));
    }

    fn flush_pitch(&mut self, sample: i64, pending: &mut BTreeSet<usize>) {
        // A pitch change is a two-byte write (NRx3 then NRx4). Splitting on
        // the first byte would use the previous high byte and leave the new
        // note on the wrong pitch, so evaluate once per sample group.
        let voices: Vec<usize> = pending.iter().copied().collect();
        pending.clear();
        for voice in voices {
            self.record_pitch(voice, sample);
        }
    }
}

/// Pulse, wave and noise notes + the wavetables they use.
pub fn collect_gb(
    vgm: &VgmFile,
    _clock: Option<u32>,
) -> (Vec<GbHit>, Vec<GbOff>, Vec<Vec<u8>>, Vec<String>) {
    let mut warnings: Vec<String> = Vec::new();
    let writes: Vec<&ChipWrite> = vgm
        .writes
        .iter()
        .filter(|w| w.chip == "gb" && w.chip_id == 0)
        .collect();
    if writes.is_empty() {
        return (Vec::new(), Vec::new(), Vec::new(), warnings);
    }

    // A merged log can preserve a sample group but not the order of its
    // writes. Some tools emit NRx4/NR44 (the trigger byte) before the period
    // low byte, duty, envelope/DAC or wave level at the same sample; on the
    // chip that trigger sees the previous (often muted) state. Apply every
    // other write at the sample first and the trigger last, matching the
    // YM2612 key-on handling. Correctly ordered logs are unchanged.
    //
    // The trigger's NRx4 also carries the period high bits. If the driver
    // follows the trigger with a full low+high pitch correction at the same
    // sample (a very common "start the voice, then set the real frequency"
    // idiom), the correction is the note's base. Do not let the late trigger
    // write put the trigger's high byte back over it.
    let mut keep_high = vec![true; writes.len()];
    for i in 0..writes.len() {
        let w = writes[i];
        if !is_trigger(w) {
            continue;
        }
        let reg = w.reg & 0x7F;
        let lo = match reg {
            REG_P1_HI => REG_P1_LO,
            REG_P2_HI => REG_P2_LO,
            REG_W_HI => REG_W_LO,
            _ => continue,
        };
        let mut low_after = false;
        let mut high_after = false;
        for nxt in &writes[i + 1..] {
            if nxt.sample != w.sample {
                break;
            }
            let nreg = nxt.reg & 0x7F;
            if nreg == lo {
                low_after = true;
            } else if nreg == reg && !is_trigger(nxt) {
                high_after = true;
            }
        }
        keep_high[i] = !(low_after && high_after);
    }

    let mut order: Vec<usize> = (0..writes.len()).collect();
    order.sort_by_key(|&i| (writes[i].sample, is_trigger(writes[i]) as u8));

    let mut st = GbState::new();
    let mut pending_pitch: BTreeSet<usize> = BTreeSet::new();
    let mut group_sample: Option<i64> = None;

    for &wi in &order {
        let w = writes[wi];
        let reg = w.reg & 0x7F;
        let val = (w.val & 0xFF) as i32;
        let t = w.sample;

        if let Some(group) = group_sample {
            if t != group {
                st.flush_pitch(group, &mut pending_pitch);
            }
        }
        group_sample = Some(t);

        if reg == REG_SWEEP {
            st.sweep = val;
            if let Some(hit) = st.active[0].as_mut() {
                if t > hit.sample_time {
                    hit.sweep_pts.push((t, val));
                }
            }
        } else if reg == REG_P1_DUTY || reg == REG_P2_DUTY {
            let voice = if reg == REG_P1_DUTY { 0 } else { 1 };
            st.duty[voice] = (val >> 6) & 3;
            st.length_val[voice] = val & 0x3F;
            if let Some(hit) = st.active[voice].as_mut() {
                if t > hit.sample_time
                    && hit.duty_pts.last().map(|p| p.1) != Some(st.duty[voice])
                {
                    hit.duty_pts.push((t, st.duty[voice]));
                }
            }
        } else if reg == REG_P1_ENV || reg == REG_P2_ENV || reg == REG_N_ENV {
            let voice = if reg == REG_P1_ENV {
                0
            } else if reg == REG_P2_ENV {
                1
            } else {
                3
            };
            st.env[voice] = val;
            let info = st.active[voice]
                .as_ref()
                .filter(|hit| t > hit.sample_time)
                .map(|hit| (hit.sample_time, hit.env_len, hit.env_dir, hit.env_vol));
            if let Some((start, env_len, env_dir, env_vol)) = info {
                if (val & 0xF8) == 0 {
                    st.close(voice, t, true); // DAC off stops the voice
                    continue;
                }
                // Volume 0 with no attack is the driver's silence/kill write
                // (0x08); it is a note-off, not a decay-rate change.
                if (val >> 4) == 0 && !((val & 8) != 0 && (val & 7) != 0) {
                    st.close(voice, t, true);
                    continue;
                }
                if t - start <= GB_ENV_REFIT {
                    // Drivers often write NRx2 just after the trigger (e.g.
                    // Blitz's / Kuma's noise: NR43, trigger, then NR42). That
                    // write is this note's envelope, not a mid-note change.
                    let hit = st.active[voice].as_mut().unwrap();
                    hit.env_vol = val >> 4;
                    hit.env_dir = (val >> 3) & 1;
                    hit.env_len = val & 7;
                    hit.env_raw = val;
                    hit.vol = val >> 4;
                    continue;
                }
                if env_len == 0
                    && (val >> 4) > 0
                    && (val & 0xF8) != 0
                    && st.env_writes[voice].is_empty()
                    && (val >> 4) >= env_vol
                {
                    // A mid-note NRx2 write that raises the initial volume
                    // without a trigger is the driver's software attack. It
                    // is a real note start in the pattern, not a zombie volume
                    // step: restart the note at the current pitch and keep the
                    // envelope fields the write carries.
                    st.close(voice, t, false);
                    st.open_hit(voice, t, Some(val >> 4));
                    continue;
                }
                if env_len > 0 {
                    // A hardware envelope whose period or direction changes
                    // mid-note is how drivers speed a decay up or slow it
                    // down. Split the note at the change; the new segment
                    // starts at the volume the old envelope had reached.
                    let new_period = val & 7;
                    let new_dir = (val >> 3) & 1;
                    if new_period != env_len || new_dir != env_dir {
                        // VGMPlay (and the driver's intent) takes the new
                        // high nibble as the level from here on, so the split
                        // segment starts at it rather than the old envelope's
                        // reached volume.
                        st.close(voice, t, false);
                        st.open_hit(voice, t, Some(val >> 4));
                        continue;
                    }
                }
                st.env_writes[voice].push((t, val));
            }
        } else if reg == REG_P1_LO || reg == REG_P2_LO || reg == REG_W_LO {
            let voice = if reg == REG_P1_LO {
                0
            } else if reg == REG_P2_LO {
                1
            } else {
                2
            };
            st.period[voice] = (st.period[voice] & 0x700) | val;
            pending_pitch.insert(voice);
        } else if reg == REG_P1_HI || reg == REG_P2_HI || reg == REG_W_HI {
            let voice = if reg == REG_P1_HI {
                0
            } else if reg == REG_P2_HI {
                1
            } else {
                2
            };
            let triggered = (val & 0x80) != 0;
            if !triggered || keep_high[wi] {
                st.period[voice] = (st.period[voice] & 0xFF) | ((val & 7) << 8);
            }
            st.length_en[voice] = (val & 0x40) != 0;
            if triggered {
                if voice == 2 {
                    st.wave_started = true;
                }
                st.trigger(voice, t);
            } else {
                pending_pitch.insert(voice);
            }
        } else if reg == REG_W_ON {
            let was_on = st.wave_on;
            st.wave_on = (val & 0x80) != 0;
            st.length_val[2] = val & 0x3F;
            if was_on && !st.wave_on {
                st.wave_started = false;
                st.close(2, t, true);
            }
        } else if reg == REG_W_LEN {
            st.length_val[2] = val;
            st.length_en[2] = (val & 0x40) != 0;
        } else if reg == REG_W_VOL {
            st.wave_vol = val;
            let v = wave_volume(val);
            let last = st.active[2]
                .as_ref()
                .filter(|hit| t > hit.sample_time)
                .map(|hit| hit.vol_pts.last().map(|p| p.1).unwrap_or(hit.vol));
            if let Some(last) = last {
                if v == 0 {
                    // NR32 = 0 mutes the voice. Treat it as a note-off so the
                    // next output level starts a fresh note instead of one
                    // enormous note carrying silent volume steps and a long
                    // pitch macro.
                    st.close(2, t, true);
                } else if v != last {
                    st.active[2].as_mut().unwrap().vol_pts.push((t, v));
                }
            } else if v > 0 && st.wave_started && st.wave_on {
                // The voice was triggered while muted (Kirby's Dream Land 2
                // sets NR32 = 0, triggers, then raises NR32 a few samples
                // later). The level rising starts the note; the pitch write
                // that follows is folded in as the note's base.
                st.open_hit(2, t, None);
            }
        } else if reg == REG_N_LEN {
            st.length_val[3] = val & 0x3F;
            st.length_en[3] = (val & 0x40) != 0;
        } else if reg == REG_N_FREQ {
            st.noise_period = val;
            st.noise_width = (val >> 3) & 1;
            if let Some(hit) = st.active[3].as_mut() {
                if t > hit.sample_time {
                    // NR43 is the noise timbre/pitch. Changing it mid-note
                    // is a sweep (a drum body), not a new note: keeping it in
                    // one note lets the hardware envelope finish its decay.
                    let last = hit
                        .pitch_pts
                        .last()
                        .map(|p| p.1)
                        .unwrap_or(hit.period & 0xF7);
                    let cur = val & 0xF7;
                    if cur != last {
                        if let Some(last_pt) = hit.pitch_pts.last_mut() {
                            if last_pt.0 == t {
                                last_pt.1 = cur;
                            } else {
                                hit.pitch_pts.push((t, cur));
                            }
                        } else {
                            hit.pitch_pts.push((t, cur));
                        }
                    }
                    if st.noise_width != hit.duty
                        && hit.duty_pts.last().map(|p| p.1) != Some(st.noise_width)
                    {
                        hit.duty_pts.push((t, st.noise_width));
                    }
                }
            }
        } else if reg == REG_N_CTL {
            st.length_en[3] = (val & 0x40) != 0;
            if (val & 0x80) != 0 {
                st.trigger(3, t);
            }
        } else if reg == REG_PAN {
            for v in 0..4 {
                let left = (val >> (4 + v)) & 1;
                let right = (val >> v) & 1;
                let new_pan = (if left != 0 { 0xF0 } else { 0 })
                    | (if right != 0 { 0x0F } else { 0 });
                if new_pan != st.pan[v] {
                    st.pan[v] = new_pan;
                    if let Some(hit) = st.active[v].as_mut() {
                        if t - hit.sample_time <= GB_ENV_REFIT {
                            // The driver writes NR51 at (or just after) the
                            // trigger; that is this note's pan, not a
                            // mid-note move.
                            hit.pan = new_pan;
                        } else if hit.pan_pts.last().map(|p| p.1) != Some(new_pan) {
                            hit.pan_pts.push((t, new_pan));
                        }
                    }
                }
            }
        } else if reg == REG_ON {
            let on = (val & 0x80) != 0;
            if st.enabled && !on {
                for v in 0..4 {
                    st.close(v, t, true);
                }
            }
            st.enabled = on;
        } else if REG_WAVE <= reg && reg <= REG_WAVE + 15 {
            let i = (reg - REG_WAVE) as usize;
            st.wave_image[i] = val as u8;
            st.wave_now(t, true);
        }
        // 0x14 master volume: the module mix is normalised separately.
    }

    if let Some(group) = group_sample {
        st.flush_pitch(group, &mut pending_pitch);
    }
    for voice in 0..4 {
        if st.active[voice].is_some() {
            st.close(voice, 0, false);
        }
    }
    // A 16-byte rewrite passes through intermediate images; only the states a
    // note starts on or settles on are real wavetables. Drop the rest and
    // remap the indices so the module stays well inside the 256-wavetable cap.
    let mut used: IndexMap<usize, usize> = IndexMap::new();
    for hit in &st.hits {
        if hit.voice != 2 {
            continue;
        }
        if !used.contains_key(&hit.wave) {
            let next = used.len();
            used.insert(hit.wave, next);
        }
        for &(_t, idx) in &hit.wave_pts {
            if !used.contains_key(&idx) {
                let next = used.len();
                used.insert(idx, next);
            }
        }
    }
    if used.len() != st.waves.len() {
        let mut pruned: Vec<Vec<u8>> = vec![Vec::new(); used.len()];
        for (&old, &new) in &used {
            pruned[new] = st.waves[old].clone();
        }
        for hit in &mut st.hits {
            if hit.voice != 2 {
                continue;
            }
            hit.wave = used[&hit.wave];
            hit.wave_pts = hit
                .wave_pts
                .iter()
                .map(|&(t, idx)| (t, used[&idx]))
                .collect();
        }
        st.waves = pruned;
    }
    if st.glitches != 0 {
        warnings.push(format!(
            "Game Boy: ignored {} sub-frame tone gates",
            st.glitches
        ));
    }
    (st.hits, st.offs, st.waves, warnings)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vgm(writes: Vec<ChipWrite>) -> VgmFile {
        let mut v = VgmFile::default();
        v.gb_dmg_clock = 4_194_304;
        v.writes = writes;
        v
    }

    fn gb(sample: i64, reg: u32, val: u32) -> ChipWrite {
        ChipWrite::new(sample, "gb", 0, reg, val)
    }

    fn note(sample: i64, env: u32, duty: u32, period: i32) -> Vec<ChipWrite> {
        vec![
            gb(sample, REG_P1_DUTY, duty << 6),
            gb(sample, REG_P1_ENV, env),
            gb(sample, REG_P1_LO, (period & 0xFF) as u32),
            gb(sample, REG_P1_HI, (((period >> 8) & 7) | 0x80) as u32),
        ]
    }

    #[test]
    fn trigger_before_its_setup_is_reordered() {
        // Merged logs can emit NRx4 before the period low byte, duty and
        // envelope at the same sample. Applying the trigger first would open
        // the note on the previous (here muted) state and lose it.
        let writes = vec![
            gb(1000, REG_P1_HI, 0x86), // trigger, period high 6
            gb(1000, REG_P1_LO, 0x00), // period low
            gb(1000, REG_P1_ENV, 0x60), // volume 6
            gb(1000, REG_P1_DUTY, 0x80), // duty 2
            gb(2000, REG_P1_ENV, 0x00), // DAC off
        ];
        let (hits, offs, _waves, _warn) = collect_gb(&vgm(writes), None);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].sample_time, 1000);
        assert_eq!(hits[0].period, 0x600);
        assert_eq!(hits[0].duty, 2);
        assert_eq!(hits[0].vol, 6);
        assert_eq!(offs.len(), 1);
        assert_eq!(offs[0].sample_time, 2000);
        assert_eq!(offs[0].voice, 0);
    }

    #[test]
    fn large_pitch_jump_after_trigger_is_a_legato_note() {
        // A clear semitone-or-more jump without a trigger is a legato melody
        // move. It gets its own note cell with EAxx so the envelope
        // continues.
        let mut writes = note(1000, 0x61, 2, 0x600);
        writes.push(gb(2000, REG_P1_LO, 0x20));
        writes.push(gb(2000, REG_P1_HI, 0x07));
        let (hits, _offs, _waves, _warn) = collect_gb(&vgm(writes), None);
        assert_eq!(hits.len(), 2);
        assert!(hits[1].legato);
        assert_eq!(hits[1].sample_time, 2000);
        assert_eq!(hits[1].period, 0x720);
        assert_eq!(gb_note(4_194_304, hits[1].period), (122, 112));
    }

    #[test]
    fn wave_trigger_while_muted_starts_at_the_level_rise() {
        // Kirby's Dream Land 2 starts the wave voice with NR32 = 0 and raises
        // the output level a few samples later. The rise starts the note.
        let img: Vec<u8> = (0x10..0x20).collect();
        let mut writes: Vec<ChipWrite> = img
            .iter()
            .enumerate()
            .map(|(i, &b)| gb(0, REG_WAVE + i as u32, b as u32))
            .collect();
        writes.extend([
            gb(0, REG_W_ON, 0x80),
            gb(0, REG_W_VOL, 0x00),
            gb(0, REG_W_LO, 0x00),
            gb(0, REG_W_HI, 0x80),
        ]);
        writes.push(gb(100, REG_W_VOL, 0x40));
        let (hits, _offs, waves, _warn) = collect_gb(&vgm(writes), None);
        let wave: Vec<&GbHit> = hits.iter().filter(|h| h.voice == 2).collect();
        assert_eq!(wave.len(), 1);
        assert_eq!(wave[0].sample_time, 100);
        assert_eq!(wave[0].vol, 10);
        assert_eq!(waves[wave[0].wave], wave_nibbles(&img));
    }

    #[test]
    fn software_envelope_replays_zombie_volume_writes() {
        // Repeated NRx2 writes during the note are the zombie volume bug.
        let mut writes = note(1000, 0x28, 2, 0x600);
        for (i, val) in [0x18u32, 0x09, 0x11, 0x08].into_iter().enumerate() {
            writes.push(gb(1000 + i as i64 * 700, REG_P1_ENV, val));
        }
        let (hits, _offs, _waves, _warn) = collect_gb(&vgm(writes), None);
        assert_eq!(hits.len(), 1);
        assert!(hits[0].soft_env);
        assert!(!hits[0].vol_pts.is_empty());
    }

    #[test]
    fn noise_table_folds_the_fast_end_to_c5() {
        // Shift 4 / divider 6 is in the platform's table.
        assert_eq!(gb_noise_note(0x46), 60 + 46);
        // Shift 1 / divider 0 has no exact entry; the nearest is shift 0 /
        // divider 1, which has the same hardware period.
        assert_eq!(gb_noise_note(0x10), gb_noise_note(0x01));
        // The platform's C-0 entry is the fastest rate (period 0.5), but
        // Furnace renders it as a near-DC rumble; the top of the range is
        // folded onto C-5.
        for nr43 in [0x00, 0x04, 0x08, 0x18, 0x22] {
            assert_eq!(gb_noise_note(nr43), 120, "{nr43:#x}");
        }
        assert_eq!(gb_noise_note(0x5E), 102);

        assert_eq!(wave_volume(0x00), 0);
        assert_eq!(wave_volume(0x20), 15);
        assert_eq!(wave_volume(0x40), 10);
        assert_eq!(wave_volume(0x60), 5);
        assert_eq!(
            wave_nibbles(&[0x12, 0x34, 0x56, 0x78, 0x9A, 0xBC, 0xDE, 0xF0,
                           0x0F, 0xED, 0xCB, 0xA9, 0x87, 0x65, 0x43, 0x21]),
            vec![
                1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 0, 0, 15, 14, 13,
                12, 11, 10, 9, 8, 7, 6, 5, 4, 3, 2, 1
            ]
        );
    }
}
