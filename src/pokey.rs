//! Atari POKEY (4-channel PSG) -> notes -> Furnace.
//!
//! Register model (VGM command 0xBB):
//!
//! ```text
//! 0/1, 2/3, 4/5, 6/7  AUDFx / AUDCx for the four voices
//! 8                   AUDCTL
//! 15                  SKCTL (serial/keyboard; not converted)
//! ```
//!
//! AUDCx: low nibble volume, bit 4 volume-only (a DC level, not a tone), bits
//! 5-7 distortion (Furnace's waveform 0-7).  There is no key-on register: a
//! voice sounds while its AUDC volume is non-zero.
//!
//! AUDCTL:
//!
//! ```text
//! bit 7  9-bit poly
//! bit 6  high clock for voice 1 (~1.79 MHz)
//! bit 5  high clock for voice 3
//! bit 4  join voices 1+2 (AUDF1 low byte, AUDF2 high byte)
//! bit 3  join voices 3+4 (AUDF3 low byte, AUDF4 high byte)
//! bit 2  high-pass filter 1+3
//! bit 1  high-pass filter 2+4
//! bit 0  15 kHz base clock for the non-high voices
//! ```
//!
//! A joined pair is one voice: the low channel's module column drives it, the
//! high channel's AUDC supplies volume and waveform, and the 16-bit AUDF value
//! is binary (AUDFlow | AUDFhigh << 8).  That is also how VGMPlay and Furnace's
//! POKEY core interpret it, so the source register value is passed through
//! rather than re-derived from a multiplicative prescaler.
//!
//! The module's POKEY platform turns a pitch-table value into the written AUDF
//! with a wave/AUDCTL dependent transform (platform/pokey.cpp, tick()).  We
//! invert that transform, so the module writes the same AUDF the source did.
//! Furnace's DIV_INS_POKEY macros are volume, arpeggio, AUDCTL, waveform and
//! pitch; this converter keeps AUDCTL and waveform as pattern effects
//! (0x11/0x10) and uses instruments for the per-frame volume contour and the
//! pitch trajectory.

use std::collections::{BTreeSet, HashMap};
use std::sync::{LazyLock, Mutex};

use indexmap::IndexMap;

use crate::parity::{py_floor_div, py_mod};
use crate::pitch::{furnace_period_note, module_clock, E5_CENTER};
use crate::vgm::VgmFile;

pub const POKEY_VOICES: usize = 4;
pub const POKEY_DEFAULT_CLOCK: u32 = 1_789_772;
// Furnace effect numbers (sysDef.cpp POKEY map).
pub const FX_WAVE: i32 = 0x10;
pub const FX_AUDCTL: i32 = 0x11;
// Furnace's POKEY pitch table maximum (DivPlatformPOKEY::notifyPitchTable).
pub const POKEY_PITCH_MAX: i32 = 0x0FFF_FFFF;

pub const REG_AUDF: [u32; 4] = [0, 2, 4, 6];
pub const REG_AUDC: [u32; 4] = [1, 3, 5, 7];
pub const REG_AUDCTL: u32 = 8;
pub const REG_SKCTL: u32 = 0x0F;

// A gate shorter than this is driver housekeeping, not music.
pub const POKEY_MIN_NOTE: i64 = 80;
// A loud level followed by a rise this large while the gate is open is a new
// hit (a drum roll jumps straight from the decayed level back to the attack).
pub const POKEY_RETRIGGER_RISE: i32 = 4;

// The platform's buzz/bass period snap tables (pokey.cpp).
pub const SNAP_LONG: [i64; 15] = [0, 1, 1, 3, 3, 6, 6, 7, 7, 10, 10, 12, 12, 13, 13];
pub const SNAP_SHORT: [i64; 15] = [2, 2, 2, 2, 5, 5, 5, 8, 8, 11, 11, 11, 11, 17, 17];
pub const SNAP_LONG16: [i64; 15] = [0, 0, 3, 3, 3, 5, 6, 6, 8, 9, 9, 11, 11, 14, 14];
pub const SNAP_SHORT16: [i64; 15] = [1, 1, 1, 4, 4, 4, 4, 4, 10, 10, 10, 10, 13, 13, 13];

