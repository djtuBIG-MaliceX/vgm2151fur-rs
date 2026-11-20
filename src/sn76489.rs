//! SN76489 (Genesis PSG) and Toshiba T6W28 serial stream -> notes.
//!
//! Literal port of `vgm2151fur/sn76489.py` from vgm2151fur 0.11.0.
//!
//! The chip has no key-on: a voice sounds while its 4-bit attenuation is below
//! 15. Tone period is 10 bits (latch low nibble, then a high data byte); the
//! noise channel takes a 3-bit mode (white/periodic + three rates or channel
//! 3's tone). Furnace's SMS platform exposes the same registers, so a source
//! period is mapped through the period pitch table (toneDivider 64) and played
//! back as the same register value.
//!
//! A dual SN76489 keeps a second, independent register file on command 0x30. A
//! T6W28 (Neo Geo Pocket Color) uses the same two streams as the two halves of
//! one stereo chip: the left half carries tone periods and left attenuation,
//! the right half right attenuation plus the noise parameters. Furnace's
//! T6W28 platform runs its pitch table at divider 16 (the noise channel at 15)
//! and takes independent left/right volumes, which this collector maps onto
//! the volume column plus 08xy panning.
//!
//! Mid-note period writes become pitch-macro points; volume writes become
//! volume-column steps. A period jump of a semitone or more is a new note.

use std::sync::Arc;

use crate::parity::{py_int, py_round};
use crate::pitch::{furnace_period_note, pitch_table, PeriodPitchTable};
use crate::vgm::{ChipWrite, VgmFile};

// Furnace's SMS platform default (COLOR_NTSC) and period divider. The VGM
// header normally says 3579545, which is within a hertz of this.
pub const PSG_FURNACE_CLOCK: f64 = 315_000_000.0 / 88.0;
pub const PSG_DEFAULT_CLOCK: i64 = 3_579_545;
pub const PSG_DIVIDER: i32 = 64;
// The writer asks for Furnace's named COLOR_NTSC clock when the header value
// is within this many hertz of it; the note/pitch tables must then use that
// named clock too, or the emitted period is a fraction of a unit off.
pub const PSG_CLOCK_TOLERANCE: f64 = 2.0;
// Furnace's T6W28 platform uses its own clock (3072000 Hz) and the SN pitch
// tables with dividers 16 (tone) and 15 (noise).
pub const T6W28_CLOCK: f64 = 3_072_000.0;
pub const T6W28_TONE_DIVIDER: f64 = 16.0;
pub const T6W28_NOISE_DIVIDER: f64 = 15.0;
pub const PSG_MAX_PERIOD: i32 = 0x3FF;
pub const PSG_MIN_NOTE: i64 = 100; // shorter gates are driver housekeeping
pub const PSG_NOTE_JUMP_ST: f64 = 1.0; // a larger period jump is a new note
pub const PSG_ATTACK_STEP: i32 = 2; // a volume rise this large is a new attack
// E8xx/E9xx move at most 15 semitones on one row. A larger channel-3 noise
// period jump is split into a pitch-only legato note instead of a multi-row
// slide, because the source changes the period register in one write.
pub const PSG_NOISE_LEGATO_ST: f64 = 15.0;

#[derive(Clone, Debug, PartialEq)]
pub struct PsgHit {
    pub sample_time: i64,
    pub voice: i32,
    /// 10-bit tone/noise period (0 for fixed noise).
    pub period: i32,
    /// Furnace volume 0-15 (15 = loudest).
    pub vol: i32,
    pub off_sample: i64,
    /// 0 = first SN76489, 1 = second/T6W28 right half.
    pub chip: i32,
    /// The instance's own clock. A dual log may run the two chips from
    /// different crystals; the note table has to invert the same clock the
    /// module ends up using, or the register period is rescaled.
    pub clock: i64,
    pub noise: bool,
    pub noise_mode: i32,
    /// Register mode while the gate is still open. `noise_mode` stays the
    /// mode the note started with, so analyze emits that as the note's own
    /// 20xy and the later changes ride `mode_pts`.
    pub noise_mode_now: i32,
    /// Fixed noise rate / channel-3 select (bits 0-1 of the noise register).
    /// Furnace derives that rate from the note, not from effect 20xy, so a
    /// mid-note change needs a new note with the new `noise_mode`.
    pub noise_spec: i32,
    /// Game Gear 08xy pan value (0xF0 left, 0x0F right, 0xFF both). Zero when
    /// the source did not declare stereo. On a T6W28 this is the independent
    /// x=left / y=right split computed from the two attenuation nibbles.
    pub pan: i32,
    pub opened_pan: i32,
    pub t6w28: bool,
    /// This note only changes pitch; it continues the previous noise gate.
    /// A channel-3 noise period jump larger than E8xx can carry in one row is
    /// split here and committed with EA01, so the new period lands instantly
    /// instead of sliding over several rows.
    pub legato: bool,
    /// The note this split continued. If the new note is muted on the same
    /// sample it has no duration and is discarded; the predecessor then still
    /// needs its key-off, or the noise channel rings until the next key-on.
    pub split_from: Option<Box<PsgHit>>,
    /// T6W28 left/right attenuation steps and their pan at the same instant.
    pub mix_pts: Vec<(i64, i32, i32)>,
    pub pan_pts: Vec<(i64, i32)>,
    pub pitch_pts: Vec<(i64, i32)>,
    pub vol_pts: Vec<(i64, i32)>,
    pub mode_pts: Vec<(i64, i32)>,
}

