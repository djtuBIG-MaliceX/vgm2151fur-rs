//! AY-3-8910 (and the YM2149-compatible register set) -> notes -> Furnace.
//!
//! Register model follows MAME/libvgm and matches Furnace's AY core:
//!
//! ```text
//! 0/1, 2/3, 4/5  tone period, 12-bit, low byte then high nibble
//! 6              noise period (5-bit)
//! 7              mixer: bits 0-2 tone enables, 3-5 noise enables (a clear
//!                bit enables), 6/7 I/O port directions
//! 8/9/10         per-channel volume: low nibble = level, bit 4 = hardware
//!                envelope instead of the level
//! 11/12          envelope period, 16-bit
//! 13             envelope shape
//! 14/15          I/O port data (not converted)
//! ```
//!
//! A tone channel sounds at `clock / (16 * period)` (period 0 reads as 1).
//! There is no key-on register: the gate is the volume (0 = off) or the mixer
//! (tone and noise both disabled = off).  MSX and Spectrum drivers use the
//! volume as the gate for percussion and retrigger the hardware envelope for
//! drum decays, so a new note starts when the voice becomes audible or the
//! envelope is re-armed.  A period change of a semitone or more that holds
//! for a frame is its own note; a smaller change stays on the note as vibrato
//! or a slide.  A period below the pitch table's range mutes the voice.
//!
//! Furnace's AY platform writes the pitch table's value straight into the
//! period registers, so a source period round-trips through
//! `pitch::furnace_period_note`.  Channel mode (tone/noise/envelope), noise
//! frequency and the hardware envelope are pattern effects
//! (20xx/21xx/22xy/23xx/24xx), which is how the platform exposes them.

use crate::pitch::{furnace_period_note, module_clock, pitch_table, NOTE_MAX};
use crate::vgm::VgmFile;

pub const AY_VOICES: usize = 3;
pub const AY_DEFAULT_CLOCK: u32 = 1_789_772; // COLOR_NTSC/2, the MSX/ZX Spectrum PSG clock
// Gates shorter than this are driver housekeeping, not music.
pub const AY_MIN_NOTE: i64 = 100;
// A loud gate level followed by the real level this soon is one note.
pub const AY_GATE_PAIR: i64 = 1000;
// ...but only when the drop is large enough to be a gate-opening trick.  A
// percussion attack decays by a step or two per frame (three or four on a
// roll), so absorbing the first frame would throw away the peak and make the
// transient too quiet.
pub const AY_GATE_DROP: i32 = 6;
// A shape write this soon after the gate belongs to the note's opening
// register burst. Anything later re-arms the envelope and is its own hit.
pub const AY_ENV_OPEN: i64 = 64;
// A level jump this large while the gate stays open is a retriggered drum
// (a snare roll writes the next hit straight from the decayed level back up).
// Without it the whole roll stayed one note with a volume macro.
pub const AY_RETRIGGER_RISE: i32 = 4;

pub const REG_TONE_LO: [usize; AY_VOICES] = [0, 2, 4];
pub const REG_TONE_HI: [usize; AY_VOICES] = [1, 3, 5];
pub const REG_VOL: [usize; AY_VOICES] = [8, 9, 10];
pub const REG_NOISE: usize = 6;
pub const REG_MIXER: usize = 7;
pub const REG_ENV_LO: usize = 11;
pub const REG_ENV_HI: usize = 12;
pub const REG_ENV_SHAPE: usize = 13;

// Furnace effect numbers (platform/ay.cpp, sysDef ayPostEffectHandlerMap).
pub const FX_MODE: i32 = 0x20; // channel mode: bits tone 1, noise 2, envelope 4 (value - 1)
pub const FX_NOISE: i32 = 0x21; // noise frequency, value = 31 - register
pub const FX_ENV_SET: i32 = 0x22; // xy = shape, channel enable
pub const FX_ENV_LO: i32 = 0x23;
pub const FX_ENV_HI: i32 = 0x24;

/// Tone frequency for a period register, in Hz (period 0 reads as 1).
pub fn ay_rate(clock: u32, period: i32) -> f64 {
    let clock = if clock == 0 { AY_DEFAULT_CLOCK } else { clock };
    f64::from(clock) / (16.0 * f64::from(i32::max(1, period)))
}

