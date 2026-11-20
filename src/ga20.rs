//! Irem GA20 ROM -> signed 8-bit sample bodies for Furnace.
//!
//! The GA20 is a four-voice sample player.  Each voice has a 20-bit start and
//! end address (both carry the low nibble zero), an 8-bit frequency register
//! and an 8-bit volume register.  Playback advances until the ROM byte is
//! zero; the end register is a bound rather than a loop point.  Furnace's GA20
//! platform re-packs the bodies and appends its own terminator, so this
//! collector only extracts the audible span.
//!
//! VGM writes command 0xBF as (register bit 7 selects the second chip,
//! register, data).  The clock is header 0xE0 for VGM 1.71+ and the ROM is
//! data block 0x93.

use std::collections::{BTreeSet, HashSet};
use std::sync::{Arc, LazyLock, Mutex};

use indexmap::IndexMap;

use crate::parity::{py_int, py_round};
pub use crate::vgm::GA20_DEFAULT_CLOCK;
use crate::vgm::{assemble_rom, VgmFile};

pub const GA20_VOICES: usize = 4;
pub const GA20_MAX_BODY: i64 = 1_048_576;
/// Furnace's GA20 pitch table plays note 108 at this period (register 0x95 for
/// a COLOR_NTSC clock).  The platform ignores a sample's center rate, so every
/// GA20 body uses the same C-4 reference.
pub const GA20_C4_DIVISOR: u32 = 428;

/// Playback rate for one GA20 frequency register value.
pub fn ga20_rate(clock: u32, freq: i32) -> f64 {
    let divider = (0x100 - (freq & 0xFF)) * 4;
    if divider <= 0 {
        return 0.0;
    }
    let clock = if clock == 0 {
        GA20_DEFAULT_CLOCK
    } else {
        clock
    };
    (clock as f64) / (divider as f64)
}

/// Furnace's GA20 playback rate at note 108 (C-4).
pub fn ga20_c4_rate(clock: u32) -> i64 {
    let clock = if clock == 0 {
        GA20_DEFAULT_CLOCK
    } else {
        clock
    };
    py_int(py_round((clock as f64) / (GA20_C4_DIVISOR as f64))).max(1000)
}

/// The 13-entry period table Furnace builds for the GA20 clock.
pub fn ga20_pitch_table(clock: u32) -> (Vec<i64>, Vec<i64>, i32) {
    let tuning = 440.0f64;
    let clock = if clock == 0 {
        GA20_DEFAULT_CLOCK
    } else {
        clock
    };
    let mut shift = 0i32;
    while shift < 14 {
        let nbase = (shift - 4) * 12;
        let fbase = (tuning * 0.0625) * 2.0f64.powf(((nbase + 3) as f64) / 12.0);
        if py_round((clock as f64 / fbase) / 64.0) <= 0x100 as f64 {
            break;
        }
        shift += 1;
    }
    let mut pitch: Vec<i64> = Vec::with_capacity(13);
    for i in 0..13 {
        let nbase = i + (shift - 5) * 12;
        let fbase = (tuning * 0.0625) * 2.0f64.powf(((nbase + 3) as f64) / 12.0);
        pitch.push(py_int(py_round((clock as f64 / fbase) / 64.0)));
    }
    let mut diff: Vec<i64> = Vec::with_capacity(13);
    for i in 0..12 {
        diff.push(pitch[i + 1] - pitch[i]);
    }
    diff.push(0);
    (pitch, diff, shift)
}

pub fn ga20_period(note: i32, fine: i32, pitch: &[i64], diff: &[i64], shift: i32) -> i32 {
    let offset = (note << 7) + fine;
    let coarse = 0.max(offset >> 7);
    let frac = offset & 127;
    let index = (coarse % 12) as usize;
    let octave = (coarse / 12) - shift;
    let mut root = pitch[index];
    let mut delta = diff[index];
    if octave > 0 {
        root = (root + (1i64 << (octave - 1))) >> octave;
        delta = (delta + (1i64 << (octave - 1))) >> octave;
    } else if octave < 0 {
        return 0x100;
    }
    (root + ((delta * frac as i64) >> 7)) as i32
}

