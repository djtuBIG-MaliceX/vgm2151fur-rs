//! NES APU (2A03) register stream → tone/noise notes and DPCM samples.
//!
//! Literal port of `vgm2151fur/nes.py` (v0.11.0).
//!
//! The APU has no key-on of its own: a pulse/triangle/noise voice sounds while
//! it is enabled in `$4015`, its length counter is loaded or halted, and (for
//! the pulses and noise) the volume nibble is non-zero. Like the SN76489
//! collector, this keeps one note across same-pitch rewrites and turns
//! mid-note register changes into pitch/volume/duty points.
//!
//! Register map (VGM command `0xB4`, register `0x00-0x17` = `$4000-$4017`):
//!
//! ```text
//! 0x00/0x04 pulse 1/2 duty+volume+envelope   0x01/0x05 sweep
//! 0x02/0x06 pulse 1/2 timer low              0x03/0x07 timer high+length
//! 0x08     triangle linear counter           0x0A/0x0B triangle timer
//! 0x0C     noise volume+envelope             0x0E/0x0F noise period+length
//! 0x10-0x13 DPCM rate/level/address/length   0x15     channel enable
//! ```
//!
//! `$4011` is a 7-bit DAC. A driver can either play 1-bit DPCM samples from
//! the `$C000` RAM (the data-block type `0xC2`) or bit-bang `$4011` at audio
//! rate, which is the same idea as the SSG volume-register PCM. Both are
//! turned into sample instruments here: DPCM bodies keep their 1-bit form, the
//! bit-bang bodies become 8-bit signed PCM.

use indexmap::IndexMap;

use crate::parity::{py_int, py_round};
use crate::pitch::furnace_period_note;
use crate::samplecluster::cluster_fragments;
use crate::vgm::{ChipWrite, PcmRamWrite, RomSlice, VgmFile, VGM_RATE};

pub const NES_CHANNELS: usize = 5;
pub const NES_DEFAULT_CLOCK: u32 = 1_789_773; // COLOR_NTSC / 2
// The exact clock Furnace's NES core runs at (COLOR_NTSC / 2).
pub const NES_FURNACE_CLOCK: f64 = 315_000_000.0 / 88.0 / 2.0;
pub const NES_CPU_DIVIDER: i32 = 16;
pub const NES_MAX_PERIOD: i32 = 0x7FF;
// Shorter than this is a click/housekeeping gate, not a note.
pub const NES_MIN_NOTE: i64 = 100;
// The APU length-counter table: the 5-bit load index maps to a number of
// half-frames (decremented at 120 Hz). Index 0 is 10, so a driver that writes
// `$400F = 0` to mark a noise hit still plays an 83 ms burst, exactly as the
// console does.
pub const NES_LENGTH_TABLE: [i32; 32] = [
    10, 254, 20, 2, 40, 4, 80, 6, 160, 8, 60, 10, 120, 12, 240, 14, 1, 18, 2, 20, 4, 22, 6, 24,
    8, 26, 10, 28, 12, 30, 14, 32,
];
// VGM samples per frame-sequencer tick: the length counter runs at 120 Hz
// (half frames) and the triangle's linear counter at 240 Hz (quarter frames).
pub const SAMPLES_PER_HALF_FRAME: f64 = VGM_RATE as f64 / 120.0;
pub const SAMPLES_PER_QUARTER_FRAME: f64 = VGM_RATE as f64 / 240.0;
// A timer this low renders above ~16 kHz, which the console's output filter
// drops. Some drivers park an unused voice at timer 0 between phrases, so a
// write there is silence, not the top of the tracker's note range.
pub const NES_MIN_PERIOD: i32 = 6;
// A period change this large is a new note rather than a bend.
pub const NES_NOTE_JUMP_ST: f64 = 1.0;
// NES drivers re-strike a note by writing a new volume without clearing the
// gate. A jump this large while a note is open is a new hit, not a volume
// step: folding it into the note's instrument would make one macro play
// several source notes. A rise from a very low level to an attack level is
// the same even when the driver leaves the period alone.
pub const NES_RETRIGGER_RISE: i32 = 4;
pub const NES_RETRIGGER_LOW: i32 = 3;
// A period write this close to the volume jump belongs to the same hit.
pub const NES_RETRIGGER_WINDOW: i64 = 128;
// A pulse period that has been held for a while and then steps by this much
// (1/64 semitones) is the next note, not a bend. Vibrato rewrites the period
// every frame, so it never looks like a held step.
pub const NES_STEP_ST: i32 = 32;
pub const NES_STEP_HOLD: i64 = 1500;

// NTSC DMC output-bit periods in CPU cycles, indexed by `$4010 & 0x0F`.
pub const DPCM_PERIODS: [i32; 16] = [
    428, 380, 340, 320, 286, 254, 226, 214, 190, 160, 142, 128, 106, 84, 72, 54,
];

// Bit-banged `$4011` PCM: same fast-write test as the SSG PCM detector.
pub const PCM_MAX_GAP: i64 = 64;
pub const PCM_MAX_MEDIAN_DT: i64 = 16;
pub const PCM_MIN_WRITES: usize = 8;
pub const PCM_SPLIT_GAP: i64 = 2048;
pub const PCM_MIN_RATE: i32 = 4000;
pub const PCM_MAX_RATE: i32 = 32000;
pub const PCM_MIN_FRAMES: usize = 128;
// Long bit-banged bodies are cut into chunks so a small tracker/sample-rate
// error cannot desync the whole song. Each chunk gets its own sample-map note
// at the source time, and 120 chunks share one map instrument. Furnace sample
// indices are one byte, so keep the chunks inside the same 240-slot budget the
// YM2612 DAC uses; a longer body gets proportionally longer chunks.
pub const NES_PCM_CHUNK_FRAMES: usize = 8192;
pub const NES_PCM_MAP_SLOTS: i32 = 120;
pub const NES_PCM_SAMPLE_BUDGET: usize = 240;
// A zero run at least this long (VGM samples) is a rest between phrases, not
// sample data. Split there first, then chunk the sounding parts.
pub const NES_PCM_SILENCE_GAP: i64 = PCM_SPLIT_GAP;

pub const REG_PULSE1: (usize, usize, usize, usize) = (0x00, 0x01, 0x02, 0x03);
pub const REG_PULSE2: (usize, usize, usize, usize) = (0x04, 0x05, 0x06, 0x07);

/// DMC output sample rate in Hz for a `$4010` rate index.
pub fn nes_dpcm_rate(clock: u32, index: i32) -> i32 {
    let period = DPCM_PERIODS[i32::max(0, i32::min(15, index)) as usize];
    let clock = if clock == 0 { NES_DEFAULT_CLOCK } else { clock };
    i32::max(1, py_int(py_round(clock as f64 / period as f64)) as i32)
}

/// Pulse/triangle tone frequency for an 11-bit timer value.
pub fn nes_period_hz(clock: u32, period: i32) -> f64 {
    let clock = if clock == 0 { NES_DEFAULT_CLOCK } else { clock };
    clock as f64 / (16.0 * (i32::max(0, period) + 1) as f64)
}

