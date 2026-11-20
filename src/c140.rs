//! Namco C140 ROM → signed samples → Furnace.
//!
//! Register model follows libvgm c140.c:
//!
//! ```text
//! register = voice << 4 | reg     (24 voices, 9-bit address space)
//! 0x00 volume R   0x01 volume L   0x02/0x03 frequency MSB/LSB   0x04 bank
//! 0x05 mode       0x06/0x07 start 0x08/0x09 end   0x0A/0x0B loop
//! ```
//!
//! Mode bits: 7 = key on (6 = retrigger when already keyed), 4 = looping,
//! 3 = mulaw. Each address step is one sample and one ROM byte. The ROM
//! address is libvgm's find_sample:
//!
//! ```text
//! adrs = (bank << 16) | start
//! System 2:  ((adrs & 0x200000) >> 2) | (adrs & 0x7ffff)
//! System 21: ((adrs & 0x300000) >> 1) + (adrs & 0x7ffff)
//! ```
//!
//! Mulaw bodies are expanded with the full 16-bit table and stored as linear
//! int16 so Furnace does not decode them again. A key-off (mode bit 7 clear
//! while the voice plays) becomes a C140Off, or a looping voice never releases.
//! The C-4 rate uses the reference step rate (freq * clock / 18874368) and the
//! flag carries 2/3 of the crystal. Sign-flip and sign-magnitude are not
//! applied.

use std::collections::BTreeSet;

use indexmap::IndexMap;

use crate::parity::{py_int, py_round};
use crate::vgm::{assemble_rom, VgmFile};

pub const C140_VOICES: usize = 24;
/// Reference step rate both MAME and libvgm use: sample_rate = clock/288 with
/// a .16 fixed-point position, so freq * clock / (288 * 65536) words a second.
pub const C140_RATE_DIVISOR: i64 = 288 * 65536;
/// Furnace's C140 core ticks at clock/192 with a plain 16-bit fraction, 1.5x
/// the reference. A clock scaled by 2/3 (12288000 -> 8192000, its default)
/// lands its playback and VGM export on the reference rate; the register
/// stream is scale-invariant either way.
pub const C140_FURNACE_CLOCK_SCALE: (u32, u32) = (2, 3);

/// C140 clock in Furnace's notation (2/3 of the crystal).
pub fn c140_furnace_clock(clock: u32) -> u32 {
    let clock = if clock == 0 { 12288000 } else { clock };
    ((clock as u64) * (C140_FURNACE_CLOCK_SCALE.0 as u64)
        / (C140_FURNACE_CLOCK_SCALE.1 as u64)) as u32
}

/// Sample frame rate for a frequency register, in Hz.
pub fn c140_rate(clock: u32, freq: i32) -> f64 {
    let clock = if clock == 0 { 12288000 } else { clock };
    (clock as f64) * ((freq & 0xFFFF) as f64) / (C140_RATE_DIVISOR as f64)
}

const fn decompress_table() -> [i32; 256] {
    let mut tbl = [0i32; 256];
    let mut i = 0usize;
    while i < 256 {
        let j: i32 = if i >= 128 { i as i32 - 256 } else { i as i32 };
        let s1 = (j & 7) as u32;
        let s2 = (j >> 3).abs() & 31;
        let val = ((0x80i32 << s1) & 0xFF00) + (s2 << (if s1 != 0 { s1 + 3 } else { 4 }));
        tbl[i] = if j < 0 { -val } else { val };
        i += 1;
    }
    tbl
}

/// MAME c140_device::device_start.
pub const C140_DECOMP: [i32; 256] = decompress_table();

/// Full table value as a signed 16-bit sample (C INT16 store).
///
/// Python `_mulaw_i16`; exposed because the C352 cross-check test imports it.
pub fn mulaw_i16(byte: u8) -> i16 {
    let v = (C140_DECOMP[(byte & 0xFF) as usize] & 0xFFFF) as u32;
    if v & 0x8000 != 0 {
        (v as i64 - 0x10000) as i16
    } else {
        v as i16
    }
}

/// libvgm find_sample. Type 0 is System 2, type 1 is System 21.
pub fn c140_rom_addr(bank: i32, start: i32, c140_type: i32) -> i64 {
    let adrs = (((bank & 0xFF) << 16) | (start & 0xFFFF)) as i64;
    if c140_type == 0 {
        return ((adrs & 0x200000) >> 2) | (adrs & 0x7FFFF);
    }
    if c140_type == 1 {
        return ((adrs & 0x300000) >> 1) + (adrs & 0x7FFFF);
    }
    adrs
}

/// Merge register steps written within ``gap`` samples (MSB/LSB pairs).
///
/// A 12-bit frequency update is two register writes a few samples apart; the
/// intermediate value is a write-order transient; only the last write of
/// each cluster is kept (same rule as the FM sweep's KC/KF merge).
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

