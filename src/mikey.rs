//! Atari Lynx Mikey: LFSR voices and CPU-fed 8-bit DAC samples.
//!
//! VGM 1.72 carries the chip clock at header 0xE4 (16 MHz on the Lynx) and
//! writes its registers with command 0x40: `40 aa vv`. Four voices at 0x20+8*n
//! have
//!
//! ```text
//! 0 volume (signed)   1 feedback taps   2 output level (the DAC byte a
//! driver streams)     3 shift register low byte   4 timer backup
//! 5 control (shift, count/reload enable, interrupt, f7, integrate)
//! 6 counter           7 shift register high nibble / control B
//! ```
//!
//! plus the shared stereo registers 0x40-0x43 attenuation, 0x44 pan enable
//! and 0x50 stereo mute.
//!
//! A voice is either an LFSR tone (the driver writes volume/feedback/timer) or
//! a software DAC (the driver streams register 2). Furnace's native Lynx
//! system supports both: `DIV_INS_MIKEY` carries the LFSR macros, and an
//! Amiga-style sample instrument plays 8-bit bodies through the same four
//! columns.
//!
//! Literal port of `vgm2151fur/mikey.py` from vgm2151fur 0.11.0.

use std::collections::{BTreeMap, BTreeSet};

use indexmap::IndexMap;

use crate::parity::{py_int, py_round};
use crate::samplecluster::{cluster_fragments, DEFAULT_HOT, DEFAULT_TARGET};
use crate::vgm::{ChipWrite, VgmFile, VGM_RATE};

pub use crate::vgm::MIKEY_DEFAULT_CLOCK;

pub const MIKEY_VOICES: usize = 4;
/// Mikey timer: period = (backup+1) << shift microseconds (1 MHz tick source).
pub const MIKEY_TIMER_CLOCK: u32 = 1_000_000;
/// Furnace's `DivPlatformLynx` pitch table: 16 MHz clock, divider 64 in the
/// default (untuned) mode, and a 26-bit period maximum.
pub const MIKEY_DIVIDER: i32 = 64;
pub const MIKEY_MAX_PERIOD: i32 = 0x3FFFFFF;

pub const REG_VOLUME: u32 = 0;
pub const REG_FEEDBACK: u32 = 1;
pub const REG_OUTPUT: u32 = 2;
pub const REG_SHIFT: u32 = 3;
pub const REG_BACKUP: u32 = 4;
pub const REG_CONTROL: u32 = 5;
pub const REG_COUNTER: u32 = 6;
pub const REG_OTHER: u32 = 7;
pub const REG_ATTEN: u32 = 0x40;
pub const REG_PAN: u32 = 0x44;
pub const REG_STEREO: u32 = 0x50;

/// A register stream faster than this on the output byte is a software DAC.
pub const MIKEY_DAC_WRITES: i64 = 32;
/// A control/feedback setup followed by a volume write this soon is a new note.
pub const MIKEY_SETUP_GAP: i64 = 64;
/// Register writes this soon after a gate are the note's own setup, not a
/// mid-note macro step; fold them into the initial timbre.
pub const MIKEY_SETTLE: i64 = 64;
/// Sub-frame gates are driver housekeeping rather than notes.
pub const MIKEY_MIN_NOTE: i64 = 80;
/// Output chunks: 0.25 s at the reconstructed rate.
pub const MIKEY_CHUNK_DIV: i32 = 4;
/// A gap of at most this many samples is the DAC's nominal write period.
pub const MIKEY_MAX_GAP: i64 = 64;

/// Python `_s8`: a byte interpreted as a signed 8-bit value.
fn s8(value: i32) -> i32 {
    let value = value & 0xFF;
    if value >= 0x80 { value - 0x100 } else { value }
}

fn abs_s8(value: i32) -> i32 {
    s8(value).abs()
}

/// Feedback/control registers -> Furnace's 10-bit duty/int macro value.
pub fn mikey_duty(feedback: i32, control: i32) -> i32 {
    (feedback & 0x3F)
        | (((feedback >> 6) & 1) << 7)
        | (((feedback >> 7) & 1) << 8)
        | (((control >> 7) & 1) << 6)
        | (((control >> 5) & 1) << 9)
}

