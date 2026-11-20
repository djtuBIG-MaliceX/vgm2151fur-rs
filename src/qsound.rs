//! Capcom QSound: sixteen 8-bit PCM voices and three ADPCM voices.
//!
//! VGM command ``0xC4`` carries a 16-bit big-endian payload followed by the
//! register number.  Data block ``0x8F`` is the sample ROM.  Furnace runs the
//! chip at a fixed 60 MHz; old 4 MHz rips describe the same chip.

use std::collections::{BTreeSet, HashMap};

use crate::parity::{py_int, py_round};
use crate::vgm::{VgmFile, VGM_RATE};

pub const QSOUND_CLOCK: u32 = 60_000_000;
/// The DSP executes one machine cycle every 2*1248 clocks.
pub const QSOUND_OUT_RATE: f64 = QSOUND_CLOCK as f64 / (2.0 * 1248.0);
pub const QSOUND_FREQ_DIV: i32 = 4096;
pub const QSOUND_MAX_FREQ: i32 = 0xEFFF;
pub const QSOUND_PCM_VOICES: usize = 16;
pub const QSOUND_ADPCM_VOICES: usize = 3;
pub const QSOUND_CHANNELS: usize = QSOUND_PCM_VOICES + QSOUND_ADPCM_VOICES;
/// One ADPCM voice is updated every third output sample, one nibble per update.
pub const QSOUND_ADPCM_NIBBLE_RATE: f64 = QSOUND_OUT_RATE / 3.0;
/// Writes this close after a phase or start key-on belong to the same note.
pub const QSOUND_SETTLE: i64 = 256;
pub const QSOUND_MIN_NOTE: i32 = 64;
/// QSound drivers use a 15-byte "loop" for one-shot bodies.
pub const QSOUND_NO_LOOP: i32 = 16;

#[derive(Clone, Debug, PartialEq)]
pub struct QSoundPcmHit {
    pub sample_time: i64,
    pub chip_id: i32,
    pub voice: usize,
    pub start: i32,
    pub end: i32,
    pub r#loop: i32,
    pub bank: i32,
    pub freq: i32,
    pub volume: i32,
    pub pan: i32,
    pub echo: i32,
    /// Index into the sample list returned by [`collect_qsound`].
    pub sample: Option<usize>,
    pub off_sample: i64,
    pub vol_pts: Vec<(i64, i32)>,
    pub freq_pts: Vec<(i64, i32)>,
    pub pan_pts: Vec<(i64, i32)>,
    pub echo_pts: Vec<(i64, i32)>,
}

