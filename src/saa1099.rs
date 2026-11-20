//! Philips SAA1099: six square/noise voices with two shared noise generators.
//!
//! The chip is a stereo PSG: registers `0x00`-`0x05` are the per-voice amplitude
//! (low nibble = left, high nibble = right), `0x08`-`0x0D` the frequency offset,
//! `0x10`-`0x12` the octave pairs, `0x14`/`0x15` the tone/noise enables, `0x16`
//! the two noise generators (one per three-voice group), `0x18`/`0x19` the two
//! envelope generators and `0x1C` the power/sync control. There is no key-on:
//! the gate is the amplitude and the tone/noise enables.
//!
//! Furnace's `DIV_SYSTEM_SAA1099` (`0x97`) is the six-voice, no-software-timer
//! variant. Its pitch table uses `divider=2` and `maximum=0xffff`; the platform
//! turns the table value into an 8-bit offset and 3-bit octave with
//!
//! ```text
//! k = bit_length(freq) - 9          (0 when freq <= 511)
//! offset = 255 - (freq >> k)        (stored as a byte)
//! octave = 7 - k
//! ```
//!
//! so the chip plays `clock * 2**octave / (512 * (511 - offset))`. Inverting
//! the platform's conversion is easier than inverting the chip formula: a
//! source `(offset, octave)` corresponds to every pitch-table value in
//! `[(511-offset) << k, (511-offset+1) << k)` with `k = 7 - octave`. We hand
//! the midpoint to the pitch-table inverse, which finds the note/E5 pair whose
//! register is closest; every one of the 2048 register pairs round-trips.
//!
//! A Furnace export writes the volume and pan macros separately, so the same
//! voice can receive several amplitude writes in one tick. The collector
//! applies all writes at a sample first and then takes one decision, so only
//! the final register state is heard.

use std::collections::{BTreeMap, BTreeSet};

use crate::pitch::furnace_period_note;
use crate::vgm::{ChipWrite, VgmFile};

pub const SAA_VOICES: usize = 6;
pub const SAA_DEFAULT_CLOCK: u32 = 8_000_000;
pub const SAA_DIVIDER: i32 = 2;
pub const SAA_PITCH_MAX: i32 = 0xFFFF;
// Furnace's three clock presets for system 0x97.
pub const SAA_CLOCK_8M: f64 = 8_000_000.0;
pub const SAA_CLOCK_NTSC: f64 = (315_000_000.0 / 88.0) * 2.0; // COLOR_NTSC * 2
pub const SAA_CLOCK_PAL: f64 = 4_433_618.75 * 8.0 / 5.0; // COLOR_PAL * 8 / 5

// A gate shorter than this is driver housekeeping, not music.
pub const SAA_MIN_NOTE: i64 = 100;
// A loud opening level followed by the real level this soon is one note.
pub const SAA_GATE_PAIR: i64 = 1000;
pub const SAA_GATE_DROP: i32 = 6;
// An amplitude jump this large while the gate stays open is a retrigger.
pub const SAA_RETRIGGER_RISE: i32 = 4;

// Furnace SAA1099 effects.
pub const FX_MODE: i32 = 0x10; // 10xy: x = noise, y = tone
pub const FX_NOISE: i32 = 0x11; // 11xx: noise generator frequency/group source
pub const FX_ENV: i32 = 0x12; // 12xx: envelope control register

/// The clock Furnace will actually run the chip at.
///
/// A clock within a couple of hertz of one of the three system presets is
/// replaced by that preset's exact double, matching the chip flag we write.
pub fn saa_module_clock(clock: f64) -> f64 {
    let value = if clock == 0.0 {
        SAA_DEFAULT_CLOCK as f64
    } else {
        clock
    };
    for preset in [SAA_CLOCK_8M, SAA_CLOCK_NTSC, SAA_CLOCK_PAL] {
        if (value - preset).abs() <= 2.0 {
            return preset;
        }
    }
    value
}

/// Pitch-table value the Furnace platform converts to this register pair.
pub fn saa_period_value(freq: i32, octave: i32) -> i32 {
    let k = 7 - (octave & 7);
    let base = 511 - (freq & 0xFF);
    // The midpoint leaves the most room for the note/fine pitch to land back
    // inside the same register bucket.
    (base << k) + if k != 0 { 1 << (k - 1) } else { 0 }
}