type Ga20NoteMap = IndexMap<i32, (i32, i32)>;

/// period -> (note, E5) for every reachable GA20 pitch value.
///
/// The reference memoizes this with `lru_cache(maxsize=8)`.
fn ga20_note_map(clock: u32) -> Arc<Ga20NoteMap> {
    static CACHE: LazyLock<Mutex<Vec<(u32, Arc<Ga20NoteMap>)>>> =
        LazyLock::new(|| Mutex::new(Vec::new()));
    let mut cache = CACHE.lock().unwrap();
    if let Some(pos) = cache.iter().position(|(c, _)| *c == clock) {
        let entry = cache.remove(pos);
        let map = entry.1.clone();
        cache.push(entry);
        return map;
    }
    let map = Arc::new(build_ga20_note_map(clock));
    if cache.len() >= 8 {
        cache.remove(0);
    }
    cache.push((clock, map.clone()));
    map
}

fn build_ga20_note_map(clock: u32) -> Ga20NoteMap {
    let (pitch, diff, shift) = ga20_pitch_table(clock);
    let mut out: Ga20NoteMap = IndexMap::new();
    for note in 0..180 {
        for e5 in 0..256 {
            let period = ga20_period(note, e5 - 0x80, &pitch, &diff, shift);
            if 1 <= period && period <= 0x100 && !out.contains_key(&period) {
                out.insert(period, (note, e5));
            }
        }
    }
    out
}

#[derive(Clone, Debug, PartialEq, Default)]
pub struct Ga20Hit {
    pub sample_time: i64,
    pub chip_id: i32,
    pub voice: usize,
    pub start: i32,
    pub end: i32,
    pub freq: i32,
    pub volume: i32,
    pub off_sample: i64,
    pub vol_pts: Vec<(i64, i32)>,
    pub freq_pts: Vec<(i64, i32)>,
}