/// Shift/other registers -> the 12-bit LFSR seed.
pub fn mikey_lfsr(shift: i32, other: i32) -> i32 {
    (shift & 0xFF) | ((other & 0xF0) << 4)
}

/// Timer divider value Furnace's Lynx pitch table expects.
///
/// The hardware fires every `(backup+1) << shift` microseconds; Furnace's
/// `MikeyFreqDiv` takes exactly that value (before dividing by the pitch
/// table's 64).
pub fn mikey_period(backup: i32, control: i32) -> i32 {
    1.max((backup + 1) << (control & 7))
}

/// Attenuation register and pan/mute bits -> Furnace's 08xy byte.
///
/// The chip uses an attenuation register when its pan-enable bit is set and
/// falls back to full scale when the bit is clear; the stereo bits mute a
/// side. VGM drivers often leave the pan bits at zero while setting the
/// attenuation bytes to 0xFF (full), so treating a clear bit as "mute" would
/// silence the whole chip.
fn effective_pan(atten: i32, pan_bits: i32, stereo: i32, voice: i32) -> i32 {
    let mut left = if pan_bits & (1 << voice) != 0 {
        (atten >> 4) & 0x0F
    } else {
        0x0F
    };
    let mut right = if pan_bits & (0x10 << voice) != 0 {
        atten & 0x0F
    } else {
        0x0F
    };
    if stereo & (1 << voice) != 0 {
        left = 0;
    }
    if stereo & (0x10 << voice) != 0 {
        right = 0;
    }
    ((left & 0x0F) << 4) | (right & 0x0F)
}

/// One LFSR tone on one module column.
#[derive(Clone, Debug, PartialEq)]
pub struct MikeyHit {
    pub sample_time: i64,
    pub chip_id: i32,
    pub voice: i32,
    pub value: i32, // timer divider at the gate
    pub vol: i32,   // 0..127
    pub clock: u32,
    pub off_sample: i64,
    pub duty: i32,
    pub lfsr: i32,
    pub pan: i32,
    pub freq_pts: Vec<(i64, i32)>,
    pub vol_pts: Vec<(i64, i32)>,
    pub duty_pts: Vec<(i64, i32)>,
    pub lfsr_pts: Vec<(i64, i32)>,
    pub pan_pts: Vec<(i64, i32)>,
}