impl Default for PsgHit {
    fn default() -> Self {
        Self {
            sample_time: 0,
            voice: 0,
            period: 0,
            vol: 0,
            off_sample: 0,
            chip: 0,
            clock: 0,
            noise: false,
            noise_mode: 0,
            noise_mode_now: -1,
            noise_spec: 0,
            pan: 0,
            opened_pan: 0,
            t6w28: false,
            legato: false,
            split_from: None,
            mix_pts: Vec::new(),
            pan_pts: Vec::new(),
            pitch_pts: Vec::new(),
            vol_pts: Vec::new(),
            mode_pts: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Default)]
pub struct PsgOff {
    pub sample_time: i64,
    pub voice: i32,
    pub chip: i32,
}

/// Register value -> (Furnace note, E5) for one period pitch table.
fn _period_note(period: i32, clock: f64, divider: f64) -> (i32, i32) {
    let value = period.clamp(0, PSG_MAX_PERIOD);
    let tbl = pitch_table(clock, divider, PSG_MAX_PERIOD);
    let shortest = tbl.value(179, 127).max(1);
    let value = value.max(shortest);
    // Prefer a note whose *base* period is exactly the source period: an E5
    // fine tune is applied after the note-on, so a one-unit late write was
    // visible as a wrong pitch at the gate. Fall back to the nearest
    // note/fine pair when the table has no exact base entry.
    let centre = py_int(py_round(tbl.note_fraction(value)));
    for note in (centre - 3).max(0)..=(centre + 3).min(179) {
        let note = note as i32;
        if tbl.value(note, 0) == value {
            return (note, 0x80);
        }
    }
    furnace_period_note(clock, divider, value, 0, PSG_MAX_PERIOD)
}

/// The clock the SMS system will actually run at.
///
/// The writer leaves a clock within `PSG_CLOCK_TOLERANCE` of the header
/// default on Furnace's named COLOR_NTSC selection; every other clock goes
/// out as `customClock`. The period table must use the same value.
pub fn psg_module_clock(clock: Option<f64>) -> f64 {
    let value = match clock {
        Some(c) if c != 0.0 => c,
        _ => PSG_DEFAULT_CLOCK as f64,
    };
    if (value - PSG_DEFAULT_CLOCK as f64).abs() <= PSG_CLOCK_TOLERANCE {
        PSG_FURNACE_CLOCK
    } else {
        value
    }
}

/// 10-bit SN76489 period -> (Furnace note, E5) for the same register.
///
/// `clock` is the chip instance's own clock (a dual log can run two
/// crystals); the inverse table must use it so the module writes the source
/// period back unchanged. Period values below the pitch table's highest note
/// (a very high source frequency, e.g. 14) cannot be represented by a note;
/// clamp to the shortest period the table can produce instead of letting the
/// platform fall back to its slowest one (1023).
pub fn psg_note(period: i32, clock: Option<f64>) -> (i32, i32) {
    _period_note(period, psg_module_clock(clock), PSG_DIVIDER as f64)
}

/// T6W28 tone register -> (note, E5) for Furnace's 16x divider.
pub fn t6w28_note(period: i32, clock: i64) -> (i32, i32) {
    _period_note(
        period,
        if clock != 0 { clock as f64 } else { T6W28_CLOCK },
        T6W28_TONE_DIVIDER,
    )
}

/// T6W28 noise period -> (note, E5) for Furnace's 15x noise divider.
pub fn t6w28_noise_note(period: i32, clock: i64) -> (i32, i32) {
    _period_note(
        period,
        if clock != 0 { clock as f64 } else { T6W28_CLOCK },
        T6W28_NOISE_DIVIDER,
    )
}

pub fn psg_pitch_table() -> Arc<PeriodPitchTable> {
    pitch_table(PSG_FURNACE_CLOCK, PSG_DIVIDER as f64, PSG_MAX_PERIOD)
}

/// Actual SN76489 tone frequency for a period, for reports/audits.
pub fn psg_rate(clock: i64, period: i32) -> f64 {
    let clock = if clock != 0 {
        clock
    } else {
        PSG_DEFAULT_CLOCK
    };
    clock as f64 / (32.0 * period.max(1) as f64)
}

fn _jump_st(a: i32, b: i32) -> f64 {
    if a <= 0 || b <= 0 {
        return 12.0;
    }
    (12.0 * (b as f64 / a as f64).log2()).abs()
}

/// Read every SN76489/T6W28 instance in a log.
pub fn collect_sn76489(
    vgm: &VgmFile,
    clock: Option<i64>,
) -> (Vec<PsgHit>, Vec<PsgOff>, Vec<String>) {
    let mut warnings: Vec<String> = Vec::new();
    let writes: Vec<&ChipWrite> = vgm
        .writes
        .iter()
        .filter(|w| w.chip == "sn76489")
        .collect();
    if writes.is_empty() {
        return (Vec::new(), Vec::new(), warnings);
    }
    let clock = match clock {
        Some(c) if c != 0 => c,
        _ => {
            if vgm.sn76489_clock != 0 {
                vgm.sn76489_clock as i64
            } else {
                PSG_DEFAULT_CLOCK
            }
        }
    };
    if vgm.sn76489_t6w28 {
        return _collect_t6w28(&writes, clock, warnings);
    }
    let mut by_chip: std::collections::BTreeMap<u8, Vec<&ChipWrite>> =
        std::collections::BTreeMap::new();
    for w in &writes {
        by_chip.entry(w.chip_id).or_default().push(w);
    }
    let stereo = vgm.sn76489_stereo;
    let mut hits: Vec<PsgHit> = Vec::new();
    let mut offs: Vec<PsgOff> = Vec::new();
    let second_clock = vgm.sn76489_clock_2 as i64;
    for (cid, ws) in &by_chip {
        // libvgm gives the second instance the first one's clock when the
        // extra header has no entry for it; a VGM 1.70+ log may carry a
        // different crystal and the writer passes that one through.
        let chip_clock = if *cid == 1 && second_clock != 0 {
            second_clock
        } else {
            clock
        };
        let (h, o, warn) = _collect_plain(ws, chip_clock, stereo, *cid as i32);
        hits.extend(h);
        offs.extend(o);
        warnings.extend(warn);
    }
    (hits, offs, warnings)
}

fn _collect_plain(
    writes: &[&ChipWrite],
    clock: i64,
    stereo: bool,
    chip: i32,
) -> (Vec<PsgHit>, Vec<PsgOff>, Vec<String>) {
    let mut state = PlainCollector {
        clock,
        stereo,
        chip,
        stereo_mask: 0xFF,
        freq: [0; 3],
        freq_high: [false; 3],
        vol: [15; 4],
        noise_mode: 0,
        latch_ch: 0,
        latch_vol: false,
        active: [None, None, None, None],
        hits: Vec::new(),
        offs: Vec::new(),
        glitches: 0,
        // A driver retriggers a tone by writing the same period and a new
        // volume on one sample. Remember the period rewrite until the volume
        // write at the same sample turns it into a note. -1 means none
        // pending.
        retrigger_t: [-1; 4],
    };
    let mut warnings: Vec<String> = Vec::new();
    state.run(writes);
    if state.glitches != 0 {
        warnings.push(format!(
            "SN76489: ignored {} sub-frame gates",
            state.glitches
        ));
    }
    (state.hits, state.offs, warnings)
}

/// The closed hit handed back to a split caller, plus whether Python's
/// `close()` appended it to `hits` (a sub-frame glitch is discarded but the
/// object still becomes `split_from`).
struct ClosedHit {
    hit: PsgHit,
    push: bool,
}

struct PlainCollector {
    clock: i64,
    stereo: bool,
    chip: i32,
    stereo_mask: i32,
    freq: [i32; 3],
    freq_high: [bool; 3],
    vol: [i32; 4],
    noise_mode: i32,
    latch_ch: usize,
    latch_vol: bool,
    active: [Option<PsgHit>; 4],
    hits: Vec<PsgHit>,
    offs: Vec<PsgOff>,
    glitches: i32,
    retrigger_t: [i64; 4],
}

impl PlainCollector {
    /// The 08xy value for one voice under the current Game Gear mask.
    fn pan_for(&self, voice: usize) -> i32 {
        if !self.stereo {
            return 0;
        }
        let left = if self.stereo_mask & (0x10 << voice) != 0 {
            0xF0
        } else {
            0
        };
        let right = if self.stereo_mask & (0x01 << voice) != 0 {
            0x0F
        } else {
            0
        };
        left | right
    }

