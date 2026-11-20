//! HuC6280 (PC Engine / TurboGrafx-16) PSG -> notes, wavetables and PCM.
//!
//! The HuC6280 PSG is a six-voice wavetable chip. VGM writes arrive on command
//! `0xB9` as a register/value pair; the register file starts with a channel
//! select latch at 0x00 followed by per-channel writes:
//!
//! ```text
//! 0x00  channel select (bits 0-2, six voices)
//! 0x01  global balance (left/right nibbles)
//! 0x02  frequency low byte
//! 0x03  frequency high nibble (12-bit period)
//! 0x04  control: bit 7 enable, bit 6 direct-D/A, bits 0-4 volume (0-31)
//! 0x05  channel balance (left/right nibbles)
//! 0x06  waveform data (5-bit) / direct-D/A sample
//! 0x07  noise (bit 7 enable on voices 4/5, bits 0-4 period)
//! 0x08  LFO frequency
//! 0x09  LFO control
//! ```
//!
//! A voice sounds at `clock / (32 * period)` and its waveform is 32 five-bit
//! samples. There is no key-on register: the enable bit plus a non-zero
//! control volume is the gate, exactly like the SN76489 collector.
//!
//! Furnace's PC Engine platform writes the pitch-table value straight into the
//! period register (no -1), so a period round-trips through `_pitch` with
//! `period_offset=0` at the PSG clock and a divider of 32.
//!
//! The waveform buffer is written through 0x06 while the voice is disabled and
//! not in direct-D/A mode; the write index resets on a 1->0 direct-D/A
//! transition and advances on each disabled write (`pce_psg.cpp`). A driver
//! usually reloads all 32 bytes before a note, and that image becomes the
//! instrument's wavetable. Rapid mid-note rewrites become the instrument's
//! wave macro.
//!
//! Direct-D/A mode turns 0x06 into a 5-bit PCM register. The DAC stream control
//! commands (0x90-0x95) feed those 8-bit DDA levels from a type-0x05 data
//! block; each play becomes an 8-bit sample body.

use std::collections::BTreeSet;

use indexmap::IndexMap;

use crate::parity::py_round;
use crate::pitch::furnace_period_note;
use crate::vgm::{DacStreamPlay, VgmFile, STREAM_C6280};

pub use crate::vgm::HUC6280_DEFAULT_CLOCK;

pub const HUC6280_CHANNELS: usize = 6;
pub const HUC6280_WAVE_LEN: usize = 32;
// The chip's counter runs at PSG clock / 32 with a 12-bit period register.
pub const HUC6280_CPU_DIVIDER: i32 = 32;
pub const HUC6280_MAX_PERIOD: i32 = 0xFFF;
// A driver writing the note's own registers a few milliseconds after the gate
// is still describing this note, not a bend (same rule as SCC/C140).
pub const HUC6280_REFIT_SAMPLES: i64 = 735;
// The LO and HI bytes of one 12-bit frequency update land within a few samples.
pub const HUC6280_FREQ_PAIR_GAP: i64 = 64;
// A driver writes the note's attack volume/balance a few samples after the
// gate. A change this close is that note's first level, not an envelope step;
// folding it into the note's own volume keeps the attack transient on the row
// instead of dropping it as a same-row volume step.
pub const HUC6280_ATTACK_WINDOW: i64 = 64;
// A whole-waveform rewrite is a burst of byte writes; only the settled image
// is a step of a morph.
pub const WAVE_BURST_SAMPLES: i64 = 64;
pub const WAVE_PTS_MAX: usize = 4096;
// Shorter gates are driver housekeeping, not music.
pub const HUC6280_MIN_NOTE: i64 = 100;

pub const REG_SELECT: u32 = 0x00;
pub const REG_GLOBAL_BALANCE: u32 = 0x01;
pub const REG_FREQ_LO: u32 = 0x02;
pub const REG_FREQ_HI: u32 = 0x03;
pub const REG_CTRL: u32 = 0x04;
pub const REG_CH_BALANCE: u32 = 0x05;
pub const REG_WAVE: u32 = 0x06;
pub const REG_NOISE: u32 = 0x07;
pub const REG_LFO_FREQ: u32 = 0x08;
pub const REG_LFO_CTRL: u32 = 0x09;

pub const CTRL_ENABLE: i32 = 0x80;
pub const CTRL_DDA: i32 = 0x40;
pub const CTRL_VOLUME: i32 = 0x1F;
pub const NOISE_ENABLE: i32 = 0x80;
pub const NOISE_MASK: i32 = 0x1F;

