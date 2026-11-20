//! Konami K005289 (Bubble System WSG) -> notes -> Furnace.
//!
//! The chip is two wavetable voices.  The VGM command is ``0x42`` with a 4-bit
//! register offset and a 12-bit value:
//!
//! ```text
//! 0/1  control: volume (bits 0-3) and waveform bank (bits 5-7)
//! 2/3  latch the 12-bit pitch (LD)
//! 4/5  trigger the latched pitch (TG)
//! ```
//!
//! Each voice reads a 32-byte waveform from the 512-byte 4-bit PROM:
//! ``prom[voice * 0x100 + waveform * 0x20 + address]``.  The PROM arrives as a
//! 0xC3 RAM data block (a 2-byte address and then the bytes).
//!
//! The counter runs at the chip clock, so a voice sounds at
//! ``clock / (32 * (freq + 1))`` with ``freq = 0xFFF - register``.  Furnace's
//! WSG platform runs the same pitch table with a divider of 32 and writes
//! ``0x1000 - value``, so the source register round-trips exactly with a
//! ``period_offset`` of 1.
//!
//! Volume is the only gate: the chip has no key-on bit, so a note starts when
//! the control volume leaves zero and ends when it returns.  A TG that changes
//! the pitch splits the interval, exactly like the SCC's period writes.

use indexmap::IndexMap;

use crate::pitch::{furnace_period_note, module_clock};
use crate::vgm::VgmFile;

pub const K005289_VOICES: usize = 2;
pub const K005289_WAVE_LEN: usize = 32;
pub const K005289_WAVE_BANKS: i32 = 8;
pub const K005289_PROM_SIZE: usize = 0x200;
pub const K005289_DEFAULT_CLOCK: u32 = 3_579_545;
pub const K005289_DIVIDER: i32 = 32;
// A driver writes pitch and the control level within one VGM frame; a write
// this soon after the gate belongs to the note itself.
pub const K005289_REFIT_SAMPLES: i64 = 735;
// A driver that morphs the waveform rewrites several banks in one burst;
// writes this close together settle on one image.
pub const WAVE_BURST_SAMPLES: i64 = 64;
pub const WAVE_PTS_MAX: usize = 4096;
// Gates shorter than this are driver housekeeping, not music.
pub const K005289_MIN_NOTE: i64 = 100;

/// Voice frequency for a 12-bit period value (``0xFFF - register``).
pub fn k005289_rate(clock: u32, period: i32) -> f64 {
    let clock = if clock == 0 {
        K005289_DEFAULT_CLOCK
    } else {
        clock
    };
    clock as f64 / (K005289_DIVIDER as f64 * (period as f64 + 1.0))
}

/// Period value -> (Furnace note, E5 fine tune).
///
/// Furnace's WSG platform writes ``0x1000 - pitchTableValue``, and the chip
/// subtracts that from 0xFFF before counting, so the table has to produce
/// ``period + 1`` for the source's ``period``.
pub fn k005289_note(clock: u32, period: i32) -> (i32, i32) {
    furnace_period_note(module_clock(clock as f64), 32.0, period, 1, 0xFFF)
}

#[derive(Clone, Debug, PartialEq)]
pub struct K005289Hit {
    pub sample_time: i64,
    pub chip_id: i32,
    pub voice: i32,
    pub period: i32, // 0xFFF - pitch register at the note start
    pub vol: i32,    // 4-bit volume register
    pub wave: Vec<u8>, // 32 unsigned PROM bytes the voice plays
    pub off_sample: i64, // gate-off sample (0 = still sounding at the end)
    // Mid-note register writes while the voice sounds.
    pub freq_pts: Vec<(i64, i32)>,
    pub vol_pts: Vec<(i64, i32)>,
    // Every (sample, image) change of the 32 PROM bytes the voice plays after
    // the gate, one entry per settled rewrite burst.
    pub wave_pts: Vec<(i64, Vec<u8>)>,
}

