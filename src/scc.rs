//! Konami K051649 / K052539 (SCC / SCC-I) -> notes -> Furnace.
//!
//! The register model follows libvgm ``k051649.c`` (the core VGM rips are
//! played with) and Furnace's ``vgsound_emu`` SCC core, which implement the
//! same chip.
//!
//! The VGM command is ``0xD2 pp aa dd``: ``pp`` is a register *bank* and
//! ``aa`` an 8-bit register inside it.  This module keeps the bank and
//! register packed as ``reg = (pp << 8) | aa``:
//!
//! ```text
//! 0x000-0x0ff  waveform RAM: 32 bytes per voice, voices 0-3 (0x060-0x07f is
//!              shared by voices 3 and 4 on the K051649)
//! 0x100-0x109  frequency: two bytes per voice, low then high (12-bit period)
//! 0x200-0x204  volume: 4-bit per voice
//! 0x300        voice enable (gate) bits, one per voice
//! 0x400-0x49f  waveform RAM for voice 4 (K052539 / SCC+)
//! 0x500-0x50f  test register (rotate and description bits)
//! ```
//!
//! A voice sounds at ``clock / (16 * (period + 1))`` and is silent while
//! ``period <= 8``.  Furnace's SCC platform runs the pitch table with the same
//! clock and divider and writes ``value - 1``, so a period register
//! round-trips exactly through the table in ``pitch.rs``.
//!
//! Gates: the chip has no key-on register, only the enable bits, and drivers
//! use either the enable bit, the volume register (0 = off), or both as the
//! gate.  A note starts when either goes from off to on and ends when either
//! goes off; the pitch (and waveform) is the one in the registers, with the
//! same one-frame refit the C140 path uses for drivers that write the note's
//! own registers a few milliseconds after the gate.

use std::collections::BTreeSet;

use indexmap::IndexMap;

use crate::pitch::{furnace_period_note, module_clock};
use crate::vgm::VgmFile;

pub const SCC_VOICES: usize = 5;
pub const SCC_WAVE_LEN: usize = 32;
pub const SCC_DEFAULT_CLOCK: u32 = 1_789_772; // MSX SCC: half the 3.579545 MHz slot clock
pub const SCC_MIN_PERIOD: i32 = 9; // the chip's counter halts a voice at period <= 8
pub const SCC_REFIT_SAMPLES: i64 = 735; // one 60 Hz VGM frame
// A driver that morphs a note's waveform rewrites the 32 bytes in one burst;
// writes this close together are the same rewrite, and this caps how many
// morph steps one note keeps.
pub const WAVE_BURST_SAMPLES: i64 = 64;
pub const WAVE_PTS_MAX: usize = 4096;
// Gates shorter than this are driver housekeeping (init sequences toggle the
// enable mask for a few samples), not music.
pub const SCC_MIN_NOTE: i64 = 100;

pub const TEST_FREQ_4BIT: u8 = 0x01;
pub const TEST_FREQ_8BIT: u8 = 0x02;
pub const TEST_ROTATE: u8 = 0x40;

// Wave RAM bytes the two shared voices share on the K051649.
const SHARED_WAVE_LO: u32 = 0x60;

/// Voice frequency for a 12-bit period register, in Hz.
pub fn scc_rate(clock: u32, period: i32) -> f64 {
    let clock = if clock == 0 { SCC_DEFAULT_CLOCK } else { clock };
    clock as f64 / (16.0 * (period + 1) as f64)
}

/// Period register -> (Furnace note, E5 fine tune) for the given clock.
///
/// Furnace's SCC platform writes ``pitch value - 1``, so the pitch table has
/// to produce ``period + 1`` for the source's ``period`` (the ``period_offset``
/// argument).
pub fn scc_note(clock: u32, period: i32) -> (i32, i32) {
    furnace_period_note(module_clock(clock as f64), 16.0, period, 1, 0xFFF)
}