/// Period register -> (Furnace note, E5 fine tune).
///
/// `divider` is the Furnace pitch-table divider: 8 for a stand-alone AY-8910,
/// 16 for the YM2203's SSG (Furnace runs that core with extDiv 16).
pub fn ay_note(clock: u32, period: i32, divider: f64) -> (i32, i32) {
    furnace_period_note(
        module_clock(f64::from(clock)),
        divider,
        i32::max(1, period),
        0,
        0xFFF,
    )
}

/// Furnace effect value for a channel mode: bits tone 1, noise 2, env 4.
pub fn ay_mode(tone: bool, noise: bool, envelope: bool) -> i32 {
    (if tone { 1 } else { 0 }) | (if noise { 2 } else { 0 }) | (if envelope { 4 } else { 0 })
}

#[derive(Clone, Debug, PartialEq)]
pub struct AyHit {
    pub sample_time: i64,
    pub chip_id: i32,
    pub voice: i32,
    pub period: i32,       // 12-bit tone period at the gate
    pub vol: i32,          // volume register low nibble
    pub env: bool,         // volume register bit 4: the envelope drives this channel
    pub tone: bool,        // mixer: tone enabled (follows later mixer writes)
    pub opened_tone: bool, // tone bit at the gate; later mixer writes leave this
    pub noise: bool,       // mixer: noise enabled
    pub noise_period: i32, // register 6 (5-bit)
    pub env_shape: i32,    // register 13
    pub env_period: i32,   // registers 11/12, 16-bit
    pub off_sample: i64,   // gate-off sample (0 = still sounding at the end)
    // When a retrigger closes this note silently and then the new gate turns
    // out to be a sub-frame glitch, this holds the last note that really
    // sounded. Its key-off has to be emitted at the glitch's close, or the
    // predecessor rings until the voice's next note.
    pub split_from: Option<Box<AyHit>>,
    // Mid-note register writes while the voice sounds.
    pub freq_pts: Vec<(i64, i32)>,
    pub vol_pts: Vec<(i64, i32)>,
    // Furnace effects the note needs: (sample, effect, value).  The note's own
    // mode/noise/envelope writes are in here at the note's own sample.
    pub fx_pts: Vec<(i64, i32, i32)>,
}