/// AUDC distortion -> Furnace waveform effect value.
///
/// Furnace's waveforms 6 and 7 both write AUDC distortion 6, but their pitch
/// transforms use different snap tables.  Wave 6's short table skips many
/// source AUDF values (51/52 among them), which changes the poly4 sampling
/// phase and therefore the timbre; wave 7's long table can represent them.
/// The original Furnace modules use 7 for AUDC 6, so keep that mapping.
pub fn _furnace_wave(dist: i32) -> i32 {
    if dist >= 6 { 7 } else { dist }
}

pub fn _pair_joined(audctl: i32, low: i32) -> bool {
    audctl & (if low == 0 { 0x10 } else { 0x08 }) != 0
}

pub fn _is_high(voice: i32, audctl: i32) -> bool {
    (voice == 0 && audctl & 0x40 != 0) || (voice == 2 && audctl & 0x20 != 0)
}

/// Python port of DivPlatformPOKEY::tick()'s pitch conversion.
///
/// `value` is the engine pitch-table value (`calcFreq()`).  Returns the AUDF
/// byte/word the platform writes to the chip after its wave and AUDCTL
/// adjustments, including the core's small high-clock offsets.
pub fn furnace_written_register(value: i32, audctl: i32, voice: i32, wave: i32, joined: bool) -> i32 {
    let mut f = i64::max(0, value as i64);
    let high = _is_high(voice, audctl);
    if !high {
        f = py_floor_div(f, 7);
        if wave == 6 {
            f = py_floor_div(f, 5);
            f >>= 1;
        } else if wave == 7 {
            f = py_floor_div(f, if audctl & 1 != 0 { 5 } else { 15 });
            f >>= 1;
        } else {
            f >>= 2;
        }
    } else if wave == 6 {
        f = py_floor_div(f * 2, 5);
    } else if wave == 7 {
        f = py_floor_div(f * 2, 15);
    }
    if audctl & 1 != 0 && !high {
        f >>= 2;
    }
    f -= 1;
    if f < 0 {
        f = 0;
    }
    let mut min_freq = 255i64;
    if wave == 7 {
        if high {
            f = 15 * py_floor_div(f, 15) + SNAP_LONG16[py_mod(f, 15) as usize] + 1;
        } else if audctl & 1 == 0 {
            f = 15 * py_floor_div(f, 15) + SNAP_LONG[py_mod(f, 15) as usize];
        }
    } else if wave == 6 {
        if high {
            f = 15 * py_floor_div(f, 15) + SNAP_SHORT16[py_mod(f, 15) as usize] + 1;
        } else if audctl & 1 == 0 {
            f = 15 * py_floor_div(f, 15) + SNAP_SHORT[py_mod(f, 15) as usize];
        }
        min_freq = 251;
    }
    if joined {
        if f > 65535 {
            f = 65535;
        }
    } else if f > min_freq {
        f = min_freq;
    }
    f as i32
}

type InvertKey = (i32, i32, i32, i32, bool);

/// Smallest pitch-table value whose platform register is near `target`.
///
/// The platform transform is monotone nondecreasing in `value` (the wave
/// snapping only quantizes it), so a binary search finds the block and a few
/// neighbours pick the closest representable register.  The reference memoizes
/// this with `lru_cache(maxsize=8192)`; the cache is a pure memo.
fn _invert_register(target: i32, audctl: i32, voice: i32, wave: i32, joined: bool) -> i32 {
    static CACHE: LazyLock<Mutex<HashMap<InvertKey, i32>>> =
        LazyLock::new(|| Mutex::new(HashMap::new()));
    let key = (target, audctl, voice, wave, joined);
    if let Some(&hit) = CACHE.lock().unwrap().get(&key) {
        return hit;
    }
    let result = invert_register_uncached(target, audctl, voice, wave, joined);
    let mut cache = CACHE.lock().unwrap();
    if cache.len() >= 8192 {
        cache.clear();
    }
    cache.insert(key, result);
    result
}