#[derive(Clone, Debug, PartialEq)]
pub struct SccHit {
    pub sample_time: i64,
    pub chip_id: i32,
    pub voice: i32,
    pub period: i32, // 12-bit frequency register at the gate
    pub vol: i32,    // 4-bit volume register at the gate
    pub wave: Vec<u8>, // 32 signed bytes the voice plays
    pub off_sample: i64, // gate-off sample (0 = still sounding at the end)
    // Mid-note register writes while the voice sounds.
    pub freq_pts: Vec<(i64, i32)>,
    pub vol_pts: Vec<(i64, i32)>,
    // Every (sample, image) change of the 32 wave-RAM bytes the voice plays
    // after the gate, one entry per rewrite burst. The analyzer merges the
    // burst into the instrument's wave macro.
    pub wave_pts: Vec<(i64, Vec<u8>)>,
}

impl Default for SccHit {
    fn default() -> Self {
        Self {
            sample_time: 0,
            chip_id: 0,
            voice: 0,
            period: 0,
            vol: 0,
            wave: Vec::new(),
            off_sample: 0,
            freq_pts: Vec::new(),
            vol_pts: Vec::new(),
            wave_pts: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct SccOff {
    pub sample_time: i64,
    pub chip_id: i32,
    pub voice: i32,
}

impl Default for SccOff {
    fn default() -> Self {
        Self {
            sample_time: 0,
            chip_id: 0,
            voice: 0,
        }
    }
}

/// A gate-to-gate interval of one voice, with everything it needs.
#[derive(Clone, Debug, PartialEq)]
pub struct SccNote {
    pub hit: SccHit,
    pub end_sample: i64, // 0 = still sounding
    pub wave_index: usize,
}

impl Default for SccNote {
    fn default() -> Self {
        Self {
            hit: SccHit::default(),
            end_sample: 0,
            wave_index: 0,
        }
    }
}

#[derive(Clone, Debug)]
struct SccChip {
    wave: [[u8; SCC_WAVE_LEN]; SCC_VOICES],
    freq: [i32; SCC_VOICES],
    vol: [i32; SCC_VOICES],
    // live gate state: enable bit and non-zero volume
    en: [bool; SCC_VOICES],
    active: [Option<SccHit>; SCC_VOICES],
    test: u8,
    gate_time: [i64; SCC_VOICES],
    // samples until which a pitch/volume/wave rewrite counts as the note's
    // own (drivers write them a few milliseconds after the gate)
    refit_until: [i64; SCC_VOICES],
}

fn blank_chip() -> SccChip {
    SccChip {
        wave: [[0; SCC_WAVE_LEN]; SCC_VOICES],
        freq: [0; SCC_VOICES],
        vol: [0; SCC_VOICES],
        en: [false; SCC_VOICES],
        active: std::array::from_fn(|_| None),
        test: 0,
        gate_time: [0; SCC_VOICES],
        refit_until: [0; SCC_VOICES],
    }
}

/// Wave RAM image, kept as the chip's raw (unsigned) bytes.
///
/// The chip reads each byte as signed 8-bit; Furnace's wavetable holds
/// ``signed + 128``, so the writer converts with ``(b + 128) & 0xFF``.  An
/// all-zero image is silence (signed zero).
pub fn wave_bytes(raw: &[u8]) -> Vec<u8> {
    raw.to_vec()
}

fn bump(acc: &mut IndexMap<&'static str, i64>, key: &'static str) {
    *acc.entry(key).or_insert(0) += 1;
}

fn wave_index(waves: &mut IndexMap<Vec<u8>, usize>, wave: &[u8]) -> usize {
    if let Some(&idx) = waves.get(wave) {
        return idx;
    }
    let idx = waves.len();
    waves.insert(wave.to_vec(), idx);
    idx
}

/// End the voice's interval at `t` (a gate went off).
fn close(
    st: &mut SccChip,
    cid: i32,
    voice: usize,
    t: i64,
    emit_off: bool,
    notes: &mut Vec<SccNote>,
    offs: &mut Vec<SccOff>,
    waves: &mut IndexMap<Vec<u8>, usize>,
    acc: &mut IndexMap<&'static str, i64>,
) {
    let Some(mut hit) = st.active[voice].take() else {
        return;
    };
    hit.off_sample = t;
    if t - hit.sample_time >= SCC_MIN_NOTE {
        if emit_off {
            offs.push(SccOff {
                sample_time: t,
                chip_id: cid,
                voice: voice as i32,
            });
        }
        let idx = wave_index(waves, &hit.wave);
        notes.push(SccNote {
            hit,
            end_sample: t,
            wave_index: idx,
        });
    } else {
        bump(acc, "glitch");
    }
    st.active[voice] = None;
}

/// Start the voice's interval at `t` (a gate went on).
fn open_gate(st: &mut SccChip, cid: i32, voice: usize, t: i64) {
    let hit = SccHit {
        sample_time: t,
        chip_id: cid,
        voice: voice as i32,
        period: st.freq[voice],
        vol: st.vol[voice],
        wave: wave_bytes(&st.wave[voice]),
        ..SccHit::default()
    };
    st.active[voice] = Some(hit);
    st.gate_time[voice] = t;
    st.refit_until[voice] = t + SCC_REFIT_SAMPLES;
}

/// Recompute the voice's gate from the enable bit and the volume.
fn update_gate(
    st: &mut SccChip,
    cid: i32,
    voice: usize,
    t: i64,
    notes: &mut Vec<SccNote>,
    offs: &mut Vec<SccOff>,
    waves: &mut IndexMap<Vec<u8>, usize>,
    acc: &mut IndexMap<&'static str, i64>,
) {
    let on = st.en[voice] && st.vol[voice] > 0;
    if on && st.active[voice].is_none() {
        open_gate(st, cid, voice, t);
    } else if !on {
        close(st, cid, voice, t, true, notes, offs, waves, acc);
    }
}

/// Walk the SCC register stream into notes.
///
/// Returns (notes, key-offs, distinct waveforms, warnings).  Waveforms are
/// deduplicated so several channels or notes sharing one image share one
/// Furnace instrument.
pub fn collect_scc(
    vgm: &VgmFile,
) -> (
    Vec<SccNote>,
    Vec<SccOff>,
    Vec<Vec<u8>>,
    Vec<String>,
) {
    let mut warnings: Vec<String> = Vec::new();
    let mut notes: Vec<SccNote> = Vec::new();
    let mut offs: Vec<SccOff> = Vec::new();
    let mut waves: IndexMap<Vec<u8>, usize> = IndexMap::new();
    let plus = vgm.k051649_plus;
    let chips: BTreeSet<i32> = vgm
        .writes
        .iter()
        .filter(|w| w.chip == "k051649")
        .map(|w| w.chip_id as i32)
        .collect();
    if chips.is_empty() {
        return (Vec::new(), Vec::new(), Vec::new(), warnings);
    }
    let mut state: IndexMap<i32, SccChip> =
        chips.iter().map(|&cid| (cid, blank_chip())).collect();
    let mut acc: IndexMap<&'static str, i64> = IndexMap::new();

    for w in &vgm.writes {
        if w.chip != "k051649" {
            continue;
        }
        let cid = w.chip_id as i32;
        let st = state.entry(cid).or_insert_with(blank_chip);
        let bank = w.reg >> 8;
        let mut addr = w.reg & 0xFF;
        let val = (w.val & 0xFF) as u8;
        if bank == 0x0 || bank == 0x4 {
            let test_mask = if bank == 0x0 { TEST_ROTATE } else { 0 };
            if st.test & test_mask != 0 {
                bump(&mut acc, "wave_locked");
                continue;
            }
            if bank == 0x4 && !plus {
                // On the K051649 the port-4 bank is not wired; libvgm treats
                // it as another write to the same wave RAM.
                addr = SHARED_WAVE_LO | (addr & 0x1F);
            } else if plus && addr >= 0xA0 {
                // Past voice 4's wave the K052539 window repeats the register
                // page. Frequency, volume and enable are taken from banks 1-3.
                continue;
            }
            let voices: Vec<usize> = if plus && (0x80..=0x9F).contains(&addr) {
                // SCC+ gives voice 4 its own 32 bytes. The K051649 does not:
                // there, voices 3 and 4 share 0x60-0x7F.
                vec![4]
            } else if !plus && addr >= SHARED_WAVE_LO {
                vec![3, 4]
            } else {
                vec![(addr >> 5) as usize]
            };
            let low = (addr & 0x1F) as usize;
            for &voice in &voices {
                st.wave[voice][low] = val;
            }
            let active: Vec<usize> = voices
                .iter()
                .copied()
                .filter(|&v| st.active[v].is_some())
                .collect();
            for voice in active {
                let img = wave_bytes(&st.wave[voice]);
                let refit = w.sample <= st.refit_until[voice];
                let h = st.active[voice].as_mut().unwrap();
                if refit {
                    // The driver rewrote the note's own waveform right after
                    // the gate; that image is what the voice plays.
                    h.wave = img;
                    continue;
                }
                if img.iter().all(|&b| b == 0) {
                    continue;
                }
                let last = h.wave_pts.last().map(|(t, i)| (*t, i.clone()));
                if let Some((last_t, _)) = &last {
                    if w.sample - *last_t <= WAVE_BURST_SAMPLES {
                        // A whole-image rewrite is a burst of byte writes;
                        // only the state it settles on is a step of the morph.
                        let at = h.wave_pts.len() - 1;
                        h.wave_pts[at] = (*last_t, img);
                        continue;
                    }
                }
                if let Some((_, last_img)) = &last {
                    if img == *last_img {
                        continue;
                    }
                }
                if h.wave_pts.len() >= WAVE_PTS_MAX {
                    bump(&mut acc, "wave_mid");
                    continue;
                }
                h.wave_pts.push((w.sample, img));
            }
        } else if bank == 0x1 {
            let voice = ((addr >> 1) as usize) % SCC_VOICES;
            if addr & 1 != 0 {
                st.freq[voice] =
                    (st.freq[voice] & 0x0FF) | (((val & 0x0F) as i32) << 8);
            } else {
                st.freq[voice] = (st.freq[voice] & 0xF00) | val as i32;
            }
            let freq = st.freq[voice];
            let refit = w.sample <= st.refit_until[voice];
            if let Some(h) = st.active[voice].as_mut() {
                // Compare with what the voice was last seen playing, so a
                // step back to the note's own pitch is recorded too.
                let cur = h.freq_pts.last().map(|p| p.1).unwrap_or(h.period);
                if cur != freq {
                    if refit {
                        // A stale key-on value replaced inside the frame is
                        // the note's own pitch (same rule as C140/C352).
                        h.period = freq;
                    } else {
                        h.freq_pts.push((w.sample, freq));
                    }
                }
            }
        } else if bank == 0x2 {
            let voice = (addr as usize) % SCC_VOICES;
            let new_vol = (val & 0x0F) as i32;
            if new_vol != st.vol[voice] {
                st.vol[voice] = new_vol;
                if new_vol > 0 {
                    if let Some(h) = st.active[voice].as_mut() {
                        // SCC drivers set the level before the gate and start
                        // the decay right after it, so a write inside the
                        // frame is a step of that decay, not the note's own
                        // level.
                        h.vol_pts.push((w.sample, new_vol));
                    }
                }
                update_gate(
                    st, cid, voice, w.sample, &mut notes, &mut offs, &mut waves,
                    &mut acc,
                );
            }
        } else if bank == 0x3 {
            for voice in 0..SCC_VOICES {
                let en = val & (1 << voice) != 0;
                if en == st.en[voice] {
                    continue;
                }
                st.en[voice] = en;
                update_gate(
                    st, cid, voice, w.sample, &mut notes, &mut offs, &mut waves,
                    &mut acc,
                );
            }
        } else if bank == 0x5 {
            if val != 0 {
                st.test = val;
                if val & (TEST_FREQ_4BIT | TEST_FREQ_8BIT) != 0 {
                    bump(&mut acc, "test_freq");
                }
            }
        }
    }

    // Close whatever is still sounding so the writer sees an open end.
    for st in state.values_mut() {
        for voice in 0..SCC_VOICES {
            if let Some(hit) = st.active[voice].take() {
                let idx = wave_index(&mut waves, &hit.wave);
                notes.push(SccNote {
                    hit,
                    end_sample: 0,
                    wave_index: idx,
                });
                st.active[voice] = None;
            }
        }
    }

    if acc.get("wave_locked").copied().unwrap_or(0) != 0 {
        warnings.push(format!(
            "{} SCC waveform writes were ignored while the test register froze the wave RAM",
            acc["wave_locked"]
        ));
    }
    if acc.get("wave_mid").copied().unwrap_or(0) != 0 {
        warnings.push(format!(
            "{} SCC waveform changes past the {}-entry cap were dropped from a note's morph",
            acc["wave_mid"], WAVE_PTS_MAX
        ));
    }
    if acc.get("test_freq").copied().unwrap_or(0) != 0 {
        warnings.push(format!(
            "{} SCC test-register writes change the frequency word width; the 12-bit interpretation is kept",
            acc["test_freq"]
        ));
    }
    let mut wave_list = vec![Vec::new(); waves.len()];
    for (wave, idx) in &waves {
        wave_list[*idx] = wave.clone();
    }
    (notes, offs, wave_list, warnings)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vgm::ChipWrite;

    fn sine() -> Vec<u8> {
        (0..32)
            .map(|i| {
                let v = 127.0 * (2.0 * std::f64::consts::PI * i as f64 / 32.0).sin();
                (crate::parity::py_round(v) as i64 & 0xFF) as u8
            })
            .collect()
    }

    fn wave_writes(offset: u32, data: &[u8]) -> Vec<ChipWrite> {
        data.iter()
            .enumerate()
            .map(|(i, &b)| {
                ChipWrite::new(0, "k051649", 0, offset + i as u32, b as u32)
            })
            .collect()
    }

    fn song(writes: Vec<ChipWrite>, plus: bool) -> VgmFile {
        VgmFile {
            writes,
            k051649_clock: 1_789_772,
            k051649_plus: plus,
            ..VgmFile::default()
        }
    }

    #[test]
    fn rate_and_note_round_trip_through_the_pitch_table() {
        assert_eq!(scc_rate(1_789_772, 253), 1_789_772.0 / (16.0 * 254.0));
        // A zero clock falls back to the SCC default.
        assert_eq!(scc_rate(0, 253), scc_rate(SCC_DEFAULT_CLOCK, 253));

        let (note, e5) = scc_note(1_789_772, 253);
        assert!((0..=179).contains(&note));
        let table = crate::pitch::pitch_table(1_789_772.0, 16.0, 0xFFF);
        assert_eq!(table.value(note, e5 - 0x80) - 1, 253);
    }

    #[test]
    fn note_and_gate_become_one_interval() {
        let wave = sine();
        let mut writes = wave_writes(0, &wave);
        writes.extend([
            ChipWrite::new(0, "k051649", 0, 0x100, 253 & 0xFF),
            ChipWrite::new(0, "k051649", 0, 0x101, 253 >> 8),
            ChipWrite::new(0, "k051649", 0, 0x200, 15),
            ChipWrite::new(100, "k051649", 0, 0x300, 0x01),
            ChipWrite::new(5000, "k051649", 0, 0x300, 0x00),
        ]);
        let (notes, offs, waves, warn) = collect_scc(&song(writes, false));
        assert!(warn.is_empty());
        assert_eq!(notes.len(), 1);
        let hit = &notes[0].hit;
        assert_eq!((hit.voice, hit.period, hit.vol, hit.sample_time), (0, 253, 15, 100));
        assert_eq!(hit.wave, wave);
        assert_eq!(hit.off_sample, 5000);
        assert_eq!(notes[0].end_sample, 5000);
        assert_eq!(notes[0].wave_index, 0);
        assert_eq!(offs.len(), 1);
        assert_eq!((offs[0].sample_time, offs[0].voice), (5000, 0));
        assert_eq!(waves, vec![wave]);
    }

    #[test]
    fn shared_wave_reaches_voices_three_and_four_only_on_k051649() {
        let wave = sine();
        let mut writes = wave_writes(0x60, &wave);
        writes.extend([
            ChipWrite::new(0, "k051649", 0, 0x106, 253),
            ChipWrite::new(0, "k051649", 0, 0x107, 0),
            ChipWrite::new(0, "k051649", 0, 0x203, 15),
            ChipWrite::new(100, "k051649", 0, 0x300, 0x08), // voice 3
        ]);
        let (notes, ..) = collect_scc(&song(writes, false));
        assert_eq!(notes.len(), 1);
        assert_eq!(notes[0].hit.voice, 3);
        assert_eq!(notes[0].hit.wave, wave);

        // On SCC+ the same address is voice 3's own wave only; voice 4 keeps
        // its separate image at 0x80-0x9F.
        let mut writes = wave_writes(0x60, &wave);
        writes.extend([
            ChipWrite::new(0, "k051649", 0, 0x108, 253),
            ChipWrite::new(0, "k051649", 0, 0x204, 15),
            ChipWrite::new(100, "k051649", 0, 0x300, 0x10), // voice 4
        ]);
        let (notes, ..) = collect_scc(&song(writes, true));
        assert_eq!(notes.len(), 1);
        assert_eq!(notes[0].hit.wave, vec![0u8; 32]);
    }

    #[test]
    fn plus_bank_four_maps_voice_four_and_ignores_the_shadow_page() {
        let wave = sine();
        let mut writes = wave_writes(0x480, &wave);
        writes.push(ChipWrite::new(0, "k051649", 0, 0x4A0, 0xFF)); // shadow
        writes.extend([
            ChipWrite::new(0, "k051649", 0, 0x108, 253),
            ChipWrite::new(0, "k051649", 0, 0x204, 15),
            ChipWrite::new(100, "k051649", 0, 0x300, 0x10), // voice 4
        ]);
        let (notes, ..) = collect_scc(&song(writes, true));
        assert_eq!(notes.len(), 1);
        assert_eq!(notes[0].hit.voice, 4);
        assert_eq!(notes[0].hit.wave, wave);
    }

    #[test]
    fn mid_note_frequency_and_volume_writes_become_points() {
        let wave = sine();
        let mut writes = wave_writes(0, &wave);
        writes.extend([
            ChipWrite::new(0, "k051649", 0, 0x100, 253),
            ChipWrite::new(0, "k051649", 0, 0x200, 15),
            ChipWrite::new(100, "k051649", 0, 0x300, 0x01),
            ChipWrite::new(200, "k051649", 0, 0x200, 8),
            // Inside the refit frame: the note's own pitch.
            ChipWrite::new(300, "k051649", 0, 0x100, 100),
            // Past the frame: a bend.
            ChipWrite::new(900, "k051649", 0, 0x100, 50),
            ChipWrite::new(5000, "k051649", 0, 0x300, 0x00),
        ]);
        let (notes, _offs, _waves, _warn) = collect_scc(&song(writes, false));
        let hit = &notes[0].hit;
        assert_eq!(hit.period, 100);
        assert_eq!(hit.freq_pts, vec![(900, 50)]);
        assert_eq!(hit.vol_pts, vec![(200, 8)]);
        assert_eq!(hit.off_sample, 5000);
    }

    #[test]
    fn wave_morph_bursts_collapse_to_settled_images() {
        let wave = sine();
        let other: Vec<u8> = wave.iter().map(|b| b.wrapping_add(3)).collect();
        let mut writes = wave_writes(0, &wave);
        writes.extend([
            ChipWrite::new(0, "k051649", 0, 0x100, 253),
            ChipWrite::new(0, "k051649", 0, 0x200, 15),
            ChipWrite::new(100, "k051649", 0, 0x300, 0x01),
        ]);
        // A whole-image rewrite after the refit frame, one sample apart.
        for (i, &b) in other.iter().enumerate() {
            writes.push(ChipWrite::new(
                1000 + i as i64,
                "k051649",
                0,
                i as u32,
                b as u32,
            ));
        }
        let (notes, _offs, _waves, _warn) = collect_scc(&song(writes, false));
        let pts = &notes[0].hit.wave_pts;
        assert_eq!(pts.len(), 1);
        assert_eq!(pts[0].0, 1000);
        assert_eq!(pts[0].1, other);
    }

    #[test]
    fn test_register_freezes_waves_and_reports_both_warnings() {
        let mut writes = vec![
            ChipWrite::new(0, "k051649", 0, 0x500, TEST_ROTATE as u32),
            // Frozen wave RAM: dropped.
            ChipWrite::new(10, "k051649", 0, 0x000, 0x55),
        ];
        let (_notes, _offs, _waves, warn) = collect_scc(&song(std::mem::take(&mut writes), false));
        assert_eq!(warn.len(), 1);
        assert!(warn[0].contains("1 SCC waveform writes were ignored"));

        let writes = vec![ChipWrite::new(20, "k051649", 0, 0x500, TEST_FREQ_4BIT as u32)];
        let (_notes, _offs, _waves, warn) = collect_scc(&song(writes, false));
        assert_eq!(warn.len(), 1);
        assert!(warn[0].contains("1 SCC test-register writes change the frequency"));
    }
}