impl Default for K005289Hit {
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
pub struct K005289Off {
    pub sample_time: i64,
    pub chip_id: i32,
    pub voice: i32,
}

impl Default for K005289Off {
    fn default() -> Self {
        Self {
            sample_time: 0,
            chip_id: 0,
            voice: 0,
        }
    }
}

/// A volume-gate interval of one voice, with everything it needs.
#[derive(Clone, Debug, PartialEq)]
pub struct K005289Note {
    pub hit: K005289Hit,
    pub end_sample: i64, // 0 = still sounding
    pub wave_index: usize,
}

impl Default for K005289Note {
    fn default() -> Self {
        Self {
            hit: K005289Hit::default(),
            end_sample: 0,
            wave_index: 0,
        }
    }
}

#[derive(Clone, Debug)]
struct K005289Chip {
    latch: [i32; K005289_VOICES],
    period: [i32; K005289_VOICES],
    vol: [i32; K005289_VOICES],
    waveform: [i32; K005289_VOICES],
    active: [Option<K005289Hit>; K005289_VOICES],
    gate_time: [i64; K005289_VOICES],
    // samples until which a pitch/wave rewrite counts as the note's own
    refit_until: [i64; K005289_VOICES],
}

fn blank_chip() -> K005289Chip {
    K005289Chip {
        latch: [0; K005289_VOICES],
        period: [0; K005289_VOICES],
        vol: [0; K005289_VOICES],
        waveform: [0; K005289_VOICES],
        active: std::array::from_fn(|_| None),
        gate_time: [0; K005289_VOICES],
        refit_until: [0; K005289_VOICES],
    }
}

/// The 512-byte PROM, stitched from its RAM-write slices.
fn prom_image(vgm: &VgmFile) -> Vec<u8> {
    let mut prom = vec![0u8; K005289_PROM_SIZE];
    for rom in &vgm.roms {
        if rom.kind != "k005289" || rom.data.is_empty() {
            continue;
        }
        let start = std::cmp::max(0, rom.start);
        if start >= K005289_PROM_SIZE as i64 {
            continue;
        }
        let start = start as usize;
        let len = std::cmp::min(rom.data.len(), K005289_PROM_SIZE - start);
        prom[start..start + len].copy_from_slice(&rom.data[..len]);
    }
    prom
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

/// The 32 PROM bytes one voice plays for a waveform bank.
fn image(prom: &[u8], voice: usize, bank: i32) -> Vec<u8> {
    let off = voice * 0x100 + (bank & (K005289_WAVE_BANKS - 1)) as usize * K005289_WAVE_LEN;
    prom[off..off + K005289_WAVE_LEN].to_vec()
}

/// End the voice's interval at `t` (the control volume reached zero).
#[allow(clippy::too_many_arguments)]
fn close(
    st: &mut K005289Chip,
    cid: i32,
    voice: usize,
    t: i64,
    notes: &mut Vec<K005289Note>,
    offs: &mut Vec<K005289Off>,
    waves: &mut IndexMap<Vec<u8>, usize>,
    acc: &mut IndexMap<&'static str, i64>,
) {
    let Some(mut hit) = st.active[voice].take() else {
        return;
    };
    hit.off_sample = t;
    if t - hit.sample_time >= K005289_MIN_NOTE {
        offs.push(K005289Off {
            sample_time: t,
            chip_id: cid,
            voice: voice as i32,
        });
        let idx = wave_index(waves, &hit.wave);
        notes.push(K005289Note {
            hit,
            end_sample: t,
            wave_index: idx,
        });
    } else {
        bump(acc, "glitch");
    }
    st.active[voice] = None;
}

/// Start the voice's interval from the latched pitch.
fn open_gate(st: &mut K005289Chip, cid: i32, voice: usize, t: i64, vol: i32, wave: Vec<u8>) {
    let hit = K005289Hit {
        sample_time: t,
        chip_id: cid,
        voice: voice as i32,
        period: st.period[voice],
        vol,
        wave,
        ..K005289Hit::default()
    };
    st.active[voice] = Some(hit);
    st.gate_time[voice] = t;
    st.refit_until[voice] = t + K005289_REFIT_SAMPLES;
}

fn record_wave(
    hit: &mut K005289Hit,
    t: i64,
    image_bytes: &[u8],
    acc: &mut IndexMap<&'static str, i64>,
) {
    if !image_bytes.iter().any(|&b| b != 0) {
        return;
    }
    if let Some((last_t, last_img)) = hit.wave_pts.last() {
        if t - *last_t <= WAVE_BURST_SAMPLES {
            // A whole-image rewrite is a burst of byte writes; only the
            // state it settles on is a step of the morph.
            if image_bytes != last_img.as_slice() {
                let at = hit.wave_pts.len() - 1;
                hit.wave_pts[at] = (*last_t, image_bytes.to_vec());
            }
            return;
        }
        if image_bytes == last_img.as_slice() {
            return;
        }
    } else if image_bytes == hit.wave.as_slice() {
        return;
    }
    if hit.wave_pts.len() >= WAVE_PTS_MAX {
        bump(acc, "wave_mid");
        return;
    }
    hit.wave_pts.push((t, image_bytes.to_vec()));
}

/// Walk the K005289 register stream into notes.
///
/// Returns (notes, key-offs, distinct waveforms, warnings).  Waveforms are
/// deduplicated so several channels or notes sharing one image share one
/// Furnace instrument.
pub fn collect_k005289(
    vgm: &VgmFile,
) -> (Vec<K005289Note>, Vec<K005289Off>, Vec<Vec<u8>>, Vec<String>) {
    let mut warnings: Vec<String> = Vec::new();
    let mut notes: Vec<K005289Note> = Vec::new();
    let mut offs: Vec<K005289Off> = Vec::new();
    let mut waves: IndexMap<Vec<u8>, usize> = IndexMap::new();
    let mut chips: Vec<i32> = vgm
        .writes
        .iter()
        .filter(|w| w.chip == "k005289")
        .map(|w| w.chip_id as i32)
        .collect();
    chips.sort_by(|a, b| a.cmp(b));
    chips.dedup();
    if chips.is_empty() {
        return (Vec::new(), Vec::new(), Vec::new(), warnings);
    }
    let prom = prom_image(vgm);
    if !prom.iter().any(|&b| b != 0) {
        // The parser already warns about the missing data block; there is no
        // waveform to play, so there is nothing to collect.
        return (Vec::new(), Vec::new(), Vec::new(), warnings);
    }
    let mut state: IndexMap<i32, K005289Chip> =
        chips.iter().map(|&cid| (cid, blank_chip())).collect();
    let mut acc: IndexMap<&'static str, i64> = IndexMap::new();

    for w in &vgm.writes {
        if w.chip != "k005289" {
            continue;
        }
        let cid = w.chip_id as i32;
        let st = state.entry(cid).or_insert_with(blank_chip);
        let off = w.reg & 0x0F;
        let val = (w.val & 0x0FFF) as i32;
        let voice = (off & 1) as usize;
        if off == 0 || off == 1 {
            let new_vol = val & 0x0F;
            let new_wave = (val >> 5) & (K005289_WAVE_BANKS - 1);
            let img = image(&prom, voice, new_wave);
            st.waveform[voice] = new_wave;
            if new_vol == 0 {
                if st.active[voice].is_some() {
                    close(st, cid, voice, w.sample, &mut notes, &mut offs, &mut waves, &mut acc);
                }
                st.vol[voice] = 0;
                continue;
            }
            if st.active[voice].is_none() {
                open_gate(st, cid, voice, w.sample, new_vol, img);
            } else if w.sample <= st.refit_until[voice] {
                // The opening control write (or a write within the same frame)
                // is the note's own level and waveform, not a macro step.
                let hit = st.active[voice].as_mut().unwrap();
                hit.vol = new_vol;
                hit.wave = img;
            } else {
                let hit = st.active[voice].as_mut().unwrap();
                if new_vol != st.vol[voice] {
                    hit.vol_pts.push((w.sample, new_vol));
                }
                record_wave(hit, w.sample, &img, &mut acc);
            }
            st.vol[voice] = new_vol;
        } else if off == 2 || off == 3 {
            st.latch[voice] = val & 0xFFF;
        } else if off == 4 || off == 5 {
            let period = 0xFFF - (st.latch[voice] & 0xFFF);
            st.period[voice] = period;
            if st.active[voice].is_none() {
                continue;
            }
            if w.sample <= st.refit_until[voice] {
                st.active[voice].as_mut().unwrap().period = period;
                continue;
            }
            let hit = st.active[voice].as_mut().unwrap();
            let cur = hit.freq_pts.last().map(|p| p.1).unwrap_or(hit.period);
            if cur != period {
                hit.freq_pts.push((w.sample, period));
            }
        }
    }

    // Close whatever is still sounding so the writer sees an open end.
    for st in state.values_mut() {
        for voice in 0..K005289_VOICES {
            if let Some(hit) = st.active[voice].take() {
                let idx = wave_index(&mut waves, &hit.wave);
                notes.push(K005289Note {
                    hit,
                    end_sample: 0,
                    wave_index: idx,
                });
                st.active[voice] = None;
            }
        }
    }

    if acc.get("glitch").copied().unwrap_or(0) != 0 {
        warnings.push(format!(
            "{} K005289 volume gate(s) shorter than {} samples were dropped",
            acc["glitch"], K005289_MIN_NOTE
        ));
    }
    if acc.get("wave_mid").copied().unwrap_or(0) != 0 {
        warnings.push(format!(
            "{} K005289 waveform changes past the {}-entry cap were dropped from a note's morph",
            acc["wave_mid"], WAVE_PTS_MAX
        ));
    }

    let mut wave_list = vec![Vec::new(); waves.len()];
    for (wave, idx) in &waves {
        wave_list[*idx] = wave.clone();
    }
    if std::env::var_os("VGM2FUR_DBG").is_some() {
        eprintln!("DBG k005289 hits={} offs={} waves={}", notes.len(), offs.len(), wave_list.len());
        for (i, n) in notes.iter().enumerate() {
            eprintln!("DBG hit {i} t={} period={} vol={} off={} freq_pts={} vol_pts={} wave_pts={}", n.hit.sample_time, n.hit.period, n.hit.vol, n.hit.off_sample, n.hit.freq_pts.len(), n.hit.vol_pts.len(), n.hit.wave_pts.len());
        }
    }
    (notes, offs, wave_list, warnings)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vgm::{ChipWrite, RomSlice};

    fn prom() -> Vec<u8> {
        (0..K005289_PROM_SIZE)
            .map(|i| ((i * 7 + 3) & 0xFF) as u8)
            .collect()
    }

    fn vgm_with(roms: Vec<RomSlice>, writes: Vec<ChipWrite>) -> VgmFile {
        VgmFile {
            roms,
            writes,
            k005289_clock: K005289_DEFAULT_CLOCK,
            ..VgmFile::default()
        }
    }

    fn prom_rom(data: Vec<u8>, start: i64) -> RomSlice {
        RomSlice {
            rom_size: data.len() as i64,
            start,
            data,
            kind: "k005289",
            ..RomSlice::default()
        }
    }

    fn ctrl(t: i64, voice: u32, vol: u32, bank: u32) -> ChipWrite {
        ChipWrite::new(t, "k005289", 0, voice, (vol & 0xF) | ((bank & 7) << 5))
    }

    #[test]
    fn rate_and_note_round_trip_through_the_pitch_table() {
        assert_eq!(
            k005289_rate(K005289_DEFAULT_CLOCK, 0x100),
            K005289_DEFAULT_CLOCK as f64 / (32.0 * 0x101 as f64)
        );
        // A zero clock falls back to the chip default.
        assert_eq!(
            k005289_rate(0, 0x100),
            k005289_rate(K005289_DEFAULT_CLOCK, 0x100)
        );

        let period = 0x400;
        let (note, e5) = k005289_note(K005289_DEFAULT_CLOCK, period);
        assert!((0..=179).contains(&note));
        let table = crate::pitch::pitch_table(
            module_clock(K005289_DEFAULT_CLOCK as f64),
            32.0,
            0xFFF,
        );
        assert_eq!(table.value(note, e5 - 0x80) - 1, period);
    }

    #[test]
    fn volume_gate_opens_and_closes_a_voice_with_its_bank_image() {
        let data = prom();
        let img = image(&data, 0, 1);
        let writes = vec![
            ctrl(0, 0, 8, 1),
            ctrl(500, 0, 9, 1),  // inside refit: the note's own level
            ctrl(800, 0, 10, 1), // past refit: a macro step
            ctrl(1000, 0, 0, 1), // gate off
            ctrl(2000, 0, 7, 2),
            ctrl(2050, 0, 0, 2),  // too short: glitch
            ctrl(3000, 1, 5, 1),  // voice 1 stays open to the end
        ];
        let vgm = vgm_with(vec![prom_rom(data, 0)], writes);
        let (notes, offs, waves, warnings) = collect_k005289(&vgm);

        assert_eq!(notes.len(), 2);
        let note = &notes[0];
        assert_eq!(note.hit.sample_time, 0);
        assert_eq!(note.hit.voice, 0);
        assert_eq!(note.hit.vol, 9);
        assert_eq!(note.hit.wave, img);
        assert_eq!(note.hit.off_sample, 1000);
        assert_eq!(note.end_sample, 1000);
        assert_eq!(note.hit.vol_pts, vec![(800, 10)]);
        assert_eq!(note.wave_index, 0);
        assert_eq!(offs.len(), 1);
        assert_eq!(
            offs[0],
            K005289Off {
                sample_time: 1000,
                chip_id: 0,
                voice: 0
            }
        );
        // The two PROM pages of this synthetic image are identical, so the
        // voices share one deduplicated waveform.
        assert_eq!(waves, vec![img.clone()]);
        assert_eq!(
            warnings,
            vec!["1 K005289 volume gate(s) shorter than 100 samples were dropped".to_string()]
        );

        // The voice still sounding when the log ends keeps an open end.
        assert_eq!(notes[1].end_sample, 0);
        assert_eq!(notes[1].hit.off_sample, 0);
        assert_eq!(notes[1].hit.voice, 1);
        assert_eq!(notes[1].hit.wave, image(&prom(), 1, 1));
        assert_eq!(notes[1].wave_index, 0);
    }

    #[test]
    fn refit_window_adopts_level_and_wave_then_records_morph_bursts() {
        let data = prom();
        let img2 = image(&data, 0, 2);
        let img3 = image(&data, 0, 3);
        let writes = vec![
            ctrl(0, 0, 8, 1),
            ctrl(100, 0, 5, 2), // inside 735 samples: note's own level/wave
            ctrl(900, 0, 6, 2), // macro step
            ctrl(1000, 0, 7, 2), // within the burst: settles on the same image
            ctrl(2000, 0, 8, 3),
            ctrl(3000, 0, 0, 3),
        ];
        let vgm = vgm_with(vec![prom_rom(data, 0)], writes);
        let (notes, offs, waves, warnings) = collect_k005289(&vgm);

        assert_eq!(notes.len(), 1);
        let hit = &notes[0].hit;
        assert_eq!(hit.vol, 5);
        assert_eq!(hit.wave, img2);
        assert_eq!(hit.off_sample, 3000);
        assert_eq!(hit.vol_pts, vec![(900, 6), (1000, 7), (2000, 8)]);
        assert_eq!(hit.wave_pts, vec![(2000, img3)]);
        assert_eq!(notes[0].wave_index, 0);
        assert_eq!(waves, vec![img2]);
        assert_eq!(offs.len(), 1);
        assert!(warnings.is_empty());
    }

    #[test]
    fn latched_pitch_triggers_split_the_interval() {
        let data = prom();
        let writes = vec![
            ctrl(0, 1, 6, 0),
            ChipWrite::new(10, "k005289", 0, 3, 0x100), // LD voice 1
            ChipWrite::new(20, "k005289", 0, 5, 0),     // TG, inside refit
            ChipWrite::new(1000, "k005289", 0, 3, 0x200),
            ChipWrite::new(2000, "k005289", 0, 5, 0), // splits the interval
            ChipWrite::new(4000, "k005289", 0, 5, 0), // same pitch: no entry
            ctrl(5000, 1, 0, 0),
        ];
        let vgm = vgm_with(vec![prom_rom(data, 0)], writes);
        let (notes, offs, _, warnings) = collect_k005289(&vgm);

        assert_eq!(notes.len(), 1);
        let hit = &notes[0].hit;
        assert_eq!(hit.voice, 1);
        assert_eq!(hit.period, 0xFFF - 0x100);
        assert_eq!(hit.freq_pts, vec![(2000, 0xFFF - 0x200)]);
        assert_eq!(notes[0].end_sample, 5000);
        assert_eq!(offs[0].voice, 1);
        // Voice 1 reads the second PROM page; voice 0's page is untouched.
        assert_eq!(hit.wave, image(&prom(), 1, 0));
        assert!(warnings.is_empty());
    }

    #[test]
    fn prom_slices_stitch_with_clamping_and_zero_prom_collects_nothing() {
        let mut first = vec![0u8; 0x20];
        first[0] = 0x11;
        first[0x1F] = 0x22;
        // A negative start clamps to 0; a slice past the end truncates.
        let roms = vec![
            prom_rom(first, -5),
            prom_rom(vec![0x33; 0x20], 0x1F0),
            prom_rom(vec![0x44; 0x20], 0x400),
        ];
        let vgm = vgm_with(
            roms.clone(),
            vec![ctrl(0, 0, 4, 0), ctrl(1000, 0, 0, 0)],
        );
        let (notes, _, waves, _) = collect_k005289(&vgm);
        assert_eq!(notes.len(), 1);
        let img = &notes[0].hit.wave;
        assert_eq!(img.len(), 32);
        assert_eq!(img[0], 0x11);
        assert_eq!(img[0x1F], 0x22);
        assert_eq!(waves.len(), 1);

        // A PROM that is entirely zero is a parser-level warning; nothing is
        // collected even when the volume gate is written.
        let empty = vgm_with(
            vec![prom_rom(vec![0u8; 0x20], 0)],
            vec![ctrl(0, 0, 4, 0)],
        );
        let (notes, offs, waves, warnings) = collect_k005289(&empty);
        assert!(notes.is_empty());
        assert!(offs.is_empty());
        assert!(waves.is_empty());
        assert!(warnings.is_empty());

        // No K005289 write at all short-circuits before the PROM is read.
        let other = vgm_with(
            roms,
            vec![ChipWrite::new(0, "k051649", 0, 0, 0x12)],
        );
        let (notes, offs, waves, warnings) = collect_k005289(&other);
        assert!(notes.is_empty());
        assert!(offs.is_empty());
        assert!(waves.is_empty());
        assert!(warnings.is_empty());
    }
}