fn invert_register_uncached(target: i32, audctl: i32, voice: i32, wave: i32, joined: bool) -> i32 {
    let target = target.clamp(0, if joined { 65535 } else { 255 });
    let written = |v: i32| furnace_written_register(v, audctl, voice, wave, joined);
    let mut lo = 1i32;
    let mut hi = 1 << 24;
    if written(hi) < target {
        return hi;
    }
    while lo < hi {
        let mid = (lo + hi) / 2;
        if written(mid) < target {
            lo = mid + 1;
        } else {
            hi = mid;
        }
    }
    if written(lo) == target {
        // Return the middle of the plateau.  The pitch table only stores a
        // few discrete values, and its nearest entry to the lower edge can
        // fall just below and write target-1; the middle keeps the note/E5
        // selection inside the plateau.
        let mut left = lo;
        let mut right = (lo + 1).min(1 << 24);
        while right < (1 << 24) && written(right) == target {
            left = right;
            right = (right * 2).min(1 << 24);
        }
        let mut a = left;
        let mut b = right;
        while a < b {
            let mid = (a + b + 1) / 2;
            if written(mid) == target {
                a = mid;
            } else {
                b = mid - 1;
            }
        }
        return (lo + a) / 2;
    }
    let mut best = lo;
    let mut best_err = (written(best) - target).abs();
    for cand in [best - 3, best - 2, best - 1, best + 1, best + 2, best + 3] {
        if cand < 1 {
            continue;
        }
        let err = (written(cand) - target).abs();
        if err < best_err {
            best = cand;
            best_err = err;
        }
    }
    best
}

/// Source AUDF (or a joined pair's 16-bit register) -> pitch-table value.
///
/// `voice` is the module voice (0-3).  For a joined pair pass the pair's low
/// byte as `audf` and the high byte as `high`; the target is the binary word
/// Furnace and VGMPlay both write.
pub fn pokey_value(audf: i32, audctl: i32, voice: i32, wave: i32, joined: bool, high: Option<i32>) -> i32 {
    let mut target = audf & 0xFF;
    if joined {
        target |= high.unwrap_or(0) << 8;
    }
    _invert_register(target, audctl & 0xFF, voice, wave & 7, joined)
}

/// Pitch-table value -> (Furnace note, E5 fine tune).
///
/// A `clock` of 0 stands for the reference's `None` (the module default).
pub fn pokey_note(clock: u32, value: i32) -> (i32, i32) {
    let clock = if clock == 0 { POKEY_DEFAULT_CLOCK } else { clock };
    furnace_period_note(
        module_clock(clock as f64),
        1.0,
        value.max(1),
        0,
        POKEY_PITCH_MAX,
    )
}

pub fn pokey_semitones(clock: u32, value: i32) -> f64 {
    let (note, e5) = pokey_note(clock, value);
    note as f64 + (e5 - E5_CENTER) as f64 / 128.0
}

#[derive(Clone, Debug, PartialEq)]
pub struct PokeyHit {
    pub sample_time: i64,
    pub chip_id: i32,
    pub voice: i32,    // module column (0-3); a joined pair lives on its low voice
    pub value: i32,    // Furnace pitch-table value at the gate
    pub vol: i32,      // AUDC volume 0-15
    pub clock: u32,
    pub off_sample: i64, // 0 = still sounding at the end
    pub wave: i32,       // effective waveform at the gate
    pub freq_pts: Vec<(i64, i32)>,
    pub vol_pts: Vec<(i64, i32)>,
}

impl Default for PokeyHit {
    fn default() -> Self {
        PokeyHit {
            sample_time: 0,
            chip_id: 0,
            voice: 0,
            value: 0,
            vol: 0,
            clock: 0,
            off_sample: 0,
            wave: 5,
            freq_pts: Vec::new(),
            vol_pts: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Default)]
pub struct PokeyOff {
    pub sample_time: i64,
    pub chip_id: i32,
    pub voice: i32,
}

#[derive(Clone, Debug)]
struct PokeyChip {
    audf: [i32; POKEY_VOICES],
    vol: [i32; POKEY_VOICES],
    wave: [i32; POKEY_VOICES],
    volonly: [bool; POKEY_VOICES],
    audctl: i32,
    skctl: i32,
    active: [Option<PokeyHit>; POKEY_VOICES],
}

fn _blank_chip() -> PokeyChip {
    PokeyChip {
        audf: [0; POKEY_VOICES],
        vol: [0; POKEY_VOICES],
        wave: [0; POKEY_VOICES],
        volonly: [false; POKEY_VOICES],
        audctl: 0,
        skctl: 0,
        active: std::array::from_fn(|_| None),
    }
}

#[derive(Default)]
struct WarningsSeen {
    volume_only: i64,
    two_tone: i64,
    glitch: i64,
    dist7: i64,
}

struct Collector {
    hits: Vec<PokeyHit>,
    offs: Vec<PokeyOff>,
    audctl_pts: Vec<(i64, i32, i32)>,
    wave_pts: Vec<(i64, i32, i32, i32)>,
    seen: WarningsSeen,
    clocks: IndexMap<i32, u32>,
}

impl Collector {
    fn joined_low(voice: usize, st: &PokeyChip) -> Option<usize> {
        let low = voice & !1;
        if _pair_joined(st.audctl, low as i32) {
            Some(low)
        } else {
            None
        }
    }