impl Default for MikeyHit {
    fn default() -> Self {
        Self {
            sample_time: 0,
            chip_id: 0,
            voice: 0,
            value: 0,
            vol: 0,
            clock: 0,
            off_sample: 0,
            duty: 0,
            lfsr: 0,
            pan: 0,
            freq_pts: Vec::new(),
            vol_pts: Vec::new(),
            duty_pts: Vec::new(),
            lfsr_pts: Vec::new(),
            pan_pts: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct MikeyOff {
    pub sample_time: i64,
    pub chip_id: i32,
    pub voice: i32,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct MikeyPcmHit {
    pub time: i64,
    pub voice: i32,
    pub pan: i32,
}

/// One banked 8-bit DAC body.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MikeySample {
    pub chip_id: i32,
    pub voice: i32,
    pub data: Vec<u8>, // signed 8-bit, at `rate`
    pub rate: i32,
    pub hits: Vec<MikeyPcmHit>,
    pub chunk: i32,
}

impl MikeySample {
    pub fn frames(&self) -> usize {
        self.data.len()
    }

    pub fn name(&self) -> String {
        format!("MIKEY{} {:03}", self.chip_id + 1, self.chunk)
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
struct BlankChannel {
    volume: i32,
    feedback: i32,
    output: i32,
    shift: i32,
    backup: i32,
    control: i32,
    counter: i32,
    other: i32,
    active: Option<MikeyHit>,
    setup_sample: i64,
    output_writes: i64,
    tone_writes: i64,
}

impl BlankChannel {
    fn new() -> Self {
        Self {
            setup_sample: -1,
            ..Self::default()
        }
    }
}

#[derive(Clone, Debug, Default)]
struct WarningsSeen {
    glitch: i64,
    negative_volume: i64,
    #[allow(dead_code)]
    clamped: i64,
}

/// Reconstruct one output-register stream as signed 8-bit PCM.
///
/// Returns `(samples, rate, first_sample_time, nominal_gap)`. The write rate
/// is the most common small gap; log jitter and pauses do not move it.
fn pcm_stream(points: &[(i64, i32)]) -> Option<(Vec<i32>, i32, i64, i64)> {
    if points.is_empty() {
        return None;
    }
    let mut gaps: IndexMap<i64, i64> = IndexMap::new();
    for pair in points.windows(2) {
        let gap = pair[1].0 - pair[0].0;
        if 0 < gap && gap <= MIKEY_MAX_GAP {
            *gaps.entry(gap).or_insert(0) += 1;
        }
    }
    if gaps.is_empty() {
        return None;
    }
    // Counter.most_common(1): the first gap seen wins a tie.
    let mut gap: Option<(i64, i64)> = None;
    for (&key, &count) in &gaps {
        if gap.is_none_or(|(_, best)| count > best) {
            gap = Some((key, count));
        }
    }
    let gap = gap.unwrap().0;
    let rate = 1_000.max(44_100.min(py_int(py_round(VGM_RATE as f64 / gap as f64)) as i32));
    let start = points[0].0;
    let end = points[points.len() - 1].0;
    let frames = 1.max(
        py_int(py_round(
            (end - start) as f64 / VGM_RATE as f64 * rate as f64,
        )) + 1,
    );
    let mut out = vec![0i32; frames as usize];
    for (i, &(when, value)) in points.iter().enumerate() {
        let first = 0.max(py_int(py_round(
            (when - start) as f64 / VGM_RATE as f64 * rate as f64,
        )));
        let mut last = if i + 1 < points.len() {
            py_int(py_round(
                (points[i + 1].0 - start) as f64 / VGM_RATE as f64 * rate as f64,
            ))
        } else {
            frames
        };
        last = (first + 1).max(frames.min(last));
        let value = s8(value);
        let mut j = first;
        while j < last {
            out[j as usize] = value;
            j += 1;
        }
    }
    Some((out, rate, start, gap))
}

fn asset_has_signal(samples: &[i32], start: usize, end: usize) -> bool {
    samples[start..end].iter().any(|&v| v != 0)
}

struct Collector {
    total_samples: i64,
    clock: u32,
    chips: Vec<i32>,
    atten: BTreeMap<i32, Vec<i32>>,
    pan_bits: BTreeMap<i32, i32>,
    stereo: BTreeMap<i32, i32>,
    pan_changes: BTreeMap<(i32, i32), Vec<(i64, i32)>>,
    state: BTreeMap<i32, Vec<BlankChannel>>,
    dac_voice: BTreeMap<(i32, i32), bool>,
    hits: Vec<MikeyHit>,
    offs: Vec<MikeyOff>,
    warnings: Vec<String>,
    warnings_seen: WarningsSeen,
}

impl Collector {
    fn new(chips: Vec<i32>, clock: u32, total_samples: i64) -> Self {
        let atten = chips
            .iter()
            .map(|&cid| (cid, vec![0xFF; MIKEY_VOICES]))
            .collect();
        let pan_bits = chips.iter().map(|&cid| (cid, 0xFF)).collect();
        let stereo = chips.iter().map(|&cid| (cid, 0)).collect();
        let state = chips
            .iter()
            .map(|&cid| {
                (
                    cid,
                    (0..MIKEY_VOICES).map(|_| BlankChannel::new()).collect(),
                )
            })
            .collect();
        Self {
            total_samples,
            clock,
            chips,
            atten,
            pan_bits,
            stereo,
            pan_changes: BTreeMap::new(),
            state,
            dac_voice: BTreeMap::new(),
            hits: Vec::new(),
            offs: Vec::new(),
            warnings: Vec::new(),
            warnings_seen: WarningsSeen::default(),
        }
    }

    /// First pass: distinguish a streamed software DAC from an LFSR voice.
    fn classify_dac(&mut self, writes: &[ChipWrite]) {
        for w in writes {
            if w.chip != "mikey" || !(0x20..0x40).contains(&w.reg) {
                continue;
            }
            let ch = ((w.reg >> 3) & 3) as usize;
            let reg = w.reg & 7;
            let st = &mut self.state.get_mut(&(w.chip_id as i32)).unwrap()[ch];
            if reg == REG_OUTPUT {
                st.output_writes += 1;
            } else {
                st.tone_writes += 1;
            }
        }
        let mut mixed = 0i64;
        let chips = self.chips.clone();
        for &cid in &chips {
            for ch in 0..MIKEY_VOICES {
                let st = &self.state[&cid][ch];
                let is_dac = st.output_writes >= MIKEY_DAC_WRITES
                    && st.output_writes > st.tone_writes;
                self.dac_voice.insert((cid, ch as i32), is_dac);
                if is_dac && st.tone_writes >= MIKEY_DAC_WRITES {
                    mixed += 1;
                }
            }
        }
        if mixed != 0 {
            self.warnings.push(format!(
                "{mixed} Mikey voice(s) mix DAC and LFSR writes; the DAC stream wins"
            ));
        }
    }

    fn effective_pan(&self, cid: i32, ch: usize) -> i32 {
        effective_pan(
            self.atten[&cid][ch],
            self.pan_bits[&cid],
            self.stereo[&cid],
            ch as i32,
        )
    }

    fn init_pan(&mut self) {
        let chips = self.chips.clone();
        for &cid in &chips {
            for ch in 0..MIKEY_VOICES {
                self.pan_changes
                    .insert((cid, ch as i32), vec![(0, self.effective_pan(cid, ch))]);
            }
        }
    }

    fn record_pan(&mut self, cid: i32, t: i64) {
        for ch in 0..MIKEY_VOICES {
            let pan = self.effective_pan(cid, ch);
            let pts = self.pan_changes.get_mut(&(cid, ch as i32)).unwrap();
            if pts[pts.len() - 1].1 != pan {
                pts.push((t, pan));
            }
        }
    }

    fn pan_at(&self, cid: i32, ch: i32, t: i64) -> i32 {
        let pts = match self.pan_changes.get(&(cid, ch)) {
            Some(pts) if !pts.is_empty() => pts,
            _ => return self.effective_pan(cid, ch as usize),
        };
        let idx = pts.partition_point(|p| p.0 <= t) as i64 - 1;
        if idx >= 0 {
            pts[idx as usize].1
        } else {
            pts[0].1
        }
    }

    fn close(&mut self, cid: i32, ch: usize, t: i64, emit_off: bool) {
        let hit = self.state.get_mut(&cid).unwrap()[ch].active.take();
        let Some(mut hit) = hit else { return };
        hit.off_sample = t;
        if t - hit.sample_time >= MIKEY_MIN_NOTE {
            self.hits.push(hit);
            if emit_off && t < self.total_samples {
                self.offs.push(MikeyOff {
                    sample_time: t,
                    chip_id: cid,
                    voice: ch as i32,
                });
            }
        } else {
            self.warnings_seen.glitch += 1;
        }
    }

    fn open_hit(&mut self, cid: i32, ch: usize, t: i64) {
        let st = &self.state[&cid][ch];
        let volume = abs_s8(st.volume);
        if st.volume < 0 || st.volume >= 128 {
            self.warnings_seen.negative_volume += 1;
        }
        let volume = volume.clamp(0, 127);
        let pan = self.effective_pan(cid, ch);
        let hit = MikeyHit {
            sample_time: t,
            chip_id: cid,
            voice: ch as i32,
            value: mikey_period(st.backup, st.control),
            vol: volume,
            clock: self.clock,
            duty: mikey_duty(st.feedback, st.control),
            lfsr: mikey_lfsr(st.shift, st.other),
            pan,
            ..MikeyHit::default()
        };
        let st = &mut self.state.get_mut(&cid).unwrap()[ch];
        st.active = Some(hit);
        st.setup_sample = -1;
    }

    fn refresh(&mut self, cid: i32, ch: usize, t: i64) {
        let (volume, control, active) = {
            let st = &self.state[&cid][ch];
            (st.volume, st.control, st.active.is_some())
        };
        let on = abs_s8(volume) > 0 && (control & 0x08) != 0; // timer count enabled
        if !on {
            if active {
                self.close(cid, ch, t, true);
            }
            return;
        }
        if !active {
            self.open_hit(cid, ch, t);
            return;
        }
        let (backup, feedback, shift, other) = {
            let st = &self.state[&cid][ch];
            (st.backup, st.feedback, st.shift, st.other)
        };
        let pan = self.effective_pan(cid, ch);
        let hit = self.state.get_mut(&cid).unwrap()[ch]
            .active
            .as_mut()
            .unwrap();
        let value = mikey_period(backup, control);
        let last = hit.freq_pts.last().map(|p| p.1).unwrap_or(hit.value);
        if value != last {
            if t - hit.sample_time <= MIKEY_SETTLE && hit.freq_pts.is_empty() {
                hit.value = value;
            } else {
                hit.freq_pts.push((t, value));
            }
        }
        let duty = mikey_duty(feedback, control);
        let last = hit.duty_pts.last().map(|p| p.1).unwrap_or(hit.duty);
        if duty != last {
            if t - hit.sample_time <= MIKEY_SETTLE && hit.duty_pts.is_empty() {
                hit.duty = duty;
            } else {
                hit.duty_pts.push((t, duty));
            }
        }
        let lfsr = mikey_lfsr(shift, other);
        let last = hit.lfsr_pts.last().map(|p| p.1).unwrap_or(hit.lfsr);
        if lfsr != last {
            if t - hit.sample_time <= MIKEY_SETTLE && hit.lfsr_pts.is_empty() {
                hit.lfsr = lfsr;
            } else {
                hit.lfsr_pts.push((t, lfsr));
            }
        }
        let volume = abs_s8(volume).clamp(0, 127);
        let last = hit.vol_pts.last().map(|p| p.1).unwrap_or(hit.vol);
        if volume != last {
            hit.vol_pts.push((t, volume));
        }
        let last = hit.pan_pts.last().map(|p| p.1).unwrap_or(hit.pan);
        if pan != last {
            hit.pan_pts.push((t, pan));
        }
    }

    fn refresh_pan(&mut self, cid: i32, t: i64) {
        for ch in 0..MIKEY_VOICES {
            if self.state[&cid][ch].active.is_none() {
                continue;
            }
            let pan = self.effective_pan(cid, ch);
            let hit = self.state.get_mut(&cid).unwrap()[ch]
                .active
                .as_mut()
                .unwrap();
            let last = hit.pan_pts.last().map(|p| p.1).unwrap_or(hit.pan);
            if pan != last {
                hit.pan_pts.push((t, pan));
            }
        }
    }

    /// Walk the register stream and fill `dac_points` in stream order.
    fn scan_writes(&mut self, writes: &[ChipWrite], dac_points: &mut BTreeMap<(i32, i32), Vec<(i64, i32)>>) {
        for w in writes {
            if w.chip != "mikey" {
                continue;
            }
            let cid = w.chip_id as i32;
            let t = w.sample;
            let reg = w.reg;
            if (0x20..0x40).contains(&reg) {
                let ch = ((reg >> 3) & 3) as usize;
                let sub = reg & 7;
                if self.dac_voice.get(&(cid, ch as i32)).copied().unwrap_or(false) {
                    if sub == REG_OUTPUT {
                        dac_points
                            .entry((cid, ch as i32))
                            .or_default()
                            .push((t, w.val as i32));
                    }
                    continue;
                }
                match sub {
                    REG_VOLUME => {
                        let old = self.state[&cid][ch].volume;
                        self.state.get_mut(&cid).unwrap()[ch].volume = w.val as i32;
                        // A signed volume byte: negative means phase-inverted
                        // output.
                        if (s8(w.val as i32) < 0) != (s8(old) < 0) {
                            self.warnings_seen.negative_volume += 1;
                        }
                        let (active, setup_sample, hit_time) = {
                            let st = &self.state[&cid][ch];
                            (
                                st.active.is_some(),
                                st.setup_sample,
                                st.active.as_ref().map(|h| h.sample_time),
                            )
                        };
                        // A full note setup (control/feedback) followed within
                        // a few samples by a volume write is the driver
                        // starting the next note. Vibrato-only backup writes do
                        // not set this flag, so the pitch macro still owns that
                        // motion.
                        if active
                            && 0 <= t - setup_sample
                            && t - setup_sample <= MIKEY_SETUP_GAP
                            && t - hit_time.unwrap() >= MIKEY_MIN_NOTE
                        {
                            self.close(cid, ch, t, false);
                            if abs_s8(self.state[&cid][ch].volume) > 0 {
                                self.open_hit(cid, ch, t);
                            }
                            self.state.get_mut(&cid).unwrap()[ch].setup_sample = -1;
                            continue;
                        }
                        self.refresh(cid, ch, t);
                    }
                    REG_FEEDBACK => {
                        self.state.get_mut(&cid).unwrap()[ch].feedback = w.val as i32;
                        self.state.get_mut(&cid).unwrap()[ch].setup_sample = t;
                        self.refresh(cid, ch, t);
                    }
                    REG_SHIFT => {
                        self.state.get_mut(&cid).unwrap()[ch].shift = w.val as i32;
                        self.refresh(cid, ch, t);
                    }
                    REG_BACKUP => {
                        self.state.get_mut(&cid).unwrap()[ch].backup = w.val as i32;
                        self.refresh(cid, ch, t);
                    }
                    REG_CONTROL => {
                        let old = self.state[&cid][ch].control;
                        {
                            let st = &mut self.state.get_mut(&cid).unwrap()[ch];
                            st.control = w.val as i32;
                            st.setup_sample = t;
                        }
                        // A 0->1 count-enable transition is the driver's key-on.
                        if (old & 0x08) == 0 && (w.val as i32 & 0x08) != 0 {
                            self.close(cid, ch, t, false);
                            if abs_s8(self.state[&cid][ch].volume) > 0 {
                                self.open_hit(cid, ch, t);
                                self.state.get_mut(&cid).unwrap()[ch].setup_sample = -1;
                            }
                            continue;
                        }
                        self.refresh(cid, ch, t);
                    }
                    REG_OTHER => {
                        self.state.get_mut(&cid).unwrap()[ch].other = w.val as i32;
                        self.refresh(cid, ch, t);
                    }
                    _ => {}
                }
                continue;
            }
            if (REG_ATTEN..REG_ATTEN + 4).contains(&reg) {
                let ch = (reg - REG_ATTEN) as usize;
                self.atten.get_mut(&cid).unwrap()[ch] = w.val as i32;
                self.record_pan(cid, t);
                self.refresh_pan(cid, t);
            } else if reg == REG_PAN {
                self.pan_bits.insert(cid, w.val as i32);
                self.record_pan(cid, t);
                self.refresh_pan(cid, t);
            } else if reg == REG_STEREO {
                self.stereo.insert(cid, w.val as i32);
                self.record_pan(cid, t);
                self.refresh_pan(cid, t);
            }
        }
    }

    fn bank_dac(
        &mut self,
        dac_points: &BTreeMap<(i32, i32), Vec<(i64, i32)>>,
        lossy: bool,
    ) -> Vec<MikeySample> {
        // Bank the DAC streams. A fixed 0.25-second grid keeps the sample data
        // byte-exact; an all-zero chunk is a rest and costs no sample.
        let mut samples: Vec<MikeySample> = Vec::new();
        let mut seen: IndexMap<(i32, i32, Vec<u8>), usize> = IndexMap::new();
        for (&(cid, ch), points) in dac_points {
            let Some((pcm, rate, start, gap)) = pcm_stream(points) else {
                self.warnings.push(format!(
                    "Mikey voice {} has DAC writes but no usable timing",
                    ch + 1
                ));
                continue;
            };
            let chunk = 1.max(rate / MIKEY_CHUNK_DIV);
            let mut first = 0usize;
            while first < pcm.len() {
                let last = pcm.len().min(first + chunk as usize);
                if !asset_has_signal(&pcm, first, last) {
                    first += chunk as usize;
                    continue;
                }
                let body: Vec<u8> = pcm[first..last].iter().map(|&v| (v & 0xFF) as u8).collect();
                let when = start
                    + py_int(py_round(
                        first as f64 / rate as f64 * VGM_RATE as f64,
                    ));
                let key = (cid, rate, body.clone());
                if let Some(&found) = seen.get(&key) {
                    samples[found].hits.push(MikeyPcmHit {
                        time: when,
                        voice: ch,
                        pan: self.pan_at(cid, ch, when),
                    });
                    first += chunk as usize;
                    continue;
                }
                let smp = MikeySample {
                    chip_id: cid,
                    voice: ch,
                    data: body,
                    rate,
                    hits: vec![MikeyPcmHit {
                        time: when,
                        voice: ch,
                        pan: self.pan_at(cid, ch, when),
                    }],
                    chunk: samples.len() as i32,
                };
                seen.insert(key, samples.len());
                samples.push(smp);
                first += chunk as usize;
            }
            if gap <= 0 {
                self.warnings
                    .push(format!("Mikey voice {} has no stable DAC rate", ch + 1));
            }
        }
        if lossy && samples.len() > 32 {
            let bodies: Vec<Vec<u8>> = samples.iter().map(|s| s.data.clone()).collect();
            let rates: Vec<i32> = samples.iter().map(|s| s.rate).collect();
            let uses: IndexMap<i32, i64> = (0..samples.len())
                .map(|i| (i as i32, samples[i].hits.len() as i64))
                .collect();
            let (keep, remap) = cluster_fragments(
                &bodies,
                &rates,
                &uses,
                DEFAULT_TARGET,
                DEFAULT_HOT,
            );
            let mut compact: Vec<MikeySample> = keep
                .iter()
                .map(|&i| samples[i as usize].clone())
                .collect();
            for (old, smp) in samples.iter().enumerate() {
                let new = remap[old];
                if new < 0 || keep[new as usize] == old as i32 {
                    continue;
                }
                compact[new as usize].hits.extend(smp.hits.iter().cloned());
            }
            if compact.len() < samples.len() {
                self.warnings.push(format!(
                    "Mikey: lossy sample clustering {} -> {} bodies",
                    samples.len(),
                    compact.len()
                ));
            }
            samples = compact;
        }
        samples
    }
}

/// Walk a Mikey register stream.
///
/// Returns `(tone_hits, tone_offs, dac_samples, warnings)`. The caller binds
/// tone hits to instrument macros and DAC hits to a 120-slot sample map.
pub fn collect_mikey(
    vgm: &VgmFile,
    lossy: bool,
) -> (Vec<MikeyHit>, Vec<MikeyOff>, Vec<MikeySample>, Vec<String>) {
    let mut warnings: Vec<String> = Vec::new();
    let chips: Vec<i32> = vgm
        .writes
        .iter()
        .filter(|w| w.chip == "mikey")
        .map(|w| w.chip_id as i32)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    if chips.is_empty() {
        return (Vec::new(), Vec::new(), Vec::new(), warnings);
    }
    let clock = if vgm.mikey_clock != 0 {
        vgm.mikey_clock
    } else {
        MIKEY_DEFAULT_CLOCK
    };
    let mut c = Collector::new(chips, clock, vgm.total_samples);
    c.classify_dac(&vgm.writes);

    c.init_pan();
    let mut dac_points: BTreeMap<(i32, i32), Vec<(i64, i32)>> = BTreeMap::new();
    c.scan_writes(&vgm.writes, &mut dac_points);

    let total_samples = c.total_samples;
    let chip_ids = c.chips.clone();
    for &cid in &chip_ids {
        for ch in 0..MIKEY_VOICES {
            c.close(cid, ch, total_samples, false);
        }
    }

    c.hits.retain(|h| h.off_sample >= h.sample_time);
    let samples = c.bank_dac(&dac_points, lossy);
    warnings.extend(c.warnings);

    if c.warnings_seen.glitch != 0 {
        warnings.push(format!(
            "{} sub-frame Mikey gate(s) skipped",
            c.warnings_seen.glitch
        ));
    }
    if c.warnings_seen.negative_volume != 0 {
        warnings.push(format!(
            "{} Mikey negative-volume write(s) play with positive amplitude (phase is inverted at the source)",
            c.warnings_seen.negative_volume
        ));
    }
    (c.hits, c.offs, samples, warnings)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(sample: i64, reg: u32, val: u32) -> ChipWrite {
        ChipWrite::new(sample, "mikey".into(), 0, reg, val)
    }

    fn vgm(writes: Vec<ChipWrite>, total: i64) -> VgmFile {
        VgmFile {
            writes,
            mikey_clock: MIKEY_DEFAULT_CLOCK,
            total_samples: total,
            ..Default::default()
        }
    }

    fn tone_writes(volume: i32, backup: i32, control: i32, feedback: i32, off: Option<i64>) -> Vec<ChipWrite> {
        let mut writes = vec![
            write(0, 0x20, volume as u32),
            write(0, 0x21, feedback as u32),
            write(0, 0x23, 0),
            write(0, 0x24, backup as u32),
            write(0, 0x25, control as u32),
            write(0, 0x27, 0),
        ];
        if let Some(off) = off {
            writes.push(write(off, 0x20, 0));
        }
        writes
    }

    #[test]
    fn helper_masks() {
        assert_eq!(mikey_duty(0x48, 0x39), 0x288);
        assert_eq!(mikey_duty(0, 0), 0);
        assert_eq!(mikey_lfsr(0x34, 0xA0), 0xA34);
        assert_eq!(mikey_period(230, 0x39), 462);
        assert_eq!(mikey_period(0, 0), 1);
        assert_eq!(s8(0x80), -128);
        assert_eq!(s8(0xFF), -1);
        assert_eq!(s8(0x7F), 127);
    }

    #[test]
    fn lfsr_note() {
        let (hits, _offs, samples, warnings) =
            collect_mikey(&vgm(tone_writes(90, 230, 0x39, 0x48, Some(8820)), 44_100), false);
        assert_eq!(hits.len(), 1);
        assert!(samples.is_empty());
        assert!(warnings.is_empty());
        let hit = &hits[0];
        assert_eq!(hit.value, 462);
        assert_eq!(hit.vol, 90);
        assert_eq!(hit.duty, 0x288);
        assert_eq!(hit.pan, 0xFF);
        assert_eq!(hit.off_sample, 8820);
    }

    #[test]
    fn volume_and_pitch_motion() {
        let mut writes = tone_writes(90, 230, 0x39, 0x48, None);
        writes.push(write(882, 0x20, 78));
        writes.push(write(1764, 0x24, 162));
        let (hits, _offs, _samples, _warnings) = collect_mikey(&vgm(writes, 44_100), false);
        let hit = &hits[0];
        assert_eq!(hit.vol_pts[0], (882, 78));
        assert_eq!(hit.freq_pts[0], (1764, 326));
    }

    #[test]
    fn setup_before_volume_splits_and_settles() {
        let writes = vec![
            write(0, 0x21, 0x84),
            write(0, 0x24, 230),
            write(0, 0x25, 0x38),
            write(5, 0x20, 40),
            write(5000, 0x21, 7),
            write(5000, 0x24, 244),
            write(5000, 0x25, 0x19),
            write(5005, 0x20, 48),
            write(10000, 0x20, 0),
        ];
        let (hits, _offs, _samples, _warnings) = collect_mikey(&vgm(writes, 12_000), false);
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].off_sample, 5005);
        assert_eq!(hits[1].sample_time, 5005);
        assert_eq!(hits[0].duty, 0x304);
        assert_eq!(hits[1].duty, 7);

        let writes = vec![
            write(0, 0x20, 40),
            write(5, 0x21, 7),
            write(5, 0x25, 0x19),
            write(9000, 0x20, 0),
        ];
        let (hits, _offs, _samples, _warnings) = collect_mikey(&vgm(writes, 10_000), false);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].duty, 7);
        assert!(hits[0].duty_pts.is_empty());
    }

    #[test]
    fn pan_register_and_dac_stream() {
        let mut writes = tone_writes(90, 230, 0x39, 0x48, None);
        writes.push(write(0, 0x40, 0x3C));
        writes.push(write(0, 0x44, 0x00));
        writes.push(write(0, 0x50, 0x00));
        let (hits, _offs, _samples, _warnings) = collect_mikey(&vgm(writes, 44_100), false);
        assert_eq!(hits[0].pan, 0xFF);

        let mut writes = Vec::new();
        for i in 0..1900i32 {
            let value = 64.0 + 60.0 * (i as f64 / 20.0).sin();
            writes.push(write(i as i64 * 6, 0x22, ((value as i64) & 0xFF) as u32));
        }
        let (_hits, _offs, samples, warnings) = collect_mikey(&vgm(writes, 44_100), false);
        assert!(!samples.is_empty());
        assert!(warnings.is_empty());
        assert_eq!(samples[0].rate, 7350);
        assert_eq!(samples[0].data.len(), 1837);
    }
}