/// Tone frequency for an offset/octave pair, in Hz.
pub fn saa_rate(clock: f64, freq: i32, octave: i32) -> f64 {
    let value = saa_module_clock(clock) * 2.0f64.powi(octave & 7);
    value / (512.0 * (511 - (freq & 0xFF)) as f64)
}

/// Platform pitch-table value -> (Furnace note, E5 fine pitch).
pub fn saa_period_note(clock: f64, period: i32) -> (i32, i32) {
    furnace_period_note(
        saa_module_clock(clock),
        SAA_DIVIDER as f64,
        period.max(1),
        0,
        SAA_PITCH_MAX,
    )
}

/// Register pair -> (Furnace note, E5 fine pitch).
pub fn saa_note(clock: f64, freq: i32, octave: i32) -> (i32, i32) {
    saa_period_note(clock, saa_period_value(freq, octave))
}

/// One gate interval of one SAA1099 voice.
#[derive(Clone, Debug, PartialEq)]
pub struct SaaHit {
    pub sample_time: i64,
    pub chip_id: u8,
    pub voice: i32,
    /// Furnace pitch-table value for the opening frequency/octave.
    pub period: i32,
    /// Opening amplitude nibbles, 0-15 each.
    pub left: i32,
    pub right: i32,
    pub tone: bool,
    pub noise: bool,
    pub noise_freq: i32,
    pub env: i32,
    /// True for the silent note that keeps an envelope generator's source
    /// oscillator at the right pitch when that voice has no audible note.
    /// The SAA envelope is clocked by channel 1/4; without this note those
    /// registers stay at reset and the envelope rate is wrong.
    pub hidden: bool,
    pub off_sample: i64,
    /// (sample, pitch-table value) after the opening.
    pub freq_pts: Vec<(i64, i32)>,
    /// (sample, left, right) after the opening.
    pub amp_pts: Vec<(i64, i32, i32)>,
    /// (sample, tone, noise) after the opening.
    pub mode_pts: Vec<(i64, bool, bool)>,
    /// (sample, noise generator value 0-3) after the opening.
    pub noise_pts: Vec<(i64, i32)>,
    /// (sample, envelope control byte) for the group's output voice.
    pub env_pts: Vec<(i64, i32)>,
}