    fn close_hit(&mut self, voice: usize, t: i64, emit_off: bool) -> Option<ClosedHit> {
        let mut hit = self.active[voice].take()?;
        hit.off_sample = t;
        if t > 0 {
            // Nothing at or after the close is heard on this note. A noise
            // mode write and the gate-off can share one source sample; the
            // point belongs to the next note, not this one.
            hit.mode_pts.retain(|pt| pt.0 < t);
            hit.pitch_pts.retain(|pt| pt.0 < t);
            hit.vol_pts.retain(|pt| pt.0 < t);
        }
        if t <= 0 {
            // End of the log while the gate is still open: a held note.
            return Some(ClosedHit { hit, push: true });
        }
        if t - hit.sample_time >= PSG_MIN_NOTE {
            if emit_off {
                self.offs.push(PsgOff {
                    sample_time: t,
                    voice: voice as i32,
                    chip: self.chip,
                });
            }
            return Some(ClosedHit { hit, push: true });
        }
        self.glitches += 1;
        if hit.split_from.is_some() {
            // This split note was muted on the sample it started, so it
            // never sounds. The note it continued still has to stop.
            self.offs.push(PsgOff {
                sample_time: t,
                voice: voice as i32,
                chip: self.chip,
            });
        }
        Some(ClosedHit { hit, push: false })
    }

    fn close(&mut self, voice: usize, t: i64, emit_off: bool) {
        if let Some(closed) = self.close_hit(voice, t, emit_off) {
            if closed.push {
                self.hits.push(closed.hit);
            }
        }
    }

    /// `close(voice, t, emit_off=False); open_hit(voice, t); new.split_from =
    /// old` (with the optional legato flag the noise path adds).
    fn reopen(&mut self, voice: usize, t: i64, legato: bool) {
        let old = self.close_hit(voice, t, false);
        self.open_hit(voice, t);
        if let Some(closed) = old {
            if closed.push {
                self.hits.push(closed.hit.clone());
            }
            if let Some(new) = self.active[voice].as_mut() {
                new.split_from = Some(Box::new(closed.hit));
                if legato {
                    new.legato = true;
                }
            }
        }
    }

    fn noise_period(&self) -> i32 {
        self.freq[2].clamp(0, PSG_MAX_PERIOD)
    }