    fn effective_volume(voice: usize, st: &PokeyChip) -> i32 {
        let low = Self::joined_low(voice, st);
        let ctrl = match low {
            Some(low) => low + 1,
            None => voice,
        };
        if st.volonly[ctrl] {
            0
        } else {
            st.vol[ctrl]
        }
    }

    fn effective_wave(voice: usize, st: &PokeyChip) -> i32 {
        let low = Self::joined_low(voice, st);
        let ctrl = match low {
            Some(low) => low + 1,
            None => voice,
        };
        st.wave[ctrl] & 7
    }

    /// True for a silent channel that still clocks a high-pass filter.
    ///
    /// AUDCTL bit 2 filters channel 1 with channel 3, bit 1 filters channel 2
    /// with channel 4.  The control channel's volume is normally 0, but its
    /// AUDF still sets the filter, so it needs a silent note in the module.
    fn filter_control(voice: usize, st: &PokeyChip) -> bool {
        (voice == 2 && st.audctl & 0x04 != 0) || (voice == 3 && st.audctl & 0x02 != 0)
    }

    fn audible(voice: usize, st: &PokeyChip) -> bool {
        let low = Self::joined_low(voice, st);
        if let Some(low) = low {
            if voice != low {
                return false;
            }
        }
        if Self::filter_control(voice, st) {
            return true;
        }
        Self::effective_volume(voice, st) > 0
    }

    fn value_for(voice: usize, st: &PokeyChip) -> i32 {
        let low = Self::joined_low(voice, st);
        if let Some(low) = low {
            return pokey_value(
                st.audf[low],
                st.audctl,
                low as i32,
                st.wave[low + 1],
                true,
                Some(st.audf[low + 1]),
            );
        }
        pokey_value(
            st.audf[voice],
            st.audctl,
            voice as i32,
            st.wave[voice],
            false,
            None,
        )
    }

    fn close(&mut self, st: &mut PokeyChip, cid: i32, voice: usize, t: i64, emit_off: bool) {
        let hit = st.active[voice].take();
        let Some(mut hit) = hit else {
            return;
        };
        hit.off_sample = t;
        if t - hit.sample_time >= POKEY_MIN_NOTE {
            self.hits.push(hit);
            if emit_off {
                self.offs.push(PokeyOff {
                    sample_time: t,
                    chip_id: cid,
                    voice: voice as i32,
                });
            }
        } else {
            self.seen.glitch += 1;
        }
    }

    fn open_hit(&mut self, st: &mut PokeyChip, cid: i32, voice: usize, t: i64, value: i32) {
        let wave = Self::effective_wave(voice, st);
        let hit = PokeyHit {
            sample_time: t,
            chip_id: cid,
            voice: voice as i32,
            value,
            vol: Self::effective_volume(voice, st),
            clock: self.clocks[&cid],
            off_sample: 0,
            wave,
            freq_pts: Vec::new(),
            vol_pts: Vec::new(),
        };
        st.active[voice] = Some(hit);
        // The module starts every channel on waveform 5, so the first note of
        // a channel must state the source's waveform even when the source
        // never changed it from its own reset value.
        self.wave_pts.push((t, cid, voice as i32, wave));
    }

