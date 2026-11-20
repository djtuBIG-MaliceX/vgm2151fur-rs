//! Konami K053260 ROM -> samples -> Furnace.
//!
//! Register model follows Furnace's vgsound_emu core (the core the module
//! plays on). Four voices, eight registers each at 0x08 + voice*8:
//!
//! ```text
//! 0/1 pitch (12-bit)   2/3 length (16-bit)   4/5/6 start (21-bit)
//! 7 volume (7-bit)
//! ```
//!
//! 0x28 key on/off in bits 0-3 and reverse in bits 4-7; 0x2A loop in bits 0-3
//! and KADPCM in bits 4-7; 0x2C/0x2D pan, three bits per voice.
//!
//! The chip reads `clock / (4096 - pitch)` source steps a second. A PCM step
//! plays one byte; an ADPCM step plays one nibble from the byte fetched every
//! two steps. Key-on pre-increments the address. The byte at `start` is never
//! played. A loop resets the address to `start` and replays the whole
//! `length` again: there is no separate loop point.
//!
//! KADPCM is the 16-entry delta table, low nibble first. Furnace stores it as
//! sample depth 7 and decodes the same way. A sample that loops is marked
//! loopable with the loop covering the whole body, which is all the chip can
//! do. Reverse playback is stored as a forward body, which keeps Furnace's
//! sample direction forward.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use indexmap::IndexMap;

use crate::parity::{py_int, py_round};
use crate::vgm::{assemble_rom, VgmFile, VGM_RATE};

pub const K053260_VOICES: usize = 4;
pub const K053260_DEFAULT_CLOCK: u32 = 3_579_545;
pub const K053260_REFIT_SAMPLES: i64 = 735;
pub const K053260_MAX_ADDR: i32 = 0x1FFFFF;
pub const K053260_DEFAULT_PAN: i32 = 4;

pub const KADPCM: [i32; 16] = [
    0, 1, 2, 4, 8, 16, 32, 64, -128, -64, -32, -16, -8, -4, -2, -1,
];
pub const PAN_LUT: [(i32, i32); 8] = [
    (0x00, 0x00),
    (0x7F, 0x00),
    (0x74, 0x34),
    (0x68, 0x49),
    (0x5A, 0x5A),
    (0x49, 0x68),
    (0x34, 0x74),
    (0x00, 0x7F),
];

/// Width of one key-on register burst; the default of Python's `merge_step_writes`.
const STEP_GAP: i64 = 16;

/// Source steps a second for a 12-bit pitch register.
pub fn k053260_rate(clock: u32, pitch: i32) -> f64 {
    let mut period = 4096 - (pitch & 0xFFF);
    if period <= 0 {
        period = 1;
    }
    if clock == 0 {
        K053260_DEFAULT_CLOCK as f64 / period as f64
    } else {
        clock as f64 / period as f64
    }
}

/// Chip pan position -> Furnace 08xy split pan (15 = loud side).
pub fn k053260_pan_fx(pan: i32) -> i32 {
    let (left, right) = PAN_LUT[(pan & 7) as usize];
    let peak = left.max(right);
    if peak <= 0 {
        return 0;
    }
    let lv = (left * 15 + peak / 2) / peak;
    let rv = (right * 15 + peak / 2) / peak;
    ((lv & 15) << 4) | (rv & 15)
}

/// Keep the last write of each cluster closer than `gap` samples.
pub fn merge_step_writes(pts: &[(i64, i32)], gap: i64) -> Vec<(i64, i32)> {
    let mut merged: Vec<(i64, i32)> = Vec::new();
    for &p in pts {
        if let Some(last) = merged.last_mut() {
            if p.0 - last.0 <= gap {
                *last = p;
                continue;
            }
        }
        merged.push(p);
    }
    merged
}