impl Ga20Hit {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        sample_time: i64,
        chip_id: i32,
        voice: usize,
        start: i32,
        end: i32,
        freq: i32,
        volume: i32,
    ) -> Self {
        Ga20Hit {
            sample_time,
            chip_id,
            voice,
            start,
            end,
            freq,
            volume,
            off_sample: 0,
            vol_pts: Vec::new(),
            freq_pts: Vec::new(),
        }
    }

    /// The key-on value supplies the note; later writes are pitch motion.
    pub fn played_rate(&self, clock: u32) -> f64 {
        ga20_rate(clock, self.freq)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Ga20Off {
    pub sample_time: i64,
    pub chip_id: i32,
    pub voice: usize,
}

#[derive(Clone, Debug, PartialEq, Default)]
pub struct Ga20Sample {
    pub chip_id: i32,
    pub start: i32,
    pub data: Vec<u8>,
    pub hits: Vec<Ga20Hit>,
}

impl Ga20Sample {
    pub fn length(&self) -> usize {
        self.data.len()
    }

    pub fn mode_freq(&self) -> i32 {
        let mut freqs: IndexMap<i32, i64> = IndexMap::new();
        for h in &self.hits {
            if h.freq != 0 {
                *freqs.entry(h.freq).or_insert(0) += 1;
            }
        }
        let mut best: Option<(i32, i64)> = None;
        for (&f, &n) in freqs.iter() {
            match best {
                Some((_, bn)) if n <= bn => {}
                _ => best = Some((f, n)),
            }
        }
        best.map(|(f, _)| f).unwrap_or(0)
    }

    /// Furnace's GA20 pitch table does not depend on the body's center rate;
    /// the writer stores the platform's C-4 reference for every body.
    pub fn c4_rate(&self, clock: u32) -> i64 {
        ga20_c4_rate(clock)
    }
}

fn covered(vgm: &VgmFile, chip_id: i32, addr: i32) -> bool {
    for s in &vgm.roms {
        if s.chip as i32 == chip_id && s.kind == "ga20" && !s.data.is_empty() {
            let start = s.start as i64;
            if start <= addr as i64 && (addr as i64) < start + s.data.len() as i64 {
                return true;
            }
        }
    }
    false
}

fn body(rom: &[u8], start: i32, end: i32, warnings: &mut Vec<String>, cid: i32) -> Vec<u8> {
    // MAME/libvgm stop on the zero byte; the end register is not used during
    // playback (games routinely write it past the real sample end).
    let start_i = start as i64;
    let limit = (rom.len() as i64).min(start_i + GA20_MAX_BODY);
    let found = if start_i >= 0 && start_i < limit {
        rom[start_i as usize..limit as usize]
            .iter()
            .position(|&b| b == 0)
            .map(|pos| start_i + pos as i64)
    } else {
        None
    };
    let mut stop = match found {
        Some(stop) => stop,
        None => -1,
    };
    if stop < 0 {
        warnings.push(format!(
            "GA20 {cid}: sample at {start:05x} has no zero terminator; the body was capped"
        ));
        stop = (rom.len() as i64).min(start_i + GA20_MAX_BODY);
        if end > start {
            stop = stop.min(end as i64);
        }
    }
    if stop <= start_i {
        return Vec::new();
    }
    // The ROM stores unsigned 8-bit samples with 0x80 as silence.  Furnace's
    // SMP2 body is signed; its GA20 uploader XORs 0x80 again.
    rom[start_i as usize..stop as usize]
        .iter()
        .map(|&value| ((value as i32 - 0x80) & 0xFF) as u8)
        .collect()
}

pub fn collect_ga20(vgm: &VgmFile) -> (Vec<Ga20Sample>, Vec<Ga20Off>, Vec<String>) {
    let ids: BTreeSet<i32> = vgm
        .writes
        .iter()
        .filter(|w| w.chip == "ga20")
        .map(|w| w.chip_id as i32)
        .collect();
    let mut warnings: Vec<String> = Vec::new();
    let mut offs: Vec<Ga20Off> = Vec::new();
    if ids.is_empty() {
        return (Vec::new(), offs, warnings);
    }
    let mut roms: IndexMap<i32, Vec<u8>> = IndexMap::new();
    for &cid in &ids {
        roms.insert(
            cid,
            assemble_rom(vgm, "ga20", cid as u8, 0xFF, false, false),
        );
    }
    if !roms.values().any(|r| !r.is_empty()) {
        warnings.push(
            "no GA20 ROM block (0x93); PCM notes will be silent until samples are added"
                .to_string(),
        );
        return (Vec::new(), offs, warnings);
    }

    let mut regs = [[0i32; 8]; GA20_VOICES];
    let mut keyed = [false; GA20_VOICES];
    let mut active: Vec<Option<usize>> = vec![None; GA20_VOICES];
    let mut hits: Vec<Ga20Hit> = Vec::new();
    for w in &vgm.writes {
        if w.chip != "ga20" {
            continue;
        }
        let voice_i = (w.reg >> 3) as i64;
        let reg = (w.reg & 0x07) as usize;
        if !(0..GA20_VOICES as i64).contains(&voice_i) {
            continue;
        }
        let voice = voice_i as usize;
        let val = (w.val as i32) & 0xFF;
        regs[voice][reg] = val;
        if reg == 0x06 {
            if (val & 0x02) != 0 {
                if let Some(idx) = active[voice] {
                    hits[idx].off_sample = w.sample;
                }
                let hit = Ga20Hit::new(
                    w.sample,
                    w.chip_id as i32,
                    voice,
                    ((regs[voice][0x00] | (regs[voice][0x01] << 8)) << 4) as i32,
                    ((regs[voice][0x02] | (regs[voice][0x03] << 8)) << 4) as i32,
                    regs[voice][0x04],
                    regs[voice][0x05],
                );
                hits.push(hit);
                active[voice] = Some(hits.len() - 1);
                keyed[voice] = true;
            } else {
                if keyed[voice] {
                    offs.push(Ga20Off {
                        sample_time: w.sample,
                        chip_id: w.chip_id as i32,
                        voice,
                    });
                    if let Some(idx) = active[voice].take() {
                        hits[idx].off_sample = w.sample;
                    }
                }
                keyed[voice] = false;
            }
            continue;
        }
        let idx = match active[voice] {
            Some(idx) if keyed[voice] => idx,
            _ => continue,
        };
        if reg == 0x04 && val != hits[idx].freq {
            hits[idx].freq_pts.push((w.sample, val));
        } else if reg == 0x05 && val != hits[idx].volume {
            hits[idx].vol_pts.push((w.sample, val));
        }
    }

    let mut by_key: IndexMap<(i32, i32, Vec<u8>), Ga20Sample> = IndexMap::new();
    let mut undumped = 0i64;
    for h in hits {
        let empty: &[u8] = &[];
        let rom = roms.get(&h.chip_id).map(Vec::as_slice).unwrap_or(empty);
        if rom.is_empty()
            || h.start as i64 >= rom.len() as i64
            || !covered(vgm, h.chip_id, h.start)
        {
            undumped += 1;
            continue;
        }
        let sample_body = body(rom, h.start, h.end, &mut warnings, h.chip_id);
        if sample_body.is_empty() {
            continue;
        }
        match by_key.entry((h.chip_id, h.start, sample_body)) {
            indexmap::map::Entry::Occupied(mut entry) => entry.get_mut().hits.push(h),
            indexmap::map::Entry::Vacant(entry) => {
                let data = entry.key().2.clone();
                entry.insert(Ga20Sample {
                    chip_id: h.chip_id,
                    start: h.start,
                    data,
                    hits: vec![h],
                });
            }
        }
    }
    if undumped != 0 {
        warnings.push(format!(
            "{undumped} GA20 triggers point at ROM that the VGM does not dump; those notes are silent"
        ));
    }
    let mut samples: Vec<Ga20Sample> = by_key.into_values().collect();
    samples.sort_by(|a, b| (a.chip_id, a.start).cmp(&(b.chip_id, b.start)));
    let mut seen: HashSet<String> = HashSet::new();
    warnings.retain(|w| seen.insert(w.clone()));
    (samples, offs, warnings)
}

/// (Furnace note, E5) that makes the platform write the source register.
///
/// Furnace's GA20 period table is coarser than equal temperament, so a simple
/// semitone is not enough.  The source frequency register itself gives the
/// period Furnace must load (`0x100 - freq`); the map returns the closest
/// reachable note/E5 pair.
pub fn ga20_note(hit: &Ga20Hit, _sample: &Ga20Sample, clock: u32) -> (i32, i32) {
    let target = 0x100 - (hit.freq & 0xFF);
    let table = ga20_note_map(clock);
    if let Some(&pair) = table.get(&target) {
        return pair;
    }
    let mut nearest: Option<i32> = None;
    let mut nearest_dist = i32::MAX;
    for &period in table.keys() {
        let dist = (period - target).abs();
        if dist < nearest_dist {
            nearest_dist = dist;
            nearest = Some(period);
        }
    }
    let nearest = nearest.expect("GA20 note map is never empty");
    table[&nearest]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rate_uses_inverted_frequency_and_default_clock() {
        assert_eq!(ga20_rate(3_579_545, 0x00), 3_579_545.0 / 1024.0);
        assert_eq!(ga20_rate(3_579_545, 0xFF), 894_886.25);
        assert_eq!(ga20_rate(3_579_545, 0x1FF), ga20_rate(3_579_545, 0xFF));
        assert_eq!(ga20_rate(0, 0x80), 6_991.298828125);
        assert_eq!(ga20_c4_rate(3_579_545), 8363);
        assert_eq!(ga20_c4_rate(0), 8363);
        assert_eq!(ga20_c4_rate(8_000_000), 18_692);
    }

    #[test]
    fn pitch_table_and_note_map_match_furnace() {
        let (pitch, diff, shift) = ga20_pitch_table(3_579_545);
        assert_eq!(
            pitch,
            vec![428, 404, 381, 360, 339, 320, 302, 285, 269, 254, 240, 226, 214]
        );
        assert_eq!(diff, vec![-24, -23, -21, -21, -19, -18, -17, -16, -15, -14, -14, -12, 0]);
        assert_eq!(shift, 7);
        assert_eq!(ga20_pitch_table(0).2, 7);
        assert_eq!(ga20_period(108, 0, &pitch, &diff, shift), 107);
        assert_eq!(ga20_period(108, 127, &pitch, &diff, shift), 101);

        let table = ga20_note_map(3_579_545);
        assert_eq!(table.len(), 255);
        assert_eq!(table.get(&256), Some(&(0, 0)));
        let mut hit = Ga20Hit::default();
        let sample = Ga20Sample::default();
        hit.freq = 0x95;
        assert_eq!(ga20_note(&hit, &sample, 3_579_545), (107, 235));
        hit.freq = 0x00;
        assert_eq!(ga20_note(&hit, &sample, 3_579_545), (0, 0));
        // 0xFF targets period 1, which the table does not reach; the nearest
        // period is 2.
        hit.freq = 0xFF;
        assert_eq!(ga20_note(&hit, &sample, 3_579_545), (174, 128));
    }

    #[test]
    fn body_stops_at_zero_and_sign_flips_rom_bytes() {
        let rom = [0x80, 0x81, 0x00, 0x82];
        let mut warnings = Vec::new();
        assert_eq!(body(&rom, 0, 0, &mut warnings, 0), vec![0x00, 0x01]);
        assert!(warnings.is_empty());

        let rom = [0x7F, 0x80, 0x81];
        let mut warnings = Vec::new();
        assert_eq!(
            body(&rom, 0, 0, &mut warnings, 0),
            vec![0xFF, 0x00, 0x01]
        );
        assert_eq!(
            warnings,
            vec!["GA20 0: sample at 00000 has no zero terminator; the body was capped"]
        );

        let mut warnings = Vec::new();
        assert_eq!(body(&rom, 0, 2, &mut warnings, 0), vec![0xFF, 0x00]);
    }

    #[test]
    fn collect_extracts_bodies_and_register_points() {
        use crate::vgm::{ChipWrite, RomSlice, VgmFile};

        fn write(vgm: &mut VgmFile, t: i64, reg: u32, val: u32) {
            vgm.writes.push(ChipWrite {
                sample: t,
                chip: "ga20".into(),
                chip_id: 0,
                reg,
                val,
            });
        }

        let mut rom = vec![0x80u8; 40];
        rom[0..3].copy_from_slice(&[0x80, 0x81, 0x00]);
        rom[16..19].copy_from_slice(&[0x90, 0x91, 0x00]);
        rom[32..34].copy_from_slice(&[0xA0, 0x00]);
        let mut vgm = VgmFile::default();
        vgm.roms.push(RomSlice {
            chip: 0,
            rom_size: 0,
            start: 0,
            data: rom,
            kind: "ga20",
            time: 0,
        });
        write(&mut vgm, 0, 0x00, 0x00);
        write(&mut vgm, 2, 0x04, 0x40);
        write(&mut vgm, 3, 0x05, 10);
        write(&mut vgm, 4, 0x06, 2);
        write(&mut vgm, 5, 0x04, 0x50);
        write(&mut vgm, 6, 0x05, 11);
        write(&mut vgm, 8, 0x06, 2);
        write(&mut vgm, 9, 0x06, 0);
        write(&mut vgm, 10, 0x00, 0x01);
        write(&mut vgm, 11, 0x04, 0x20);
        write(&mut vgm, 12, 0x05, 5);
        write(&mut vgm, 13, 0x06, 2);
        write(&mut vgm, 14, 0x05, 6);
        write(&mut vgm, 15, 0x06, 0);
        write(&mut vgm, 16, 0x00, 0x02);
        write(&mut vgm, 17, 0x04, 0);
        write(&mut vgm, 18, 0x06, 2);

        let (samples, offs, warnings) = collect_ga20(&vgm);
        assert!(warnings.is_empty());
        assert_eq!(offs.len(), 2);
        assert_eq!(
            offs[0],
            Ga20Off {
                sample_time: 9,
                chip_id: 0,
                voice: 0
            }
        );
        assert_eq!(
            offs[1],
            Ga20Off {
                sample_time: 15,
                chip_id: 0,
                voice: 0
            }
        );
        assert_eq!(samples.len(), 3);
        assert_eq!((samples[0].start, samples[0].data.clone()), (0, vec![0, 1]));
        assert_eq!(samples[0].mode_freq(), 64);
        assert_eq!(samples[0].hits.len(), 2);
        let hit = &samples[0].hits[0];
        assert_eq!(
            (
                hit.sample_time,
                hit.voice,
                hit.start,
                hit.end,
                hit.freq,
                hit.volume,
                hit.off_sample
            ),
            (4, 0, 0, 0, 64, 10, 8)
        );
        assert_eq!(hit.vol_pts, vec![(6, 11)]);
        assert_eq!(hit.freq_pts, vec![(5, 80)]);
        assert_eq!(
            (
                samples[0].hits[1].sample_time,
                samples[0].hits[1].freq,
                samples[0].hits[1].volume,
                samples[0].hits[1].off_sample
            ),
            (8, 80, 11, 9)
        );
        assert_eq!(
            (samples[1].start, samples[1].data.clone()),
            (16, vec![16, 17])
        );
        assert_eq!(
            (
                samples[1].hits[0].sample_time,
                samples[1].hits[0].freq,
                samples[1].hits[0].volume,
                samples[1].hits[0].off_sample
            ),
            (13, 32, 5, 15)
        );
        assert_eq!(samples[1].hits[0].vol_pts, vec![(14, 6)]);
        assert_eq!((samples[2].start, samples[2].data.clone()), (32, vec![32]));
        assert_eq!(
            (
                samples[2].hits[0].freq,
                samples[2].hits[0].volume,
                samples[2].hits[0].off_sample
            ),
            (0, 6, 0)
        );
        assert_eq!(ga20_note(hit, &samples[0], 0), (97, 233));
    }

    #[test]
    fn mode_freq_picks_the_first_most_common_nonzero_freq() {
        let mut sample = Ga20Sample::default();
        assert_eq!(sample.mode_freq(), 0);
        sample.hits.push(Ga20Hit::new(0, 0, 0, 0, 0, 5, 0));
        sample.hits.push(Ga20Hit::new(1, 0, 0, 0, 0, 7, 0));
        sample.hits.push(Ga20Hit::new(2, 0, 0, 0, 0, 7, 0));
        sample.hits.push(Ga20Hit::new(3, 0, 0, 0, 0, 0, 0));
        assert_eq!(sample.mode_freq(), 7);
        assert_eq!(sample.length(), 0);
        assert_eq!(sample.c4_rate(3_579_545), 8363);
        // Counter.most_common keeps the first inserted value on a tie.
        let mut tie = Ga20Sample::default();
        tie.hits.push(Ga20Hit::new(0, 0, 0, 0, 0, 5, 0));
        tie.hits.push(Ga20Hit::new(1, 0, 0, 0, 0, 7, 0));
        assert_eq!(tie.mode_freq(), 5);
    }
}