/// 11-bit NES timer -> (Furnace note, E5) for the same register.
///
/// Furnace's NES platform computes `period = pitch_table_value - 1`, so the
/// inverse asks the table for `period + 1`.
pub fn nes_note(clock: u32, period: i32) -> (i32, i32) {
    let clock = if clock == 0 { NES_DEFAULT_CLOCK } else { clock };
    furnace_period_note(
        clock as f64,
        NES_CPU_DIVIDER as f64,
        i32::max(0, i32::min(NES_MAX_PERIOD, period)),
        1,
        NES_MAX_PERIOD,
    )
}

/// 1/64-semitone change from one NES timer value to another.
///
/// A timer of 0 is a valid (highest) pitch, so the +1 in the ratio is the
/// real register value; a move to or from 0 is a large jump, not a bend.
fn _period_delta(base: i32, new: i32) -> i32 {
    if base < 0 || new < 0 {
        return 0;
    }
    py_int(py_round(
        64.0 * 12.0 * ((base + 1) as f64 / (new + 1) as f64).log2(),
    )) as i32
}

/// Furnace's NES noise column for a `$400E` period (properNoiseLayout).
fn _noise_note(period: i32) -> i32 {
    60 + (15 - (period & 0x0F))
}

#[derive(Clone, Debug, PartialEq)]
pub struct NesHit {
    pub sample_time: i64,
    pub voice: i32, // 0/1 pulse, 2 triangle, 3 noise
    pub period: i32, // 11-bit tone timer, 0-15 noise period
    pub vol: i32,   // $4000/$4004/$400C low nibble
    pub duty: i32,  // pulse duty 0-3; noise mode 0-1
    pub env_mode: i32, // (register >> 4) & 3
    // A dual-APU log interleaves two chips on one command, so every note keeps
    // the instance its register file belonged to.
    pub chip_id: i32,
    pub linear: i32, // triangle $4008
    pub off_sample: i64,
    pub pitch_pts: Vec<(i64, i32)>,
    pub vol_pts: Vec<(i64, i32)>,
    pub duty_pts: Vec<(i64, i32)>,
    pub env_pts: Vec<(i64, i32)>,
    pub linear_pts: Vec<(i64, i32)>,
}