/// Pitch the hit actually plays at.
///
/// A pitch write just after key-on is the note's own pitch, not a bend: some
/// drivers key the voice before its registers are set. The first write inside
/// the frame replaces a stale key-on value.
pub fn hit_played_pitch(hit: &K053260Hit) -> i32 {
    let steps = merge_step_writes(&hit.freq_pts, STEP_GAP);
    if let Some(&first) = steps.first() {
        if first.1 != 0
            && (first.0 - hit.sample_time <= K053260_REFIT_SAMPLES
                || (hit.pitch & 0xFFF) == 0)
        {
            return first.1;
        }
    }
    hit.pitch & 0xFFF
}

/// (volume, pan) at the note attack, using the first in-frame write.
pub fn hit_played_volume(hit: &K053260Hit) -> (i32, i32) {
    let mut vol = hit.volume;
    let mut pan = hit.pan;
    for &(t, v, p) in &hit.state_pts {
        if t - hit.sample_time > K053260_REFIT_SAMPLES {
            break;
        }
        vol = v;
        pan = p;
    }
    (vol, pan)
}

/// Source steps the voice advances from key-on until `end_sample`.
pub fn hit_played_steps(hit: &K053260Hit, end_sample: i64, clock: u32) -> f64 {
    if end_sample <= hit.sample_time {
        return 0.0;
    }
    let mut freq = hit.pitch & 0xFFF;
    let mut prev = hit.sample_time;
    let mut total = 0.0;
    for &(t, f) in &hit.freq_pts {
        if t <= prev {
            freq = f;
            continue;
        }
        if t >= end_sample {
            break;
        }
        total += k053260_rate(clock, freq) * (t - prev) as f64 / VGM_RATE as f64;
        freq = f;
        prev = t;
    }
    total += k053260_rate(clock, freq) * (end_sample - prev) as f64 / VGM_RATE as f64;
    total
}

#[derive(Clone, Debug, PartialEq)]
pub struct K053260Hit {
    pub sample_time: i64,
    pub chip_id: i32,
    pub voice: usize,
    pub start: i32,
    pub length: i32,
    pub pitch: i32,
    pub volume: i32,
    pub pan: i32,
    pub r#loop: bool,
    pub adpcm: bool,
    pub reverse: bool,
    pub off_sample: i64,
    // Changes while keyed: (sample, volume, pan). The attack values are not
    // repeated here.
    pub state_pts: Vec<(i64, i32, i32)>,
    pub freq_pts: Vec<(i64, i32)>,
    pub last_state: (i32, i32),
}

