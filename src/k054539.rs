//! Konami K054539 ROM -> sample bodies for the Generic PCM DAC.
//!
//! The K054539 has eight voices and no Furnace platform. Each hardware voice is
//! converted to one 1-channel Generic PCM DAC system, so pitch, volume, panning
//! and note cuts remain per voice.
//!
//! Register model follows libvgm k054539.c:
//!
//! ```text
//!     0x00-0x02 pitch delta (24-bit)     0x03 volume (0 = max, 0x40 = -36 dB)
//!     0x04 reverb send                   0x05 pan (0x11-0x1f / 0x81-0x8f)
//!     0x08-0x0a loop address             0x0c-0x0e start address
//!     0x200+2*ch type (bits 2-3), reverse (bit 5), loop (byte +1 bit 0)
//!     0x214 key-on bits                  0x215 key-off bits
//!     0x22c active bits                  0x22e ROM bank (readback only)
//!     0x22f bit 0 = PCM enable, bit 7 = disable register updates
//! ```
//!
//! Sample formats are signed 8-bit (marker 0x80), signed 16-bit LE (marker
//! 0x00 0x80) and 4-bit DPCM (marker 0x88). A marker ends a one-shot or jumps
//! to the loop address. The body is decoded to linear 8/16-bit PCM; DPCM is
//! decoded to 16-bit. Reverb is not representable on the Generic PCM DAC and
//! is reported as a conversion limit.

use std::collections::{BTreeMap, BTreeSet, HashSet};

use indexmap::IndexMap;

use crate::parity;
use crate::vgm::{VgmFile, assemble_rom};

pub const K054539_VOICES: usize = 8;
pub const K054539_OUT_DIV: u32 = 384;
pub const K054539_RATE_DIVISOR: u32 = K054539_OUT_DIV * 65536;
pub const K054539_DEFAULT_CLOCK: u32 = 18_432_000;
pub const K054539_MAX_BODY: usize = 4 * 1024 * 1024;

pub const FORMAT_8BIT: i32 = 0;
pub const FORMAT_16BIT: i32 = 1;
pub const FORMAT_DPCM: i32 = 2;

#[allow(dead_code)]
pub const K054539_UPDATE_AT_KEYON: u8 = 0x04;

pub const DPCM_STEP: [i32; 16] = [
    0, 1, 2, 4, 8, 16, 32, 64, 0, -64, -32, -16, -8, -4, -2, -1,
];

/// Sample frame rate for a pitch delta, in frames/second.
pub fn k054539_rate(clock: u32, delta: i32) -> f64 {
    let clock = if clock == 0 {
        K054539_DEFAULT_CLOCK
    } else {
        clock
    };
    clock as f64 * delta.abs() as f64 / K054539_RATE_DIVISOR as f64
}

pub fn k054539_out_rate(clock: u32) -> i32 {
    let clock = if clock == 0 {
        K054539_DEFAULT_CLOCK
    } else {
        clock
    };
    std::cmp::max(
        1000,
        parity::py_int(parity::py_round(clock as f64 / K054539_OUT_DIV as f64)) as i32,
    )
}

/// Linear amplitude for the attenuation byte (0 = unity).
pub fn k054539_volume(level: i32) -> f64 {
    10.0f64.powf(-36.0 * (level & 0xFF) as f64 / 64.0 / 20.0)
}

pub fn k054539_pan_index(pan: i32) -> i32 {
    if (0x81..=0x8F).contains(&pan) {
        return pan - 0x81;
    }
    if (0x11..=0x1F).contains(&pan) {
        return pan - 0x11;
    }
    7 // centre
}

/// (left, right) 0-255 amplitudes, square-root constant-power law.
pub fn k054539_pan_levels(pan: i32) -> (i32, i32) {
    let idx = k054539_pan_index(pan);
    let left = (idx as f64).sqrt() / 14.0f64.sqrt();
    let right = ((14 - idx) as f64).sqrt() / 14.0f64.sqrt();
    (
        parity::py_int(parity::py_round(left * 255.0)) as i32,
        parity::py_int(parity::py_round(right * 255.0)) as i32,
    )
}