/// A frequency write this soon after key-on is the note's own pitch, not a
/// mid-note bend. Some drivers key the voice on before setting its frequency,
/// leaving a stale snapshot at key-on; their real pitch write lands inside the
/// frame (735 samples at 60 Hz), so the first in-frame write is the note.
pub const C140_REFIT_SAMPLES: i64 = 735;

/// The frequency a hit actually plays at (0 when the driver never set one).
///
/// A frequency write just after key-on is the note's own pitch and replaces
/// the stale key-on value; a later write only wins when the key-on value was
/// zero (nothing was playing yet).
pub fn hit_played_freq(hit: &C140Hit) -> i32 {
    let steps = merge_step_writes(&hit.freq_pts, 16);
    if let Some(&first) = steps.first() {
        if first.1 != 0
            && (first.0 - hit.sample_time <= C140_REFIT_SAMPLES || (hit.freq & 0xFFFF) == 0)
        {
            return first.1;
        }
    }
    hit.freq & 0xFFFF
}

#[derive(Clone, Debug, PartialEq)]
pub struct C140Hit {
    pub sample_time: i64,
    pub chip_id: i32,
    pub voice: i32,
    /// Address count, one byte per step.
    pub start: i32,
    pub end: i32,
    pub loop_: i32,
    pub loop_on: bool,
    pub compressed: bool,
    pub bank: i32,
    pub freq: i32,
    pub vol_l: i32,
    pub vol_r: i32,
    /// Key-off sample (0 = still playing at the end of the VGM).
    pub off_sample: i64,
    /// Mid-note register writes while keyed: volume as the (L, R) pair (the
    /// balance is part of the write), frequency as the combined 12-bit value.
    pub vol_pts: Vec<(i64, i32, i32)>,
    pub freq_pts: Vec<(i64, i32)>,
}