// The chip scales a 4-bit balance nibble through this table before summing the
// three attenuators. The converter maps the balance register straight to
// Furnace's 08xy, so the core applies the same table itself; the constant is
// only used for the effective-level helper and docs.
pub const BALANCE_SCALE: [i32; 16] = [
    0x00, 0x03, 0x05, 0x07, 0x09, 0x0B, 0x0D, 0x0F, 0x10, 0x13, 0x15, 0x17, 0x19, 0x1B, 0x1D,
    0x1F,
];

/// The balance nibble whose `BALANCE_SCALE` entry is closest to `att`.
const fn scale_inv(att: i32) -> i32 {
    let mut best = 0usize;
    let mut i = 1usize;
    while i < 16 {
        if (BALANCE_SCALE[i] - att).abs() < (BALANCE_SCALE[best] - att).abs() {
            best = i;
        }
        i += 1;
    }
    best as i32
}

const fn balance_inv() -> [i32; 32] {
    let mut out = [0i32; 32];
    let mut a = 0usize;
    while a < 32 {
        out[a] = scale_inv(a as i32);
        a += 1;
    }
    out
}

const BALANCE_INV: [i32; 32] = balance_inv();

/// Split the PCE attenuators into (volume column, 08xy pan).
///
/// The chip's per-side level is `0x1F - min(0x1F, gbal + bal + al)` where
/// `al = 0x1F - control_vol` and each balance nibble becomes
/// `0x1F - BALANCE_SCALE[nibble]`. A driver may use the control volume or the
/// balance register (or both) for its envelope, so the converter puts the
/// louder side's level on the volume column and the left/right ratio on the
/// pan. Feeding that back through the same chip model reproduces the original
/// two levels exactly: the louder side is nibble 15 with `al = 0x1F - peak`,
/// and the quieter side picks the nibble whose attenuation closes the gap.
pub fn pce_effective(control_vol: i32, balance: i32) -> (i32, i32) {
    let al = 0x1F - (control_vol & 0x1F);
    let bl = 0x1F - BALANCE_SCALE[((balance >> 4) & 0x0F) as usize];
    let br = 0x1F - BALANCE_SCALE[(balance & 0x0F) as usize];
    let left = 0x1F - (bl + al).min(0x1F);
    let right = 0x1F - (br + al).min(0x1F);
    let peak = left.max(right);
    if peak <= 0 {
        return (0, 0);
    }
    let pl = BALANCE_INV[(left + 0x1F - peak) as usize];
    let pr = BALANCE_INV[(right + 0x1F - peak) as usize];
    (peak, ((pl & 15) << 4) | (pr & 15))
}

/// Voice frequency for a 12-bit period register, in Hz.
pub fn huc6280_period_hz(clock: u32, period: i32) -> f64 {
    let p = if period != 0 { period } else { 4096 };
    let clock = if clock != 0 {
        clock
    } else {
        HUC6280_DEFAULT_CLOCK
    };
    (clock as f64) / ((HUC6280_CPU_DIVIDER * p) as f64)
}

/// Period register -> (Furnace note, E5) for the same register.
///
/// Furnace's PC Engine platform writes the pitch-table value without an
/// offset, and its table runs at the doubled CPU clock with a divider of 64,
/// which is the PSG clock with a divider of 32.
pub fn huc6280_note(clock: u32, period: i32) -> (i32, i32) {
    let clock = if clock != 0 {
        clock
    } else {
        HUC6280_DEFAULT_CLOCK
    };
    furnace_period_note(
        clock as f64,
        HUC6280_CPU_DIVIDER as f64,
        period.clamp(0, HUC6280_MAX_PERIOD),
        0,
        HUC6280_MAX_PERIOD,
    )
}

/// Furnace note for a `0x07` periodic-noise value (properNoiseLayout).
///
/// The platform writes `note - 60` back to the noise register, so the
/// register value is the note's offset from C-4.
pub fn huc6280_noise_note(noise: i32) -> i32 {
    60 + (noise & NOISE_MASK)
}

// A period of 0 loads the counter with 4096, the longest (lowest) period.
#[allow(dead_code)]
fn effective_period(period: i32) -> i32 {
    if period != 0 { period } else { 4096 }
}

/// 1/64-semitone change from one 12-bit period to another.
#[allow(dead_code)]
fn period_delta(base: i32, new: i32) -> i32 {
    let base = effective_period(base.max(0)) as f64;
    let new = effective_period(new.max(0)) as f64;
    py_round(64.0 * 12.0 * (base / new).log2()) as i32
}