impl AyHit {
    /// Python's `AyHit(...)` with every defaulted field left at its default.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        sample_time: i64,
        chip_id: i32,
        voice: i32,
        period: i32,
        vol: i32,
        env: bool,
        tone: bool,
        opened_tone: bool,
        noise: bool,
        noise_period: i32,
        env_shape: i32,
        env_period: i32,
    ) -> Self {
        AyHit {
            sample_time,
            chip_id,
            voice,
            period,
            vol,
            env,
            tone,
            opened_tone,
            noise,
            noise_period,
            env_shape,
            env_period,
            off_sample: 0,
            split_from: None,
            freq_pts: Vec::new(),
            vol_pts: Vec::new(),
            fx_pts: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct AyOff {
    pub sample_time: i64,
    pub chip_id: i32,
    pub voice: i32,
}

// The mixer's power-up value is undefined on a real AY. A zeroed image
// enables noise on all three voices, which turns a song's opening notes into
// tone-plus-hiss wherever the driver writes pitch and volume before its
// first $07. Tone-only (this value) matches the first mixer byte those
// drivers write ($BA / $B8, noise disabled on every voice). A driver that
// wants noise says so there. Every other register stays at reset.
pub const REG_MIXER_DEFAULT: i32 = 0x38;

struct AyChipState {
    reg: [i32; 16],
    active: [Option<AyHit>; AY_VOICES],
}

fn blank_chip() -> AyChipState {
    let mut reg = [0i32; 16];
    reg[REG_MIXER] = REG_MIXER_DEFAULT;
    AyChipState {
        reg,
        active: [None, None, None],
    }
}

fn tone_on(reg: &[i32; 16], voice: usize) -> bool {
    (reg[REG_MIXER] & (1 << voice)) == 0
}

fn noise_on(reg: &[i32; 16], voice: usize) -> bool {
    (reg[REG_MIXER] & (1 << (voice + 3))) == 0
}

fn current_period(reg: &[i32; 16], voice: usize) -> i32 {
    reg[REG_TONE_LO[voice]] | ((reg[REG_TONE_HI[voice]] & 0x0F) << 8)
}

fn env_period(reg: &[i32; 16]) -> i32 {
    reg[REG_ENV_LO] | (reg[REG_ENV_HI] << 8)
}

/// Smallest period the module can express for this clock.
///
/// The pitch table tops out at B-9, and drivers mute a channel by writing a
/// period below that (an inaudible tone): the SCC-style "silence gate".
/// Treating it as silence keeps those housekeeping writes from becoming
/// notes with a pitch nothing can represent.
pub fn ay_min_period(clock: u32, divider: f64) -> i32 {
    let clock = if clock == 0 { AY_DEFAULT_CLOCK } else { clock };
    let tbl = pitch_table(module_clock(f64::from(clock)), divider, 0xFFF);
    i32::max(2, tbl.value(NOTE_MAX, 127))
}

/// True when the voice can be heard with the current registers.
fn audible(reg: &[i32; 16], voice: usize, min_period: i32) -> bool {
    let mut tone = tone_on(reg, voice);
    if tone {
        let period = current_period(reg, voice);
        tone = period >= i32::max(1, min_period);
    }
    if !(tone || noise_on(reg, voice)) {
        // The AY mixes tone and noise into the DAC; with both disabled the
        // channel outputs a constant at the volume level. A *fixed* level is
        // DC and inaudible, but the hardware envelope makes that level an
        // audio-rate waveform (the `$0D` decay heard under a muted voice).
        // Keep the note alive while the envelope drives it.
        return (reg[REG_VOL[voice]] & 0x10) != 0;
    }
    if (reg[REG_VOL[voice]] & 0x10) != 0 {
        return true; // the hardware envelope drives this channel
    }
    (reg[REG_VOL[voice]] & 0x0F) > 0
}

/// Effects a note needs: mode, noise frequency, hardware envelope.
fn note_fx(reg: &[i32; 16], voice: usize) -> Vec<(i32, i32)> {
    let env = (reg[REG_VOL[voice]] & 0x10) != 0;
    let mut fx = vec![(
        FX_MODE,
        ay_mode(tone_on(reg, voice), noise_on(reg, voice), env) - 1,
    )];
    if noise_on(reg, voice) {
        fx.push((FX_NOISE, 31 - (reg[REG_NOISE] & 0x1F)));
    }
    if env {
        fx.push((
            FX_ENV_SET,
            ((reg[REG_ENV_SHAPE] & 0x0F) << 4) | 1,
        ));
        fx.push((FX_ENV_LO, env_period(reg) & 0xFF));
        fx.push((FX_ENV_HI, (env_period(reg) >> 8) & 0xFF));
    }
    fx
}

/// Remove a note-start envelope setup that a volume write turned off.
///
/// The note opens while the previous envelope bit is still set, so `note_fx`
/// queues the shape/period. If the real volume write a few samples later
/// clears bit 4, keeping that setup would turn the envelope back on after the
/// mode-off write and make the note follow the generator instead of its
/// fixed level.
fn drop_env_fx(hit: &mut AyHit) {
    hit.fx_pts
        .retain(|entry| !matches!(entry.1, FX_ENV_SET | FX_ENV_LO | FX_ENV_HI));
}

/// Turn the note's opening state into fixed volume.
///
/// Used when the envelope bit clears inside the opening write burst. The
/// pattern row can only carry the settled state, so drop the envelope setup
/// and replace the opening mode value rather than appending a second mode
/// write: the last value on the row wins.
fn settle_env_off(hit: &mut AyHit, sample: i64) {
    hit.env = false;
    drop_env_fx(hit);
    let off_mode = ay_mode(hit.tone, hit.noise, false) - 1;
    for i in 0..hit.fx_pts.len() {
        let (t, fx, _val) = hit.fx_pts[i];
        if fx == FX_MODE && t <= hit.sample_time {
            hit.fx_pts[i] = (t, fx, off_mode);
            return;
        }
    }
    hit.fx_pts.push((sample, FX_MODE, off_mode));
}

struct AyCollector {
    min_period: i32,
    hits: Vec<AyHit>,
    offs: Vec<AyOff>,
    // Python's Counter also tallies "glitch"; that value is never read.
    io_ports: i32,
}

impl AyCollector {
    fn open_hit(
        &mut self,
        st: &mut AyChipState,
        cid: u8,
        voice: usize,
        t: i64,
        split_from: Option<AyHit>,
    ) {
        let reg = &st.reg;
        let env = (reg[REG_VOL[voice]] & 0x10) != 0;
        let tone = tone_on(reg, voice);
        let mut hit = AyHit::new(
            t,
            i32::from(cid),
            voice as i32,
            current_period(reg, voice),
            reg[REG_VOL[voice]] & 0x0F,
            env,
            tone,
            tone,
            noise_on(reg, voice),
            reg[REG_NOISE] & 0x1F,
            reg[REG_ENV_SHAPE] & 0x0F,
            env_period(reg),
        );
        hit.fx_pts = note_fx(reg, voice)
            .into_iter()
            .map(|(fx, val)| (t, fx, val))
            .collect();
        hit.split_from = split_from.map(Box::new);
        st.active[voice] = Some(hit);
    }

    /// Python `close_hit`, but over an owned hit. Returns the hit when
    /// `keep_root` asks for it (a retrigger keeps it as the next note's
    /// `split_from`); the hit is also pushed to `hits` when it is long
    /// enough, so a root that was recorded keeps the same final state.
    fn close_hit_value(
        &mut self,
        mut hit: AyHit,
        cid: u8,
        voice: usize,
        t: i64,
        emit_off: bool,
        keep_root: bool,
    ) -> Option<AyHit> {
        hit.off_sample = t;
        if t - hit.sample_time >= AY_MIN_NOTE {
            if emit_off {
                self.offs.push(AyOff {
                    sample_time: t,
                    chip_id: i32::from(cid),
                    voice: voice as i32,
                });
            }
            if keep_root {
                self.hits.push(hit.clone());
                return Some(hit);
            }
            self.hits.push(hit);
            return None;
        }
        if hit.split_from.is_some() && emit_off {
            // The note before this one was closed without a key-off by the
            // same retrigger. This gate never sounded, so the predecessor is
            // still ringing and needs the cut it never got.
            self.offs.push(AyOff {
                sample_time: t,
                chip_id: i32::from(cid),
                voice: voice as i32,
            });
        }
        if keep_root {
            Some(hit)
        } else {
            None
        }
    }

    fn close_hit(&mut self, st: &mut AyChipState, cid: u8, voice: usize, t: i64, emit_off: bool) {
        let Some(hit) = st.active[voice].take() else {
            return;
        };
        self.close_hit_value(hit, cid, voice, t, emit_off, false);
    }

    fn update_gate(
        &mut self,
        st: &mut AyChipState,
        cid: u8,
        voice: usize,
        t: i64,
        retrigger: bool,
    ) {
        let on = audible(&st.reg, voice, self.min_period);
        if on && (st.active[voice].is_none() || retrigger) {
            let mut root = None;
            if let Some(hit) = st.active[voice].take() {
                let mut hit = self
                    .close_hit_value(hit, cid, voice, t, false, true)
                    .expect("keep_root always returns the hit");
                root = Some(match hit.split_from.take() {
                    Some(boxed) => *boxed,
                    None => hit,
                });
            }
            self.open_hit(st, cid, voice, t, root);
        } else if !on {
            self.close_hit(st, cid, voice, t, true);
        }
    }
}

/// Walk an AY-compatible register stream into notes (Python `collect_ay8910`
/// with its `chip="ay8910"`, `clock=None` and `divider=8` defaults).
pub fn collect_ay8910(vgm: &VgmFile) -> (Vec<AyHit>, Vec<AyOff>, Vec<String>) {
    collect_ay8910_with(vgm, "ay8910", None, 8.0)
}

/// Walk an AY-compatible register stream into notes.
///
/// `chip` selects the write stream (the YM2203's SSG uses `ym2203_ssg`), and
/// `divider` is the Furnace pitch-table divider (8 for an AY-8910, 16 for the
/// YM2203 SSG).  Returns (hits, key-offs, warnings); each hit is one gate
/// interval of one voice with everything the writer needs.
pub fn collect_ay8910_with(
    vgm: &VgmFile,
    chip: &str,
    clock: Option<u32>,
    divider: f64,
) -> (Vec<AyHit>, Vec<AyOff>, Vec<String>) {
    let mut warnings: Vec<String> = Vec::new();
    let mut chips: Vec<u8> = vgm
        .writes
        .iter()
        .filter(|w| w.chip == chip)
        .map(|w| w.chip_id)
        .collect();
    chips.sort_unstable();
    chips.dedup();
    if chips.is_empty() {
        return (Vec::new(), Vec::new(), warnings);
    }
    let mut state: Vec<(u8, AyChipState)> = chips.iter().map(|&cid| (cid, blank_chip())).collect();
    let clock = match clock {
        Some(c) if c != 0 => c,
        _ => {
            if vgm.ay8910_clock != 0 {
                vgm.ay8910_clock
            } else {
                AY_DEFAULT_CLOCK
            }
        }
    };
    let min_period = ay_min_period(clock, divider);
    let mut collector = AyCollector {
        min_period,
        hits: Vec::new(),
        offs: Vec::new(),
        io_ports: 0,
    };

    for w in &vgm.writes {
        if w.chip != chip {
            continue;
        }
        let cid = w.chip_id;
        let st_pos = state.iter().position(|(c, _)| *c == cid);
        let st = match st_pos {
            Some(pos) => &mut state[pos].1,
            None => {
                state.push((cid, blank_chip()));
                &mut state.last_mut().expect("just pushed").1
            }
        };
        let reg = &mut st.reg;
        if w.reg > 0x0F {
            continue;
        }
        let r = w.reg as usize;
        let val = (w.val & 0xFF) as i32;
        // A same-value $0D write still re-arms the envelope on real hardware
        // (set_shape resets the counter), so it must not be dropped. It is
        // handled below as either the opening burst or a new hit.
        if reg[r] == val && r != REG_ENV_SHAPE {
            continue;
        }
        reg[r] = val;
        let mut voice: Option<usize> = None;
        for i in 0..AY_VOICES {
            if r == REG_TONE_LO[i] || r == REG_TONE_HI[i] {
                voice = Some(i);
                break;
            }
        }
        if let Some(voice) = voice {
            let new = current_period(&st.reg, voice);
            let audible_now = audible(&st.reg, voice, min_period);
            let tone_now = tone_on(&st.reg, voice);
            if let Some(hit) = st.active[voice].as_mut() {
                if audible_now && tone_now {
                    // Compare with what the voice was last seen playing, so a
                    // step back to the note's own pitch is recorded too.
                    // Period writes while the tone is disabled are the noise
                    // channel's leftover register, not a pitch.
                    let cur = hit.freq_pts.last().map(|p| p.1).unwrap_or(hit.period);
                    if new != cur {
                        hit.freq_pts.push((w.sample, new));
                    }
                }
            }
            // A period below the music range is how drivers mute a channel
            // while leaving the volume up. That is a gate-off, not a pitch.
            collector.update_gate(st, cid, voice, w.sample, false);
            continue;
        }
        if let Some(voice) = REG_VOL.iter().position(|&x| x == r) {
            let audible_now = audible(&st.reg, voice, min_period);
            if st.active[voice].is_none() {
                if audible_now {
                    collector.open_hit(st, cid, voice, w.sample, None);
                }
                continue;
            }
            if !audible_now {
                collector.close_hit(st, cid, voice, w.sample, true);
                continue;
            }
            if val & 0x10 != 0 {
                // The chip restarts the envelope on the enable write; that is
                // how MSX drivers trigger drum decays.
                collector.update_gate(st, cid, voice, w.sample, true);
                continue;
            }
            let level = val & 0x0F;
            let mut hit = st.active[voice].take().expect("checked above");
            if !hit.env {
                let prev = hit.vol_pts.last().map(|p| p.1).unwrap_or(hit.vol);
                if level - prev >= AY_RETRIGGER_RISE {
                    // The driver keeps the gate open and jumps the level back
                    // up for the next hit. Close this one silently and let the
                    // new level open a fresh note, so the roll reaches the
                    // pattern instead of living only in the volume macro.
                    let closed = collector
                        .close_hit_value(hit, cid, voice, w.sample, false, true)
                        .expect("keep_root always returns the hit");
                    let mut closed = closed;
                    let root = match closed.split_from.take() {
                        Some(boxed) => *boxed,
                        None => closed,
                    };
                    collector.open_hit(st, cid, voice, w.sample, Some(root));
                    continue;
                }
            }
            if w.sample - hit.sample_time <= AY_GATE_PAIR
                && hit.vol - level >= AY_GATE_DROP
            {
                // Loud gate then the real quiet level: keep the quiet level
                // as the note's opening, not a separate volume step.  A small
                // drop is the first frame of a normal decay and must keep the
                // loud attack.
                hit.vol = level;
                let window = hit.sample_time + AY_GATE_PAIR;
                hit.vol_pts.retain(|p| p.0 > window);
                if hit.env {
                    settle_env_off(&mut hit, w.sample);
                }
                st.active[voice] = Some(hit);
                continue;
            }
            if hit.env {
                // The level write also clears the envelope bit. Inside the
                // opening burst, the settled fixed level is the note's real
                // start. After that the envelope has run for several rows, so
                // keep its note-start setup and append the mode-off write.
                if w.sample - hit.sample_time <= AY_GATE_PAIR {
                    hit.vol = level;
                    let window = hit.sample_time + AY_GATE_PAIR;
                    hit.vol_pts.retain(|p| p.0 > window);
                    settle_env_off(&mut hit, w.sample);
                    st.active[voice] = Some(hit);
                    continue;
                }
                hit.env = false;
                let off_mode = ay_mode(hit.tone, hit.noise, false) - 1;
                hit.fx_pts.push((w.sample, FX_MODE, off_mode));
            }
            hit.vol_pts.push((w.sample, level));
            st.active[voice] = Some(hit);
            continue;
        }
        if r == REG_MIXER {
            for voice in 0..AY_VOICES {
                let tone = tone_on(&st.reg, voice);
                let noise = noise_on(&st.reg, voice);
                let (old_tone, old_noise) = match st.active[voice].as_ref() {
                    Some(hit) => (hit.tone, hit.noise),
                    None => continue,
                };
                if (tone, noise) == (old_tone, old_noise) {
                    continue;
                }
                if noise && !old_noise && tone {
                    // The mixer adding noise to a voice that is still
                    // sounding (both tone bits on) is a new percussion hit.
                    // Drivers hold the volume open through a drum's noise
                    // attack and its tonal body, so without this the attack is
                    // glued to the end of the previous body's instrument macro
                    // and never gets its own row. A tone-to-noise handover
                    // (tone off as noise comes on) stays one note: that is a
                    // mode change on the same sound, not a new attack. The
                    // period and noise-period writes just before this mixer
                    // write belong to the new hit.
                    collector.update_gate(st, cid, voice, w.sample, true);
                    continue;
                }
                let env = (st.reg[REG_VOL[voice]] & 0x10) != 0;
                let noise_reg = st.reg[REG_NOISE] & 0x1F;
                let hit = st.active[voice].as_mut().expect("checked above");
                hit.fx_pts.push((
                    w.sample,
                    FX_MODE,
                    ay_mode(tone, noise, env) - 1,
                ));
                if noise && !old_noise {
                    // A tone-to-noise handover stays on the same note, but
                    // the driver may have set the noise period while noise
                    // was off. Carry it over so the first tick does not use
                    // the previous hit's period.
                    hit.fx_pts.push((w.sample, FX_NOISE, 31 - noise_reg));
                }
                hit.tone = tone;
                hit.noise = noise;
            }
            for voice in 0..AY_VOICES {
                collector.update_gate(st, cid, voice, w.sample, false);
            }
            continue;
        }
        if r == REG_NOISE {
            for voice in 0..AY_VOICES {
                if let Some(hit) = st.active[voice].as_mut() {
                    if hit.noise {
                        hit.fx_pts.push((w.sample, FX_NOISE, 31 - (val & 0x1F)));
                    }
                }
            }
            continue;
        }
        if r == REG_ENV_SHAPE {
            for voice in 0..AY_VOICES {
                let sample_time = match st.active[voice].as_ref() {
                    Some(hit) if hit.env => hit.sample_time,
                    _ => continue,
                };
                if w.sample - sample_time > AY_ENV_OPEN {
                    // The chip re-arms the envelope on every $0D write, even
                    // one that repeats the current shape. A tracker can only
                    // restart it with a note (the instrument's phase-reset
                    // macro forces the write), so this is a new hit at the
                    // source's own rate instead of one long held note.
                    let hit = st.active[voice].take().expect("checked above");
                    let mut closed = collector
                        .close_hit_value(hit, cid, voice, w.sample, false, true)
                        .expect("keep_root always returns the hit");
                    let root = match closed.split_from.take() {
                        Some(boxed) => *boxed,
                        None => closed,
                    };
                    collector.open_hit(st, cid, voice, w.sample, Some(root));
                } else {
                    let hit = st.active[voice].as_mut().expect("checked above");
                    hit.fx_pts
                        .push((w.sample, FX_ENV_SET, ((val & 0x0F) << 4) | 1));
                }
            }
            continue;
        }
        if r == REG_ENV_LO || r == REG_ENV_HI {
            let period = env_period(&st.reg);
            for voice in 0..AY_VOICES {
                if let Some(hit) = st.active[voice].as_mut() {
                    if hit.env {
                        hit.fx_pts.push((w.sample, FX_ENV_LO, period & 0xFF));
                        hit.fx_pts.push((w.sample, FX_ENV_HI, (period >> 8) & 0xFF));
                    }
                }
            }
            continue;
        }
        if r == 14 || r == 15 {
            collector.io_ports += 1;
        }
    }

    for (_, st) in state.iter_mut() {
        for voice in 0..AY_VOICES {
            if let Some(mut hit) = st.active[voice].take() {
                // Still sounding at the end of the log: no key-off row.
                hit.off_sample = 0;
                collector.hits.push(hit);
            }
        }
    }

    if collector.io_ports != 0 {
        let label = if chip == "ym2203_ssg" {
            "YM2203 SSG"
        } else {
            "AY-8910"
        };
        warnings.push(format!(
            "{} {} I/O port writes are not converted",
            collector.io_ports, label
        ));
    }
    (collector.hits, collector.offs, warnings)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vgm::ChipWrite;

    fn song(writes: Vec<ChipWrite>, clock: u32) -> VgmFile {
        VgmFile {
            writes,
            ay8910_clock: clock,
            ..VgmFile::default()
        }
    }

    fn gate_writes() -> Vec<ChipWrite> {
        vec![
            ChipWrite::new(0, "ay8910", 0, 0, 254),
            ChipWrite::new(100, "ay8910", 0, 8, 15),
            ChipWrite::new(5000, "ay8910", 0, 8, 0),
        ]
    }

    #[test]
    fn rate_note_and_min_period_follow_the_pitch_table() {
        assert_eq!(
            ay_rate(1_789_773, 254),
            1_789_773.0 / (16.0 * 254.0)
        );
        // A zero clock falls back to the AY default.
        assert_eq!(ay_rate(0, 254), ay_rate(AY_DEFAULT_CLOCK, 254));
        assert_eq!(ay_note(1_789_773, 254, 8.0), (117, 123));
        assert_eq!(ay_min_period(1_789_772, 8.0), 7);
        assert_eq!(ay_min_period(3_579_545, 16.0), 7);

        assert_eq!(ay_mode(false, false, false), 0);
        assert_eq!(ay_mode(true, false, false), 1);
        assert_eq!(ay_mode(true, true, false), 3);
        assert_eq!(ay_mode(false, false, true), 4);
        assert_eq!(ay_mode(true, true, true), 7);
    }

    #[test]
    fn volume_gate_opens_and_closes_one_note() {
        let (hits, offs, warnings) = collect_ay8910(&song(gate_writes(), 0));
        assert!(warnings.is_empty());
        assert_eq!(hits.len(), 1);
        let hit = &hits[0];
        assert_eq!(
            (hit.sample_time, hit.chip_id, hit.voice, hit.period, hit.vol),
            (100, 0, 0, 254, 15)
        );
        assert_eq!(
            (hit.env, hit.tone, hit.opened_tone, hit.noise),
            (false, true, true, false)
        );
        assert_eq!(hit.off_sample, 5000);
        assert_eq!(offs.len(), 1);
        assert_eq!(
            (offs[0].sample_time, offs[0].chip_id, offs[0].voice),
            (5000, 0, 0)
        );
    }

    #[test]
    fn loud_gate_then_quiet_level_keeps_the_quiet_opening() {
        let writes = vec![
            ChipWrite::new(0, "ay8910", 0, 0, 254),
            ChipWrite::new(100, "ay8910", 0, 8, 15),
            ChipWrite::new(300, "ay8910", 0, 8, 3),
            ChipWrite::new(5000, "ay8910", 0, 8, 0),
        ];
        let (hits, offs, _) = collect_ay8910(&song(writes, 0));
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].vol, 3);
        // The opening level write is not a separate volume step.
        assert!(hits[0].vol_pts.is_empty());
        assert_eq!(offs.len(), 1);
    }

    #[test]
    fn hardware_envelope_opens_with_its_furnace_effects() {
        let writes = vec![
            ChipWrite::new(0, "ay8910", 0, 0, 254),
            ChipWrite::new(100, "ay8910", 0, 8, 0x10),
        ];
        let (hits, offs, _) = collect_ay8910(&song(writes, 0));
        assert_eq!(hits.len(), 1);
        let hit = &hits[0];
        assert!(hit.env);
        assert_eq!(hit.vol, 0);
        // Still sounding at the end of the log: no key-off row.
        assert_eq!(hit.off_sample, 0);
        assert!(offs.is_empty());
        assert_eq!(
            hit.fx_pts,
            vec![
                (100, FX_MODE, 4),
                (100, FX_ENV_SET, 1),
                (100, FX_ENV_LO, 0),
                (100, FX_ENV_HI, 0),
            ]
        );
    }

    #[test]
    fn env_shape_rewrite_after_the_burst_retriggers_with_a_split() {
        let writes = vec![
            ChipWrite::new(0, "ay8910", 0, 0, 254),
            ChipWrite::new(100, "ay8910", 0, 8, 0x10),
            ChipWrite::new(200, "ay8910", 0, 13, 8),
            ChipWrite::new(5000, "ay8910", 0, 8, 0),
        ];
        let (hits, offs, _) = collect_ay8910(&song(writes, 0));
        assert_eq!(hits.len(), 2);
        assert_eq!((hits[0].sample_time, hits[0].off_sample), (100, 200));
        assert!(hits[0].split_from.is_none());
        assert_eq!((hits[1].sample_time, hits[1].off_sample), (200, 5000));
        assert_eq!(hits[1].env_shape, 8);
        assert!(hits[1].split_from.is_some());
        assert_eq!(hits[1].fx_pts[1], (200, FX_ENV_SET, 0x81));
        assert_eq!(
            offs.iter().map(|o| o.sample_time).collect::<Vec<_>>(),
            vec![5000]
        );
    }

    #[test]
    fn level_jump_while_gate_open_splits_the_note() {
        let writes = vec![
            ChipWrite::new(0, "ay8910", 0, 0, 254),
            ChipWrite::new(100, "ay8910", 0, 8, 15),
            ChipWrite::new(150, "ay8910", 0, 8, 4),
            ChipWrite::new(300, "ay8910", 0, 8, 9),
            ChipWrite::new(5000, "ay8910", 0, 8, 0),
        ];
        let (hits, offs, _) = collect_ay8910(&song(writes, 0));
        assert_eq!(hits.len(), 2);
        assert_eq!((hits[0].sample_time, hits[0].off_sample), (100, 300));
        assert_eq!(hits[0].vol, 4);
        assert!(hits[0].vol_pts.is_empty());
        assert_eq!((hits[1].sample_time, hits[1].off_sample), (300, 5000));
        assert_eq!(hits[1].vol, 9);
        assert!(hits[1].split_from.is_some());
        assert_eq!(
            offs.iter().map(|o| o.sample_time).collect::<Vec<_>>(),
            vec![5000]
        );
    }

    #[test]
    fn io_port_writes_warn_with_the_stream_label() {
        let mut writes = vec![ChipWrite::new(0, "ym2203_ssg", 2, 14, 0x55)];
        let (hits, offs, warnings) = collect_ay8910_with(
            &song(writes.clone(), 3_579_545),
            "ym2203_ssg",
            Some(3_579_545),
            16.0,
        );
        assert!(hits.is_empty() && offs.is_empty());
        assert_eq!(
            warnings,
            vec!["1 YM2203 SSG I/O port writes are not converted".to_string()]
        );

        writes = vec![
            ChipWrite::new(0, "ay8910", 0, 15, 0x01),
            ChipWrite::new(10, "ay8910", 0, 14, 0x02),
        ];
        let (_, _, warnings) = collect_ay8910(&song(writes, 0));
        assert_eq!(
            warnings,
            vec!["2 AY-8910 I/O port writes are not converted".to_string()]
        );
    }
}