    /// Follow a channel-3 period write. True when the note was split.
    fn set_noise_period(&mut self, t: i64) -> bool {
        let period = self.noise_period();
        let hit = self.active[3].as_ref().expect("noise gate is open");
        if hit.period == period {
            return false;
        }
        if t == hit.sample_time {
            self.active[3].as_mut().unwrap().period = period;
            return false;
        }
        let last = hit.pitch_pts.last().map(|p| p.1).unwrap_or(hit.period);
        if _jump_st(last, period) > PSG_NOISE_LEGATO_ST {
            self.reopen(3, t, true);
            return true;
        }
        let hit = self.active[3].as_mut().unwrap();
        if hit.pitch_pts.last().map(|p| p.1) != Some(period) {
            hit.pitch_pts.push((t, period));
        }
        false
    }

    fn open_hit(&mut self, voice: usize, t: i64) {
        let pan = self.pan_for(voice);
        let mut level = 15 - self.vol[voice];
        if self.stereo && pan == 0 {
            // Both Game Gear sides are off for this voice. The SMS panning
            // command cannot say "both off" (zero is read as centre), so the
            // level column carries the silence.
            level = 0;
        }
        let period = if voice < 3 {
            self.freq[voice]
        } else if (self.noise_mode & 3) == 3 {
            self.noise_period()
        } else {
            0
        };
        self.active[voice] = Some(PsgHit {
            sample_time: t,
            voice: voice as i32,
            period,
            vol: level,
            chip: self.chip,
            clock: self.clock,
            noise: voice == 3,
            noise_mode: if voice == 3 { self.noise_mode } else { 0 },
            noise_mode_now: if voice == 3 { self.noise_mode } else { -1 },
            noise_spec: if voice == 3 { self.noise_mode & 3 } else { 0 },
            pan,
            opened_pan: pan,
            ..Default::default()
        });
    }

    fn evaluate_tone(&mut self, voice: usize, t: i64) {
        if voice == 2 && self.active[3].is_some() && (self.noise_mode & 3) == 3 {
            // Noise mode 3 borrows tone generator 3's period. Track that
            // register so the noise note follows it too.
            self.set_noise_period(t);
        }
        // Period 0 is the usual silence gate on this chip (the tone generator
        // cannot divide by it). It must close the note, not become a new one.
        let on = self.freq_high[voice] && self.vol[voice] < 15 && self.freq[voice] > 0;
        if self.retrigger_t[voice] >= 0 {
            let level = 15 - self.vol[voice];
            let last_level = match self.active[voice].as_ref() {
                Some(hit) => hit.vol_pts.last().map(|p| p.1).unwrap_or(hit.vol),
                None => level,
            };
            if self.retrigger_t[voice] == t
                && on
                && self.active[voice].is_some()
                && level != last_level
            {
                // The driver wrote this voice's period again at this sample
                // without changing it, and is now writing a different volume.
                // That pair is a note retrigger, not a volume step on the held
                // note. A volume write with the same level is housekeeping.
                self.retrigger_t[voice] = -1;
                self.reopen(voice, t, false);
                return;
            }
            self.retrigger_t[voice] = -1;
        }
        let present = self.active[voice].is_some();
        if on && !present {
            self.open_hit(voice, t);
            return;
        }
        if !on {
            if present {
                self.close(voice, t, true);
            }
            return;
        }
        let hit = self.active[voice].as_ref().expect("gate is open");
        let same_sample = t == hit.sample_time;
        let period = self.freq[voice].clamp(0, PSG_MAX_PERIOD);
        if period != hit.period {
            if same_sample {
                // The high byte and the gate share one sample: the final
                // period is the note.
                self.active[voice].as_mut().unwrap().period = period;
            } else {
                // Compare with the pitch the voice is on now, not the note's
                // original period: a slow glide is many sub-semitone steps,
                // and comparing against the start accumulated the drift and
                // split it into a new note every semitone.
                let last = hit.pitch_pts.last().map(|p| p.1).unwrap_or(hit.period);
                if _jump_st(last, period) > PSG_NOTE_JUMP_ST {
                    self.close(voice, t, false);
                    self.open_hit(voice, t);
                    return;
                }
                let hit = self.active[voice].as_mut().unwrap();
                if hit.pitch_pts.last().map(|p| p.1) != Some(period) {
                    hit.pitch_pts.push((t, period));
                }
            }
        }
        let level = 15 - self.vol[voice];
        let hit = self.active[voice].as_ref().unwrap();
        let last_level = hit.vol_pts.last().map(|p| p.1).unwrap_or(hit.vol);
        if last_level != level {
            if same_sample {
                self.active[voice].as_mut().unwrap().vol = level;
            } else if level >= last_level + PSG_ATTACK_STEP {
                // A clear attenuation jump back up is a new attack, not just a
                // volume step. Put a note on the row so it shows in the
                // pattern; the SN76489 has no envelope to restart.
                self.reopen(voice, t, false);
                return;
            } else {
                let hit = self.active[voice].as_mut().unwrap();
                if hit.vol_pts.last().map(|p| p.1) != Some(level) {
                    hit.vol_pts.push((t, level));
                }
            }
        }
    }