#[derive(Clone, Debug, PartialEq, Default)]
pub struct Huc6280Hit {
    pub sample_time: i64,
    pub chip_id: i32,
    pub voice: usize,
    pub period: i32,
    /// Control volume (0-31).
    pub vol: i32,
    /// 32 five-bit samples.
    pub wave: Vec<u8>,
    /// Channel balance register (0x05).
    pub balance: i32,
    /// 0x07 register.
    pub noise: i32,
    pub off_sample: i64,
    pub pitch_pts: Vec<(i64, i32)>,
    pub vol_pts: Vec<(i64, i32)>,
    pub balance_pts: Vec<(i64, i32)>,
    pub wave_pts: Vec<(i64, Vec<u8>)>,
    pub noise_pts: Vec<(i64, i32)>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Huc6280Off {
    pub sample_time: i64,
    pub chip_id: i32,
    pub voice: usize,
}

/// A gate-to-gate interval of one voice, with everything it needs.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct Huc6280Note {
    pub hit: Huc6280Hit,
    /// 0 = still sounding.
    pub end_sample: i64,
    pub wave_index: usize,
}

struct ChipState {
    select: i32,
    wave: [[u8; HUC6280_WAVE_LEN]; HUC6280_CHANNELS],
    widx: [usize; HUC6280_CHANNELS],
    freq: [i32; HUC6280_CHANNELS],
    control: [i32; HUC6280_CHANNELS],
    balance: [i32; HUC6280_CHANNELS],
    noise: [i32; HUC6280_CHANNELS],
    global_balance: i32,
    lfo_freq: i32,
    lfo_ctrl: i32,
    active: [Option<Huc6280Hit>; HUC6280_CHANNELS],
    #[allow(dead_code)]
    gate_time: [i64; HUC6280_CHANNELS],
    refit_until: [i64; HUC6280_CHANNELS],
}

impl ChipState {
    fn blank() -> Self {
        ChipState {
            select: 0,
            wave: [[0u8; HUC6280_WAVE_LEN]; HUC6280_CHANNELS],
            widx: [0; HUC6280_CHANNELS],
            freq: [0; HUC6280_CHANNELS],
            control: [0; HUC6280_CHANNELS],
            balance: [0; HUC6280_CHANNELS],
            noise: [0; HUC6280_CHANNELS],
            global_balance: 0,
            lfo_freq: 0,
            lfo_ctrl: 0,
            active: std::array::from_fn(|_| None),
            gate_time: [0; HUC6280_CHANNELS],
            refit_until: [0; HUC6280_CHANNELS],
        }
    }
}

fn is_on(st: &ChipState, voice: usize) -> bool {
    let control = st.control[voice];
    (control & CTRL_ENABLE) != 0 && (control & CTRL_VOLUME) > 0
}

struct Huc6280Collector {
    notes: Vec<Huc6280Note>,
    offs: Vec<Huc6280Off>,
    waves: IndexMap<Vec<u8>, usize>,
    acc: IndexMap<String, i64>,
    state: IndexMap<i32, ChipState>,
}

impl Huc6280Collector {
    fn wave_index(&mut self, wave: &[u8]) -> usize {
        if let Some(&idx) = self.waves.get(wave) {
            return idx;
        }
        let idx = self.waves.len();
        self.waves.insert(wave.to_vec(), idx);
        idx
    }

    fn open_gate(&mut self, cid: i32, voice: usize, t: i64) {
        let st = self.state.get_mut(&cid).expect("chip state exists");
        let period = st.freq[voice];
        let vol = st.control[voice] & CTRL_VOLUME;
        let wave = st.wave[voice].to_vec();
        let balance = st.balance[voice];
        let noise = st.noise[voice];
        st.active[voice] = Some(Huc6280Hit {
            sample_time: t,
            chip_id: cid,
            voice,
            period,
            vol,
            wave,
            balance,
            noise,
            ..Huc6280Hit::default()
        });
        st.gate_time[voice] = t;
        st.refit_until[voice] = t + HUC6280_REFIT_SAMPLES;
    }

    fn close(&mut self, cid: i32, voice: usize, t: i64, emit_off: bool) {
        let hit = self
            .state
            .get_mut(&cid)
            .expect("chip state exists")
            .active[voice]
            .take();
        let Some(mut hit) = hit else {
            return;
        };
        hit.off_sample = t;
        if t - hit.sample_time >= HUC6280_MIN_NOTE {
            if emit_off {
                self.offs.push(Huc6280Off {
                    sample_time: t,
                    chip_id: cid,
                    voice,
                });
            }
            let wave_index = self.wave_index(&hit.wave);
            self.notes.push(Huc6280Note {
                hit,
                end_sample: t,
                wave_index,
            });
        } else {
            *self.acc.entry("glitch".into()).or_insert(0) += 1;
        }
    }

    fn update_gate(&mut self, cid: i32, voice: usize, t: i64) {
        let st = self.state.get(&cid).expect("chip state exists");
        let on = is_on(st, voice);
        let hit = st.active[voice].is_some();
        if on && !hit {
            self.open_gate(cid, voice, t);
        } else if !on && hit {
            self.close(cid, voice, t, true);
        }
    }