impl C140Hit {
    /// Python's `C140Hit(...)` with every defaulted field left at its default.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        sample_time: i64,
        chip_id: i32,
        voice: i32,
        start: i32,
        end: i32,
        loop_: i32,
        loop_on: bool,
        compressed: bool,
        bank: i32,
        freq: i32,
        vol_l: i32,
        vol_r: i32,
    ) -> Self {
        C140Hit {
            sample_time,
            chip_id,
            voice,
            start,
            end,
            loop_,
            loop_on,
            compressed,
            bank,
            freq,
            vol_l,
            vol_r,
            off_sample: 0,
            vol_pts: Vec::new(),
            freq_pts: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct C140Off {
    pub sample_time: i64,
    pub chip_id: i32,
    pub voice: i32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct C140Sample {
    pub start: i32,
    /// Signed 8-bit, or signed 16-bit LE when `depth == 16`.
    pub data: Vec<u8>,
    pub loop_start: Option<i32>,
    pub loop_end: Option<i32>,
    pub hits: Vec<C140Hit>,
    pub depth: i32,
}

impl C140Sample {
    /// Frame count. A mulaw body is two bytes per frame.
    pub fn length(&self) -> usize {
        if self.depth == 16 {
            self.data.len() / 2
        } else {
            self.data.len()
        }
    }

    /// Most common frequency the sample is played at (0 when unknown).
    ///
    /// Uses the played frequency (key-on or the post-key-on refit write), so
    /// drivers that key a voice on with stale zero registers before writing
    /// the frequency do not pin the reference to 0.
    pub fn mode_freq(&self) -> i32 {
        if self.hits.is_empty() {
            return 0;
        }
        let mut freqs: IndexMap<i32, i64> = IndexMap::new();
        for h in &self.hits {
            let f = hit_played_freq(h);
            if f != 0 {
                *freqs.entry(f).or_insert(0) += 1;
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

    pub fn c4_rate(&self, clock: u32) -> i32 {
        let rate = c140_rate(clock, self.mode_freq());
        (py_int(py_round(rate)) as i32).max(1000)
    }
}

/// True when addr falls inside a dumped C140 ROM slice.
fn covered(vgm: &VgmFile, chip_id: i32, addr: i64) -> bool {
    for s in &vgm.roms {
        if s.chip as i32 == chip_id && s.kind == "c140" && !s.data.is_empty() {
            let start = s.start as i64;
            if start <= addr && addr < start + s.data.len() as i64 {
                return true;
            }
        }
    }
    false
}

/// Linear: raw signed bytes. Mulaw: int16 LE of the full table value.
fn decode_body(rom: &[u8], addr: i64, n_frames: i64, compressed: bool) -> Vec<u8> {
    let avail = rom.len() as i64 - addr;
    let n_frames = 0i64.max(n_frames.min(avail));
    if addr >= rom.len() as i64 {
        return Vec::new();
    }
    let addr = addr as usize;
    let n_frames = n_frames as usize;
    let raw = &rom[addr..addr + n_frames];
    if !compressed {
        return raw.to_vec();
    }
    let mut out = Vec::with_capacity(raw.len() * 2);
    for &b in raw {
        out.extend_from_slice(&mulaw_i16(b).to_le_bytes());
    }
    out
}

/// Frame count after dropping trailing silence. Never cuts the loop.
fn trim_frames(body: &[u8], width: usize, loop_off: Option<i32>) -> usize {
    let mut frames = body.len() / width;
    let mut floor = 2usize;
    if let Some(loop_off) = loop_off {
        floor = floor.max((loop_off + 64) as usize);
    }
    let silence = vec![0u8; width];
    while frames > floor && body[(frames - 1) * width..frames * width] == silence[..] {
        frames -= 1;
    }
    frames
}

pub fn collect_c140(vgm: &VgmFile) -> (Vec<u8>, Vec<C140Sample>, Vec<C140Off>, Vec<String>) {
    let ids: BTreeSet<i32> = vgm
        .writes
        .iter()
        .filter(|w| w.chip == "c140")
        .map(|w| w.chip_id as i32)
        .collect();
    let mut roms: IndexMap<i32, Vec<u8>> = IndexMap::new();
    for cid in ids {
        roms.insert(cid, assemble_rom(vgm, "c140", cid as u8, 0, false, true));
    }
    let mut warnings: Vec<String> = Vec::new();
    let mut offs: Vec<C140Off> = Vec::new();
    if !roms.values().any(|r| !r.is_empty()) && roms.is_empty() {
        return (Vec::new(), Vec::new(), offs, warnings);
    }

    let mut reg = [[0i32; 16]; C140_VOICES];
    let mut keyed = [false; C140_VOICES];
    let mut hits: Vec<C140Hit> = Vec::new();
    let mut active: Vec<Option<usize>> = vec![None; C140_VOICES];
    let mut vol_last = [(0i32, 0i32); C140_VOICES];
    let mut freq_last = [0i32; C140_VOICES];

    for w in &vgm.writes {
        if w.chip != "c140" {
            continue;
        }
        let addr = w.reg as i64 & 0x1FF;
        if addr >= 0x180 {
            continue;
        }
        let voice = (addr >> 4) as usize;
        let r = (addr & 0x0F) as usize;
        let val = w.val as i32 & 0xFF;
        if r == 0x05 {
            if (val & 0x80) != 0 || ((val & 0x40) != 0 && keyed[voice]) {
                let st = (reg[voice][0x06] << 8) | reg[voice][0x07];
                let en = (reg[voice][0x08] << 8) | reg[voice][0x09];
                let lp = (reg[voice][0x0A] << 8) | reg[voice][0x0B];
                if let Some(idx) = active[voice] {
                    hits[idx].off_sample = w.sample;
                }
                let hit = C140Hit::new(
                    w.sample,
                    w.chip_id as i32,
                    voice as i32,
                    st,
                    en,
                    lp,
                    (val & 0x10) != 0,
                    (val & 0x08) != 0,
                    reg[voice][0x04],
                    (reg[voice][0x02] << 8) | reg[voice][0x03],
                    reg[voice][0x01],
                    reg[voice][0x00],
                );
                vol_last[voice] = (hit.vol_l, hit.vol_r);
                freq_last[voice] = hit.freq;
                hits.push(hit);
                active[voice] = Some(hits.len() - 1);
                keyed[voice] = true;
            } else {
                if keyed[voice] {
                    offs.push(C140Off {
                        sample_time: w.sample,
                        chip_id: w.chip_id as i32,
                        voice: voice as i32,
                    });
                    if let Some(idx) = active[voice] {
                        hits[idx].off_sample = w.sample;
                        active[voice] = None;
                    }
                }
                keyed[voice] = false;
            }
        } else {
            reg[voice][r] = val;
            if keyed[voice] {
                if let Some(idx) = active[voice] {
                    let hit = &mut hits[idx];
                    if r == 0x00 || r == 0x01 {
                        let pair = (reg[voice][0x01], reg[voice][0x00]);
                        if pair != vol_last[voice] {
                            hit.vol_pts.push((w.sample, pair.0, pair.1));
                            vol_last[voice] = pair;
                        }
                    } else if r == 0x02 || r == 0x03 {
                        let f = (reg[voice][0x02] << 8) | reg[voice][0x03];
                        if f != freq_last[voice] {
                            hit.freq_pts.push((w.sample, f));
                            freq_last[voice] = f;
                        }
                    }
                }
            }
        }
    }

    if hits.is_empty() {
        return (Vec::new(), Vec::new(), offs, warnings);
    }
    if !roms.values().any(|r| !r.is_empty()) {
        warnings.push(
            "no C140 ROM block (0x8D); PCM notes will be silent until samples are added".into(),
        );
        return (Vec::new(), Vec::new(), offs, warnings);
    }

    let mut by_key: IndexMap<(i32, i32, i32, Option<i32>, i64, bool), C140Sample> = IndexMap::new();
    let mut undumped = 0i64;
    let c140_type = vgm.c140_type as i32 & 0x03;
    for h in hits {
        let rom: &[u8] = roms.get(&h.chip_id).map(|r| r.as_slice()).unwrap_or(&[]);
        if rom.is_empty() {
            continue;
        }
        let end_addr = h.end.max(h.start + 1);
        let n_frames = end_addr - h.start;
        let mut loop_off: Option<i32> = None;
        if h.loop_on && h.start <= h.loop_ && h.loop_ < end_addr {
            loop_off = Some(h.loop_ - h.start);
        }
        let base = c140_rom_addr(h.bank, h.start, c140_type);
        if base < 0 || base >= rom.len() as i64 || !covered(vgm, h.chip_id, base) {
            undumped += 1;
            continue;
        }
        let depth = if h.compressed { 16 } else { 8 };
        let key = (
            h.chip_id,
            h.start,
            end_addr,
            loop_off,
            base,
            h.compressed,
        );
        if !by_key.contains_key(&key) {
            let body = decode_body(rom, base, n_frames as i64, h.compressed);
            let width = if h.compressed { 2 } else { 1 };
            let frames = trim_frames(&body, width, loop_off);
            if let Some(off) = loop_off {
                if off as i64 >= frames as i64 {
                    loop_off = None;
                }
            }
            let smp = C140Sample {
                start: h.start,
                data: body[..frames * width].to_vec(),
                loop_start: loop_off,
                loop_end: if loop_off.is_some() {
                    Some(frames as i32)
                } else {
                    None
                },
                hits: Vec::new(),
                depth,
            };
            by_key.insert(key, smp);
        }
        by_key.get_mut(&key).unwrap().hits.push(h);
    }
    if undumped != 0 {
        warnings.push(format!(
            "{undumped} C140 triggers point at ROM that the VGM does not dump; those notes are silent"
        ));
    }
    let mut samples: Vec<C140Sample> = by_key.into_values().collect();
    samples.sort_by(|a, b| {
        let a_loop = a.loop_start.filter(|&v| v != 0).unwrap_or(-1);
        let b_loop = b.loop_start.filter(|&v| v != 0).unwrap_or(-1);
        (a.start, a_loop).cmp(&(b.start, b_loop))
    });
    let rom0 = roms
        .get(&0)
        .cloned()
        .unwrap_or_else(|| roms.values().next().cloned().unwrap_or_default());
    (rom0, samples, offs, warnings)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn furnace_clock_is_two_thirds() {
        assert_eq!(c140_furnace_clock(12288000), 8192000);
        assert_eq!(c140_furnace_clock(0), 8192000);
        assert_eq!(c140_furnace_clock(18874368), 12582912);
    }

    #[test]
    fn rate_uses_reference_divisor() {
        assert!((c140_rate(12288000, 0x1000) - 2666.6666666666665).abs() < 1e-9);
        assert_eq!(c140_rate(12288000, 0), 0.0);
        assert!((c140_rate(0, 0x800) - 1333.3333333333333).abs() < 1e-9);
    }

    #[test]
    fn rom_address_modes() {
        assert_eq!(c140_rom_addr(0x20, 0x1234, 0), 0x81234);
        assert_eq!(c140_rom_addr(0x20, 0x1234, 1), 0x101234);
        assert_eq!(c140_rom_addr(0x20, 0x1234, 2), 0x201234);
        assert_eq!(c140_rom_addr(0x300, 0x1234, 0), 0x1234);
    }

    #[test]
    fn mulaw_table_sign_and_range() {
        assert_eq!(mulaw_i16(0), 0);
        assert_eq!(mulaw_i16(1), 256);
        assert_eq!(mulaw_i16(128), -256);
        assert_eq!(mulaw_i16(0xFF), -17408);
        assert_eq!(C140_DECOMP[0], 0);
        assert_eq!(C140_DECOMP[128], -256);
    }

    #[test]
    fn step_merge_and_refit() {
        assert_eq!(
            merge_step_writes(&[(10, 1), (20, 2), (40, 3)], 16),
            vec![(20, 2), (40, 3)]
        );
        let mut hit = C140Hit::new(100, 0, 0, 0, 10, 0, false, false, 0, 0, 0, 0);
        hit.freq_pts = vec![(200, 1000)];
        assert_eq!(hit_played_freq(&hit), 1000);
        hit.freq_pts = vec![(1000, 1000)];
        hit.freq = 500;
        assert_eq!(hit_played_freq(&hit), 500);
        hit.freq = 0;
        assert_eq!(hit_played_freq(&hit), 1000);
    }
}