/// Furnace 08xy from the 15 K054539 pan positions.
pub fn k054539_pan_fx(pan: i32) -> i32 {
    let (left, right) = k054539_pan_levels(pan);
    ((parity::py_int(parity::py_round(left as f64 * 15.0 / 255.0)) as i32 & 15) << 4)
        | (parity::py_int(parity::py_round(right as f64 * 15.0 / 255.0)) as i32 & 15)
}

pub fn k054539_volume_fx(level: i32) -> i32 {
    (parity::py_int(parity::py_round(k054539_volume(level) * 255.0)) as i32).clamp(0, 255)
}

/// Pitch delta a hit actually uses.
///
/// Drivers that key on before writing the pitch leave the key-on snapshot at
/// zero. The first in-frame delta write is then the note's own pitch.
pub fn hit_played_delta(hit: &K054539Hit) -> i32 {
    if hit.delta != 0 {
        return hit.delta.abs();
    }
    for &(_t, delta) in &hit.freq_pts {
        if delta != 0 {
            return delta.abs();
        }
    }
    0
}

#[allow(dead_code)]
fn _pow2_mask(size: i64) -> i64 {
    if size <= 0 {
        return 0;
    }
    let n = size - 1;
    let bits = if n == 0 {
        0
    } else {
        64 - (n as u64).leading_zeros() as i64
    };
    (1i64 << bits) - 1
}

fn _covered(vgm: &VgmFile, chip_id: i32, addr: i64) -> bool {
    for s in &vgm.roms {
        if s.chip as i64 == chip_id as i64 && s.kind == "k054539" && !s.data.is_empty() {
            let start = s.start as i64;
            if start <= addr && addr < start + s.data.len() as i64 {
                return true;
            }
        }
    }
    false
}

#[derive(Clone, Debug, PartialEq)]
pub struct K054539Hit {
    pub sample_time: i64,
    pub chip_id: i32,
    pub voice: i32,
    pub start: i64,
    pub r#loop: i64,
    pub loop_on: bool,
    pub format: i32,
    pub reverse: bool,
    pub delta: i32,
    pub vol: i32,
    pub pan: i32,
    pub reverb: i32,
    pub off_sample: i64,
    pub vol_pts: Vec<(i64, i32, i32)>,
    pub freq_pts: Vec<(i64, i32)>,
}