    fn update_pitch(&mut self, cid: i32, voice: usize, t: i64) {
        let st = self.state.get_mut(&cid).expect("chip state exists");
        let period = st.freq[voice];
        let refit_until = st.refit_until[voice];
        let Some(hit) = st.active[voice].as_mut() else {
            return;
        };
        let cur = hit.pitch_pts.last().map(|p| p.1).unwrap_or(hit.period);
        if cur == period {
            return;
        }
        if t <= refit_until {
            // The driver rewrote the note's own pitch inside the frame.
            hit.period = period;
            return;
        }
        // A 12-bit update is a LO/HI pair; the two writes can land a few
        // samples apart. Keep only the settled value so the transient low byte
        // never becomes a pitch step (a 730-sample merge would instead eat the
        // driver's per-frame vibrato).
        if let Some(last) = hit.pitch_pts.last_mut() {
            if t - last.0 <= HUC6280_FREQ_PAIR_GAP {
                *last = (t, period);
                return;
            }
        }
        hit.pitch_pts.push((t, period));
    }

    fn update_volume(&mut self, cid: i32, voice: usize, t: i64) {
        let st = self.state.get_mut(&cid).expect("chip state exists");
        let vol = st.control[voice] & CTRL_VOLUME;
        let Some(hit) = st.active[voice].as_mut() else {
            return;
        };
        if t == hit.sample_time {
            hit.vol = vol;
        } else if hit.vol_pts.last().map(|p| p.1) != Some(vol) {
            hit.vol_pts.push((t, vol));
        }
    }

    fn update_balance(&mut self, cid: i32, voice: usize, t: i64) {
        let st = self.state.get_mut(&cid).expect("chip state exists");
        let balance = st.balance[voice];
        let Some(hit) = st.active[voice].as_mut() else {
            return;
        };
        if t == hit.sample_time {
            hit.balance = balance;
        } else if hit.balance_pts.last().map(|p| p.1) != Some(balance) {
            hit.balance_pts.push((t, balance));
        }
    }

    fn update_noise(&mut self, cid: i32, voice: usize, t: i64) {
        let st = self.state.get_mut(&cid).expect("chip state exists");
        let noise = st.noise[voice];
        let Some(hit) = st.active[voice].as_mut() else {
            return;
        };
        if t == hit.sample_time {
            hit.noise = noise;
        } else if hit.noise_pts.last().map(|p| p.1) != Some(noise) {
            hit.noise_pts.push((t, noise));
        }
    }

    fn write_wave(&mut self, cid: i32, voice: usize, val: i32, t: i64) {
        let st = self.state.get_mut(&cid).expect("chip state exists");
        let control = st.control[voice];
        if (control & CTRL_DDA) == 0 {
            // A waveform byte, not a direct-D/A sample.
            let wi = st.widx[voice];
            st.wave[voice][wi] = (val & 0x1F) as u8;
        }
        if (control & 0xC0) == 0x00 {
            st.widx[voice] = (st.widx[voice] + 1) & 0x1F;
        }
        if st.active[voice].is_none() || (control & CTRL_DDA) != 0 {
            return;
        }
        let img = st.wave[voice].to_vec();
        if img.iter().all(|&b| b == 0) {
            return;
        }
        let hit = st.active[voice].as_mut().expect("hit is active");
        let last = hit.wave_pts.last().cloned();
        if let Some((last_t, _)) = &last {
            if t - *last_t <= WAVE_BURST_SAMPLES {
                // A whole-image rewrite is a burst of byte writes.
                if let Some(entry) = hit.wave_pts.last_mut() {
                    entry.1 = img;
                }
                return;
            }
        }
        if let Some((_, last_img)) = &last {
            if *last_img == img {
                return;
            }
        }
        if hit.wave_pts.len() >= WAVE_PTS_MAX {
            *self.acc.entry("wave_mid".into()).or_insert(0) += 1;
            return;
        }
        hit.wave_pts.push((t, img));
    }