impl Default for K053260Hit {
    fn default() -> Self {
        Self {
            sample_time: 0,
            chip_id: 0,
            voice: 0,
            start: 0,
            length: 0,
            pitch: 0,
            volume: 0,
            pan: 0,
            r#loop: false,
            adpcm: false,
            reverse: false,
            off_sample: 0,
            state_pts: Vec::new(),
            freq_pts: Vec::new(),
            last_state: (0, K053260_DEFAULT_PAN),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct K053260Off {
    pub sample_time: i64,
    pub chip_id: i32,
    pub voice: usize,
}

#[derive(Clone, Debug, PartialEq)]
pub struct K053260Sample {
    pub start: i32,
    pub data: Vec<u8>,
    pub hits: Vec<K053260Hit>,
    pub loop_start: Option<i64>,
    pub loop_end: Option<i64>,
    /// 8 = PCM, 7 = K053260 ADPCM.
    pub depth: i32,
    pub reverse: bool,
}

impl Default for K053260Sample {
    fn default() -> Self {
        Self {
            start: 0,
            data: Vec::new(),
            hits: Vec::new(),
            loop_start: None,
            loop_end: None,
            depth: 8,
            reverse: false,
        }
    }
}

impl K053260Sample {
    /// Played frames: one per PCM byte, two per ADPCM byte.
    pub fn length(&self) -> usize {
        if self.depth == 7 {
            self.data.len() * 2
        } else {
            self.data.len()
        }
    }

    pub fn mode_pitch(&self) -> i32 {
        if self.hits.is_empty() {
            return 0;
        }
        // Counter.most_common(1): highest count, first-seen on ties.
        let mut freqs: IndexMap<i32, i64> = IndexMap::new();
        for h in &self.hits {
            let f = hit_played_pitch(h);
            if f != 0 {
                *freqs.entry(f).or_insert(0) += 1;
            }
        }
        if freqs.is_empty() {
            return 0;
        }
        let mut best_f = 0;
        let mut best_n = i64::MIN;
        for (f, n) in freqs {
            if n > best_n {
                best_n = n;
                best_f = f;
            }
        }
        best_f
    }

    pub fn c4_rate(&self, clock: u32) -> i32 {
        std::cmp::max(
            1000,
            py_int(py_round(k053260_rate(clock, self.mode_pitch()))) as i32,
        )
    }
}

/// True when the masked address falls inside a dumped ROM slice.
fn covered(vgm: &VgmFile, chip_id: i32, addr: i32) -> bool {
    let addr = (addr & K053260_MAX_ADDR) as i64;
    for s in &vgm.roms {
        if s.chip as i32 == chip_id && s.kind == "k053260" && !s.data.is_empty() {
            let start = s.start as i64;
            if start <= addr && addr < start + s.data.len() as i64 {
                return true;
            }
        }
    }
    false
}

/// `n` ROM bytes from a 21-bit wrapping address; gaps read as zero.
fn read_rom(rom: &[u8], addr: i32, n: i32) -> Vec<u8> {
    let mask = K053260_MAX_ADDR as i64;
    let mut out = vec![0u8; n as usize];
    for i in 0..n {
        let a = (addr as i64 + i as i64) & mask;
        if (a as usize) < rom.len() {
            out[i as usize] = rom[a as usize];
        }
    }
    out
}

/// Drop trailing silent bytes, never below `floor`.
fn trim(data: &[u8], floor: usize) -> Vec<u8> {
    let mut end = data.len();
    while end > floor && data[end - 1] == 0 {
        end -= 1;
    }
    data[..end].to_vec()
}

struct K053260State {
    reg: [[i32; 8]; K053260_VOICES],
    key: i32,
    loop_: i32,
    adpcm: i32,
    pan: [i32; K053260_VOICES],
    // Logs that never write 0x2F still play; honor bit 1 once it appears.
    ctrl: i32,
    active: [Option<usize>; K053260_VOICES],
    keyed: [bool; K053260_VOICES],
    rev: [bool; K053260_VOICES],
}

fn blank_state() -> K053260State {
    K053260State {
        reg: [[0; 8]; K053260_VOICES],
        key: 0,
        loop_: 0,
        adpcm: 0,
        pan: [K053260_DEFAULT_PAN; K053260_VOICES],
        ctrl: 0x02,
        active: [None; K053260_VOICES],
        keyed: [false; K053260_VOICES],
        rev: [false; K053260_VOICES],
    }
}

fn state<'a>(
    chips: &'a mut HashMap<i32, K053260State>,
    cid: i32,
) -> &'a mut K053260State {
    chips.entry(cid).or_insert_with(blank_state)
}

/// True when the last key write at this sample clears `voice`.
///
/// Drivers often rewrite 0x2A and stop the voice in the same tick. A mode
/// change there never reaches the output.
fn keyed_off_at(vgm: &VgmFile, index: usize, chip_id: i32, voice: usize) -> bool {
    let t = vgm.writes[index].sample;
    let mut last: Option<i32> = None;
    let mut j = index + 1;
    while j < vgm.writes.len() && vgm.writes[j].sample == t {
        let w2 = &vgm.writes[j];
        if w2.chip == "k053260"
            && w2.chip_id as i32 == chip_id
            && w2.reg as i32 == 0x28
        {
            last = Some(w2.val as i32);
        }
        j += 1;
    }
    match last {
        Some(v) => ((v >> voice) & 1) == 0,
        None => false,
    }
}

/// One body per distinct (start, length, ADPCM, direction, loop) keyed.
pub fn collect_k053260(
    vgm: &VgmFile,
) -> (Vec<u8>, Vec<K053260Sample>, Vec<K053260Off>, Vec<String>) {
    let mut id_set: BTreeSet<i32> = BTreeSet::new();
    for w in &vgm.writes {
        if w.chip == "k053260" {
            id_set.insert(w.chip_id as i32);
        }
    }
    let mut roms: BTreeMap<i32, Vec<u8>> = BTreeMap::new();
    for cid in id_set {
        roms.insert(cid, assemble_rom(vgm, "k053260", cid as u8, 0, false, true));
    }
    let mut warnings: Vec<String> = Vec::new();
    let mut offs: Vec<K053260Off> = Vec::new();
    let mut acc: IndexMap<String, i64> = IndexMap::new();
    if roms.is_empty() {
        return (Vec::new(), Vec::new(), offs, warnings);
    }

    let mut chips: HashMap<i32, K053260State> = HashMap::new();
    let mut hits: Vec<K053260Hit> = Vec::new();

    for (wi, w) in vgm.writes.iter().enumerate() {
        if w.chip != "k053260" {
            continue;
        }
        let chip_id = w.chip_id as i32;
        let st = state(&mut chips, chip_id);
        let addr = (w.reg as i32) & 0x3F;
        let val = (w.val as i32) & 0xFF;
        if addr < 0x08 {
            continue;
        }
        if addr < 0x28 {
            let voice = ((addr - 0x08) >> 3) as usize;
            let r = ((addr - 0x08) & 7) as usize;
            let prev = st.reg[voice][r];
            st.reg[voice][r] = val;
            let hit_idx = st.active[voice];
            if hit_idx.is_none() || !st.keyed[voice] || prev == val {
                continue;
            }
            let idx = hit_idx.unwrap();
            if r == 7 {
                let vol = val & 0x7F;
                let pan_v = st.pan[voice];
                if (vol, pan_v) != hits[idx].last_state {
                    hits[idx].state_pts.push((w.sample, vol, pan_v));
                    hits[idx].last_state = (vol, pan_v);
                }
            } else if r == 0 || r == 1 {
                let pitch =
                    (st.reg[voice][0] | ((st.reg[voice][1] & 0x0F) << 8)) & 0xFFF;
                if hits[idx].freq_pts.is_empty()
                    || hits[idx].freq_pts.last().unwrap().1 != pitch
                {
                    hits[idx].freq_pts.push((w.sample, pitch));
                }
            } else if (2..=6).contains(&r) && hits[idx].r#loop {
                // A running loop reloads the live start/length at the wrap. A
                // Furnace sample cannot move its loop. The key-on values win.
                *acc.entry("geometry_loop".to_string()).or_insert(0) += 1;
            }
            continue;
        }
        if addr == 0x28 {
            let prev = st.key;
            st.key = val;
            for voice in 0..K053260_VOICES {
                let up = (val >> voice) & 1;
                let was = (prev >> voice) & 1;
                let reverse = (val & (0x10 << voice)) != 0;
                if up != 0 && was == 0 {
                    if let Some(idx) = st.active[voice] {
                        hits[idx].off_sample = w.sample;
                        offs.push(K053260Off {
                            sample_time: w.sample,
                            chip_id,
                            voice,
                        });
                    }
                    st.keyed[voice] = true;
                    if st.ctrl & 0x02 == 0 {
                        *acc.entry("disabled".to_string()).or_insert(0) += 1;
                        st.active[voice] = None;
                        continue;
                    }
                    let volume = st.reg[voice][7] & 0x7F;
                    let hit = K053260Hit {
                        sample_time: w.sample,
                        chip_id,
                        voice,
                        start: st.reg[voice][4]
                            | (st.reg[voice][5] << 8)
                            | ((st.reg[voice][6] & 0x1F) << 16),
                        length: st.reg[voice][2] | (st.reg[voice][3] << 8),
                        pitch: (st.reg[voice][0]
                            | ((st.reg[voice][1] & 0x0F) << 8))
                            & 0xFFF,
                        volume,
                        pan: st.pan[voice],
                        r#loop: (st.loop_ & (1 << voice)) != 0,
                        adpcm: (st.adpcm & (1 << voice)) != 0,
                        reverse,
                        off_sample: 0,
                        state_pts: Vec::new(),
                        freq_pts: Vec::new(),
                        last_state: (volume, st.pan[voice]),
                    };
                    hits.push(hit);
                    st.active[voice] = Some(hits.len() - 1);
                } else if up == 0 && was != 0 {
                    if let Some(idx) = st.active[voice] {
                        hits[idx].off_sample = w.sample;
                        offs.push(K053260Off {
                            sample_time: w.sample,
                            chip_id,
                            voice,
                        });
                        st.active[voice] = None;
                    }
                    st.keyed[voice] = false;
                } else if up != 0 && was != 0 && reverse != st.rev[voice] {
                    *acc.entry("direction".to_string()).or_insert(0) += 1;
                }
                st.rev[voice] = reverse;
            }
            continue;
        }
        if addr == 0x2A {
            let prev_loop = st.loop_;
            let prev_adpcm = st.adpcm;
            st.loop_ = val & 0x0F;
            st.adpcm = (val >> 4) & 0x0F;
            for voice in 0..K053260_VOICES {
                if st.active[voice].is_none() {
                    continue;
                }
                if keyed_off_at(vgm, wi, chip_id, voice) {
                    continue;
                }
                if ((prev_adpcm & (1 << voice)) != 0)
                    != ((st.adpcm & (1 << voice)) != 0)
                {
                    *acc.entry("adpcm_change".to_string()).or_insert(0) += 1;
                }
                if ((prev_loop & (1 << voice)) != 0)
                    != ((st.loop_ & (1 << voice)) != 0)
                {
                    *acc.entry("loop_change".to_string()).or_insert(0) += 1;
                }
            }
            continue;
        }
        if addr == 0x2C || addr == 0x2D {
            let first = if addr == 0x2C { 0 } else { 2 };
            st.pan[first] = val & 7;
            st.pan[first + 1] = (val >> 3) & 7;
            for voice in [first, first + 1] {
                let hit_idx = st.active[voice];
                if hit_idx.is_none() || !st.keyed[voice] {
                    continue;
                }
                let idx = hit_idx.unwrap();
                let v = st.reg[voice][7] & 0x7F;
                let p = st.pan[voice];
                if (v, p) != hits[idx].last_state {
                    hits[idx].state_pts.push((w.sample, v, p));
                    hits[idx].last_state = (v, p);
                }
            }
            continue;
        }
        if addr == 0x2F {
            st.ctrl = val;
            continue;
        }
    }