    fn evaluate_noise(&mut self, t: i64) {
        let on = self.vol[3] < 15;
        let present = self.active[3].is_some();
        if on && !present {
            self.open_hit(3, t);
            return;
        }
        if !on {
            if present {
                self.close(3, t, true);
            }
            return;
        }
        let hit = self.active[3].as_ref().expect("noise gate is open");
        let same_sample = t == hit.sample_time;
        let current = if hit.noise_mode_now >= 0 {
            hit.noise_mode_now
        } else {
            hit.noise_mode
        };
        let noise_mode = self.noise_mode;
        if current != noise_mode {
            let spec = noise_mode & 3;
            if same_sample {
                let hit = self.active[3].as_mut().unwrap();
                hit.noise_mode = noise_mode;
                hit.noise_mode_now = noise_mode;
                hit.noise_spec = spec;
            } else if hit.noise_spec != spec {
                // The fixed rate (or the channel-3 switch) selects the note
                // on this platform; 20xy only carries the white/periodic and
                // channel-3 bits. A rate change is therefore a new note, just
                // like the source's noise-register write.
                self.reopen(3, t, false);
                return;
            } else {
                let hit = self.active[3].as_mut().unwrap();
                if hit.mode_pts.last().map(|p| p.1) != Some(noise_mode) {
                    hit.mode_pts.push((t, noise_mode));
                }
                hit.noise_mode_now = noise_mode;
                hit.noise_spec = spec;
            }
        }
        if (noise_mode & 3) == 3 && self.set_noise_period(t) {
            return;
        }
        let level = 15 - self.vol[3];
        let hit = self.active[3].as_ref().unwrap();
        let last_level = hit.vol_pts.last().map(|p| p.1).unwrap_or(hit.vol);
        if last_level != level {
            if same_sample {
                self.active[3].as_mut().unwrap().vol = level;
            } else if level >= last_level + PSG_ATTACK_STEP {
                // Same attack rule as the tone voices: a clear jump back up is
                // a new note, not a volume step inside one note.
                self.reopen(3, t, false);
                return;
            } else {
                let hit = self.active[3].as_mut().unwrap();
                if hit.vol_pts.last().map(|p| p.1) != Some(level) {
                    hit.vol_pts.push((t, level));
                }
            }
        }
    }

    fn run(&mut self, writes: &[&ChipWrite]) {
        for w in writes {
            let b = (w.val & 0xFF) as i32;
            let t = w.sample;
            if w.reg == 1 {
                // Game Gear stereo mask. A change during a held note is a pan
                // point; a mask that turns both sides off also mutes that
                // voice through the level column (the SMS 08xy zero reads as
                // centre).
                if b != self.stereo_mask {
                    self.stereo_mask = b;
                    for voice in 0..4 {
                        if self.active[voice].is_none() {
                            continue;
                        }
                        let new_pan = self.pan_for(voice);
                        let old_pan = self.active[voice].as_ref().unwrap().pan;
                        if new_pan == old_pan {
                            continue;
                        }
                        let cur_vol = self.vol[voice];
                        let hit = self.active[voice].as_mut().unwrap();
                        if t == hit.sample_time {
                            hit.pan = new_pan;
                            hit.opened_pan = new_pan;
                            if new_pan == 0 {
                                hit.vol = 0;
                            } else if old_pan == 0 {
                                hit.vol = 15 - cur_vol;
                            }
                            continue;
                        }
                        if new_pan != 0 {
                            hit.pan_pts.push((t, new_pan));
                        }
                        if new_pan == 0 {
                            hit.vol_pts.push((t, 0));
                        } else if old_pan == 0 {
                            hit.vol_pts.push((t, 15 - cur_vol));
                        }
                        hit.pan = new_pan;
                    }
                }
                continue;
            }
            if b & 0x80 != 0 {
                self.latch_ch = ((b >> 5) & 3) as usize;
                self.latch_vol = (b & 0x10) != 0;
                let data = b & 0x0F;
                if self.latch_vol {
                    self.vol[self.latch_ch] = data;
                    if self.latch_ch == 3 {
                        self.evaluate_noise(t);
                    } else {
                        self.evaluate_tone(self.latch_ch, t);
                    }
                } else if self.latch_ch == 3 {
                    self.noise_mode = data & 0x07;
                    self.evaluate_noise(t);
                } else {
                    self.freq[self.latch_ch] = (self.freq[self.latch_ch] & 0x3F0) | data;
                }
            } else {
                if self.latch_vol || self.latch_ch == 3 {
                    continue;
                }
                let data = b & 0x3F;
                let ch = self.latch_ch;
                let old_period = self.freq[ch];
                self.freq[ch] = (old_period & 0x00F) | (data << 4);
                self.freq_high[ch] = true;
                let retrigger = old_period > 0
                    && self.freq[ch] == old_period
                    && self.active[ch].is_some()
                    && t > self.active[ch].as_ref().unwrap().sample_time;
                self.evaluate_tone(ch, t);
                if retrigger {
                    // Wait for a volume write at this sample. A bare period
                    // refresh is only housekeeping.
                    self.retrigger_t[ch] = t;
                }
            }
        }
        for voice in 0..4 {
            if self.active[voice].is_some() {
                // Still sounding at the end of the log: a held note, no off.
                self.close(voice, 0, false);
            }
        }
    }
}

fn _collect_t6w28(
    writes: &[&ChipWrite],
    clock: i64,
    mut warnings: Vec<String>,
) -> (Vec<PsgHit>, Vec<PsgOff>, Vec<String>) {
    let mut state = T6W28Collector {
        clock,
        freq: [0; 3],
        freq_high: [false; 3],
        vol_l: [15; 4],
        vol_r: [15; 4],
        noise_select: 0,
        noise_white: false,
        noise_extra: 0,
        latch_l_ch: 0,
        latch_r_ch: 0,
        latch_l_vol: false,
        latch_r_vol: false,
        active: [None, None, None, None],
        hits: Vec::new(),
        offs: Vec::new(),
        glitches: 0,
    };
    state.run(writes);
    if state.glitches != 0 {
        warnings.push(format!("T6W28: ignored {} sub-frame gates", state.glitches));
    }
    (state.hits, state.offs, warnings)
}

/// Decode the two SN76489-compatible halves of one T6W28.
///
/// The left stream (command 0x50) owns the three tone periods and the left
/// attenuation; the right stream (0x30) owns the right attenuation and the
/// noise select/extra period. Furnace's T6W28 reads an independent panL and
/// panR, so the two attenuations become one volume column plus an 08xy split.
struct T6W28Collector {
    clock: i64,
    freq: [i32; 3],
    freq_high: [bool; 3],
    vol_l: [i32; 4],
    vol_r: [i32; 4],
    noise_select: i32,
    noise_white: bool,
    noise_extra: i32,
    latch_l_ch: usize,
    latch_r_ch: usize,
    latch_l_vol: bool,
    latch_r_vol: bool,
    active: [Option<PsgHit>; 4],
    hits: Vec<PsgHit>,
    offs: Vec<PsgOff>,
    glitches: i32,
}

impl T6W28Collector {
    fn levels(&self, voice: usize) -> Option<(i32, i32)> {
        let att_l = self.vol_l[voice];
        let att_r = self.vol_r[voice];
        let low = att_l.min(att_r);
        if low >= 15 {
            return None;
        }
        let pan_l = 15 - (att_l - low);
        let pan_r = 15 - (att_r - low);
        Some((15 - low, (pan_l << 4) | pan_r))
    }