    fn run(&mut self, vgm: &VgmFile) {
        for w in &vgm.writes {
            if w.chip != "huc6280" {
                continue;
            }
            let cid = w.chip_id as i32;
            self.state.entry(cid).or_insert_with(ChipState::blank);
            let reg = w.reg & 0x7F;
            let val = (w.val & 0xFF) as i32;
            let t = w.sample;
            if reg == REG_SELECT {
                self.state.get_mut(&cid).unwrap().select = val & 0x07;
                continue;
            }
            if reg == REG_GLOBAL_BALANCE {
                self.state.get_mut(&cid).unwrap().global_balance = val;
                continue;
            }
            if reg == REG_LFO_FREQ {
                self.state.get_mut(&cid).unwrap().lfo_freq = val;
                continue;
            }
            if reg == REG_LFO_CTRL {
                self.state.get_mut(&cid).unwrap().lfo_ctrl = val;
                continue;
            }
            let voice = self.state.get(&cid).unwrap().select as usize;
            if voice >= HUC6280_CHANNELS {
                continue;
            }
            if reg == REG_FREQ_LO {
                let st = self.state.get_mut(&cid).unwrap();
                st.freq[voice] = (st.freq[voice] & 0x0F00) | val;
                self.update_pitch(cid, voice, t);
            } else if reg == REG_FREQ_HI {
                let st = self.state.get_mut(&cid).unwrap();
                st.freq[voice] = (st.freq[voice] & 0x00FF) | ((val & 0x0F) << 8);
                self.update_pitch(cid, voice, t);
            } else if reg == REG_CTRL {
                let st = self.state.get_mut(&cid).unwrap();
                let old = st.control[voice];
                if (old & CTRL_DDA) != 0 && (val & CTRL_DDA) == 0 {
                    st.widx[voice] = 0;
                }
                if (old & CTRL_ENABLE) == 0 && (val & CTRL_ENABLE) != 0 && (val & CTRL_DDA) == 0 {
                    st.widx[voice] = (st.widx[voice] + 1) & 0x1F;
                }
                st.control[voice] = val;
                self.update_gate(cid, voice, t);
                self.update_volume(cid, voice, t);
            } else if reg == REG_CH_BALANCE {
                self.state.get_mut(&cid).unwrap().balance[voice] = val;
                self.update_balance(cid, voice, t);
            } else if reg == REG_WAVE {
                self.write_wave(cid, voice, val, t);
            } else if reg == REG_NOISE {
                self.state.get_mut(&cid).unwrap().noise[voice] = val;
                self.update_noise(cid, voice, t);
            }
        }

        let cids: Vec<i32> = self.state.keys().copied().collect();
        for cid in cids {
            for voice in 0..HUC6280_CHANNELS {
                if self.state[&cid].active[voice].is_some() {
                    let hit = self
                        .state
                        .get_mut(&cid)
                        .unwrap()
                        .active[voice]
                        .take()
                        .expect("hit is active");
                    let wave_index = self.wave_index(&hit.wave);
                    self.notes.push(Huc6280Note {
                        hit,
                        end_sample: 0,
                        wave_index,
                    });
                }
            }
        }
    }

    fn finish(mut self) -> (Vec<Huc6280Note>, Vec<Huc6280Off>, Vec<Vec<u8>>, Vec<String>) {
        let mut warnings: Vec<String> = Vec::new();
        if let Some(&glitch) = self.acc.get("glitch") {
            if glitch != 0 {
                warnings.push(format!(
                    "HuC6280: ignored {glitch} sub-frame tone gates"
                ));
            }
        }
        if let Some(&wave_mid) = self.acc.get("wave_mid") {
            if wave_mid != 0 {
                warnings.push(format!(
                    "{wave_mid} HuC6280 waveform changes past the \
                     {WAVE_PTS_MAX}-entry cap were dropped from a note's morph"
                ));
            }
        }
        let mut wave_list = vec![Vec::new(); self.waves.len()];
        for (wave, idx) in self.waves.drain(..) {
            wave_list[idx] = wave;
        }
        (self.notes, self.offs, wave_list, warnings)
    }
}

/// Walk the 0xB9 register stream into notes.
///
/// Returns (notes, key-offs, distinct waveforms, warnings). Waveforms are
/// deduplicated so voices or notes sharing one image share one instrument.
pub fn collect_huc6280(
    vgm: &VgmFile,
) -> (
    Vec<Huc6280Note>,
    Vec<Huc6280Off>,
    Vec<Vec<u8>>,
    Vec<String>,
) {
    let mut warnings: Vec<String> = Vec::new();
    let chips: Vec<i32> = vgm
        .writes
        .iter()
        .filter(|w| w.chip == "huc6280")
        .map(|w| w.chip_id as i32)
        .collect::<BTreeSet<i32>>()
        .into_iter()
        .collect();
    if chips.is_empty() {
        return (Vec::new(), Vec::new(), Vec::new(), warnings);
    }
    let state: IndexMap<i32, ChipState> = chips
        .into_iter()
        .map(|cid| (cid, ChipState::blank()))
        .collect();
    let mut collector = Huc6280Collector {
        notes: Vec::new(),
        offs: Vec::new(),
        waves: IndexMap::new(),
        acc: IndexMap::new(),
        state,
    };
    collector.run(vgm);
    let (notes, offs, wave_list, mut collected_warnings) = collector.finish();
    warnings.append(&mut collected_warnings);
    (notes, offs, wave_list, warnings)
}

// ---------------------------------------------------------------------------
// Direct-D/A PCM (DAC stream control 0x90-0x95 and direct 0x06 writes)