    if hits.is_empty() {
        flush(&acc, &mut warnings);
        return (Vec::new(), Vec::new(), offs, warnings);
    }
    if !roms.values().any(|r| !r.is_empty()) {
        warnings.push(
            "no K053260 ROM block (0x8E); samples are silent until they are added".to_string(),
        );
        flush(&acc, &mut warnings);
        return (Vec::new(), Vec::new(), offs, warnings);
    }

    let mut by_key: IndexMap<(i32, i32, i32, bool, bool, bool), K053260Sample> =
        IndexMap::new();
    for h in &hits {
        if h.length <= 0 {
            // The core returns before reading ROM when the length is zero.
            continue;
        }
        let rom: &[u8] = roms.get(&h.chip_id).map(|r| r.as_slice()).unwrap_or(&[]);
        if rom.is_empty() {
            *acc.entry("undumped".to_string()).or_insert(0) += 1;
            continue;
        }
        let first = if h.reverse {
            (h.start - 1) & K053260_MAX_ADDR
        } else {
            (h.start + 1) & K053260_MAX_ADDR
        };
        if !covered(vgm, h.chip_id, first) {
            *acc.entry("undumped".to_string()).or_insert(0) += 1;
            continue;
        }
        let raw = if h.reverse {
            let mut v = read_rom(rom, h.start - h.length, h.length);
            v.reverse();
            v
        } else {
            read_rom(rom, h.start + 1, h.length)
        };
        let raw: Vec<u8> = if h.adpcm && h.reverse {
            raw.iter()
                .map(|b| ((b & 0x0F) << 4) | (b >> 4))
                .collect()
        } else {
            raw
        };
        let key = (
            h.chip_id,
            h.start,
            h.length,
            h.adpcm,
            h.reverse,
            h.r#loop,
        );
        if !by_key.contains_key(&key) {
            // A loop covers the programmed length. KADPCM zero nibbles hold the
            // accumulator. Its length also matters. Trim only PCM one-shots.
            let body = if h.r#loop || h.adpcm {
                raw
            } else {
                trim(&raw, 2)
            };
            if body.is_empty() {
                continue;
            }
            let frames = if h.adpcm {
                body.len() as i64 * 2
            } else {
                body.len() as i64
            };
            let smp = K053260Sample {
                start: h.start,
                data: body,
                hits: Vec::new(),
                loop_start: if h.r#loop { Some(0) } else { None },
                loop_end: if h.r#loop { Some(frames) } else { None },
                depth: if h.adpcm { 7 } else { 8 },
                reverse: h.reverse,
            };
            by_key.insert(key, smp);
        }
        by_key.get_mut(&key).unwrap().hits.push(h.clone());
    }

    flush(&acc, &mut warnings);
    let mut samples: Vec<K053260Sample> = by_key.into_values().collect();
    samples.sort_by(|a, b| {
        (
            a.start,
            a.depth,
            a.reverse,
            a.loop_start.unwrap_or(-1),
        )
            .cmp(&(
                b.start,
                b.depth,
                b.reverse,
                b.loop_start.unwrap_or(-1),
            ))
    });
    let rom = roms
        .get(&0)
        .cloned()
        .unwrap_or_else(|| roms.values().next().cloned().unwrap_or_default());
    (rom, samples, offs, warnings)
}