impl Default for QSoundPcmHit {
    fn default() -> Self {
        QSoundPcmHit {
            sample_time: 0,
            chip_id: 0,
            voice: 0,
            start: 0,
            end: 0,
            r#loop: 0,
            bank: 0,
            freq: 0,
            volume: 0,
            pan: 0,
            echo: 0,
            sample: None,
            off_sample: 0,
            vol_pts: Vec::new(),
            freq_pts: Vec::new(),
            pan_pts: Vec::new(),
            echo_pts: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct QSoundAdpcmHit {
    pub sample_time: i64,
    pub chip_id: i32,
    pub voice: usize,
    pub start: i32,
    pub end: i32,
    pub bank: i32,
    pub volume: i32,
    pub pan: i32,
    /// Index into the sample list returned by [`collect_qsound`].
    pub sample: Option<usize>,
    pub off_sample: i64,
    pub vol_pts: Vec<(i64, i32)>,
    pub pan_pts: Vec<(i64, i32)>,
}

impl Default for QSoundAdpcmHit {
    fn default() -> Self {
        QSoundAdpcmHit {
            sample_time: 0,
            chip_id: 0,
            voice: 0,
            start: 0,
            end: 0,
            bank: 0,
            volume: 0,
            pan: 0,
            sample: None,
            off_sample: 0,
            vol_pts: Vec::new(),
            pan_pts: Vec::new(),
        }
    }
}

/// One entry of a [`QSoundSample`]'s `hits` list.  Python keeps the live hit
/// objects there; the Rust port keeps clones after the run has finished.
#[derive(Clone, Debug, PartialEq)]
pub enum QSoundHit {
    Pcm(QSoundPcmHit),
    Adpcm(QSoundAdpcmHit),
}

#[derive(Clone, Debug, PartialEq)]
pub struct QSoundSample {
    pub kind: &'static str, // "pcm" | "adpcm"
    pub data: Vec<u8>,
    pub loop_start: i32,
    pub loop_end: i32,
    pub hits: Vec<QSoundHit>,
}

impl Default for QSoundSample {
    fn default() -> Self {
        QSoundSample {
            kind: "pcm",
            data: Vec::new(),
            loop_start: -1,
            loop_end: -1,
            hits: Vec::new(),
        }
    }
}

impl QSoundSample {
    pub fn rate(&self) -> f64 {
        if self.kind == "adpcm" {
            QSOUND_ADPCM_NIBBLE_RATE
        } else {
            QSOUND_OUT_RATE
        }
    }
}

/// The rate Furnace's fixed 60 MHz QSound core will use.
pub fn qsound_effective_clock(clock: u32) -> u32 {
    if clock != 0 && clock < 5_000_000 {
        return clock * 15;
    }
    QSOUND_CLOCK
}

pub fn qsound_freq_hz(freq: i32) -> f64 {
    QSOUND_OUT_RATE * freq.max(1) as f64 / QSOUND_FREQ_DIV as f64
}

/// (linear pan 0-32, Furnace surround flag) for a pan register.
pub fn qsound_pan(raw: i32) -> (i32, i32) {
    let surround = if (raw & 0x40) != 0 { 0 } else { 1 };
    let p = raw & 0x3F;
    let linear = if surround == 0 {
        p
    } else {
        (p - 0x10).clamp(0, 32)
    };
    (linear, surround)
}

/// Closest (left, right) 4-bit split for a linear 0-32 pan.
pub fn qsound_pan_split(linear: i32) -> (i32, i32) {
    let linear = linear.clamp(0, 32);
    let mut best: (i32, i32) = (0, 0);
    let mut best_err: i32 = 1 << 30;
    for left in 0..16i32 {
        for right in 0..16i32 {
            let value = if left == right {
                16
            } else {
                let diff = right - left;
                // Furnace truncates `pan*range` to int; match that exactly.
                py_int((1.0 + diff as f64 / left.max(right) as f64) * 0.5 * 32.0) as i32
            };
            let err = (value - linear).abs();
            if err < best_err
                || (err == best_err && (left - right).abs() < (best.0 - best.1).abs())
            {
                best = (left, right);
                best_err = err;
            }
        }
    }
    best
}

/// Assemble QSound 0x8F slices and return (ROM, covered intervals).
pub fn assemble_qsound_rom(vgm: &VgmFile) -> (Vec<u8>, Vec<(i64, i64)>) {
    let mut slices: Vec<&crate::vgm::RomSlice> =
        vgm.roms.iter().filter(|r| r.kind == "qsound").collect();
    if slices.is_empty() {
        return (Vec::new(), Vec::new());
    }
    let size = slices
        .iter()
        .map(|r| r.rom_size)
        .chain(
            slices
                .iter()
                .map(|r| r.start + r.data.len() as i64),
        )
        .chain(std::iter::once(0))
        .max()
        .unwrap_or(0);
    let mut rom = vec![0u8; size as usize];
    let mut covered: Vec<(i64, i64)> = Vec::new();
    slices.sort_by_key(|r| r.start);
    for sl in slices {
        if sl.data.is_empty() {
            continue;
        }
        let start = size.min(0.max(sl.start));
        let end = size.min(start + sl.data.len() as i64);
        let take = (end - start) as usize;
        if take != 0 {
            rom[start as usize..end as usize].copy_from_slice(&sl.data[..take]);
        }
        covered.push((start, end));
    }
    covered.sort();
    let mut merged: Vec<(i64, i64)> = Vec::new();
    for (a, b) in covered {
        if let Some(last) = merged.last_mut() {
            if a <= last.1 {
                last.1 = last.1.max(b);
                continue;
            }
        }
        merged.push((a, b));
    }
    (rom, merged)
}

fn _covered(intervals: &[(i64, i64)], start: i64, end: i64) -> bool {
    if end <= start {
        return false;
    }
    for &(a, b) in intervals {
        if a <= start && end <= b {
            return true;
        }
        if a > start {
            break;
        }
    }
    false
}

fn _volume_to_255(raw: i32, peak: i32) -> i32 {
    if raw <= 0 {
        return 0;
    }
    let scale = peak.max(16383);
    (py_int(py_round(raw as f64 * 255.0 / scale as f64)) as i32).clamp(1, 255)
}

pub fn _delay_fx(raw: i32) -> i32 {
    (raw - 1370).clamp(0, 2725)
}

pub fn _feedback_fx(raw: i32) -> i32 {
    // Furnace's 10xx effect is an 8-bit level written as value<<6.
    (py_int(py_round(raw as f64 / 64.0)) as i32).clamp(0, 255)
}

pub fn _echo_fx(raw: i32) -> i32 {
    // QSound channel echo register is written as value<<7.
    (raw >> 7).clamp(0, 255)
}

pub use self::_delay_fx as qsound_delay_fx;
pub use self::_echo_fx as qsound_echo_fx;
pub use self::_feedback_fx as qsound_feedback_fx;

/// (body_start, body_end, loop_start, loop_end, wraps).
///
/// `wraps` means the sample runs from `start` to the end of the 64 KB bank
/// and then from 0 to `end`.  Real QSound hardware wraps its 16-bit address
/// counter; several drivers use that for a one-shot body.
fn _effective_body(start: i32, end: i32, r#loop: i32) -> Option<(i32, i32, i32, i32, bool)> {
    let start = start & 0xFFFF;
    let end = end & 0xFFFF;
    if r#loop > QSOUND_NO_LOOP {
        if start < end {
            let length = end - start;
            let loop_start = end - r#loop - start;
            let loop_end = length;
            if 0 <= loop_start && loop_start < loop_end {
                return Some((start, end, loop_start, loop_end, false));
            }
            return Some((start, end, -1, -1, false));
        }
        // A loop length that spans the apparent body start selects the loop
        // start directly; this is how a driver pads the end of a looping body.
        let body_start = (end - r#loop) & 0xFFFF;
        if body_start < end {
            return Some((body_start, end, 0, end - body_start, false));
        }
        return None;
    }
    if start < end {
        return Some((start, end, -1, -1, false));
    }
    // One-shot wrap-around body.
    Some((start, end, -1, -1, true))
}

/// Python `rom[start:end]` for non-negative indices, padded/clamped.
fn rom_slice(rom: &[u8], start: i64, end: i64) -> Vec<u8> {
    let len = rom.len() as i64;
    let s = start.max(0).min(len);
    let e = end.max(0).min(len);
    if e <= s {
        return Vec::new();
    }
    rom[s as usize..e as usize].to_vec()
}

#[derive(Clone, Debug)]
struct PcmVoiceState {
    bank: i32,
    start: i32,
    end: i32,
    r#loop: i32,
    freq: i32,
    vol: i32,
    pan: i32,
    echo: i32,
    enabled: bool,
    phase_t: i64,
    start_t: i64,
}

struct PcmCollect<'a> {
    writes: &'a [(i64, i32, i32)],
    rom: &'a [u8],
    covered: &'a [(i64, i64)],
    chip_id: i32,
    voice: usize,
    peak_volume: i32,
    warnings: &'a mut Vec<String>,
    hits: Vec<QSoundPcmHit>,
    samples: Vec<QSoundSample>,
    seen: HashMap<(Vec<u8>, i32, i32), usize>,
    state: PcmVoiceState,
    active: Option<usize>,
}

impl<'a> PcmCollect<'a> {
    fn new(
        writes: &'a [(i64, i32, i32)],
        rom: &'a [u8],
        covered: &'a [(i64, i64)],
        chip_id: i32,
        voice: usize,
        peak_volume: i32,
        warnings: &'a mut Vec<String>,
    ) -> Self {
        PcmCollect {
            writes,
            rom,
            covered,
            chip_id,
            voice,
            peak_volume,
            warnings,
            hits: Vec::new(),
            samples: Vec::new(),
            seen: HashMap::new(),
            state: PcmVoiceState {
                bank: 0x8000,
                start: 0,
                end: 0,
                r#loop: 0,
                freq: 0,
                vol: 0,
                pan: 0x120,
                echo: 0,
                enabled: false,
                phase_t: -100000,
                start_t: -100000,
            },
            active: None,
        }
    }

    fn close(&mut self, when: i64, off: bool) {
        if let Some(idx) = self.active {
            if off && when > self.hits[idx].sample_time {
                self.hits[idx].off_sample = when;
            }
        }
        self.active = None;
    }

    #[allow(clippy::too_many_arguments)]
    fn open_hit(
        &mut self,
        when: i64,
        start: i32,
        end: i32,
        r#loop: i32,
        bank: i32,
        freq: i32,
        vol: i32,
        pan: i32,
        echo: i32,
    ) {
        let Some((bstart, bend, mut loop_start, mut loop_end, wraps)) =
            _effective_body(start, end, r#loop)
        else {
            return;
        };
        let base = (((bank & 0x7FFF) << 16) & 0xFFFFFF) as i64;
        let (mut data, want, covered_here) = if wraps {
            let mut first = rom_slice(self.rom, base + bstart as i64, base + 0x10000);
            first.extend_from_slice(&rom_slice(self.rom, base, base + bend as i64));
            let want = (0x10000 - bstart) + bend;
            let covered_here = _covered(self.covered, base + bstart as i64, base + 0x10000)
                && _covered(self.covered, base, base + bend as i64);
            (first, want, covered_here)
        } else {
            let body = rom_slice(self.rom, base + bstart as i64, base + bend as i64);
            let want = bend - bstart;
            let covered_here =
                _covered(self.covered, base + bstart as i64, base + bend as i64);
            (body, want, covered_here)
        };
        if want <= 0 {
            return;
        }
        if data.len() < want as usize {
            data.resize(want as usize, 0);
        }
        // Furnace's QSound PCM voices are limited to 64 KB per sample.
        if data.len() > 65520 {
            data.truncate(65520);
            self.warnings.push(format!(
                "QSound voice {} sample is longer than Furnace's 64 KB limit; it was truncated",
                self.voice + 1
            ));
        }
        if loop_start >= 0 && loop_end as i64 > data.len() as i64 {
            loop_start = -1;
            loop_end = -1;
        }
        if !covered_here {
            self.warnings.push(format!(
                "QSound voice {} sample range is not covered by the ROM blocks; it is silent",
                self.voice + 1
            ));
        }
        let key = (data.clone(), loop_start, loop_end);
        let smp = match self.seen.get(&key) {
            Some(&idx) => idx,
            None => {
                let idx = self.samples.len();
                self.samples.push(QSoundSample {
                    kind: "pcm",
                    data: data.clone(),
                    loop_start,
                    loop_end,
                    hits: Vec::new(),
                });
                self.seen.insert(key, idx);
                idx
            }
        };
        let mut hit = QSoundPcmHit {
            sample_time: when,
            chip_id: self.chip_id,
            voice: self.voice,
            start: bstart,
            end: bend,
            r#loop,
            bank,
            freq: freq.max(1),
            volume: _volume_to_255(vol, self.peak_volume),
            pan,
            echo,
            sample: Some(smp),
            off_sample: 0,
            vol_pts: Vec::new(),
            freq_pts: Vec::new(),
            pan_pts: Vec::new(),
            echo_pts: Vec::new(),
        };
        let rate = qsound_freq_hz(freq);
        if loop_start < 0 {
            hit.off_sample =
                when + py_int(py_round(data.len() as f64 / rate * VGM_RATE as f64));
        }
        self.hits.push(hit);
        self.active = Some(self.hits.len() - 1);
    }

    /// Open a note, looking ahead for the driver's companion writes.
    fn trigger(&mut self, t: i64, i: usize) {
        self.close(t, true);
        let mut start = self.state.start;
        let mut end = self.state.end;
        let mut r#loop = self.state.r#loop;
        let mut bank = self.state.bank;
        let mut freq = self.state.freq;
        let mut vol = self.state.vol;
        let mut pan = self.state.pan;
        let mut echo = self.state.echo;
        let mut first_freq = false;
        let mut first_vol = false;
        let mut j = i + 1;
        while j < self.writes.len() {
            let (tt, rr, vv) = self.writes[j];
            if tt - t > QSOUND_SETTLE {
                break;
            }
            if rr == 3 && (vv & 0x8000) != 0 {
                // A start and phase written for the same note are one event.
                j += 1;
                continue;
            }
            match rr {
                1 => start = vv,
                5 => end = vv,
                4 => r#loop = vv,
                8 => bank = vv,
                2 => {
                    if vv != 0 && !first_freq {
                        // A zero frequency is the driver clearing the previous
                        // note; the following nonzero write is this note's pitch.
                        freq = vv;
                        first_freq = true;
                    }
                }
                6 => {
                    if vv != 0 && !first_vol {
                        vol = vv;
                        first_vol = true;
                    }
                }
                9 => pan = vv,
                10 => echo = vv,
                _ => {}
            }
            j += 1;
        }
        self.state.start = start;
        self.state.end = end;
        self.state.r#loop = r#loop;
        self.state.bank = bank;
        self.state.freq = freq;
        self.state.vol = vol;
        self.state.pan = pan;
        self.state.echo = echo;
        self.open_hit(t, start, end, r#loop, bank, freq, vol, pan, echo);
    }

    fn run(&mut self) {
        let mut i = 0;
        while i < self.writes.len() {
            let (t, reg, val) = self.writes[i];
            if reg == 8 {
                // bank for this voice
                self.state.bank = val;
                i += 1;
                continue;
            }
            if reg == 3 {
                if (val & 0x8000) == 0 {
                    self.state.enabled = false;
                    if self.active.is_some() {
                        self.close(t, true);
                    }
                    i += 1;
                    continue;
                }
                self.state.enabled = true;
                // A phase write paired with a very recent start write belongs to
                // the note that start already opened.
                if !(self.active.is_some()
                    && t - self.state.start_t <= QSOUND_SETTLE)
                {
                    self.trigger(t, i);
                }
                self.state.phase_t = t;
                i += 1;
                continue;
            }
            if reg == 1 {
                self.state.start = val;
                // QSound keeps a voice enabled and many drivers rewrite the start
                // address to retrigger it.  Treat that as a key-on unless a phase
                // write opened the same note just before.
                if self.state.enabled && t - self.state.phase_t > QSOUND_SETTLE {
                    self.trigger(t, i);
                    self.state.start_t = t;
                }
                i += 1;
                continue;
            }
            if reg == 2 {
                self.state.freq = val;
                if let Some(idx) = self.active {
                    self.hits[idx].freq_pts.push((t, val.max(1)));
                }
                i += 1;
                continue;
            }
            if reg == 4 {
                self.state.r#loop = val;
                i += 1;
                continue;
            }
            if reg == 5 {
                self.state.end = val;
                i += 1;
                continue;
            }
            if reg == 6 {
                self.state.vol = val;
                if let Some(idx) = self.active {
                    let level = _volume_to_255(val, self.peak_volume);
                    self.hits[idx].vol_pts.push((t, level));
                    if level == 0 {
                        self.close(t, true);
                    }
                }
                i += 1;
                continue;
            }
            if reg == 9 {
                self.state.pan = val;
                if let Some(idx) = self.active {
                    self.hits[idx].pan_pts.push((t, val));
                }
                i += 1;
                continue;
            }
            if reg == 10 {
                self.state.echo = val;
                if let Some(idx) = self.active {
                    self.hits[idx].echo_pts.push((t, val));
                }
                i += 1;
                continue;
            }
            i += 1;
        }
    }
}

#[derive(Clone, Debug)]
struct AdpcmVoiceState {
    start: i32,
    end: i32,
    bank: i32,
    vol: i32,
    pan: i32,
}

struct AdpcmCollect<'a> {
    writes: &'a [(i64, i32, i32)],
    rom: &'a [u8],
    covered: &'a [(i64, i64)],
    chip_id: i32,
    voice: usize,
    peak_volume: i32,
    warnings: &'a mut Vec<String>,
    hits: Vec<QSoundAdpcmHit>,
    samples: Vec<QSoundSample>,
    seen: HashMap<Vec<u8>, usize>,
    state: AdpcmVoiceState,
    active: Option<usize>,
    previous_key: i32,
}

impl<'a> AdpcmCollect<'a> {
    fn new(
        writes: &'a [(i64, i32, i32)],
        rom: &'a [u8],
        covered: &'a [(i64, i64)],
        chip_id: i32,
        voice: usize,
        peak_volume: i32,
        warnings: &'a mut Vec<String>,
    ) -> Self {
        AdpcmCollect {
            writes,
            rom,
            covered,
            chip_id,
            voice,
            peak_volume,
            warnings,
            hits: Vec::new(),
            samples: Vec::new(),
            seen: HashMap::new(),
            state: AdpcmVoiceState {
                start: 0,
                end: 0,
                bank: 0x8000,
                vol: 0,
                pan: 0x120,
            },
            active: None,
            previous_key: 0,
        }
    }

    fn run(&mut self) {
        let mut i = 0;
        while i < self.writes.len() {
            let (t, reg, val) = self.writes[i];
            if reg == 4 {
                self.state.start = val;
                i += 1;
                continue;
            }
            if reg == 5 {
                self.state.end = val;
                i += 1;
                continue;
            }
            if reg == 6 {
                self.state.bank = val;
                i += 1;
                continue;
            }
            if reg == 7 {
                self.state.vol = val;
                if let Some(idx) = self.active {
                    let level = _volume_to_255(val, self.peak_volume);
                    self.hits[idx].vol_pts.push((t, level));
                    if level == 0 {
                        self.hits[idx].off_sample = t;
                        self.active = None;
                    }
                }
                i += 1;
                continue;
            }
            if reg == 8 {
                self.state.pan = val;
                if let Some(idx) = self.active {
                    self.hits[idx].pan_pts.push((t, val));
                }
                i += 1;
                continue;
            }
            if reg == 0 {
                // key-on flag
                if val != 0 && self.previous_key == 0 {
                    if self.active.is_some() {
                        self.active = None;
                    }
                    let mut start = self.state.start;
                    let mut end = self.state.end;
                    let mut bank = self.state.bank;
                    let mut vol = self.state.vol;
                    let mut pan = self.state.pan;
                    let mut j = i + 1;
                    let mut first_vol = false;
                    while j < self.writes.len() {
                        let (tt, rr, vv) = self.writes[j];
                        if tt - t > QSOUND_SETTLE {
                            break;
                        }
                        if rr == 0 && vv != 0 {
                            break;
                        }
                        match rr {
                            4 => start = vv,
                            5 => end = vv,
                            6 => bank = vv,
                            7 => {
                                if !first_vol {
                                    vol = vv;
                                    first_vol = true;
                                }
                            }
                            8 => pan = vv,
                            _ => {}
                        }
                        j += 1;
                    }
                    self.state.start = start;
                    self.state.end = end;
                    self.state.bank = bank;
                    self.state.vol = vol;
                    self.state.pan = pan;
                    if end > start + 1 && (bank & 0x8000) != 0 {
                        let base = (((bank & 0x7FFF) << 16) & 0xFFFFFF) as i64;
                        let mut data =
                            rom_slice(self.rom, base + start as i64, base + end as i64);
                        if data.len() < (end - start) as usize {
                            data.resize((end - start) as usize, 0);
                        }
                        if !_covered(
                            self.covered,
                            base + start as i64,
                            base + end as i64,
                        ) {
                            self.warnings.push(format!(
                                "QSound ADPCM voice {} sample range is not covered by the ROM blocks; it is silent",
                                self.voice + 1
                            ));
                        }
                        let smp = match self.seen.get(&data) {
                            Some(&idx) => idx,
                            None => {
                                let idx = self.samples.len();
                                self.samples.push(QSoundSample {
                                    kind: "adpcm",
                                    data: data.clone(),
                                    loop_start: -1,
                                    loop_end: -1,
                                    hits: Vec::new(),
                                });
                                self.seen.insert(data.clone(), idx);
                                idx
                            }
                        };
                        let hit = QSoundAdpcmHit {
                            sample_time: t,
                            chip_id: self.chip_id,
                            voice: self.voice,
                            start,
                            end,
                            bank,
                            volume: _volume_to_255(vol, self.peak_volume),
                            pan,
                            sample: Some(smp),
                            off_sample: t
                                + py_int(py_round(
                                    (2 * data.len()) as f64
                                        / QSOUND_ADPCM_NIBBLE_RATE
                                        * VGM_RATE as f64,
                                )),
                            vol_pts: Vec::new(),
                            pan_pts: Vec::new(),
                        };
                        self.hits.push(hit);
                        self.active = Some(self.hits.len() - 1);
                    }
                    i += 1;
                    self.previous_key = val;
                    continue;
                }
                self.previous_key = val;
                i += 1;
                continue;
            }
            i += 1;
        }
    }
}

/// Walk the QSound register stream.
pub fn collect_qsound(
    vgm: &VgmFile,
) -> (
    Vec<QSoundPcmHit>,
    Vec<QSoundAdpcmHit>,
    Vec<QSoundSample>,
    Vec<String>,
) {
    let mut warnings: Vec<String> = Vec::new();
    let chips: BTreeSet<i32> = vgm
        .writes
        .iter()
        .filter(|w| w.chip == "qsound")
        .map(|w| w.chip_id as i32)
        .collect();
    if chips.is_empty() {
        return (Vec::new(), Vec::new(), Vec::new(), warnings);
    }
    let (rom, covered) = assemble_qsound_rom(vgm);
    if rom.is_empty() {
        warnings.push("no QSound ROM block (0x8F); sample voices will be silent".to_string());
    }
    let mut peak_volume = 0i32;
    for w in &vgm.writes {
        if w.chip != "qsound" {
            continue;
        }
        let reg = w.reg as i32;
        if reg < 0x80 && (reg & 7) == 6 {
            peak_volume = peak_volume.max(w.val as i32);
        } else if (0xCD..=0xD5).contains(&reg) && (reg - 0xCD) % 4 == 0 {
            peak_volume = peak_volume.max(w.val as i32);
        }
    }
    let mut pcm_hits: Vec<QSoundPcmHit> = Vec::new();
    let mut adpcm_hits: Vec<QSoundAdpcmHit> = Vec::new();
    let mut samples: Vec<QSoundSample> = Vec::new();
    for cid in &chips {
        let mut voice_writes: Vec<Vec<(i64, i32, i32)>> =
            vec![Vec::new(); QSOUND_CHANNELS];
        for w in &vgm.writes {
            if w.chip != "qsound" || w.chip_id as i32 != *cid {
                continue;
            }
            let reg = w.reg as i32;
            let val = w.val as i32;
            if reg < 0x80 {
                let ch = reg >> 3;
                let sub = reg & 7;
                if sub == 0 {
                    // Bank register applies to the next channel, matching the
                    // QSound core's rotated map.
                    voice_writes[((ch + 1) % 16) as usize]
                        .push((w.sample, 8, val));
                } else {
                    voice_writes[ch as usize].push((w.sample, sub, val));
                }
            } else if (0x80..0x90).contains(&reg) {
                voice_writes[(reg - 0x80) as usize].push((w.sample, 9, val));
            } else if (0xBA..0xCA).contains(&reg) {
                voice_writes[(reg - 0xBA) as usize].push((w.sample, 10, val));
            } else if (0x90..=0x92).contains(&reg) {
                voice_writes[16 + (reg - 0x90) as usize].push((w.sample, 8, val));
            } else if (0xCA..=0xD5).contains(&reg) {
                let base = 0xCA + 4 * ((reg - 0xCA) / 4);
                let index = reg - base;
                let v = (reg - 0xCA) / 4;
                voice_writes[16 + v as usize]
                    .push((w.sample, index + 4, val));
            } else if (0xD6..=0xD8).contains(&reg) {
                voice_writes[16 + (reg - 0xD6) as usize]
                    .push((w.sample, 0, val));
            }
        }
        for voice in 0..16 {
            let mut coll = PcmCollect::new(
                &voice_writes[voice],
                &rom,
                &covered,
                *cid,
                voice,
                peak_volume,
                &mut warnings,
            );
            coll.run();
            let base = samples.len();
            for h in &mut coll.hits {
                if let Some(s) = h.sample {
                    h.sample = Some(s + base);
                }
            }
            pcm_hits.extend(coll.hits);
            samples.extend(coll.samples);
        }
        for voice in 0..3 {
            let mut coll = AdpcmCollect::new(
                &voice_writes[16 + voice],
                &rom,
                &covered,
                *cid,
                voice,
                peak_volume,
                &mut warnings,
            );
            coll.run();
            let base = samples.len();
            for h in &mut coll.hits {
                if let Some(s) = h.sample {
                    h.sample = Some(s + base);
                }
            }
            adpcm_hits.extend(coll.hits);
            samples.extend(coll.samples);
        }
    }
    pcm_hits.sort_by(|a, b| {
        (a.sample_time, a.chip_id, a.voice).cmp(&(b.sample_time, b.chip_id, b.voice))
    });
    adpcm_hits.sort_by(|a, b| {
        (a.sample_time, a.chip_id, a.voice).cmp(&(b.sample_time, b.chip_id, b.voice))
    });
    // Python keeps the live hit objects on each sample; rebuild the lists from
    // the final hits so later register writes (off_sample) are reflected.
    for hit in &pcm_hits {
        if let Some(idx) = hit.sample {
            samples[idx].hits.push(QSoundHit::Pcm(hit.clone()));
        }
    }
    for hit in &adpcm_hits {
        if let Some(idx) = hit.sample {
            samples[idx].hits.push(QSoundHit::Adpcm(hit.clone()));
        }
    }
    let mut deduped: Vec<String> = Vec::new();
    for message in warnings {
        if !deduped.contains(&message) {
            deduped.push(message);
        }
    }
    (pcm_hits, adpcm_hits, samples, deduped)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vgm::{ChipWrite, RomSlice, VgmFile};

    fn write(vgm: &mut VgmFile, t: i64, reg: u32, val: u32) {
        vgm.writes.push(ChipWrite {
            sample: t,
            chip: "qsound",
            chip_id: 0,
            reg,
            val,
        });
    }

    fn rom_data() -> Vec<u8> {
        (0..0x1000).map(|i| ((i * 3) & 0xFF) as u8).collect()
    }

    fn pcm_song() -> VgmFile {
        let mut vgm = VgmFile::default();
        vgm.roms.push(RomSlice {
            chip: 0,
            rom_size: 0x1000,
            start: 0,
            data: rom_data(),
            kind: "qsound",
            time: 0,
        });
        write(&mut vgm, 0, 0x78, 0x8000); // bank for voice 0
        write(&mut vgm, 1, 0x01, 0x0100);
        write(&mut vgm, 2, 0x05, 0x0200);
        write(&mut vgm, 3, 0x04, 0x40);
        write(&mut vgm, 5, 0x03, 0x8000);
        write(&mut vgm, 6, 0x02, 4096);
        write(&mut vgm, 7, 0x06, 1200);
        write(&mut vgm, 8, 0x80, 0x14D);
        write(&mut vgm, 9, 0xBA, 896);
        write(&mut vgm, 100, 0x06, 0);
        vgm
    }

    #[test]
    fn pcm_hit_and_linear_pan() {
        let (pcm, adpcm, samples, warnings) = collect_qsound(&pcm_song());
        assert!(warnings.is_empty());
        assert_eq!(pcm.len(), 1);
        assert!(adpcm.is_empty());
        let hit = &pcm[0];
        assert_eq!(hit.freq, 4096);
        assert_eq!(hit.start, 0x0100);
        assert_eq!(hit.end, 0x0200);
        assert_eq!(samples[0].loop_start, 0x100 - 0x40);
        assert_eq!(samples[0].loop_end, 0x100);
        assert_eq!(samples[0].kind, "pcm");
        assert_eq!(samples[0].data.len(), 0x100);
        assert_eq!(samples[0].hits.len(), 1);
        assert_eq!(hit.volume, 19);
        assert_eq!(hit.off_sample, 100);
        assert_eq!(qsound_pan(0x14D), (13, 0));
        assert_eq!(hit.pan_pts, vec![(8, 0x14D)]);
        assert_eq!(hit.echo_pts, vec![(9, 896)]);
    }

    #[test]
    fn start_address_retriggers_while_enabled() {
        let mut vgm = VgmFile::default();
        vgm.roms.push(RomSlice {
            chip: 0,
            rom_size: 0x1000,
            start: 0,
            data: rom_data(),
            kind: "qsound",
            time: 0,
        });
        write(&mut vgm, 0, 0x78, 0x8000);
        write(&mut vgm, 1, 0x01, 0x0100);
        write(&mut vgm, 2, 0x05, 0x0200);
        write(&mut vgm, 3, 0x04, 0x40);
        write(&mut vgm, 4, 0x06, 1200);
        write(&mut vgm, 5, 0x02, 4096);
        write(&mut vgm, 6, 0x03, 0x8000);
        // Phase stays enabled; a start rewrite is the next key-on.
        write(&mut vgm, 1000, 0x01, 0x0100);
        write(&mut vgm, 1001, 0x06, 900);
        write(&mut vgm, 1002, 0x02, 4096);

        let (pcm, _adpcm, _samples, _warnings) = collect_qsound(&vgm);
        assert_eq!(pcm.len(), 2);
        assert_eq!(pcm[0].sample_time, 6);
        assert_eq!(pcm[1].sample_time, 1000);
        assert_eq!(pcm[0].off_sample, 1000);
        assert_eq!(pcm[1].volume, 14);
    }

    #[test]
    fn adpcm_hit() {
        let mut vgm = VgmFile::default();
        vgm.roms.push(RomSlice {
            chip: 0,
            rom_size: 0x10000,
            start: 0,
            data: rom_data(),
            kind: "qsound",
            time: 0,
        });
        write(&mut vgm, 0, 0xCC, 0x8000); // ADPCM0 bank
        write(&mut vgm, 1, 0xCA, 0x0010);
        write(&mut vgm, 2, 0xCB, 0x0020);
        write(&mut vgm, 3, 0xD6, 0);
        write(&mut vgm, 4, 0xD6, 1);
        write(&mut vgm, 5, 0xCD, 16383);

        let (pcm, adpcm, samples, warnings) = collect_qsound(&vgm);
        assert!(warnings.is_empty());
        assert!(pcm.is_empty());
        assert_eq!(adpcm.len(), 1);
        assert_eq!(samples.len(), 1);
        assert_eq!(samples[0].kind, "adpcm");
        assert_eq!(samples[0].data.len(), 16);
        assert!(adpcm[0].off_sample > 0);
        assert_eq!(adpcm[0].start, 0x10);
        assert_eq!(adpcm[0].end, 0x20);
        assert_eq!(samples[0].hits.len(), 1);
        assert_eq!(adpcm[0].vol_pts, vec![(5, 255)]);
    }

    #[test]
    fn pan_split_and_effects_match_furnace() {
        assert_eq!(qsound_pan_split(0), (1, 0));
        assert_eq!(qsound_pan_split(32), (0, 1));
        assert_eq!(qsound_pan_split(16), (0, 0));
        assert_eq!(qsound_pan(0x20), (16, 1));
        assert_eq!(qsound_pan(0x50), (16, 0));

        assert_eq!(qsound_effective_clock(4_000_000), 60_000_000);
        assert_eq!(qsound_effective_clock(0), 60_000_000);
        assert_eq!(qsound_effective_clock(12_000_000), 60_000_000);
        assert_eq!(qsound_freq_hz(4096), QSOUND_OUT_RATE);
        assert_eq!(qsound_freq_hz(0), QSOUND_OUT_RATE / 4096.0);

        assert_eq!(qsound_delay_fx(1370), 0);
        assert_eq!(qsound_delay_fx(5000), 2725);
        assert_eq!(qsound_feedback_fx(255), 4);
        assert_eq!(qsound_feedback_fx(16383), 255);
        assert_eq!(qsound_echo_fx(0x3F80), 127);
        assert_eq!(qsound_echo_fx(0x8000), 255);

        assert_eq!(
            _effective_body(0x100, 0x200, 0x40),
            Some((0x100, 0x200, 0xC0, 0x100, false))
        );
        assert_eq!(
            _effective_body(0x200, 0x100, 0),
            Some((0x200, 0x100, -1, -1, true))
        );
        assert_eq!(
            _effective_body(0x10, 0x100, 20),
            Some((0x10, 0x100, 220, 240, false))
        );
        assert_eq!(
            _effective_body(0x200, 0x100, 0x40),
            Some((0xC0, 0x100, 0, 0x40, false))
        );
        assert_eq!(
            _effective_body(0x200, 0x100, 1),
            Some((0x200, 0x100, -1, -1, true))
        );
        assert_eq!(
            _effective_body(0x200, 0x100, 0x100),
            Some((0, 0x100, 0, 0x100, false))
        );
        assert_eq!(_effective_body(0x200, 0x100, 0x200), None);
    }

    #[test]
    fn qsound_rom_assembly_merges_and_reads_wraps() {
        let mut vgm = VgmFile::default();
        vgm.roms.push(RomSlice {
            chip: 0,
            rom_size: 0x20,
            start: 0x14,
            data: vec![9, 9],
            kind: "qsound",
            time: 0,
        });
        vgm.roms.push(RomSlice {
            chip: 0,
            rom_size: 0x20,
            start: 0x10,
            data: vec![1, 2, 3, 4],
            kind: "qsound",
            time: 0,
        });
        let (rom, covered) = assemble_qsound_rom(&vgm);
        assert_eq!(rom.len(), 0x20);
        assert_eq!(&rom[0x10..0x16], &[1, 2, 3, 4, 9, 9]);
        assert_eq!(covered, vec![(0x10, 0x16)]);

        // One-shot body from start to the end of the bank, then 0..end.
        let mut wrap = VgmFile::default();
        wrap.roms.push(RomSlice {
            chip: 0,
            rom_size: 0x10000,
            start: 0,
            data: vec![10, 11, 12, 13],
            kind: "qsound",
            time: 0,
        });
        write(&mut wrap, 0, 0x78, 0x8000);
        write(&mut wrap, 1, 0x01, 0xFFFE);
        write(&mut wrap, 2, 0x05, 2);
        write(&mut wrap, 3, 0x03, 0x8000);
        write(&mut wrap, 4, 0x02, 4096);
        let (pcm, _adpcm, samples, warnings) = collect_qsound(&wrap);
        assert_eq!(pcm.len(), 1);
        assert_eq!(samples[0].data, vec![0, 0, 10, 11]);
        assert_eq!(
            warnings,
            vec![
                "QSound voice 1 sample range is not covered by the ROM blocks; it is silent"
            ]
        );
        assert_eq!(pcm[0].start, 0xFFFE);
        assert_eq!(pcm[0].end, 2);
    }
}