/// One 8-bit body converted from a stream of 5-bit DDA levels.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct Huc6280PcmSample {
    pub name: String,
    pub data: Vec<u8>,
    pub rate: i32,
    pub loop_: bool,
    pub start: i64,
    pub end: i64,
}

#[derive(Clone, Debug, PartialEq, Default)]
pub struct Huc6280PcmHit {
    pub sample_time: i64,
    pub end_time: i64,
    pub sample_index: usize,
    /// Channel the DDA stream writes (0-5).
    pub voice: usize,
    pub chip_id: i32,
    /// The DAC stream byte rate at this play. The driver changes it 23442
    /// times on the faceball rip, so it is the note's pitch, not a fixed
    /// per-body property.
    pub rate: i32,
}

/// 5-bit DDA level -> the 8-bit body Furnace's PCE sample path replays.
///
/// Furnace turns a sample byte back into its DDA register with
/// `(int8_t)(b ^ 0x80) >> 3 & 0x1f`. Choosing `b = ((v << 3) + 0x80)` makes
/// that expression return `v` for all 0-31, so the register stream is
/// reproduced exactly.
pub fn _dda_to_s8(body: &[u8]) -> Vec<u8> {
    body.iter()
        .map(|&v| ((v & 0x1F) as u16 * 8 + 0x80) as u8)
        .collect()
}

/// The DDA body one DAC-stream play reads, or None if the bank is absent.
fn pcm_body(play: &DacStreamPlay, vgm: &VgmFile) -> Option<(Vec<u8>, bool)> {
    if play.block >= 0 {
        if (play.block as usize) < vgm.huc6280_pcm_blocks.len() {
            return Some((vgm.huc6280_pcm_blocks[play.block as usize].clone(), play.loop_));
        }
        return None;
    }
    let bank = &vgm.huc6280_pcm;
    if bank.is_empty() {
        return None;
    }
    let start = (play.offset & 0x7FFF_FFFF) as usize;
    let end = if play.length > 0 {
        start.saturating_add(play.length as usize)
    } else {
        bank.len()
    };
    let body = if start >= bank.len() {
        Vec::new()
    } else {
        bank[start..end.min(bank.len())].to_vec()
    };
    Some((body, play.loop_))
}