impl Default for NesHit {
    fn default() -> Self {
        NesHit {
            sample_time: 0,
            voice: 0,
            period: 0,
            vol: 0,
            duty: 0,
            env_mode: 0,
            chip_id: 0,
            linear: 0,
            off_sample: 0,
            pitch_pts: Vec::new(),
            vol_pts: Vec::new(),
            duty_pts: Vec::new(),
            env_pts: Vec::new(),
            linear_pts: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct NesOff {
    pub sample_time: i64,
    pub voice: i32,
    pub chip_id: i32,
}

impl Default for NesOff {
    fn default() -> Self {
        NesOff {
            sample_time: 0,
            voice: 0,
            chip_id: 0,
        }
    }
}

/// One 1-bit DPCM body from the `$C000` sample RAM.
#[derive(Clone, Debug, PartialEq)]
pub struct NesDpcmSample {
    pub start: i64,
    pub length: i64,
    pub data: Vec<u8>,
    pub rate_index: i32,
    pub loop_: bool,
    pub chip_id: i32,
}

impl Default for NesDpcmSample {
    fn default() -> Self {
        NesDpcmSample {
            start: 0,
            length: 0,
            data: Vec::new(),
            rate_index: 0,
            loop_: false,
            chip_id: 0,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct NesDpcmHit {
    pub sample_time: i64,
    pub sample_index: i32,
    pub rate_index: i32,
    pub end_time: i64,
    pub chip_id: i32,
}

impl Default for NesDpcmHit {
    fn default() -> Self {
        NesDpcmHit {
            sample_time: 0,
            sample_index: 0,
            rate_index: 0,
            end_time: 0,
            chip_id: 0,
        }
    }
}

/// One 8-bit body from a bit-banged `$4011` stream.
#[derive(Clone, Debug, PartialEq)]
pub struct NesPcmSample {
    pub name: String,
    pub data: Vec<u8>,
    pub rate: i32,
    pub start: i64,
    pub end: i64,
    pub chip_id: i32,
}

impl Default for NesPcmSample {
    fn default() -> Self {
        NesPcmSample {
            name: String::new(),
            data: Vec::new(),
            rate: 0,
            start: 0,
            end: 0,
            chip_id: 0,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct NesPcmHit {
    pub sample_time: i64,
    pub end_time: i64,
    pub sample_index: i32,
    pub chip_id: i32,
}

impl Default for NesPcmHit {
    fn default() -> Self {
        NesPcmHit {
            sample_time: 0,
            end_time: 0,
            sample_index: 0,
            chip_id: 0,
        }
    }
}

/// The `0xB4` stream split by APU instance (register bit 7).
///
/// Two APUs share one command byte, so every collector keeps their register
/// files apart: a shared machine lets one chip's write close the other's open
/// gate and puts ten voices' notes on five columns.
fn _nes_writes_by_chip(vgm: &VgmFile) -> IndexMap<i32, Vec<&ChipWrite>> {
    let mut out: IndexMap<i32, Vec<&ChipWrite>> = IndexMap::new();
    for w in &vgm.writes {
        if w.chip == "nes" {
            out.entry((w.chip_id & 1) as i32).or_default().push(w);
        }
    }
    out
}

/// Volume nibble is a level (constant mode) or the envelope is running.
fn _tone_audible(vol: i32, env_mode: i32) -> bool {
    if env_mode & 1 != 0 {
        return vol > 0;
    }
    true
}

/// Pulse, triangle and noise notes from the `0xB4` register stream.
///
/// A dual-APU log (header bit 30, register bit 7) interleaves two independent
/// register files on the same command, so each instance runs its own machine.
/// One shared machine would let chip 2's `$4000` rewrite chip 1's open pulse
/// gate and collapse both APUs onto the same five columns.
pub fn collect_nes_tones(
    vgm: &VgmFile,
    _clock: Option<u32>,
) -> (Vec<NesHit>, Vec<NesOff>, Vec<String>) {
    let mut warnings: Vec<String> = Vec::new();
    let by_cid = _nes_writes_by_chip(vgm);
    if by_cid.is_empty() {
        return (Vec::new(), Vec::new(), warnings);
    }

    let mut hits: Vec<NesHit> = Vec::new();
    let mut offs: Vec<NesOff> = Vec::new();
    let mut glitches = 0;
    let total = vgm.total_samples;
    let mut ids: Vec<i32> = by_cid.keys().copied().collect();
    ids.sort();
    for cid in ids {
        let (chip_hits, chip_offs, chip_glitches) =
            _nes_tones_one(&by_cid[&cid], cid, total);
        hits.extend(chip_hits);
        offs.extend(chip_offs);
        glitches += chip_glitches;
    }
    hits.sort_by(|a, b| {
        (a.sample_time, a.chip_id, a.voice).cmp(&(b.sample_time, b.chip_id, b.voice))
    });
    offs.sort_by(|a, b| {
        (a.sample_time, a.chip_id, a.voice).cmp(&(b.sample_time, b.chip_id, b.voice))
    });
    if glitches != 0 {
        warnings.push(format!("NES APU: ignored {glitches} sub-frame tone gates"));
    }
    (hits, offs, warnings)
}

/// The voice machine of one 2A03 instance.
///
/// `writes` are already limited to that instance and in stream order. Returns
/// (hits, offs, dropped sub-frame gates).
fn _nes_tones_one(
    writes: &[&ChipWrite],
    chip_id: i32,
    total: i64,
) -> (Vec<NesHit>, Vec<NesOff>, i32) {
    let mut machine = NesToneMachine {
        writes,
        chip_id,
        period: [0; 3],
        freq_hi: [false; 3],
        length: [0; 4], // half-frames loaded, 0 = counter never loaded
        vol: [0; 3],    // pulse 1, pulse 2, noise
        duty: [0; 2],   // pulse 1, pulse 2
        env_mode: [0; 3],
        linear: 0,
        noise_period: 0,
        noise_mode: 0,
        enabled: 0x1F,
        active: [None, None, None, None],
        hits: Vec::new(),
        offs: Vec::new(),
        glitches: 0,
        // Absolute sample at which the current gate's counters run out, 0 =
        // held. A length reload that arrives after it starts a new hit; one
        // that arrives before it just extends the same note (drivers that hold
        // a voice by rewriting the length register every frame).
        gate_end: [0; 4],
        // Volume jumps that may be a re-attack, and the last small period
        // change that may belong to the same hit.
        rise_t: [-1; 4],
        period_change_t: [-1; 4],
        last_period_t: [-1; 4],
        // Last period value acted on. A write of the same value is not a
        // change, while a return to the note's base pitch still is (and must
        // reach the pitch macro).
        prev_period: [0; 3],
    };
    machine.run();
    let end_total = total;
    for voice in 0..4 {
        if machine.active[voice].is_none() {
            continue;
        }
        let end = machine.gate_end[voice];
        if end != 0 && (end_total == 0 || end <= end_total) {
            // A percussion gate that ran out before the stream did: end the
            // note there so the pattern shows the hit's real length.
            machine.close(voice, end, true);
        } else {
            machine.close(voice, 0, false);
        }
    }
    (machine.hits, machine.offs, machine.glitches)
}

struct NesToneMachine<'a> {
    writes: &'a [&'a ChipWrite],
    chip_id: i32,
    period: [i32; 3],
    freq_hi: [bool; 3],
    length: [i32; 4],
    vol: [i32; 3],
    duty: [i32; 2],
    env_mode: [i32; 3],
    linear: i32,
    noise_period: i32,
    noise_mode: i32,
    enabled: i32,
    active: [Option<NesHit>; 4],
    hits: Vec<NesHit>,
    offs: Vec<NesOff>,
    glitches: i32,
    gate_end: [i64; 4],
    rise_t: [i64; 4],
    period_change_t: [i64; 4],
    last_period_t: [i64; 4],
    prev_period: [i32; 3],
}

impl NesToneMachine<'_> {
    /// Sample where a voice's own counter closes the gate, 0 for a held one.
    ///
    /// The reference player gates each noise hit by its length counter (a
    /// `$400F` write of 0 loads the table's 10 half-frames, an 83 ms burst)
    /// and each triangle note by its linear counter ((value/240) s), while
    /// pulse notes keep sounding until the driver's next write. Drivers mark
    /// a hit by rewriting those registers, so a reload that arrives after the
    /// gate ran out starts a new note and one that arrives before it extends
    /// the same note.
    fn gate_end_at(&self, voice: usize, t: i64) -> i64 {
        if voice == 3 {
            if self.env_mode[2] & 2 != 0 || self.length[3] == 0 {
                return 0;
            }
            return t + py_int(py_round(self.length[3] as f64 * SAMPLES_PER_HALF_FRAME));
        }
        if voice == 2 {
            if self.linear & 0x80 != 0 || (self.linear & 0x7F) == 0 {
                return 0;
            }
            return t + py_int(py_round(
                (self.linear & 0x7F) as f64 * SAMPLES_PER_QUARTER_FRAME,
            ));
        }
        0
    }

    fn close(&mut self, voice: usize, t: i64, emit_off: bool) {
        let Some(mut hit) = self.active[voice].take() else {
            return;
        };
        hit.off_sample = t;
        self.gate_end[voice] = 0;
        let duration = t - hit.sample_time;
        if t <= 0 {
            self.hits.push(hit);
        } else if duration >= NES_MIN_NOTE {
            self.hits.push(hit);
            if emit_off {
                self.offs.push(NesOff {
                    sample_time: t,
                    voice: voice as i32,
                    chip_id: self.chip_id,
                });
            }
        } else {
            self.glitches += 1;
        }
    }

    fn open_hit(&mut self, voice: usize, t: i64) {
        self.last_period_t[voice] = t;
        if voice != 3 {
            self.prev_period[voice] = self.period[voice];
        }
        self.gate_end[voice] = self.gate_end_at(voice, t);
        if voice == 3 {
            self.active[voice] = Some(NesHit {
                sample_time: t,
                voice: 3,
                period: self.noise_period,
                vol: self.vol[2],
                duty: self.noise_mode,
                env_mode: self.env_mode[2] & 3,
                chip_id: self.chip_id,
                ..Default::default()
            });
        } else {
            self.active[voice] = Some(NesHit {
                sample_time: t,
                voice: voice as i32,
                period: self.period[voice],
                vol: self.vol[voice],
                duty: if voice < 2 { self.duty[voice] } else { 0 },
                env_mode: self.env_mode[voice] & 3,
                chip_id: self.chip_id,
                linear: if voice == 2 { self.linear } else { 0 },
                ..Default::default()
            });
        }
    }

    fn evaluate(&mut self, voice: usize, t: i64, rearm: bool) {
        // The voice's own counter may already have closed this note. End it at
        // that time, so the pattern gets the note-off where the chip went
        // quiet, and let this write start the next note. A reload that arrives
        // while the gate is still open only extends the same note (drivers
        // hold a voice by rewriting the length register every frame).
        if self.active[voice].is_some() && self.gate_end[voice] != 0 && self.gate_end[voice] <= t
        {
            let end = self.gate_end[voice];
            self.close(voice, end, true);
        } else if rearm && self.active[voice].is_some() {
            self.gate_end[voice] = self.gate_end_at(voice, t);
        }
        let on;
        if voice == 3 {
            let mut b = self.enabled & 0x08 != 0 && _tone_audible(self.vol[2], self.env_mode[2]);
            b = b && (self.env_mode[2] & 2 != 0 || self.length[3] > 0);
            on = b;
        } else if voice == 2 {
            let mut b = self.enabled & 0x04 != 0 && self.freq_hi[2];
            b = b && self.period[2] >= NES_MIN_PERIOD;
            b = b
                && (self.linear & 0x7F) > 0
                && (self.length[2] > 0 || self.linear & 0x80 != 0);
            on = b;
        } else {
            let mut b = self.enabled & (1 << voice) != 0 && self.freq_hi[voice];
            b = b && self.period[voice] >= NES_MIN_PERIOD;
            b = b && _tone_audible(self.vol[voice], self.env_mode[voice]);
            b = b && (self.env_mode[voice] & 2 != 0 || self.length[voice] > 0);
            on = b;
        }
        if on && self.active[voice].is_none() {
            self.open_hit(voice, t);
            return;
        }
        if !on {
            if self.active[voice].is_some() {
                self.close(voice, t, true);
            }
            return;
        }
        let (hit_start, hit_period, hit_duty, hit_linear, hit_vol, hit_env) = {
            let hit = self.active[voice].as_ref().expect("active voice");
            (
                hit.sample_time,
                hit.period,
                hit.duty,
                hit.linear,
                hit.vol,
                hit.env_mode,
            )
        };
        if voice == 3 {
            if self.noise_period != hit_period {
                self.close(voice, t, false);
                self.open_hit(voice, t);
                return;
            }
            if self.noise_mode != hit_duty {
                if t == hit_start {
                    self.active[voice].as_mut().unwrap().duty = self.noise_mode;
                } else {
                    let need = self.active[voice]
                        .as_ref()
                        .map_or(false, |h| h.duty_pts.last().map_or(true, |p| p.1 != self.noise_mode));
                    if need {
                        self.active[voice]
                            .as_mut()
                            .unwrap()
                            .duty_pts
                            .push((t, self.noise_mode));
                    }
                }
            }
        } else if voice == 2 {
            if self.period[2] != self.prev_period[2] {
                self.prev_period[2] = self.period[2];
                if t == hit_start {
                    self.active[2].as_mut().unwrap().period = self.period[2];
                } else if _period_delta(hit_period, self.period[2]).abs()
                    >= (NES_NOTE_JUMP_ST * 64.0) as i32
                {
                    self.close(voice, t, false);
                    self.open_hit(voice, t);
                    return;
                } else {
                    let period = self.period[2];
                    let need = self.active[2]
                        .as_ref()
                        .map_or(false, |h| h.pitch_pts.last().map_or(true, |p| p.1 != period));
                    if need {
                        self.active[2]
                            .as_mut()
                            .unwrap()
                            .pitch_pts
                            .push((t, period));
                    }
                }
            }
            if self.linear != hit_linear {
                if t == hit_start {
                    self.active[2].as_mut().unwrap().linear = self.linear;
                } else {
                    let linear = self.linear;
                    let need = self.active[2]
                        .as_ref()
                        .map_or(false, |h| h.linear_pts.last().map_or(true, |p| p.1 != linear));
                    if need {
                        self.active[2]
                            .as_mut()
                            .unwrap()
                            .linear_pts
                            .push((t, linear));
                    }
                }
            }
        } else {
            if self.period[voice] != self.prev_period[voice] {
                self.prev_period[voice] = self.period[voice];
                let change = _period_delta(hit_period, self.period[voice]);
                if t == hit_start {
                    self.active[voice].as_mut().unwrap().period = self.period[voice];
                    self.last_period_t[voice] = t;
                } else if change.abs() >= (NES_NOTE_JUMP_ST * 64.0) as i32 {
                    self.close(voice, t, false);
                    self.open_hit(voice, t);
                    return;
                } else if change.abs() >= NES_STEP_ST
                    && t - self.last_period_t[voice] >= NES_STEP_HOLD
                {
                    // A held pitch step is the next note in a melody, not a
                    // bend. Keeping it in the pitch macro made one instrument
                    // play several source notes.
                    self.close(voice, t, false);
                    self.open_hit(voice, t);
                    return;
                } else if self.rise_t[voice] >= 0
                    && t - self.rise_t[voice] <= NES_RETRIGGER_WINDOW
                {
                    // The volume just jumped and the pitch moved: a new hit.
                    self.close(voice, t, false);
                    self.open_hit(voice, t);
                    return;
                } else {
                    let period = self.period[voice];
                    let need = self.active[voice]
                        .as_ref()
                        .map_or(false, |h| h.pitch_pts.last().map_or(true, |p| p.1 != period));
                    if need {
                        self.period_change_t[voice] = t;
                        self.active[voice]
                            .as_mut()
                            .unwrap()
                            .pitch_pts
                            .push((t, period));
                    }
                    self.last_period_t[voice] = t;
                }
            }
            if self.duty[voice] != hit_duty {
                if t == hit_start {
                    self.active[voice].as_mut().unwrap().duty = self.duty[voice];
                } else {
                    let duty = self.duty[voice];
                    let need = self.active[voice]
                        .as_ref()
                        .map_or(false, |h| h.duty_pts.last().map_or(true, |p| p.1 != duty));
                    if need {
                        self.active[voice]
                            .as_mut()
                            .unwrap()
                            .duty_pts
                            .push((t, duty));
                    }
                }
            }
        }
        if voice != 2 {
            let slot = if voice == 3 { 2 } else { voice };
            if self.vol[slot] != hit_vol {
                if t == hit_start {
                    self.active[voice].as_mut().unwrap().vol = self.vol[slot];
                } else {
                    let vol = self.vol[slot];
                    let need = self.active[voice]
                        .as_ref()
                        .map_or(false, |h| h.vol_pts.last().map_or(true, |p| p.1 != vol));
                    if need {
                        self.active[voice]
                            .as_mut()
                            .unwrap()
                            .vol_pts
                            .push((t, vol));
                    }
                }
            }
            if (self.env_mode[slot] & 3) != hit_env {
                let env = self.env_mode[slot] & 3;
                if t == hit_start {
                    self.active[voice].as_mut().unwrap().env_mode = env;
                } else {
                    let need = self.active[voice]
                        .as_ref()
                        .map_or(false, |h| h.env_pts.last().map_or(true, |p| p.1 != env));
                    if need {
                        self.active[voice]
                            .as_mut()
                            .unwrap()
                            .env_pts
                            .push((t, env));
                    }
                }
            }
        }
    }

    fn evaluate_all(&mut self, t: i64) {
        for voice in 0..4 {
            self.evaluate(voice, t, false);
        }
    }

    /// True when the next write completes this timer update.
    ///
    /// A driver that writes the timer low byte first leaves a stale high byte
    /// for a sample or two. Evaluating that half-updated period turns a real
    /// note into a spurious bend or split: Santa Claus writes 270 -> 0 as
    /// low 0x00 (270 & 0x700 = 256) then high 0xC0, and 256 is a held step
    /// that closed the note without a note-off. Let the high write evaluate
    /// the combined period instead.
    fn timer_high_follows(&self, index: usize, low_reg: u32, t: i64) -> bool {
        let high_reg: u32 = match low_reg {
            0x02 => 0x03,
            0x06 => 0x07,
            0x0A => 0x0B,
            _ => unreachable!(),
        };
        let stop = usize::min(index + 4, self.writes.len());
        for nxt in &self.writes[index + 1..stop] {
            if nxt.sample > t + 8 {
                break;
            }
            if (nxt.reg & 0x7F) == high_reg {
                return true;
            }
        }
        false
    }

    fn run(&mut self) {
        for index in 0..self.writes.len() {
            let w = self.writes[index];
            let reg = w.reg & 0x7F;
            let val = w.val & 0xFF;
            let t = w.sample;
            if reg == 0x00 || reg == 0x04 {
                let voice = if reg == 0x00 { 0 } else { 1 };
                let before = self.vol[voice];
                self.vol[voice] = (val & 0x0F) as i32;
                self.env_mode[voice] = ((val >> 4) & 3) as i32;
                self.duty[voice] = ((val >> 6) & 3) as i32;
                if self.active[voice].is_some()
                    && t > self.active[voice].as_ref().unwrap().sample_time
                    && self.vol[voice] - before >= NES_RETRIGGER_RISE
                {
                    // A re-attack: the driver raised the level from (near)
                    // silence without clearing the gate, or moved the pitch at
                    // the same time. Start a new note instead of turning
                    // several source notes into one long volume/pitch macro.
                    self.rise_t[voice] = t;
                    let recent = self.period_change_t[voice] >= 0
                        && t - self.period_change_t[voice] <= NES_RETRIGGER_WINDOW;
                    if before <= NES_RETRIGGER_LOW || recent {
                        self.close(voice, t, false);
                        self.open_hit(voice, t);
                    }
                }
                self.evaluate(voice, t, false);
            } else if reg == 0x02 || reg == 0x06 {
                let voice = if reg == 0x02 { 0 } else { 1 };
                self.period[voice] = (self.period[voice] & 0x700) | val as i32;
                // Some drivers write the timer high byte before the low one.
                // The high write then opened the note on the previous low
                // byte; the low write is the one that completes this pitch.
                // The other order leaves a stale high byte for a sample or
                // two, so wait for the high write when one follows.
                if self.freq_hi[voice] && !self.timer_high_follows(index, reg, t) {
                    self.evaluate(voice, t, false);
                }
            } else if reg == 0x03 || reg == 0x07 {
                let voice = if reg == 0x03 { 0 } else { 1 };
                self.period[voice] = (self.period[voice] & 0xFF) | (((val & 7) as i32) << 8);
                self.length[voice] = NES_LENGTH_TABLE[((val >> 3) & 0x1F) as usize];
                self.freq_hi[voice] = true;
                self.evaluate(voice, t, true);
            } else if reg == 0x08 {
                self.linear = val as i32;
                self.evaluate(2, t, false);
            } else if reg == 0x0A {
                self.period[2] = (self.period[2] & 0x700) | val as i32;
                if self.freq_hi[2] && !self.timer_high_follows(index, reg, t) {
                    self.evaluate(2, t, false);
                }
            } else if reg == 0x0B {
                self.period[2] = (self.period[2] & 0xFF) | (((val & 7) as i32) << 8);
                self.length[2] = NES_LENGTH_TABLE[((val >> 3) & 0x1F) as usize];
                self.freq_hi[2] = true;
                self.evaluate(2, t, true);
            } else if reg == 0x0C {
                self.vol[2] = (val & 0x0F) as i32;
                self.env_mode[2] = ((val >> 4) & 3) as i32;
                self.evaluate(3, t, false);
            } else if reg == 0x0E {
                self.noise_period = (val & 0x0F) as i32;
                self.noise_mode = ((val >> 7) & 1) as i32;
                self.evaluate(3, t, false);
            } else if reg == 0x0F {
                self.length[3] = NES_LENGTH_TABLE[((val >> 3) & 0x1F) as usize];
                self.evaluate(3, t, true);
            } else if reg == 0x15 {
                self.enabled = (val & 0x1F) as i32;
                self.evaluate_all(t);
            }
            // 0x01/0x05 sweep, 0x09/0x0D unused, 0x10-0x13 DPCM, 0x17 frame ctl
        }
    }
}

/// (stream time, address, data) for one APU's DPCM RAM dumps, in order.
///
/// Every 2A03 has its own `$C000` sample RAM, so a type-`0xC2` dump only
/// belongs to the instance named by bit 31 of its length. A driver may reload
/// the same 4 KB window while the song plays, so the image is only valid from
/// the block's position in the stream. The old code applied every block up
/// front, which left each hit playing whatever the last block happened to
/// contain.
fn _dpcm_dumps(vgm: &VgmFile, chip_id: i32) -> Vec<(i64, i64, Vec<u8>)> {
    let mut out: Vec<(i64, i64, Vec<u8>)> = vgm
        .roms
        .iter()
        .filter(|s: &&RomSlice| s.kind == "nes_dpcm" && (s.chip as i32) == chip_id)
        .map(|s| (s.time, s.start & 0xFFFF, s.data.clone()))
        .collect();
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

/// Python slice `data[start:stop]` (clamped, negative indices from the end).
fn _py_slice(data: &[u8], start: i64, stop: i64) -> &[u8] {
    let len = data.len() as i64;
    let s = if start < 0 { len + start } else { start };
    let e = if stop < 0 { len + stop } else { stop };
    let s = s.clamp(0, len);
    let e = e.clamp(0, len);
    if e < s {
        return &data[s as usize..s as usize];
    }
    &data[s as usize..e as usize]
}

/// Replay this instance's 0x68 bank copies whose time has arrived.
///
/// `base` is where this APU's own type-`0x07` database starts inside the
/// concatenated bank. A dual-APU log carries one database per chip and every
/// 0x68 source address is relative to the chip it names, so a shared bank
/// would read the other cart's samples.
fn _apply_dpcm_bank(
    ram: &mut [u8],
    covered: &mut [u8],
    vgm: &VgmFile,
    ram_writes: &[&PcmRamWrite],
    upto: i64,
    cursor: &mut usize,
    base: usize,
) {
    let bank = &vgm.nes_dpcm_bank;
    while *cursor < ram_writes.len() && ram_writes[*cursor].sample <= upto {
        let r = ram_writes[*cursor];
        *cursor += 1;
        let start = base as i64 + r.src;
        let chunk = _py_slice(bank, start, start + r.length);
        for (i, &b) in chunk.iter().enumerate() {
            let addr = ((r.dest + i as i64) & 0xFFFF) as usize;
            ram[addr] = b;
            covered[addr] = 1;
        }
    }
}

/// Standard DPCM playback: `$4012`/`$4013` + `$4015` bit 4.
///
/// Both instances read the module's one sample list, so a body two APUs play
/// is stored once and each hit says which chip plays it.
pub fn collect_nes_dpcm(
    vgm: &VgmFile,
    _clock: Option<u32>,
) -> (Vec<NesDpcmSample>, Vec<NesDpcmHit>, Vec<String>) {
    let mut warnings: Vec<String> = Vec::new();
    let by_cid = _nes_writes_by_chip(vgm);
    if by_cid.is_empty() {
        return (Vec::new(), Vec::new(), warnings);
    }

    let mut samples: Vec<NesDpcmSample> = Vec::new();
    let mut hits: Vec<NesDpcmHit> = Vec::new();
    // The same address/length can hold different data after a bank reload, and
    // the two chips share one body when they play the same window.
    let mut by_key: IndexMap<(i64, i64, bool, Vec<u8>), i32> = IndexMap::new();
    let mut starts = 0i32;
    let mut undumped = 0i32;
    let has_data = vgm.pcm_ram_writes.iter().any(|r| r.type_ == 7)
        || vgm.roms.iter().any(|s| s.kind == "nes_dpcm");
    let mut ids: Vec<i32> = by_cid.keys().copied().collect();
    ids.sort();
    for cid in ids {
        let (chip_starts, chip_undumped) = _collect_dpcm_chip(
            vgm,
            &by_cid[&cid],
            cid,
            &mut samples,
            &mut hits,
            &mut by_key,
        );
        starts += chip_starts;
        undumped += chip_undumped;
    }

    if starts != 0 && !has_data {
        warnings.push(format!(
            "{starts} NES DPCM trigger(s) but no DPCM data block or RAM dump; \
             those notes are silent"
        ));
    } else if undumped != 0 {
        warnings.push(format!(
            "{undumped} NES DPCM sample(s) point at RAM the VGM does not dump; \
             those notes are silent"
        ));
    }
    (samples, hits, warnings)
}

/// One APU's DMC voice. Returns (triggers, triggers with no dumped data).
fn _collect_dpcm_chip(
    vgm: &VgmFile,
    writes: &[&ChipWrite],
    chip_id: i32,
    samples: &mut Vec<NesDpcmSample>,
    hits: &mut Vec<NesDpcmHit>,
    by_key: &mut IndexMap<(i64, i64, bool, Vec<u8>), i32>,
) -> (i32, i32) {
    // 64 KB CPU-address image of the dumped DPCM RAM (0xAA = silence). RAM
    // dumps and 0x68 bank copies are replayed in stream order, so a hit reads
    // the bank that was loaded when the driver triggered it.
    let mut ram = vec![0xAAu8; 0x10000];
    let mut covered = vec![0u8; 0x10000];
    let dumps = _dpcm_dumps(vgm, chip_id);
    let mut dump_cursor = 0usize;

    let mut bank_writes: Vec<&PcmRamWrite> = vgm
        .pcm_ram_writes
        .iter()
        .filter(|r| r.type_ == 7 && (r.chip_id as i32) == chip_id) // NES DPCM database
        .collect();
    bank_writes.sort_by(|a, b| a.sample.cmp(&b.sample));
    let mut bank_cursor = 0usize;
    let base = vgm.nes_dpcm_bank_base.get(&(chip_id as u8)).copied().unwrap_or(0);

    let mut rate = 0i32;
    let mut loop_bit = false;
    let mut addr_reg = 0i32;
    let mut len_reg = 0i32;
    let mut playing = false;
    let mut armed = false;
    let mut last_hit: Option<usize> = None;
    let mut undumped = 0i32;
    let mut starts = 0i32;

    for w in writes {
        let reg = w.reg & 0x7F;
        let val = w.val & 0xFF;
        let t = w.sample;
        while dump_cursor < dumps.len() && dumps[dump_cursor].0 <= t {
            let (_t, start, data) = &dumps[dump_cursor];
            dump_cursor += 1;
            for (i, &b) in data.iter().enumerate() {
                let addr = ((start + i as i64) & 0xFFFF) as usize;
                ram[addr] = b;
                covered[addr] = 1;
            }
        }
        _apply_dpcm_bank(
            &mut ram,
            &mut covered,
            vgm,
            &bank_writes,
            t,
            &mut bank_cursor,
            base,
        );
        if reg == 0x10 {
            rate = (val & 0x0F) as i32;
            loop_bit = val & 0x40 != 0;
        } else if reg == 0x12 {
            addr_reg = val as i32;
        } else if reg == 0x13 {
            len_reg = val as i32;
            armed = true;
        } else if reg == 0x15 {
            let bit = val & 0x10 != 0;
            if bit && !playing && armed {
                let addr = (0xC000 + (addr_reg << 6)) & 0xFFFF;
                let length = (len_reg << 4) + 1;
                playing = true;
                starts += 1;
                let covered_bytes = (0..length)
                    .filter(|i| covered[((addr + i) & 0xFFFF) as usize] != 0)
                    .count();
                if covered_bytes == 0 {
                    undumped += 1;
                }
                let body: Vec<u8> = (0..length)
                    .map(|i| ram[((addr + i) & 0xFFFF) as usize])
                    .collect();
                let key = (addr as i64, length as i64, loop_bit, body.clone());
                let idx = match by_key.get(&key) {
                    Some(&idx) => idx,
                    None => {
                        let idx = samples.len() as i32;
                        by_key.insert(key, idx);
                        samples.push(NesDpcmSample {
                            start: addr as i64,
                            length: length as i64,
                            data: body,
                            rate_index: rate,
                            loop_: loop_bit,
                            chip_id,
                        });
                        idx
                    }
                };
                last_hit = Some(hits.len());
                hits.push(NesDpcmHit {
                    sample_time: t,
                    end_time: 0,
                    sample_index: idx,
                    rate_index: rate,
                    chip_id,
                });
            } else if !bit && playing {
                playing = false;
                // Only this chip's own last trigger stops.
                if let Some(i) = last_hit {
                    hits[i].end_time = t;
                }
            }
        }
    }
    (starts, undumped)
}

fn _median_dt(points: &[(i64, i32)]) -> i64 {
    if points.len() < 2 {
        return 0;
    }
    let mut dts: Vec<i64> = (0..points.len() - 1)
        .map(|i| points[i + 1].0 - points[i].0)
        .collect();
    dts.sort();
    dts[dts.len() / 2]
}

/// High-rate `$4011` writes grouped into bodies.
fn _pcm_bursts(points: &[(i64, i32)]) -> Vec<Vec<(i64, i32)>> {
    let mut out: Vec<Vec<(i64, i32)>> = Vec::new();
    let mut cur: Vec<(i64, i32)> = Vec::new();
    let mut prev: Option<i64> = None;
    for &point in points {
        if prev.is_some() && point.0 - prev.unwrap() <= PCM_MAX_GAP {
            cur.push(point);
        } else {
            if cur.len() >= PCM_MIN_WRITES && _median_dt(&cur) <= PCM_MAX_MEDIAN_DT {
                out.push(cur);
            }
            cur = vec![point];
        }
        prev = Some(point.0);
    }
    if cur.len() >= PCM_MIN_WRITES && _median_dt(&cur) <= PCM_MAX_MEDIAN_DT {
        out.push(cur);
    }
    out
}

/// Sample-and-hold the 7-bit DAC on a uniform timeline (signed 8-bit).
fn _pcm_resample(points: &[(i64, i32)], rate: i32) -> Vec<u8> {
    let t0 = points[0].0;
    let t1 = points[points.len() - 1].0;
    let span = i64::max(1, t1 - t0);
    let frames = usize::max(
        1,
        py_int(py_round(span as f64 * rate as f64 / VGM_RATE as f64)) as usize + 1,
    );
    let mut out = vec![0u8; frames];
    let mut i = 0usize;
    let mut cur = points[0].1;
    let step = VGM_RATE as f64 / rate as f64;
    for k in 0..frames {
        let t = t0 as f64 + k as f64 * step;
        while i + 1 < points.len() && points[i + 1].0 as f64 <= t {
            i += 1;
            cur = points[i].1;
        }
        // Furnace's NES PCM path writes (data8 + 0x80) >> 1 to $4011.
        out[k] = (((cur << 1) - 0x80) & 0xFF) as u8;
    }
    out
}

/// Split resampled PCM at long zero runs; return (frame offset, body).
fn _pcm_sounding_pieces(data: &[u8], rate: i32) -> Vec<(i64, Vec<u8>)> {
    let gap = usize::max(
        1,
        py_int(py_round(
            NES_PCM_SILENCE_GAP as f64 * rate as f64 / VGM_RATE as f64,
        )) as usize,
    );
    let mut out: Vec<(i64, Vec<u8>)> = Vec::new();
    let mut start = 0usize;
    let mut i = 0usize;
    let n = data.len();
    while i < n {
        if data[i] != 0 {
            i += 1;
            continue;
        }
        let mut j = i;
        while j < n && data[j] == 0 {
            j += 1;
        }
        if j - i >= gap {
            if i > start {
                out.push((start as i64, data[start..i].to_vec()));
            }
            start = j;
        }
        i = j;
    }
    if start < n {
        out.push((start as i64, data[start..].to_vec()));
    }
    out
}

/// Direct `$4011` bit-bang PCM (the NES equivalent of SSG volume PCM).
///
/// A dual-APU log bit-bangs either chip, and a burst is one chip's stream: two
/// interleaved drivers would look like one burst with doubled gaps and be
/// thrown away. So the writes are grouped per instance first, while the chunk
/// budget and the body list stay shared.
pub fn collect_nes_pcm(
    vgm: &VgmFile,
    _clock: Option<u32>,
) -> (Vec<NesPcmSample>, Vec<NesPcmHit>, Vec<String>) {
    let warnings: Vec<String> = Vec::new();
    let mut streams: IndexMap<i32, Vec<(i64, i32)>> = IndexMap::new();
    for w in &vgm.writes {
        if w.chip != "nes" || (w.reg & 0x7F) != 0x11 {
            continue;
        }
        let points = streams.entry((w.chip_id & 1) as i32).or_default();
        let value = (w.val & 0x7F) as i32;
        if !points.is_empty() && points[points.len() - 1].0 == w.sample {
            let n = points.len() - 1;
            points[n] = (w.sample, value);
        } else {
            points.push((w.sample, value));
        }
    }
    if streams.is_empty() {
        return (Vec::new(), Vec::new(), warnings);
    }
    // Join short holes, then split at real pauses.
    let mut merged: Vec<(i32, Vec<(i64, i32)>)> = Vec::new();
    let mut ids: Vec<i32> = streams.keys().copied().collect();
    ids.sort();
    for cid in ids {
        let bursts = _pcm_bursts(&streams[&cid]);
        let mut chip_merged: Vec<Vec<(i64, i32)>> = Vec::new();
        for burst in bursts {
            let last_t = chip_merged
                .last()
                .map(|seg| seg[seg.len() - 1].0)
                .unwrap_or(0);
            if !burst.is_empty()
                && !chip_merged.is_empty()
                && burst[0].0 - last_t < PCM_SPLIT_GAP
            {
                chip_merged.last_mut().unwrap().extend(burst);
            } else if !burst.is_empty() {
                chip_merged.push(burst);
            }
        }
        merged.extend(chip_merged.into_iter().map(|seg| (cid, seg)));
    }
    if merged.is_empty() {
        return (Vec::new(), Vec::new(), warnings);
    }
    // (merged index, rate, frame offset, body)
    let mut prepared: Vec<(usize, i32, i64, Vec<u8>)> = Vec::new();
    for (mi, (_cid, seg)) in merged.iter().enumerate() {
        if seg.len() < 2 {
            continue;
        }
        let median = _median_dt(seg);
        if median <= 0 {
            continue;
        }
        let rate = i32::max(
            PCM_MIN_RATE,
            i32::min(
                PCM_MAX_RATE,
                py_int(py_round(VGM_RATE as f64 / median as f64)) as i32,
            ),
        );
        let data = _pcm_resample(seg, rate);
        if data.len() < PCM_MIN_FRAMES {
            continue;
        }
        for (base_off, piece) in _pcm_sounding_pieces(&data, rate) {
            if !piece.is_empty() {
                prepared.push((mi, rate, base_off, piece));
            }
        }
    }
    if prepared.is_empty() {
        return (Vec::new(), Vec::new(), warnings);
    }
    let total_frames: usize = prepared.iter().map(|p| p.3.len()).sum();
    let mut chunk_frames = NES_PCM_CHUNK_FRAMES;
    if total_frames > chunk_frames * NES_PCM_SAMPLE_BUDGET {
        chunk_frames = (total_frames as f64 / NES_PCM_SAMPLE_BUDGET as f64).ceil() as usize;
    }
    let dual = streams.len() > 1;
    let mut samples: Vec<NesPcmSample> = Vec::new();
    let mut hits: Vec<NesPcmHit> = Vec::new();
    let mut by_data: IndexMap<Vec<u8>, i32> = IndexMap::new();
    for (mi, rate, base_off, data) in &prepared {
        let cid = merged[*mi].0;
        let seg = &merged[*mi].1;
        let total = data.len();
        let mut off = 0usize;
        while off < total {
            let end = usize::min(off + chunk_frames, total);
            let part = data[off..end].to_vec();
            let start_t = seg[0].0
                + py_int(py_round(
                    (*base_off + off as i64) as f64 * VGM_RATE as f64 / *rate as f64,
                ));
            let final_chunk = off + part.len() >= total;
            let end_t = seg[0].0
                + py_int(py_round(
                    (*base_off + total as i64) as f64 * VGM_RATE as f64 / *rate as f64,
                ));
            let idx = match by_data.get(&part) {
                Some(&idx) => idx,
                None => {
                    let idx = samples.len() as i32;
                    by_data.insert(part.clone(), idx);
                    samples.push(NesPcmSample {
                        name: if dual {
                            format!("NES PCM {}-{:02}", cid + 1, idx)
                        } else {
                            format!("NES PCM {:02}", idx)
                        },
                        data: part,
                        rate: *rate,
                        start: start_t,
                        end: end_t,
                        chip_id: cid,
                    });
                    idx
                }
            };
            hits.push(NesPcmHit {
                sample_time: start_t,
                // Non-final chunks are cut by the next chunk's note. The last
                // chunk of each sounding piece gets the OFF that covers the
                // following silent gap.
                end_time: if final_chunk { end_t } else { 0 },
                sample_index: idx,
                chip_id: cid,
            });
            off = end;
        }
    }
    (samples, hits, warnings)
}

/// Optionally fold short NES raw-PCM bodies into the hottest/longest ones.
pub fn cluster_nes_pcm(
    samples: Vec<NesPcmSample>,
    mut hits: Vec<NesPcmHit>,
    target: i32,
    hot: i32,
) -> (Vec<NesPcmSample>, Vec<NesPcmHit>) {
    if samples.len() as i64 <= target as i64 {
        return (samples, hits);
    }
    let mut uses: IndexMap<i32, i64> = IndexMap::new();
    for hit in &hits {
        *uses.entry(hit.sample_index).or_insert(0) += 1;
    }
    let bodies: Vec<Vec<u8>> = samples.iter().map(|smp| smp.data.clone()).collect();
    let rates: Vec<i32> = samples.iter().map(|smp| smp.rate).collect();
    let (keep, remap) = cluster_fragments(&bodies, &rates, &uses, target, hot);
    if keep.len() == samples.len() {
        return (samples, hits);
    }
    let new_samples: Vec<NesPcmSample> = keep
        .iter()
        .map(|&i| samples[i as usize].clone())
        .collect();
    for hit in &mut hits {
        hit.sample_index = remap[hit.sample_index as usize];
    }
    (new_samples, hits)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn w(sample: i64, chip_id: u8, reg: u32, val: u32) -> ChipWrite {
        ChipWrite::new(sample, "nes", chip_id, reg, val)
    }

    fn vgm_with(writes: Vec<ChipWrite>) -> VgmFile {
        let mut vgm = VgmFile::default();
        vgm.writes = writes;
        vgm
    }

    #[test]
    fn helpers_and_pulse_note_tracking() {
        assert_eq!(NES_DEFAULT_CLOCK, 1_789_773);
        assert_eq!(nes_dpcm_rate(0, 0), 4182); // 1789773 / 428
        assert_eq!(nes_dpcm_rate(NES_DEFAULT_CLOCK, 15), 33144); // / 54
        assert_eq!(nes_dpcm_rate(NES_DEFAULT_CLOCK, 99), 33144); // clamped
        assert_eq!(nes_dpcm_rate(NES_DEFAULT_CLOCK, -4), 4182); // clamped
        assert_eq!(nes_period_hz(0, 0), 1_789_773.0 / 16.0);
        assert_eq!(_noise_note(0), 75);
        assert_eq!(_noise_note(15), 60);
        assert_eq!(_period_delta(100, 100), 0);
        assert_eq!(_period_delta(100, -1), 0);

        let vgm = vgm_with(vec![
            w(10, 0, 0x00, 0x8F), // duty 2, constant volume 15
            w(20, 0, 0x02, 0xFD), // timer low
            w(30, 0, 0x03, 0x08), // timer high 0, length index 1 (254)
            w(40, 0, 0x00, 0x8B), // volume 11, same gate
            w(200, 0, 0x15, 0x1E), // pulse 1 off after NES_MIN_NOTE
        ]);
        let (hits, offs, warnings) = collect_nes_tones(&vgm, None);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].sample_time, 30);
        assert_eq!(hits[0].voice, 0);
        assert_eq!(hits[0].period, 0xFD);
        assert_eq!(hits[0].duty, 2);
        assert_eq!(hits[0].vol, 15);
        assert_eq!(hits[0].off_sample, 200);
        assert_eq!(hits[0].vol_pts, vec![(40, 11)]);
        assert_eq!(
            offs,
            vec![NesOff {
                sample_time: 200,
                voice: 0,
                chip_id: 0
            }]
        );
        assert!(warnings.is_empty());
    }

    #[test]
    fn triangle_note_ends_at_its_linear_counter() {
        let vgm = vgm_with(vec![
            w(10, 0, 0x08, 0x7F),
            w(20, 0, 0x0A, 0xFD),
            w(30, 0, 0x0B, 0x08), // timer high 0, length index 1
        ]);
        let (hits, offs, _warnings) = collect_nes_tones(&vgm, None);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].voice, 2);
        assert_eq!(hits[0].linear, 0x7F);
        // 127 quarter frames at 44100 Hz = 23336.25 samples, rounded.
        assert_eq!(hits[0].off_sample, 30 + 23336);
        assert_eq!(offs.len(), 1);
        assert_eq!(offs[0].sample_time, 30 + 23336);
        assert_eq!(offs[0].voice, 2);
    }

    #[test]
    fn dual_apu_instances_keep_separate_register_files() {
        let mut writes = Vec::new();
        for (cid, t0) in [(0u8, 0i64), (1u8, 1000i64)] {
            writes.push(w(t0 + 10, cid, 0x00, 0x8F));
            writes.push(w(t0 + 20, cid, 0x02, 0xFD));
            writes.push(w(t0 + 30, cid, 0x03, 0x08));
        }
        let vgm = vgm_with(writes);
        let (hits, offs, _warnings) = collect_nes_tones(&vgm, None);
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].chip_id, 0);
        assert_eq!(hits[1].chip_id, 1);
        assert!(hits[0].sample_time < hits[1].sample_time);
        assert!(offs.is_empty());
    }

    #[test]
    fn dpcm_bank_copy_replays_in_stream_order() {
        let mut vgm = vgm_with(vec![
            w(0, 0, 0x12, 0x00),
            w(0, 0, 0x13, 0x00),
            w(0, 0, 0x15, 0x10),   // before the bank copy: 0xAA
            w(150, 0, 0x15, 0x00), // stop
            w(200, 0, 0x15, 0x10), // after the copy at 100: 0x11
        ]);
        vgm.nes_dpcm_bank = vec![0x11, 0x22, 0x33, 0x44];
        vgm.nes_dpcm_bank_base.insert(0, 0);
        vgm.pcm_ram_writes.push(PcmRamWrite {
            sample: 100,
            type_: 7,
            chip_id: 0,
            src: 0,
            dest: 0xC000,
            length: 4,
        });
        let (samples, hits, warnings) = collect_nes_dpcm(&vgm, None);
        assert_eq!(samples.len(), 2);
        assert_eq!(samples[0].start, 0xC000);
        assert_eq!(samples[0].length, 1);
        assert_eq!(samples[0].rate_index, 0);
        assert!(!samples[0].loop_);
        assert_eq!(samples[0].data, vec![0xAA]);
        assert_eq!(samples[1].data, vec![0x11]);
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].sample_index, 0);
        assert_eq!(hits[0].end_time, 150);
        assert_eq!(
            warnings,
            vec![
                "1 NES DPCM sample(s) point at RAM the VGM does not dump; those notes are silent"
                    .to_string()
            ]
        );
    }

    #[test]
    fn bit_banged_dac_becomes_one_signed_pcm_body() {
        let mut writes = Vec::new();
        for i in 0..200i64 {
            writes.push(w(i, 0, 0x11, (64 + i % 10) as u32));
        }
        let vgm = vgm_with(writes);
        let (samples, hits, warnings) = collect_nes_pcm(&vgm, None);
        assert_eq!(samples.len(), 1);
        assert_eq!(samples[0].rate, 32000); // median dt 1 -> clamped
        assert_eq!(samples[0].name, "NES PCM 00");
        assert!(samples[0].data.len() >= PCM_MIN_FRAMES);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].sample_time, samples[0].start);
        assert_eq!(hits[0].end_time, samples[0].end);
        assert!(warnings.is_empty());
    }
}