    fn refresh(&mut self, st: &mut PokeyChip, cid: i32, voice: usize, t: i64) {
        let on = Self::audible(voice, st);
        if !on {
            if st.active[voice].is_some() {
                self.close(st, cid, voice, t, true);
            }
            return;
        }
        let value = Self::value_for(voice, st);
        if st.active[voice].is_none() {
            self.open_hit(st, cid, voice, t, value);
            return;
        }
        let wave = Self::effective_wave(voice, st);
        let level = Self::effective_volume(voice, st);
        let mut retrigger = false;
        {
            let hit = st.active[voice].as_mut().unwrap();
            let cur = hit.freq_pts.last().map(|p| p.1).unwrap_or(hit.value);
            if value != cur {
                hit.freq_pts.push((t, value));
            }
            if wave != hit.wave {
                hit.wave = wave;
                self.wave_pts.push((t, cid, voice as i32, wave));
            }
            let prev = hit.vol_pts.last().map(|p| p.1).unwrap_or(hit.vol);
            if level != prev {
                if level - prev >= POKEY_RETRIGGER_RISE {
                    // A drum roll jumps straight from the decayed level back to
                    // the attack while the gate stays open.  Start a new note so
                    // the hit reaches the pattern instead of living in the tail of
                    // the previous volume macro.
                    retrigger = true;
                } else {
                    hit.vol_pts.push((t, level));
                }
            }
        }
        if retrigger {
            self.close(st, cid, voice, t, false);
            self.open_hit(st, cid, voice, t, value);
        }
    }