/// DAC-stream PCM plays (0x93 offset/length, 0x95 whole-block).
pub fn collect_huc6280_pcm(
    vgm: &VgmFile,
) -> (Vec<Huc6280PcmSample>, Vec<Huc6280PcmHit>, Vec<String>) {
    let mut warnings: Vec<String> = Vec::new();
    let mut samples: Vec<Huc6280PcmSample> = Vec::new();
    let mut hits: Vec<Huc6280PcmHit> = Vec::new();
    let plays: Vec<&DacStreamPlay> = vgm
        .dac_stream_plays
        .iter()
        .filter(|p| p.chip_type == STREAM_C6280)
        .collect();
    if plays.is_empty() {
        return (samples, hits, warnings);
    }
    let readable = !vgm.huc6280_pcm.is_empty() || !vgm.huc6280_pcm_blocks.is_empty();
    if !readable {
        warnings.push(format!(
            "{} HuC6280 DAC stream play(s) but no type-0x05 PCM data block; those samples are silent",
            plays.len()
        ));
        return (samples, hits, warnings);
    }

    let mut by_data: IndexMap<(Vec<u8>, bool), usize> = IndexMap::new();
    let mut unreadable = 0usize;
    for play in plays {
        let Some((body, loop_)) = pcm_body(play, vgm) else {
            unreadable += 1;
            continue;
        };
        if body.is_empty() {
            unreadable += 1;
            continue;
        }
        let data = _dda_to_s8(&body);
        let key = (data.clone(), loop_);
        let idx = match by_data.get(&key) {
            Some(&idx) => idx,
            None => {
                let idx = samples.len();
                by_data.insert(key, idx);
                let rate = if play.rate != 0 {
                    play.rate as i32
                } else {
                    16000
                };
                samples.push(Huc6280PcmSample {
                    name: format!("HuC6280 PCM {idx:02}"),
                    data,
                    rate: rate.max(1),
                    loop_,
                    start: play.sample,
                    end: play.stop_sample,
                });
                idx
            }
        };
        let mut voice = (play.port & 0x07) as usize;
        if voice >= HUC6280_CHANNELS {
            voice = 0;
        }
        let mut end = play.stop_sample;
        if end == 0 && play.length > 0 && play.rate > 0 {
            end = play.sample
                + py_round((body.len() as f64 / play.rate as f64) * 44100.0) as i64;
        }
        let rate = if play.rate != 0 {
            play.rate as i32
        } else {
            16000
        };
        hits.push(Huc6280PcmHit {
            sample_time: play.sample,
            end_time: end,
            sample_index: idx,
            voice,
            chip_id: play.chip_id as i32,
            rate: rate.max(1),
        });
    }
    if unreadable != 0 {
        warnings.push(format!(
            "{unreadable} HuC6280 DAC stream play(s) point at missing block or bank data; those samples are silent"
        ));
    }
    (samples, hits, warnings)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vgm::ChipWrite;

    fn w(sample: i64, reg: u32, val: u32) -> ChipWrite {
        ChipWrite::new(sample, "huc6280", 0, reg, val)
    }

    fn square() -> Vec<u8> {
        let mut out = vec![31u8; 16];
        out.resize(32, 0u8);
        out
    }

    /// The register sequence a PCE driver uses to start one wavetable note.
    fn load_wave(
        wave_vals: &[u8],
        period: i32,
        volume: i32,
        balance: i32,
        sample: i64,
        channel: u32,
    ) -> Vec<ChipWrite> {
        let mut writes = vec![
            w(sample, 0x01, 0xFF),
            w(sample, 0x00, channel),
            w(sample, 0x04, 0x00),
        ];
        for &v in wave_vals {
            writes.push(w(sample, 0x06, v as u32));
        }
        writes.extend([
            w(sample, 0x04, 0x40),
            w(sample, 0x02, (period & 0xFF) as u32),
            w(sample, 0x03, ((period >> 8) & 0x0F) as u32),
            w(sample, 0x05, balance as u32),
            w(sample, 0x04, (0x80 | (volume & 0x1F)) as u32),
        ]);
        writes
    }

    fn vgm(writes: Vec<ChipWrite>) -> VgmFile {
        let mut out = VgmFile::default();
        out.total_samples = 400_000;
        out.huc6280_clock = 3_579_545;
        out.writes = writes;
        out
    }

    #[test]
    fn wave_note_and_dedup() {
        let notes_waves = collect_huc6280(&vgm(load_wave(&square(), 0x100, 31, 0xFF, 0, 0)));
        let (notes, offs, waves, warn) = notes_waves;
        assert_eq!(notes.len(), 1);
        assert!(offs.is_empty());
        assert_eq!(warn, Vec::<String>::new());
        let hit = &notes[0].hit;
        assert_eq!(hit.voice, 0);
        assert_eq!(hit.period, 0x100);
        assert_eq!(hit.vol, 31);
        assert_eq!(hit.wave, square());
        assert_eq!(waves, vec![square()]);
        assert_eq!(notes[0].end_sample, 0);

        let mut writes = load_wave(&square(), 0x100, 31, 0xFF, 0, 0);
        writes.extend(load_wave(&square(), 0x100, 31, 0xFF, 0, 1));
        let (notes, _offs, waves, _warn) = collect_huc6280(&vgm(writes));
        assert_eq!(notes.len(), 2);
        assert_eq!(waves.len(), 1);
    }

    #[test]
    fn gate_off_closes_and_short_gate_is_a_glitch() {
        let mut writes = load_wave(&square(), 0x100, 31, 0xFF, 0, 0);
        writes.push(w(10_000, 0x04, 0x00));
        let (notes, offs, _waves, warn) = collect_huc6280(&vgm(writes));
        assert_eq!(notes.len(), 1);
        assert_eq!(notes[0].hit.off_sample, 10_000);
        assert_eq!(notes[0].end_sample, 10_000);
        assert_eq!(offs.len(), 1);
        assert_eq!(offs[0].sample_time, 10_000);
        assert!(warn.is_empty());

        // A 50-sample gate is dropped and counted.
        let mut writes = load_wave(&square(), 0x100, 31, 0xFF, 0, 0);
        writes.push(w(50, 0x04, 0x00));
        let (notes, offs, waves, warn) = collect_huc6280(&vgm(writes));
        assert!(notes.is_empty());
        assert!(offs.is_empty());
        assert!(waves.is_empty());
        assert_eq!(warn, vec!["HuC6280: ignored 1 sub-frame tone gates"]);
    }

    #[test]
    fn pitch_refit_pair_and_volume_timelines() {
        let mut writes = load_wave(&square(), 0x100, 31, 0xFF, 0, 0);
        // Attack balance just after the gate is recorded as a start-timeline
        // point (the analyzer folds it into the note's own level).
        writes.push(w(2, 0x05, 0xCC));
        // Volume and balance steps on a later row become points.
        writes.push(w(1000, 0x04, 0x8F));
        writes.push(w(1000, 0x05, 0x99));
        // LO/HI pair inside the refit window is the note's own pitch.
        writes.push(w(700, 0x02, 0x10));
        writes.push(w(730, 0x03, 0x01));
        // Later pair collapses to its settled value at the second write.
        writes.push(w(2000, 0x02, 0x34));
        writes.push(w(2010, 0x03, 0x02));
        // Vibrato at frame distance stays separate steps.
        writes.push(w(3000, 0x02, 0x20));
        writes.push(w(3730, 0x02, 0x28));
        writes.push(w(5000, 0x04, 0x00));
        let (notes, _offs, _waves, warn) = collect_huc6280(&vgm(writes));
        assert!(warn.is_empty());
        let hit = &notes[0].hit;
        assert_eq!(hit.period, 0x110);
        assert_eq!(hit.vol_pts, vec![(1000, 15)]);
        assert_eq!(hit.balance_pts, vec![(2, 0xCC), (1000, 0x99)]);
        assert_eq!(
            hit.pitch_pts,
            vec![(2010, 0x234), (3000, 0x220), (3730, 0x228)]
        );
        assert_eq!(hit.balance, 0xFF);
    }

    #[test]
    fn pce_effective_and_note_mapping() {
        assert_eq!(pce_effective(31, 0xF0), (31, 0xF0));
        assert_eq!(pce_effective(31, 0x00), (0, 0));
        assert_eq!(pce_effective(0, 0xF0), (0, 0));
        assert_eq!(pce_effective(16, 0x8F), (16, 0x8F));
        assert_eq!(pce_effective(20, 0x3C), (14, 0x8F));

        assert_eq!(huc6280_note(3_579_545, 0), (177, 128));
        assert_eq!(huc6280_note(3_579_545, 1000), (81, 164));
        assert_eq!(huc6280_note(3_579_545, 4095), (57, 114));
        assert!((huc6280_period_hz(3_579_545, 0) - 27.309_761_047_363_28).abs() < 1e-9);
        assert_eq!(huc6280_period_hz(0, 100), 1_118.607_812_5);
        assert_eq!(huc6280_noise_note(0x9F), 91);
        assert_eq!(huc6280_noise_note(0xA0), 60);
    }

    #[test]
    fn dda_conversion_and_stream_play() {
        let converted = _dda_to_s8(&[0, 1, 31, 16]);
        assert_eq!(converted, vec![0x80, 0x88, 0x78, 0x00]);
        for (v, &b) in [0u8, 1, 31, 16].iter().zip(converted.iter()) {
            let signed = (b ^ 0x80) as i8;
            assert_eq!(((signed as i32) >> 3) & 0x1F, *v as i32);
        }

        let pcm: Vec<u8> = vec![16, 20, 24, 28, 24, 20, 16, 12];
        let mut v = vgm(Vec::new());
        v.huc6280_pcm_blocks.push(pcm.clone());
        v.dac_stream_plays.push(DacStreamPlay {
            sample: 100,
            chip_type: STREAM_C6280,
            chip_id: 0,
            stream: 0,
            port: 2,
            reg: 6,
            offset: 0,
            length: 8,
            mode: 0,
            rate: 8000,
            loop_: false,
            reverse: false,
            step: 1,
            step_base: 0,
            bank: 5,
            block: 0,
            stop_sample: 0,
        });
        let (samples, hits, warn) = collect_huc6280_pcm(&v);
        assert!(warn.is_empty());
        assert_eq!(samples.len(), 1);
        assert_eq!(samples[0].name, "HuC6280 PCM 00");
        assert_eq!(samples[0].rate, 8000);
        assert_eq!(samples[0].data, _dda_to_s8(&pcm));
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].voice, 2);
        assert_eq!(hits[0].sample_time, 100);
        // 8 bytes at 8000 Hz = 1 ms = 44.1 samples.
        assert_eq!(hits[0].end_time, 144);

        // A block id past the table is unreadable and warns.
        v.dac_stream_plays[0].block = 3;
        let (samples, hits, warn) = collect_huc6280_pcm(&v);
        assert!(samples.is_empty());
        assert!(hits.is_empty());
        assert_eq!(
            warn,
            vec![
                "1 HuC6280 DAC stream play(s) point at missing block or bank data; \
                 those samples are silent"
            ]
        );
    }

    #[test]
    fn pcm_without_bank_warns() {
        let mut v = vgm(Vec::new());
        v.dac_stream_plays.push(DacStreamPlay {
            sample: 0,
            chip_type: STREAM_C6280,
            chip_id: 0,
            stream: 1,
            port: 0,
            reg: 6,
            offset: 0,
            length: 4,
            mode: 0,
            rate: 0,
            loop_: false,
            reverse: false,
            step: 1,
            step_base: 0,
            bank: 5,
            block: -1,
            stop_sample: 0,
        });
        let (samples, hits, warn) = collect_huc6280_pcm(&v);
        assert!(samples.is_empty());
        assert!(hits.is_empty());
        assert_eq!(
            warn,
            vec![
                "1 HuC6280 DAC stream play(s) but no type-0x05 PCM data block; \
                 those samples are silent"
            ]
        );
    }
}