impl Default for K054539Hit {
    fn default() -> Self {
        Self {
            sample_time: 0,
            chip_id: 0,
            voice: 0,
            start: 0,
            r#loop: 0,
            loop_on: false,
            format: 0,
            reverse: false,
            delta: 0,
            vol: 0,
            pan: 0,
            reverb: 0,
            off_sample: 0,
            vol_pts: Vec::new(),
            freq_pts: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct K054539Off {
    pub sample_time: i64,
    pub chip_id: i32,
    pub voice: i32,
}

impl Default for K054539Off {
    fn default() -> Self {
        Self {
            sample_time: 0,
            chip_id: 0,
            voice: 0,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct K054539Sample {
    pub start: i64,
    pub end: i64,
    pub data: Vec<u8>,
    pub depth: i32,
    pub loop_start: Option<i64>,
    pub loop_end: Option<i64>,
    pub reverse: bool,
    pub hits: Vec<K054539Hit>,
}

impl Default for K054539Sample {
    fn default() -> Self {
        Self {
            start: 0,
            end: 0,
            data: Vec::new(),
            depth: 0,
            loop_start: None,
            loop_end: None,
            reverse: false,
            hits: Vec::new(),
        }
    }
}

impl K054539Sample {
    pub fn width(&self) -> usize {
        if self.depth == 16 { 2 } else { 1 }
    }

    pub fn length(&self) -> usize {
        self.data.len() / self.width()
    }

    pub fn mode_delta(&self) -> i32 {
        let mut values: IndexMap<i32, usize> = IndexMap::new();
        for h in &self.hits {
            let delta = hit_played_delta(h);
            if delta != 0 {
                *values.entry(delta).or_insert(0) += 1;
            }
        }
        let mut best: Option<(i32, usize)> = None;
        for (&value, &count) in &values {
            if best.map_or(true, |(_v, best_count)| count > best_count) {
                best = Some((value, count));
            }
        }
        best.map(|(value, _count)| value).unwrap_or(0)
    }

    pub fn c4_rate(&self, clock: u32) -> i32 {
        std::cmp::max(
            1000,
            parity::py_int(parity::py_round(k054539_rate(clock, self.mode_delta()))) as i32,
        )
    }
}

/// Internal voice state. `active` stores an index into the hit list instead of
/// the hit itself: Python mutates `st.active[ch].off_sample` and expects the
/// same object to be visible in `hits`, which Rust ownership cannot alias.
#[derive(Clone, Debug, PartialEq)]
pub struct _K054539State {
    pub regs: Vec<u8>,
    pub pos_latch: Vec<[u8; 3]>,
    pub keyed: Vec<bool>,
    pub active: Vec<Option<usize>>,
    pub key_last: i32,
    pub enabled: bool,
}

impl Default for _K054539State {
    fn default() -> Self {
        Self {
            regs: vec![0u8; 0x230],
            pos_latch: vec![[0u8; 3]; K054539_VOICES],
            keyed: vec![false; K054539_VOICES],
            active: vec![None; K054539_VOICES],
            key_last: 0,
            enabled: false,
        }
    }
}

impl _K054539State {
    pub fn regupdate(&self) -> bool {
        (self.regs[0x22F] & 0x80) == 0
    }

    pub fn pcm_enabled(&self) -> bool {
        (self.regs[0x22F] & 0x01) != 0
    }
}

fn _start_addr(regs: &[u8], ch: usize) -> i64 {
    let b = ch * 0x20;
    regs[b + 0x0C] as i64 | ((regs[b + 0x0D] as i64) << 8) | ((regs[b + 0x0E] as i64) << 16)
}

fn _loop_addr(regs: &[u8], ch: usize) -> i64 {
    let b = ch * 0x20;
    regs[b + 0x08] as i64 | ((regs[b + 0x09] as i64) << 8) | ((regs[b + 0x0A] as i64) << 16)
}

#[allow(dead_code)]
fn _channel_regs(regs: &[u8], ch: usize) -> &[u8] {
    &regs[ch * 0x20..ch * 0x20 + 0x20]
}

/// First end-marker address at or after start, or -1.
fn _find_marker(rom: &[u8], start: i64, fmt: i32) -> i64 {
    let n = rom.len() as i64;
    if start >= n {
        return -1;
    }
    let begin = start as usize;
    if fmt == FORMAT_8BIT {
        let limit = n.min(start + K054539_MAX_BODY as i64) as usize;
        return rom[begin..limit]
            .iter()
            .position(|&b| b == 0x80)
            .map(|i| (begin + i) as i64)
            .unwrap_or(-1);
    }
    if fmt == FORMAT_16BIT {
        let end = (n - 1).min(start + K054539_MAX_BODY as i64);
        let mut i = start;
        while i < end {
            let ui = i as usize;
            if rom[ui] == 0x00 && rom[ui + 1] == 0x80 {
                return i;
            }
            i += 2;
        }
        return -1;
    }
    if fmt == FORMAT_DPCM {
        let limit = n.min(start + K054539_MAX_BODY as i64) as usize;
        return rom[begin..limit]
            .iter()
            .position(|&b| b == 0x88)
            .map(|i| (begin + i) as i64)
            .unwrap_or(-1);
    }
    -1
}

fn _decode_dpcm(rom: &[u8], start: i64, end: i64) -> Vec<u8> {
    let mut out = Vec::new();
    let mut acc: i32 = 0;
    for i in start as usize..end as usize {
        let byte = rom[i];
        for nib in [byte & 0x0F, byte >> 4] {
            acc += DPCM_STEP[nib as usize] << 8;
            acc = acc.clamp(-32768, 32767);
            out.extend_from_slice(&(acc as i16).to_le_bytes());
        }
    }
    out
}

fn _body(
    rom: &[u8],
    hit: &K054539Hit,
    warnings: &mut Vec<String>,
    chip_id: i32,
) -> Option<(Vec<u8>, i32, Option<i64>, Option<i64>)> {
    let start = hit.start;
    if start < 0 || start >= rom.len() as i64 {
        return None;
    }
    let mut marker = _find_marker(rom, start, hit.format);
    if marker < 0 {
        marker = (rom.len() as i64).min(start + K054539_MAX_BODY as i64);
        warnings.push(format!(
            "K054539 {chip_id}: sample at {start:06x} has no end marker; the body was capped"
        ));
    }
    let (mut data, depth, mut loop_start, mut loop_end, frames);
    if hit.format == FORMAT_8BIT {
        if marker <= start {
            return None;
        }
        data = rom[start as usize..marker as usize].to_vec();
        depth = 8;
        frames = data.len() as i64;
        loop_start = if hit.loop_on && start <= hit.r#loop && hit.r#loop < marker {
            Some(hit.r#loop - start)
        } else {
            None
        };
        loop_end = if loop_start.is_some() {
            Some(frames)
        } else {
            None
        };
    } else if hit.format == FORMAT_16BIT {
        if marker - start < 2 {
            return None;
        }
        marker -= (marker - start) & 1;
        data = rom[start as usize..marker as usize].to_vec();
        depth = 16;
        frames = data.len() as i64 / 2;
        loop_start = if hit.loop_on && start <= hit.r#loop && hit.r#loop < marker {
            Some(parity::py_floor_div(hit.r#loop - start, 2))
        } else {
            None
        };
        loop_end = if loop_start.is_some() {
            Some(frames)
        } else {
            None
        };
    } else if hit.format == FORMAT_DPCM {
        if marker <= start {
            return None;
        }
        data = _decode_dpcm(rom, start, marker);
        depth = 16;
        frames = data.len() as i64 / 2;
        loop_start = if hit.loop_on && start <= hit.r#loop && hit.r#loop < marker {
            Some((hit.r#loop - start) * 2)
        } else {
            None
        };
        loop_end = if loop_start.is_some() {
            Some(frames)
        } else {
            None
        };
    } else {
        return None;
    }
    if hit.reverse {
        if depth == 16 {
            let mut reversed = Vec::with_capacity(data.len());
            for chunk in data.chunks(2).rev() {
                reversed.extend_from_slice(chunk);
            }
            data = reversed;
        } else {
            data.reverse();
        }
        if let (Some(ls), Some(le)) = (loop_start, loop_end) {
            loop_start = Some(frames - le);
            loop_end = Some(frames - ls);
        }
    }
    Some((data, depth, loop_start, loop_end))
}

fn _open_hit(
    st: &mut _K054539State,
    _vgm: &VgmFile,
    cid: i32,
    ch: usize,
    when: i64,
    hits: &mut Vec<K054539Hit>,
    warnings: &mut Vec<String>,
) {
    let b = ch * 0x20;
    let base2 = 0x200 + ch * 2;
    let regs = &st.regs;
    let fmt = ((regs[base2] >> 2) & 0x03) as i32;
    let reverse = (regs[base2] & 0x20) != 0;
    let loop_on = (regs[base2 + 1] & 0x01) != 0;
    let start = _start_addr(regs, ch);
    let loop_addr = _loop_addr(regs, ch);
    let delta = regs[b] as i32 | ((regs[b + 1] as i32) << 8) | ((regs[b + 2] as i32) << 16);
    let vol = regs[b + 0x03] as i32;
    let pan = regs[b + 0x05] as i32;
    let reverb = regs[b + 0x04] as i32;
    let hit = K054539Hit {
        sample_time: when,
        chip_id: cid,
        voice: ch as i32,
        start,
        r#loop: loop_addr,
        loop_on,
        format: fmt,
        reverse,
        delta,
        vol,
        pan,
        reverb,
        off_sample: 0,
        vol_pts: Vec::new(),
        freq_pts: Vec::new(),
    };
    if let Some(idx) = st.active[ch] {
        hits[idx].off_sample = when;
    }
    hits.push(hit);
    st.active[ch] = Some(hits.len() - 1);
    st.keyed[ch] = true;
    if reverb != 0 {
        warnings.push(format!(
            "K054539 {cid}: reverb send is not representable on Generic PCM DAC"
        ));
    }
}

fn _close_voice(
    st: &mut _K054539State,
    cid: i32,
    ch: usize,
    when: i64,
    hits: &mut Vec<K054539Hit>,
    offs: &mut Vec<K054539Off>,
) {
    if let Some(idx) = st.active[ch] {
        hits[idx].off_sample = when;
        st.active[ch] = None;
    }
    st.keyed[ch] = false;
    offs.push(K054539Off {
        sample_time: when,
        chip_id: cid,
        voice: ch as i32,
    });
}

fn _keyon(
    st: &mut _K054539State,
    vgm: &VgmFile,
    cid: i32,
    ch: usize,
    when: i64,
    hits: &mut Vec<K054539Hit>,
    warnings: &mut Vec<String>,
    vol_last: &mut IndexMap<(i32, usize), (i32, i32)>,
    freq_last: &mut IndexMap<(i32, usize), i32>,
) {
    if !st.regupdate() {
        return;
    }
    if st.pcm_enabled() {
        for r in 0..3 {
            st.regs[ch * 0x20 + 0x0C + r] = st.pos_latch[ch][r];
        }
    }
    st.keyed[ch] = true;
    if st.pcm_enabled() {
        _open_hit(st, vgm, cid, ch, when, hits, warnings);
        let b = ch * 0x20;
        vol_last.insert(
            (cid, ch),
            (st.regs[b + 0x03] as i32, st.regs[b + 0x05] as i32),
        );
        freq_last.insert(
            (cid, ch),
            st.regs[b] as i32
                | ((st.regs[b + 1] as i32) << 8)
                | ((st.regs[b + 2] as i32) << 16),
        );
    }
}

fn _keyoff(
    st: &mut _K054539State,
    cid: i32,
    ch: usize,
    when: i64,
    hits: &mut Vec<K054539Hit>,
    offs: &mut Vec<K054539Off>,
) {
    if !st.regupdate() {
        return;
    }
    if st.keyed[ch] || st.active[ch].is_some() {
        _close_voice(st, cid, ch, when, hits, offs);
    }
}

pub fn collect_k054539(vgm: &VgmFile) -> (Vec<K054539Sample>, Vec<K054539Off>, Vec<String>) {
    let mut id_set: BTreeSet<i32> = BTreeSet::new();
    for w in &vgm.writes {
        if w.chip == "k054539" {
            id_set.insert(w.chip_id as i32);
        }
    }
    let ids: Vec<i32> = id_set.into_iter().collect();
    let mut roms: BTreeMap<i32, Vec<u8>> = BTreeMap::new();
    for &cid in &ids {
        roms.insert(
            cid,
            assemble_rom(vgm, "k054539", cid as u8, 0xFFu8, false, true),
        );
    }
    let mut warnings: Vec<String> = Vec::new();
    let mut offs: Vec<K054539Off> = Vec::new();
    let mut hits: Vec<K054539Hit> = Vec::new();
    let mut states: BTreeMap<i32, _K054539State> = BTreeMap::new();
    for &cid in &ids {
        states.insert(cid, _K054539State::default());
    }
    let mut vol_last: IndexMap<(i32, usize), (i32, i32)> = IndexMap::new();
    let mut freq_last: IndexMap<(i32, usize), i32> = IndexMap::new();

    for w in &vgm.writes {
        if w.chip != "k054539" {
            continue;
        }
        let cid = w.chip_id as i32;
        let off = (w.reg & 0x7FFF) as usize;
        let val = (w.val & 0xFF) as u8;
        let st = states.get_mut(&cid).unwrap();
        if off == 0x214 {
            // The core's key-on bits are processed before the register is
            // stored, but 0x214 is not a field the PCM update reads.
            st.regs[off] = val;
            for ch in 0..K054539_VOICES {
                if val & (1 << ch) != 0 {
                    _keyon(
                        st,
                        vgm,
                        cid,
                        ch,
                        w.sample,
                        &mut hits,
                        &mut warnings,
                        &mut vol_last,
                        &mut freq_last,
                    );
                }
            }
            continue;
        }
        if off == 0x215 {
            st.regs[off] = val;
            for ch in 0..K054539_VOICES {
                if val & (1 << ch) != 0 {
                    _keyoff(st, cid, ch, w.sample, &mut hits, &mut offs);
                }
            }
            continue;
        }
        if off == 0x22F {
            let old_enabled = st.enabled;
            st.regs[off] = val;
            st.enabled = (val & 0x01) != 0;
            if st.enabled && !old_enabled {
                for ch in 0..K054539_VOICES {
                    if st.keyed[ch] && st.active[ch].is_none() {
                        _open_hit(st, vgm, cid, ch, w.sample, &mut hits, &mut warnings);
                    }
                }
            } else if !st.enabled && old_enabled {
                for ch in 0..K054539_VOICES {
                    if st.active[ch].is_some() {
                        _close_voice(st, cid, ch, w.sample, &mut hits, &mut offs);
                    }
                }
            }
            continue;
        }
        st.regs[off] = val;
        if (0x00..0x100).contains(&off) {
            let ch = off >> 5;
            let r = off & 0x1F;
            if st.pcm_enabled() && (0x0C..=0x0E).contains(&r) {
                // libvgm forces UPDATE_AT_KEYON on. A start write is latched
                // and copied at the next key-on while PCM is enabled.
                st.pos_latch[ch][r - 0x0C] = val;
                continue;
            }
            if st.keyed[ch] {
                if let Some(idx) = st.active[ch] {
                    let b = ch * 0x20;
                    if r == 0x03 {
                        let pair = (st.regs[b + 0x03] as i32, st.regs[b + 0x05] as i32);
                        if Some(pair) != vol_last.get(&(cid, ch)).copied() {
                            hits[idx].vol_pts.push((w.sample, pair.0, pair.1));
                            vol_last.insert((cid, ch), pair);
                        }
                    } else if r == 0x05 {
                        let pair = (st.regs[b + 0x03] as i32, st.regs[b + 0x05] as i32);
                        if Some(pair) != vol_last.get(&(cid, ch)).copied() {
                            hits[idx].vol_pts.push((w.sample, pair.0, pair.1));
                            vol_last.insert((cid, ch), pair);
                        }
                    } else if r == 0x00 || r == 0x01 || r == 0x02 {
                        let delta = st.regs[b] as i32
                            | ((st.regs[b + 1] as i32) << 8)
                            | ((st.regs[b + 2] as i32) << 16);
                        if Some(delta) != freq_last.get(&(cid, ch)).copied() {
                            hits[idx].freq_pts.push((w.sample, delta));
                            freq_last.insert((cid, ch), delta);
                        }
                    }
                }
            }
        }
    }

    // A key-on that is closed at the same VGM sample produces no audio: both
    // register writes take effect before the next hardware tick. Some logs
    // start with a full key-on/key-off mask pair to reset every voice; those
    // zero-length hits must not become samples or notes.
    hits.retain(|h| !(h.off_sample != 0 && h.off_sample <= h.sample_time));
    if hits.is_empty() {
        return (Vec::new(), offs, warnings);
    }
    if !roms.values().any(|rom| !rom.is_empty()) {
        warnings.push(
            "no K054539 ROM block (0x8C); PCM notes will be silent until samples are added"
                .to_string(),
        );
        return (Vec::new(), offs, warnings);
    }

    let mut by_key: IndexMap<(i32, i64, i64, i64, i32, bool), K054539Sample> = IndexMap::new();
    let mut undumped = 0u64;
    for h in &hits {
        let empty: Vec<u8> = Vec::new();
        let rom = roms.get(&h.chip_id).unwrap_or(&empty);
        if rom.is_empty() {
            continue;
        }
        let marker = _find_marker(rom, h.start, h.format);
        let end = if marker >= 0 {
            marker
        } else {
            (rom.len() as i64).min(h.start + K054539_MAX_BODY as i64)
        };
        if h.start >= rom.len() as i64 || !_covered(vgm, h.chip_id, h.start) {
            undumped += 1;
            continue;
        }
        if end <= h.start {
            continue;
        }
        let key = (
            h.chip_id,
            h.start,
            end,
            if h.loop_on { h.r#loop } else { -1 },
            h.format,
            h.reverse,
        );
        if let Some(smp) = by_key.get_mut(&key) {
            smp.hits.push(h.clone());
            continue;
        }
        let body = _body(rom, h, &mut warnings, h.chip_id);
        let Some((data, depth, loop_start, loop_end)) = body else {
            continue;
        };
        let smp = K054539Sample {
            start: h.start,
            end,
            data,
            depth,
            loop_start,
            loop_end,
            reverse: h.reverse,
            hits: vec![h.clone()],
        };
        by_key.insert(key, smp);
    }
    if undumped != 0 {
        warnings.push(format!(
            "{undumped} K054539 triggers point at ROM that the VGM does not dump; those notes are silent"
        ));
    }
    let mut samples: Vec<K054539Sample> = by_key.into_values().collect();
    samples.sort_by(|a, b| {
        (
            a.start,
            a.loop_start.filter(|&v| v != 0).unwrap_or(-1),
            a.depth,
        )
            .cmp(&(
                b.start,
                b.loop_start.filter(|&v| v != 0).unwrap_or(-1),
                b.depth,
            ))
    });
    // A file with several distinct send values would otherwise repeat the
    // same warning on every key-on.
    let mut seen: HashSet<String> = HashSet::new();
    warnings.retain(|w| seen.insert(w.clone()));
    (samples, offs, warnings)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hit_8bit(start: i64, end_loop: i64, loop_on: bool, reverse: bool) -> K054539Hit {
        K054539Hit {
            start,
            r#loop: end_loop,
            loop_on,
            format: FORMAT_8BIT,
            reverse,
            ..Default::default()
        }
    }

    #[test]
    fn rate_uses_default_clock_and_absolute_delta() {
        assert_eq!(k054539_rate(0, 0x1_0000), 48000.0);
        assert_eq!(k054539_rate(K054539_DEFAULT_CLOCK, -0x1_0000), 48000.0);
        assert_eq!(k054539_out_rate(0), 48000);
        assert_eq!(k054539_out_rate(384), 1000);
    }

    #[test]
    fn volume_and_pan_levels_match_register_law() {
        assert_eq!(k054539_volume_fx(0), 255);
        assert_eq!(k054539_volume_fx(0x40), 4);
        assert_eq!(k054539_pan_levels(0x88), (180, 180));
        assert_eq!(k054539_pan_fx(0x81), 0x0F);
        assert_eq!(k054539_pan_fx(0x8F), 0xF0);
        assert_eq!(k054539_pan_fx(0x88), 0xBB);
        // Values outside both pan ranges fall back to the centre index.
        assert_eq!(k054539_pan_index(0x00), 7);
        assert_eq!(k054539_pan_index(0x90), 7);
    }

    #[test]
    fn hit_played_delta_falls_back_to_first_written_frequency() {
        let mut hit = hit_8bit(0, 0, false, false);
        assert_eq!(hit_played_delta(&hit), 0);
        hit.freq_pts = vec![(10, 0), (20, -300)];
        assert_eq!(hit_played_delta(&hit), 300);
        hit.delta = -0x200;
        assert_eq!(hit_played_delta(&hit), 0x200);
    }

    #[test]
    fn body_decodes_8bit_marker_loop_and_reverse() {
        let rom = [1u8, 2, 3, 0x80, 9];
        let hit = hit_8bit(0, 1, true, false);
        let (data, depth, loop_start, loop_end) =
            _body(&rom, &hit, &mut Vec::new(), 0).unwrap();
        assert_eq!(data, vec![1, 2, 3]);
        assert_eq!(depth, 8);
        assert_eq!(loop_start, Some(1));
        assert_eq!(loop_end, Some(3));

        let rev = hit_8bit(0, 1, true, true);
        let (data, _depth, loop_start, loop_end) =
            _body(&rom, &rev, &mut Vec::new(), 0).unwrap();
        assert_eq!(data, vec![3, 2, 1]);
        assert_eq!(loop_start, Some(0));
        assert_eq!(loop_end, Some(2));
    }

    #[test]
    fn body_caps_missing_marker_and_decodes_dpcm() {
        let rom = [1u8, 2, 3];
        let hit = hit_8bit(0, 0, false, false);
        let mut warnings = Vec::new();
        let (data, depth, loop_start, _) = _body(&rom, &hit, &mut warnings, 7).unwrap();
        assert_eq!(data, vec![1, 2, 3]);
        assert_eq!(depth, 8);
        assert!(loop_start.is_none());
        assert_eq!(
            warnings,
            vec![
                "K054539 7: sample at 000000 has no end marker; the body was capped".to_string()
            ]
        );

        let dpcm = [0x10u8];
        let hit = K054539Hit {
            start: 0,
            r#loop: 0,
            loop_on: false,
            format: FORMAT_DPCM,
            ..Default::default()
        };
        let (data, depth, loop_start, loop_end) =
            _body(&dpcm, &hit, &mut Vec::new(), 0).unwrap();
        assert_eq!(data, vec![0x00, 0x00, 0x00, 0x01]);
        assert_eq!(depth, 16);
        assert!(loop_start.is_none());
        assert!(loop_end.is_none());
    }
}