    fn close_hit(&mut self, voice: usize, t: i64, emit_off: bool) -> Option<ClosedHit> {
        let mut hit = self.active[voice].take()?;
        hit.off_sample = t;
        // A two-write gate-off (left then right at the same sample) briefly
        // looks like a one-sided fade. Nothing at or after the close is heard.
        if t > 0 {
            hit.mix_pts.retain(|pt| pt.0 < t);
            hit.pitch_pts.retain(|pt| pt.0 < t);
            hit.mode_pts.retain(|pt| pt.0 < t);
        }
        if t <= 0 {
            return Some(ClosedHit { hit, push: true });
        }
        if t - hit.sample_time >= PSG_MIN_NOTE {
            if emit_off {
                self.offs.push(PsgOff {
                    sample_time: t,
                    voice: voice as i32,
                    chip: 0,
                });
            }
            return Some(ClosedHit { hit, push: true });
        }
        self.glitches += 1;
        Some(ClosedHit { hit, push: false })
    }

    fn close(&mut self, voice: usize, t: i64, emit_off: bool) {
        if let Some(closed) = self.close_hit(voice, t, emit_off) {
            if closed.push {
                self.hits.push(closed.hit);
            }
        }
    }

    fn noise_period(&self) -> i32 {
        let value = if self.noise_select < 3 {
            0x100 << self.noise_select
        } else {
            self.noise_extra
        };
        value.clamp(0, PSG_MAX_PERIOD)
    }

    fn open_hit(&mut self, voice: usize, t: i64) {
        let Some((level, pan)) = self.levels(voice) else {
            return;
        };
        self.active[voice] = Some(PsgHit {
            sample_time: t,
            voice: voice as i32,
            period: if voice < 3 {
                self.freq[voice]
            } else {
                self.noise_period()
            },
            vol: level,
            chip: 0,
            clock: self.clock,
            noise: voice == 3,
            noise_mode: if voice == 3 {
                if self.noise_white { 4 } else { 0 }
            } else {
                0
            },
            noise_mode_now: if voice == 3 {
                if self.noise_white { 4 } else { 0 }
            } else {
                -1
            },
            pan,
            opened_pan: pan,
            t6w28: true,
            ..Default::default()
        });
    }

    fn set_levels(&mut self, voice: usize, t: i64) {
        let Some(hit) = self.active[voice].as_ref() else {
            return;
        };
        let Some((level, pan)) = self.levels(voice) else {
            return;
        };
        if t == hit.sample_time {
            let hit = self.active[voice].as_mut().unwrap();
            hit.vol = level;
            hit.pan = pan;
            hit.opened_pan = pan;
        } else {
            let hit = self.active[voice].as_ref().unwrap();
            let last = hit
                .mix_pts
                .last()
                .map(|pt| (pt.1, pt.2))
                .unwrap_or((hit.vol, hit.pan));
            if (level, pan) != last {
                self.active[voice]
                    .as_mut()
                    .unwrap()
                    .mix_pts
                    .push((t, level, pan));
            }
        }
    }

    fn evaluate_tone(&mut self, voice: usize, t: i64) {
        let lp = self.levels(voice);
        let on = self.freq_high[voice] && self.freq[voice] > 0 && lp.is_some();
        let present = self.active[voice].is_some();
        if on && !present {
            self.open_hit(voice, t);
            return;
        }
        if !on {
            if present {
                self.close(voice, t, true);
            }
            return;
        }
        let hit = self.active[voice].as_ref().expect("gate is open");
        let same_sample = t == hit.sample_time;
        let period = self.freq[voice].clamp(0, PSG_MAX_PERIOD);
        if period != hit.period {
            if same_sample {
                self.active[voice].as_mut().unwrap().period = period;
            } else {
                let last = hit.pitch_pts.last().map(|p| p.1).unwrap_or(hit.period);
                if _jump_st(last, period) > PSG_NOTE_JUMP_ST {
                    self.close(voice, t, false);
                    self.open_hit(voice, t);
                    return;
                }
                let hit = self.active[voice].as_mut().unwrap();
                if hit.pitch_pts.last().map(|p| p.1) != Some(period) {
                    hit.pitch_pts.push((t, period));
                }
            }
        }
        self.set_levels(voice, t);
    }