impl Default for SaaHit {
    fn default() -> Self {
        SaaHit {
            sample_time: 0,
            chip_id: 0,
            voice: 0,
            period: 0,
            left: 0,
            right: 0,
            tone: false,
            noise: false,
            noise_freq: 0,
            env: 0,
            hidden: false,
            off_sample: 0,
            freq_pts: Vec::new(),
            amp_pts: Vec::new(),
            mode_pts: Vec::new(),
            noise_pts: Vec::new(),
            env_pts: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct SaaOff {
    pub sample_time: i64,
    pub chip_id: u8,
    pub voice: i32,
}

/// The Python dict `_blank_chip()`; field names follow the register roles.
struct ChipState {
    amp: [i32; SAA_VOICES],
    freq: [i32; SAA_VOICES],
    oct: [i32; SAA_VOICES],
    tone: i32,      // register 0x14, bits 0-5
    noise: i32,     // register 0x15, bits 0-5
    noise_ctl: i32, // register 0x16, two 2-bit fields
    env: [i32; 2],  // registers 0x18/0x19
    power: bool,    // register 0x1C bit 0
    active: [Option<SaaHit>; SAA_VOICES],
}

fn _blank_chip() -> ChipState {
    ChipState {
        amp: [0; SAA_VOICES],
        freq: [0; SAA_VOICES],
        oct: [0; SAA_VOICES],
        tone: 0,
        noise: 0,
        noise_ctl: 0,
        env: [0, 0],
        power: true,
        active: std::array::from_fn(|_| None),
    }
}

fn _tone_on(st: &ChipState, voice: usize) -> bool {
    (st.tone & (1 << voice)) != 0
}

fn _noise_on(st: &ChipState, voice: usize) -> bool {
    (st.noise & (1 << voice)) != 0
}

fn _noise_freq(st: &ChipState, voice: usize) -> i32 {
    (st.noise_ctl >> if voice < 3 { 0 } else { 4 }) & 0x03
}

fn _audible(st: &ChipState, voice: usize) -> bool {
    if !st.power {
        return false;
    }
    if st.amp[voice] == 0 {
        return false;
    }
    _tone_on(st, voice) || _noise_on(st, voice)
}

fn _period(st: &ChipState, voice: usize) -> i32 {
    saa_period_value(st.freq[voice], st.oct[voice])
}

/// True when this voice clocks an enabled internal envelope.
///
/// The envelope control's bit 7 enables it and bit 5 selects the source
/// channel's frequency (clear) instead of the external timer. Only that
/// mode needs the silent source note.
fn _env_source_on(st: &ChipState, voice: usize) -> bool {
    if voice % 3 != 1 {
        return false;
    }
    let env = st.env[voice / 3];
    (env & 0x80) != 0 && (env & 0x20) == 0
}

#[derive(Default)]
struct Acc {
    glitch: i64,
    bad_reg: i64,
    unknown_reg: i64,
}

fn open_hit(st: &mut ChipState, cid: u8, voice: usize, t: i64) {
    st.active[voice] = Some(SaaHit {
        sample_time: t,
        chip_id: cid,
        voice: voice as i32,
        period: _period(st, voice),
        left: st.amp[voice] & 0x0F,
        right: (st.amp[voice] >> 4) & 0x0F,
        tone: _tone_on(st, voice),
        noise: _noise_on(st, voice),
        noise_freq: _noise_freq(st, voice),
        env: st.env[voice / 3],
        hidden: false,
        off_sample: 0,
        freq_pts: Vec::new(),
        amp_pts: Vec::new(),
        mode_pts: Vec::new(),
        noise_pts: Vec::new(),
        env_pts: Vec::new(),
    });
}

/// Silent note that keeps the envelope's source oscillator running.
fn open_hidden(st: &mut ChipState, cid: u8, voice: usize, t: i64) {
    st.active[voice] = Some(SaaHit {
        sample_time: t,
        chip_id: cid,
        voice: voice as i32,
        period: _period(st, voice),
        left: 0,
        right: 0,
        tone: true,
        noise: false,
        noise_freq: 0,
        env: 0,
        hidden: true,
        off_sample: 0,
        freq_pts: Vec::new(),
        amp_pts: Vec::new(),
        mode_pts: Vec::new(),
        noise_pts: Vec::new(),
        env_pts: Vec::new(),
    });
}

fn close_hit(
    st: &mut ChipState,
    cid: u8,
    voice: usize,
    t: i64,
    emit_off: bool,
    hits: &mut Vec<SaaHit>,
    offs: &mut Vec<SaaOff>,
    acc: &mut Acc,
) {
    let Some(mut hit) = st.active[voice].take() else {
        return;
    };
    hit.off_sample = t;
    if t - hit.sample_time >= SAA_MIN_NOTE {
        hits.push(hit);
        if emit_off {
            offs.push(SaaOff {
                sample_time: t,
                chip_id: cid,
                voice: voice as i32,
            });
        }
    } else {
        acc.glitch += 1;
    }
    st.active[voice] = None;
}

fn last_amp(hit: &SaaHit) -> (i32, i32) {
    match hit.amp_pts.last() {
        Some(&(_, left, right)) => (left, right),
        None => (hit.left, hit.right),
    }
}

fn last_period(hit: &SaaHit) -> i32 {
    match hit.freq_pts.last() {
        Some(&(_, period)) => period,
        None => hit.period,
    }
}

fn last_mode(hit: &SaaHit) -> (bool, bool) {
    match hit.mode_pts.last() {
        Some(&(_, tone, noise)) => (tone, noise),
        None => (hit.tone, hit.noise),
    }
}

fn last_noise(hit: &SaaHit) -> i32 {
    match hit.noise_pts.last() {
        Some(&(_, noise)) => noise,
        None => hit.noise_freq,
    }
}

fn last_env(hit: &SaaHit) -> i32 {
    match hit.env_pts.last() {
        Some(&(_, env)) => env,
        None => hit.env,
    }
}

fn apply_write(st: &mut ChipState, r: u32, val: u32, acc: &mut Acc) {
    let val = (val & 0xFF) as i32;
    if r <= 5 {
        st.amp[r as usize] = val;
    } else if (8..=13).contains(&r) {
        st.freq[(r - 8) as usize] = val;
    } else if r == 0x10 {
        st.oct[0] = val & 7;
        st.oct[1] = (val >> 4) & 7;
    } else if r == 0x11 {
        st.oct[2] = val & 7;
        st.oct[3] = (val >> 4) & 7;
    } else if r == 0x12 {
        st.oct[4] = val & 7;
        st.oct[5] = (val >> 4) & 7;
    } else if r == 0x14 {
        st.tone = val & 0x3F;
    } else if r == 0x15 {
        st.noise = val & 0x3F;
    } else if r == 0x16 {
        st.noise_ctl = val & 0xFF;
    } else if r == 0x18 {
        st.env[0] = val;
    } else if r == 0x19 {
        st.env[1] = val;
    } else if r == 0x1C {
        st.power = (val & 0x01) != 0;
    } else if r > 0x1F {
        acc.bad_reg += 1;
    } else if val != 0 {
        acc.unknown_reg += 1;
    }
}

fn evaluate(
    cid: u8,
    st: &mut ChipState,
    t: i64,
    hits: &mut Vec<SaaHit>,
    offs: &mut Vec<SaaOff>,
    acc: &mut Acc,
) {
    for voice in 0..SAA_VOICES {
        let active = st.active[voice].clone();
        let on = _audible(st, voice);
        let env_source = _env_source_on(st, voice);
        if let Some(active) = active {
            if active.hidden {
                if on {
                    close_hit(st, cid, voice, t, false, hits, offs, acc);
                    open_hit(st, cid, voice, t);
                } else if !env_source {
                    close_hit(st, cid, voice, t, true, hits, offs, acc);
                } else {
                    let period = _period(st, voice);
                    if period != last_period(&active) {
                        st.active[voice]
                            .as_mut()
                            .unwrap()
                            .freq_pts
                            .push((t, period));
                    }
                }
                continue;
            }
            if !on {
                close_hit(st, cid, voice, t, true, hits, offs, acc);
                if env_source {
                    open_hidden(st, cid, voice, t);
                }
                continue;
            }

            let left = st.amp[voice] & 0x0F;
            let right = (st.amp[voice] >> 4) & 0x0F;
            let tone = _tone_on(st, voice);
            let noise = _noise_on(st, voice);
            let (prev_left, prev_right) = last_amp(&active);
            let (prev_tone, prev_noise) = last_mode(&active);
            let newmax = left.max(right);
            let oldmax = prev_left.max(prev_right);

            // A loud opening followed immediately by the real level is the
            // driver's gate trick, not a separate note.
            if newmax < oldmax
                && t - active.sample_time <= SAA_GATE_PAIR
                && oldmax - newmax >= SAA_GATE_DROP
                && active.amp_pts.is_empty()
            {
                let live = st.active[voice].as_mut().unwrap();
                live.left = left;
                live.right = right;
                continue;
            }

            // A level jump back up while the gate stays open is a retriggered
            // drum or a new note, not a step in the current decay.
            if newmax - oldmax >= SAA_RETRIGGER_RISE {
                close_hit(st, cid, voice, t, false, hits, offs, acc);
                open_hit(st, cid, voice, t);
                continue;
            }

            // Adding noise to a sounding tone voice is a new percussion hit.
            if noise && !prev_noise && tone {
                close_hit(st, cid, voice, t, false, hits, offs, acc);
                open_hit(st, cid, voice, t);
                continue;
            }

            let period = _period(st, voice);
            let nf = _noise_freq(st, voice);
            let env = if voice % 3 == 2 {
                st.env[voice / 3]
            } else {
                0
            };
            let live = st.active[voice].as_mut().unwrap();
            if period != last_period(&active) {
                live.freq_pts.push((t, period));
            }
            if (left, right) != (prev_left, prev_right) {
                live.amp_pts.push((t, left, right));
            }
            if (tone, noise) != (prev_tone, prev_noise) {
                live.mode_pts.push((t, tone, noise));
            }
            if nf != last_noise(&active) {
                live.noise_pts.push((t, nf));
            }
            if voice % 3 == 2 && env != last_env(&active) {
                live.env_pts.push((t, env));
            }
        } else if on {
            open_hit(st, cid, voice, t);
        } else if env_source {
            open_hidden(st, cid, voice, t);
        }
    }
}

/// Walk the 0xBD register stream into notes.
///
/// All writes at one sample are applied before any gate decision is made;
/// a Furnace export often writes the volume and pan macros as separate
/// register writes on the same tick.
pub fn collect_saa1099(vgm: &VgmFile) -> (Vec<SaaHit>, Vec<SaaOff>, Vec<String>) {
    let mut warnings: Vec<String> = Vec::new();
    let mut hits: Vec<SaaHit> = Vec::new();
    let mut offs: Vec<SaaOff> = Vec::new();
    let chips: BTreeSet<u8> = vgm
        .writes
        .iter()
        .filter(|w: &&ChipWrite| w.chip == "saa1099")
        .map(|w| w.chip_id)
        .collect();
    if chips.is_empty() {
        return (Vec::new(), Vec::new(), warnings);
    }
    let mut state: BTreeMap<u8, ChipState> =
        chips.into_iter().map(|cid| (cid, _blank_chip())).collect();
    let mut acc = Acc::default();

    let saa_writes: Vec<&ChipWrite> = vgm
        .writes
        .iter()
        .filter(|w: &&ChipWrite| w.chip == "saa1099")
        .collect();
    let mut index = 0;
    while index < saa_writes.len() {
        let sample = saa_writes[index].sample;
        let mut touched: BTreeSet<u8> = BTreeSet::new();
        while index < saa_writes.len() && saa_writes[index].sample == sample {
            let w = saa_writes[index];
            let st = state.entry(w.chip_id).or_insert_with(_blank_chip);
            apply_write(st, w.reg, w.val, &mut acc);
            touched.insert(w.chip_id);
            index += 1;
        }
        for cid in touched {
            let st = state.get_mut(&cid).unwrap();
            evaluate(cid, st, sample, &mut hits, &mut offs, &mut acc);
        }
    }

    for st in state.values_mut() {
        for voice in 0..SAA_VOICES {
            if let Some(mut hit) = st.active[voice].take() {
                // Still sounding at the end of the log: no key-off row.
                hit.off_sample = 0;
                hits.push(hit);
                st.active[voice] = None;
            }
        }
    }

    if acc.glitch != 0 {
        warnings.push(format!(
            "{} SAA1099 gate(s) shorter than {} samples were dropped",
            acc.glitch, SAA_MIN_NOTE
        ));
    }
    if acc.unknown_reg != 0 {
        warnings.push(format!(
            "{} nonzero SAA1099 write(s) to unused registers were ignored",
            acc.unknown_reg
        ));
    }
    (hits, offs, warnings)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pitch::{pitch_table, E5_CENTER};

    fn song(writes: Vec<(i64, u32, u32)>) -> VgmFile {
        let mut vgm = VgmFile::default();
        vgm.writes = writes
            .into_iter()
            .map(|(sample, reg, val)| ChipWrite::new(sample, "saa1099", 0, reg, val))
            .collect();
        vgm
    }

    fn hit_at(hits: &[SaaHit], sample: i64) -> &SaaHit {
        hits.iter()
            .find(|h| h.sample_time == sample)
            .expect("hit at sample")
    }

    #[test]
    fn module_clock_snaps_to_the_three_presets() {
        assert_eq!(saa_module_clock(0.0), 8_000_000.0);
        assert_eq!(saa_module_clock(8_000_001.5), SAA_CLOCK_8M);
        assert_eq!(saa_module_clock(SAA_CLOCK_NTSC + 1.0), SAA_CLOCK_NTSC);
        assert_eq!(saa_module_clock(SAA_CLOCK_PAL - 2.0), SAA_CLOCK_PAL);
        assert_eq!(saa_module_clock(7_000_000.0), 7_000_000.0);
    }

    #[test]
    fn period_values_round_trip_through_the_pitch_table() {
        assert_eq!(saa_period_value(0, 7), 511);
        assert_eq!(saa_period_value(0, 0), 65472);
        assert_eq!(saa_period_value(0xFF, 3), 4104);
        assert!((saa_rate(8_000_000.0, 0, 7) - 8_000_000.0 * 128.0 / (512.0 * 511.0)).abs() < 1e-9);

        let tbl = pitch_table(8_000_000.0, 2.0, SAA_PITCH_MAX);
        for freq in 0..256 {
            for octave in 0..8 {
                let period = saa_period_value(freq, octave);
                let (note, e5) = saa_period_note(8_000_000.0, period);
                assert!((0..=crate::pitch::NOTE_MAX).contains(&note));
                let value = tbl.value(note, e5 - E5_CENTER);
                let k = 7 - octave;
                let base = 511 - freq;
                assert!(
                    (base << k) <= value && value < ((base + 1) << k),
                    "freq={freq} oct={octave} period={period} value={value}"
                );
            }
        }
    }

    #[test]
    fn gate_interval_becomes_one_hit_and_one_off() {
        let (hits, offs, warnings) =
            collect_saa1099(&song(vec![(0, 0x00, 0x1F), (0, 0x14, 0x01), (1000, 0x00, 0x00)]));
        assert_eq!(hits.len(), 1);
        assert_eq!(offs.len(), 1);
        let hit = hit_at(&hits, 0);
        assert_eq!((hit.left, hit.right), (15, 1));
        assert!(hit.tone && !hit.noise);
        assert_eq!(hit.off_sample, 1000);
        assert_eq!(hit.period, saa_period_value(0, 0));
        assert_eq!(offs[0].sample_time, 1000);
        assert_eq!(offs[0].voice, 0);
        assert!(warnings.is_empty());

        // The second chip instance is tracked by the write's chip id.
        let mut vgm = VgmFile::default();
        vgm.writes = vec![
            ChipWrite::new(0, "saa1099", 1, 0x00, 0x0F),
            ChipWrite::new(0, "saa1099", 1, 0x14, 0x01),
            ChipWrite::new(1000, "saa1099", 1, 0x00, 0x00),
        ];
        let (hits, offs, _) = collect_saa1099(&vgm);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].chip_id, 1);
        assert_eq!(offs[0].chip_id, 1);
    }

    #[test]
    fn gate_pair_trick_and_retrigger_split_notes() {
        let (hits, offs, _) = collect_saa1099(&song(vec![
            (0, 0x00, 0xFF),
            (0, 0x14, 0x01),
            (100, 0x00, 0x11),
            (2000, 0x00, 0x00),
        ]));
        assert_eq!(hits.len(), 1);
        let hit = hit_at(&hits, 0);
        assert_eq!((hit.left, hit.right), (1, 1));
        assert!(hit.amp_pts.is_empty());
        assert_eq!(hit.off_sample, 2000);
        assert_eq!(offs.len(), 1);

        let (hits, offs, _) = collect_saa1099(&song(vec![
            (0, 0x00, 0x01),
            (0, 0x14, 0x01),
            (1500, 0x00, 0x0F),
            (3000, 0x00, 0x00),
        ]));
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].sample_time, 0);
        assert_eq!(hits[0].off_sample, 1500);
        assert_eq!(hits[1].sample_time, 1500);
        assert_eq!(hits[1].off_sample, 3000);
        assert_eq!(offs.len(), 1);
        assert_eq!(offs[0].sample_time, 3000);