fn flush(acc: &IndexMap<String, i64>, warnings: &mut Vec<String>) {
    if let Some(&n) = acc.get("undumped") {
        if n != 0 {
            warnings.push(format!(
                "{n} K053260 triggers point at ROM that the VGM does not dump; those notes are silent"
            ));
        }
    }
    if let Some(&n) = acc.get("disabled") {
        if n != 0 {
            warnings.push(format!(
                "{n} K053260 key-ons happened while sound output was disabled; those notes are silent"
            ));
        }
    }
    if let Some(&n) = acc.get("adpcm_change") {
        if n != 0 {
            warnings.push(format!(
                "{n} K053260 KADPCM mode changes while a voice was playing are not converted"
            ));
        }
    }
    if let Some(&n) = acc.get("loop_change") {
        if n != 0 {
            warnings.push(format!(
                "{n} K053260 loop changes while a voice was playing are not converted; the key-on mode is used"
            ));
        }
    }
    if let Some(&n) = acc.get("direction") {
        if n != 0 {
            warnings.push(format!(
                "{n} K053260 direction changes while a voice was playing are not converted; the key-on direction is used"
            ));
        }
    }
    if let Some(&n) = acc.get("geometry_loop") {
        if n != 0 {
            warnings.push(format!(
                "{n} K053260 start/length writes while a looping voice was playing were ignored (the key-on values are used)"
            ));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rate_and_pan_follow_the_reference() {
        assert!((k053260_rate(3_579_545, 0xFC0) - 55930.390625).abs() < 1e-9);
        assert!((k053260_rate(3_579_545, 0xF80) - 27965.1953125).abs() < 1e-9);
        assert_eq!(k053260_rate(0, 0), 3_579_545.0 / 4096.0);
        assert_eq!(k053260_rate(3_579_545, 0xFFF), 3_579_545.0);
        let pans: Vec<i32> = (0..8).map(k053260_pan_fx).collect();
        assert_eq!(pans, vec![0x00, 0xF0, 0xF7, 0xFB, 0xFF, 0xBF, 0x7F, 0x0F]);
        assert_eq!(k053260_pan_fx(8), 0x00);
    }

    #[test]
    fn merge_keeps_last_write_of_each_cluster() {
        let pts = [(0i64, 1i32), (5, 2), (30, 3), (40, 4)];
        assert_eq!(
            merge_step_writes(&pts, STEP_GAP),
            vec![(5, 2), (40, 4)]
        );
        assert_eq!(merge_step_writes(&[], STEP_GAP), Vec::new());
    }

    #[test]
    fn pitch_refit_prefers_the_first_frame_write() {
        let mut h = K053260Hit::default();
        h.sample_time = 60000;
        h.pitch = 0x300;
        h.freq_pts = vec![(60200, 0xFC0)];
        assert_eq!(hit_played_pitch(&h), 0xFC0);
        // Two writes of one register pair merge; the last one is the pitch.
        h.freq_pts = vec![(60200, 0x300), (60210, 0xFC0)];
        assert_eq!(hit_played_pitch(&h), 0xFC0);
        // Outside the refit frame the key-on pitch stands.
        h.freq_pts = vec![(70000, 0xFC0)];
        assert_eq!(hit_played_pitch(&h), 0x300);
        // A zero key-on pitch always takes the later write.
        h.pitch = 0;
        assert_eq!(hit_played_pitch(&h), 0xFC0);
    }

    #[test]
    fn volume_points_use_the_first_in_frame_write() {
        let mut h = K053260Hit::default();
        h.sample_time = 60000;
        h.volume = 100;
        h.pan = 4;
        h.state_pts = vec![(60200, 40, 1), (70000, 90, 7)];
        assert_eq!(hit_played_volume(&h), (40, 1));
        h.state_pts = vec![(70000, 90, 7)];
        assert_eq!(hit_played_volume(&h), (100, 4));
    }

    #[test]
    fn sample_length_mode_pitch_and_c4_rate() {
        let mut smp = K053260Sample::default();
        smp.start = 0x100;
        smp.data = vec![1, 2, 3, 4];
        smp.depth = 7;
        assert_eq!(smp.length(), 8);
        assert_eq!(smp.mode_pitch(), 0);
        assert_eq!(smp.c4_rate(3_579_545), 1000);

        let mut a = K053260Hit::default();
        a.sample_time = 60000;
        a.pitch = 0xFC0;
        smp.hits.push(a.clone());
        smp.hits.push(a.clone());
        a.pitch = 0xF80;
        smp.hits.push(a);
        assert_eq!(smp.mode_pitch(), 0xFC0);
        assert_eq!(smp.c4_rate(3_579_545), 55930);
    }

    #[test]
    fn steps_follow_the_frequency_timeline() {
        let mut h = K053260Hit::default();
        h.sample_time = 0;
        h.pitch = 0xFC0;
        h.freq_pts = vec![(22050, 0xF80)];
        // One step per frame: half a second at each pitch.
        assert!((hit_played_steps(&h, 44100, 3_579_545) - 55930.390625 * 0.5
            - 27965.1953125 * 0.5)
            .abs()
            < 1e-6);
        assert_eq!(hit_played_steps(&h, 0, 3_579_545), 0.0);
    }
}