    fn evaluate_noise(&mut self, t: i64) {
        let on = self.vol_l[3] < 15 || self.vol_r[3] < 15;
        let present = self.active[3].is_some();
        if on && !present {
            self.open_hit(3, t);
            return;
        }
        if !on {
            if present {
                self.close(3, t, true);
            }
            return;
        }
        let hit = self.active[3].as_ref().expect("noise gate is open");
        let same_sample = t == hit.sample_time;
        let mode = if self.noise_white { 4 } else { 0 };
        let current = if hit.noise_mode_now >= 0 {
            hit.noise_mode_now
        } else {
            hit.noise_mode
        };
        if current != mode {
            if same_sample {
                self.active[3].as_mut().unwrap().noise_mode = mode;
            } else {
                let hit = self.active[3].as_ref().unwrap();
                if hit.mode_pts.last().map(|p| p.1) != Some(mode) {
                    self.active[3]
                        .as_mut()
                        .unwrap()
                        .mode_pts
                        .push((t, mode));
                }
            }
            self.active[3].as_mut().unwrap().noise_mode_now = mode;
        }
        // The T6W28 noise select also picks the extra period (0x100/0x200/
        // 0x400/right register). That changes the period, and the note's
        // pitch points already carry it, so the select itself is not a note.
        let period = self.noise_period();
        let hit = self.active[3].as_ref().unwrap();
        if hit.period != period {
            if t == hit.sample_time {
                self.active[3].as_mut().unwrap().period = period;
            } else {
                let hit = self.active[3].as_ref().unwrap();
                if hit.pitch_pts.last().map(|p| p.1) != Some(period) {
                    self.active[3]
                        .as_mut()
                        .unwrap()
                        .pitch_pts
                        .push((t, period));
                }
            }
        }
        self.set_levels(3, t);
    }