        // Adding noise to a sounding tone voice starts a percussion hit.
        let (hits, offs, _) = collect_saa1099(&song(vec![
            (0, 0x00, 0x0F),
            (0, 0x14, 0x01),
            (500, 0x15, 0x01),
            (2000, 0x00, 0x00),
        ]));
        assert_eq!(hits.len(), 2);
        assert!(!hits[0].noise);
        assert!(hits[1].tone && hits[1].noise);
        assert_eq!(hits[1].sample_time, 500);
        assert_eq!(hits[1].off_sample, 2000);
        assert_eq!(offs.len(), 1);
    }

    #[test]
    fn hidden_envelope_source_and_warnings() {
        let (hits, _, _) = collect_saa1099(&song(vec![
            (0, 0x18, 0x80),
            (0, 0x00, 0x00),
            (100, 0x09, 0x20),
            (100, 0x10, 0x03),
            (900, 0x08, 0x00),
        ]));
        assert_eq!(hits.len(), 1);
        let hit = hit_at(&hits, 0);
        assert!(hit.hidden);
        assert!(hit.tone && !hit.noise);
        assert_eq!(hit.left, 0);
        assert_eq!(hit.freq_pts, vec![(100, saa_period_value(0x20, 0))]);

        let (_, _, warnings) = collect_saa1099(&song(vec![
            (0, 0x00, 0x0F),
            (0, 0x14, 0x01),
            (50, 0x00, 0x00),
            (60, 0x1D, 0x01),
            (70, 0x20, 0x01),
        ]));
        assert_eq!(
            warnings,
            vec![
                format!("1 SAA1099 gate(s) shorter than {SAA_MIN_NOTE} samples were dropped"),
                "1 nonzero SAA1099 write(s) to unused registers were ignored".to_string(),
            ]
        );
    }
}