    fn module_voice_for_audf(voice: usize, st: &PokeyChip) -> usize {
        Self::joined_low(voice, st).unwrap_or(voice)
    }
}

/// Walk a POKEY register stream into notes.
///
/// Returns `(hits, offs, audctl_pts, wave_pts, warnings)`.  AUDCTL is a
/// chip-wide register, so its changes are returned as `(sample, chip, value)`;
/// waveform changes are per module column as `(sample, chip, voice, wave)`.
/// The emitter turns both into 0x11/0x10 row effects.
pub fn collect_pokey(
    vgm: &VgmFile,
) -> (
    Vec<PokeyHit>,
    Vec<PokeyOff>,
    Vec<(i64, i32, i32)>,
    Vec<(i64, i32, i32, i32)>,
    Vec<String>,
) {
    let mut warnings: Vec<String> = Vec::new();
    let chips: BTreeSet<i32> = vgm
        .writes
        .iter()
        .filter(|w| w.chip == "pokey")
        .map(|w| w.chip_id as i32)
        .collect();
    if chips.is_empty() {
        return (
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            warnings,
        );
    }
    let base_clock = if vgm.pokey_clock == 0 {
        POKEY_DEFAULT_CLOCK
    } else {
        vgm.pokey_clock
    };
    let mut clocks: IndexMap<i32, u32> = IndexMap::new();
    for &cid in &chips {
        let clock = if cid == 1 {
            if vgm.pokey_clock_2 == 0 {
                base_clock
            } else {
                vgm.pokey_clock_2
            }
        } else {
            base_clock
        };
        clocks.insert(cid, clock);
    }
    let mut state: IndexMap<i32, PokeyChip> =
        chips.iter().map(|&cid| (cid, _blank_chip())).collect();
    let mut col = Collector {
        hits: Vec::new(),
        offs: Vec::new(),
        audctl_pts: Vec::new(),
        wave_pts: Vec::new(),
        seen: WarningsSeen::default(),
        clocks,
    };

    for w in &vgm.writes {
        if w.chip != "pokey" {
            continue;
        }
        let cid = w.chip_id as i32;
        let st = state.entry(cid).or_insert_with(_blank_chip);
        if w.reg > REG_SKCTL {
            continue;
        }
        let r = w.reg;
        let val = (w.val & 0xFF) as i32;
        let t = w.sample;
        if r == REG_AUDCTL {
            if val == st.audctl {
                continue;
            }
            let old = st.audctl;
            st.audctl = val;
            col.audctl_pts.push((t, cid, val));
            // Join transitions move a pair between two columns.
            for low in [0usize, 2] {
                let bit = if low == 0 { 0x10 } else { 0x08 };
                let was = old & bit != 0;
                let now = val & bit != 0;
                if was == now {
                    continue;
                }
                let high = low + 1;
                if st.active[low].is_some() {
                    col.close(st, cid, low, t, false);
                }
                if st.active[high].is_some() {
                    col.close(st, cid, high, t, false);
                }
                if now {
                    if Collector::audible(low, st) {
                        let value = Collector::value_for(low, st);
                        col.open_hit(st, cid, low, t, value);
                    }
                } else {
                    if Collector::audible(low, st) {
                        let value = Collector::value_for(low, st);
                        col.open_hit(st, cid, low, t, value);
                    }
                    if Collector::audible(high, st) {
                        let value = Collector::value_for(high, st);
                        col.open_hit(st, cid, high, t, value);
                    }
                }
            }
            // The clock/join bits retune every sounding voice.
            for voice in 0..POKEY_VOICES {
                if st.active[voice].is_some() {
                    col.refresh(st, cid, voice, t);
                }
            }
            // A filter control channel (3 for 1+3, 4 for 2+4) is silent by
            // volume but still needs a note so its AUDF drives the filter.
            for voice in [2usize, 3] {
                if Collector::filter_control(voice, st) || st.active[voice].is_some() {
                    col.refresh(st, cid, voice, t);
                }
            }
            continue;
        }
        if r == REG_SKCTL {
            if val & 0x80 != 0 && st.skctl & 0x80 == 0 {
                col.seen.two_tone += 1;
            }
            st.skctl = val;
            continue;
        }
        if REG_AUDF.contains(&r) {
            let voice = (r / 2) as usize;
            st.audf[voice] = val;
            let module = Collector::module_voice_for_audf(voice, st);
            // A low join byte that is not the output byte still changes the
            // pair's combined period.  A silent filter control also needs its
            // note opened/retuned here.
            col.refresh(st, cid, module, t);
            continue;
        }
        if REG_AUDC.contains(&r) {
            let voice = (r / 2) as usize;
            st.vol[voice] = val & 0x0F;
            let dist = (val >> 5) & 7;
            if dist == 7 {
                col.seen.dist7 += 1;
            }
            st.wave[voice] = _furnace_wave(dist);
            st.volonly[voice] = val & 0x10 != 0;
            if val & 0x10 != 0 {
                col.seen.volume_only += 1;
            }
            let low = Collector::joined_low(voice, st);
            if let Some(low) = low {
                if voice == low {
                    // The joined pair's output is controlled by the high byte's
                    // AUDC; the low register is ignored while joined.
                    continue;
                }
            }
            let module = low.unwrap_or(voice);
            col.refresh(st, cid, module, t);
            continue;
        }
    }

    for st in state.values_mut() {
        for voice in 0..POKEY_VOICES {
            if let Some(mut hit) = st.active[voice].take() {
                hit.off_sample = 0;
                col.hits.push(hit);
            }
        }
    }

    if col.seen.volume_only != 0 {
        warnings.push(format!(
            "{} POKEY volume-only (AUDC bit 4) writes are not converted (Furnace has no DC/DAC mode)",
            col.seen.volume_only
        ));
    }
    if col.seen.two_tone != 0 {
        warnings.push(format!(
            "{} POKEY two-tone (SKCTL bit 7) setup writes are not converted",
            col.seen.two_tone
        ));
    }
    if col.seen.dist7 != 0 {
        warnings.push(format!(
            "{} POKEY AUDC distortion-7 writes play as distortion 6 (Furnace's wave map has no separate 7 tap)",
            col.seen.dist7
        ));
    }
    if col.seen.glitch != 0 {
        warnings.push(format!(
            "{} sub-frame POKEY gate(s) skipped",
            col.seen.glitch
        ));
    }
    (col.hits, col.offs, col.audctl_pts, col.wave_pts, warnings)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vgm::ChipWrite;

    fn vgm(writes: Vec<ChipWrite>) -> VgmFile {
        VgmFile {
            writes,
            pokey_clock: POKEY_DEFAULT_CLOCK,
            ..VgmFile::default()
        }
    }

    fn pokey(sample: i64, reg: u32, val: u32) -> ChipWrite {
        ChipWrite::new(sample, "pokey", 0, reg, val)
    }

    #[test]
    fn furnace_written_register_matches_platform() {
        let cases: [(i32, i32, i32, i32, bool, i32); 8] = [
            (1000, 0, 0, 5, false, 34),
            (1000, 0, 0, 7, false, 3),
            (1000, 1, 0, 7, false, 2),
            (1000, 0, 2, 7, false, 3),
            (1000, 0x40, 0, 7, false, 132),
            (1000, 0, 0, 6, false, 17),
            (1000, 0, 2, 6, false, 17),
            (0x12_3456, 0x10, 0, 7, true, 5680),
        ];
        for (value, audctl, voice, wave, joined, want) in cases {
            assert_eq!(
                furnace_written_register(value, audctl, voice, wave, joined),
                want,
                "value={value} audctl={audctl} voice={voice} wave={wave}"
            );
        }
    }

    #[test]
    fn plain_high_and_15khz_round_trip() {
        let cases = [
            (0x00, 0, 5, 100),
            (0x40, 0, 5, 100), // high clock voice 1
            (0x20, 2, 5, 100), // high clock voice 3
            (0x01, 2, 5, 200), // 15 kHz
        ];
        for (audctl, voice, wave, target) in cases {
            let t = pokey_value(target, audctl, voice, wave, false, None);
            assert_eq!(
                furnace_written_register(t, audctl, voice, wave, false),
                target,
                "audctl={audctl} voice={voice} target={target}"
            );
        }
        // Waveform snapping still represents the source register.
        for wave in [6, 7] {
            for (audctl, voice, joined) in [(0x00, 0, false), (0x40, 0, false)] {
                let t0 = pokey_value(80, audctl, voice, 5, joined, None);
                let target = furnace_written_register(t0, audctl, voice, wave, joined);
                let t = pokey_value(target & 0xFF, audctl, voice, wave, joined, None);
                assert_eq!(
                    furnace_written_register(t, audctl, voice, wave, joined),
                    target,
                    "wave={wave} audctl={audctl} voice={voice}"
                );
            }
        }
    }

    #[test]
    fn joined_binary_register() {
        // AUDF1=0xA7, AUDF2=0x06, AUDCTL=0x50 (high clock 1 + join 1+2).
        let t = pokey_value(0xA7, 0x50, 0, 5, true, Some(0x06));
        assert_eq!(furnace_written_register(t, 0x50, 0, 5, true), 0x06A7);
        let (note, _e5) = pokey_note(POKEY_DEFAULT_CLOCK, t);
        assert!((0..=179).contains(&note));
    }

    #[test]
    fn gate_and_volume_envelope() {
        let mut writes = vec![
            pokey(0, 8, 0x00),
            pokey(0, 0, 100),
            pokey(0, 1, 0xAF),
        ];
        for k in 1..15i64 {
            writes.push(pokey(735 * k, 1, 0xA0 | (15 - k) as u32));
        }
        writes.push(pokey(735 * 15, 1, 0x00));
        let (hits, offs, audctl, waves, warn) = collect_pokey(&vgm(writes));
        assert_eq!(hits.len(), 1);
        let hit = &hits[0];
        assert_eq!((hit.voice, hit.vol), (0, 15));
        assert_eq!(hit.vol_pts.len(), 14);
        assert_eq!(hit.vol_pts.last().copied(), Some((735 * 14, 1)));
        assert_eq!(
            offs.iter().map(|o| o.sample_time).collect::<Vec<_>>(),
            vec![735 * 15]
        );
        assert_eq!(furnace_written_register(hit.value, 0, 0, 5, false), 100);
        assert!(audctl.is_empty());
        assert_eq!(waves[0], (0, 0, 0, 5));
        assert!(warn.is_empty());
    }

    #[test]
    fn joined_pair_uses_high_audc_and_tracks_high_byte() {
        let writes = vec![
            pokey(0, 8, 0x50),
            pokey(0, 0, 0xA7),
            pokey(0, 2, 0x06),
            pokey(0, 3, 0xA5),    // AUDC2: vol 5, wave 5
            pokey(600, 2, 0x07),  // high byte moves
            pokey(1200, 3, 0x00), // volume off
        ];
        let (hits, offs, _audctl, _waves, _warn) = collect_pokey(&vgm(writes));
        assert_eq!(hits.len(), 1);
        let hit = &hits[0];
        assert_eq!(hit.voice, 0);
        assert_eq!(hit.vol, 5);
        assert_eq!(
            furnace_written_register(hit.value, 0x50, 0, 5, true),
            0x06A7
        );
        assert_eq!(hit.freq_pts.len(), 1);
        assert_eq!(hit.freq_pts[0].0, 600);
        assert_eq!(
            furnace_written_register(hit.freq_pts[0].1, 0x50, 0, 5, true),
            0x07A7
        );
        assert_eq!(
            offs.iter().map(|o| o.sample_time).collect::<Vec<_>>(),
            vec![1200]
        );
    }
}