    fn run(&mut self, writes: &[&ChipWrite]) {
        for w in writes {
            let b = (w.val & 0xFF) as i32;
            let t = w.sample;
            if w.reg == 1 {
                // The Game Gear mask has no T6W28 meaning; both halves are
                // already a stereo pair.
                continue;
            }
            if w.chip_id == 0 {
                if b & 0x80 != 0 {
                    self.latch_l_ch = ((b >> 5) & 3) as usize;
                    self.latch_l_vol = (b & 0x10) != 0;
                    let data = b & 0x0F;
                    if self.latch_l_vol {
                        self.vol_l[self.latch_l_ch] = data;
                        if self.latch_l_ch == 3 {
                            self.evaluate_noise(t);
                        } else {
                            self.evaluate_tone(self.latch_l_ch, t);
                        }
                    } else if self.latch_l_ch == 3 {
                        // The left half's noise register does nothing on a
                        // T6W28.
                    } else {
                        let ch = self.latch_l_ch;
                        self.freq[ch] = (self.freq[ch] & 0x3F0) | data;
                    }
                } else {
                    if self.latch_l_vol || self.latch_l_ch == 3 {
                        continue;
                    }
                    let ch = self.latch_l_ch;
                    self.freq[ch] = (self.freq[ch] & 0x00F) | ((b & 0x3F) << 4);
                    self.freq_high[ch] = true;
                    self.evaluate_tone(ch, t);
                }
                continue;
            }
            // Right half.
            if b & 0x80 != 0 {
                self.latch_r_ch = ((b >> 5) & 3) as usize;
                self.latch_r_vol = (b & 0x10) != 0;
                let data = b & 0x0F;
                if self.latch_r_vol {
                    self.vol_r[self.latch_r_ch] = data;
                    if self.latch_r_ch == 3 {
                        self.evaluate_noise(t);
                    } else {
                        self.evaluate_tone(self.latch_r_ch, t);
                    }
                } else if self.latch_r_ch == 2 {
                    self.noise_extra = (self.noise_extra & 0x3F0) | data;
                    self.evaluate_noise(t);
                } else if self.latch_r_ch == 3 {
                    self.noise_select = data & 3;
                    self.noise_white = (data & 4) != 0;
                    self.evaluate_noise(t);
                }
                // Right-half channel 0/1 tone writes are unused by the
                // hardware.
            } else {
                if self.latch_r_vol {
                    continue;
                }
                if self.latch_r_ch == 2 {
                    self.noise_extra = (self.noise_extra & 0x00F) | ((b & 0x3F) << 4);
                    self.evaluate_noise(t);
                } else if self.latch_r_ch == 3 {
                    self.noise_select = b & 3;
                    self.noise_white = (b & 4) != 0;
                    self.evaluate_noise(t);
                }
            }
        }
        for voice in 0..4 {
            if self.active[voice].is_some() {
                self.close(voice, 0, false);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(sample: i64, chip_id: u8, reg: u32, val: u32) -> ChipWrite {
        ChipWrite::new(sample, "sn76489", chip_id, reg, val)
    }

    fn hits(writes: Vec<ChipWrite>, clock: i64, stereo: bool) -> (Vec<PsgHit>, Vec<PsgOff>) {
        let vgm = VgmFile {
            sn76489_clock: clock as u32,
            sn76489_stereo: stereo,
            writes,
            ..Default::default()
        };
        let (hits, offs, _warn) = collect_sn76489(&vgm, None);
        (hits, offs)
    }

    #[test]
    fn psg_note_is_the_same_period() {
        let tbl = psg_pitch_table();
        for period in [2, 14, 31, 100, 511, 1023] {
            let (note, e5) = psg_note(period, None);
            assert_eq!(tbl.value(note, e5 - 0x80), period, "period {period}");
        }
    }

    #[test]
    fn psg_note_uses_the_source_clock() {
        let clock = 2_858_084;
        let tbl = pitch_table(clock as f64, PSG_DIVIDER as f64, PSG_MAX_PERIOD);
        for period in [2, 14, 31, 100, 254, 511, 1023] {
            let (note, e5) = psg_note(period, Some(clock as f64));
            assert_eq!(tbl.value(note, e5 - 0x80), period, "period {period}");
        }
    }

    #[test]
    fn plain_collector_gates_on_attenuation() {
        let (hits, offs) = hits(
            vec![
                write(0, 0, 0, 0x8F),
                write(0, 0, 0, 0x01),
                write(0, 0, 0, 0x90),
                write(200, 0, 0, 0x9F),
            ],
            3_579_545,
            false,
        );
        assert_eq!(hits.len(), 1);
        let h = &hits[0];
        assert_eq!(h.voice, 0);
        assert_eq!(h.period, 31);
        assert_eq!(h.vol, 15);
        assert_eq!(h.sample_time, 0);
        assert_eq!(h.off_sample, 200);
        assert_eq!(h.clock, 3_579_545);
        assert!(!h.noise);
        assert_eq!(offs, vec![PsgOff { sample_time: 200, voice: 0, chip: 0 }]);
    }

    #[test]
    fn sub_semitone_writes_become_pitch_points_and_large_jumps_split() {
        let (hits, _offs) = hits(
            vec![
                write(0, 0, 0, 0x84),
                write(0, 0, 0, 0x06),
                write(0, 0, 0, 0x90),
                write(150, 0, 0, 0x85),
                write(150, 0, 0, 0x06),
                write(300, 0, 0, 0x83),
                write(300, 0, 0, 0x03),
            ],
            3_579_545,
            false,
        );
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].period, 100);
        assert_eq!(hits[0].pitch_pts, vec![(150, 101)]);
        assert_eq!(hits[0].off_sample, 300);
        assert_eq!(hits[1].period, 51);
        // A plain tone period jump starts a new note without a split link.
        assert!(hits[1].split_from.is_none());
        assert_eq!(hits[1].sample_time, 300);
    }

    #[test]
    fn stereo_mask_changes_record_pan_points() {
        let (hits, _offs) = hits(
            vec![
                write(0, 0, 0, 0x8F),
                write(0, 0, 0, 0x01),
                write(0, 0, 0, 0x90),
                write(200, 0, 1, 0xF0),
                write(400, 0, 0, 0x9F),
            ],
            3_579_545,
            true,
        );
        assert_eq!(hits.len(), 1);
        let h = &hits[0];
        assert_eq!(h.opened_pan, 0xFF);
        assert_eq!(h.pan, 0xF0);
        assert_eq!(h.pan_pts, vec![(200, 0xF0)]);
    }

    #[test]
    fn t6w28_tables_round_trip() {
        let tone = pitch_table(T6W28_CLOCK, T6W28_TONE_DIVIDER, PSG_MAX_PERIOD);
        for period in [8, 14, 31, 100, 511, 1023] {
            let (note, e5) = t6w28_note(period, T6W28_CLOCK as i64);
            assert_eq!(tone.value(note, e5 - 0x80), period, "period {period}");
        }
        // Periods below the table's shortest note clamp like the plain PSG.
        let (note, e5) = t6w28_note(2, T6W28_CLOCK as i64);
        assert_eq!(
            tone.value(note, e5 - 0x80),
            tone.value(179, 127)
        );
        let noise = pitch_table(T6W28_CLOCK, T6W28_NOISE_DIVIDER, PSG_MAX_PERIOD);
        for period in [0x100, 0x200, 0x3FF] {
            let (note, e5) = t6w28_noise_note(period, T6W28_CLOCK as i64);
            assert_eq!(noise.value(note, e5 - 0x80), period, "period {period}");
        }
    }

    #[test]
    fn t6w28_pairs_the_two_attenuation_nibbles() {
        let vgm = VgmFile {
            sn76489_clock: 3_072_000,
            sn76489_t6w28: true,
            writes: vec![
                write(0, 0, 0, 0x8F),
                write(0, 0, 0, 0x01),
                write(0, 0, 0, 0x96),
                write(0, 1, 0, 0x9A),
                write(400, 0, 0, 0x9F),
                write(400, 1, 0, 0x9F),
            ],
            ..Default::default()
        };
        let (hits, offs, warn) = collect_sn76489(&vgm, None);
        assert!(warn.is_empty());
        assert_eq!(hits.len(), 1);
        let h = &hits[0];
        assert!(h.t6w28);
        assert_eq!(h.period, 31);
        assert_eq!(h.vol, 9); // min attenuation 6 -> level 9
        assert_eq!(h.pan, 0xFB); // left loud, right 4 steps down
        assert_eq!(h.opened_pan, 0xFB);
        assert_eq!(h.off_sample, 400);
        assert_eq!(offs.len(), 1);
        assert_eq!(offs[0].sample_time, 400);
    }
}
