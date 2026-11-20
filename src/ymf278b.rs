//! Yamaha YMF278B (OPL4): OPL3-shaped FM plus 24 wave channels.
//!
//! The FM half is the OPL3 register map and is collected by the shared OPL path.
//! This module owns the wave half: `$D0 02 aa dd` writes 24 slots of 10 registers
//! each (`0x08-0xF7`, slot = (reg-8) % 24, sub-register = (reg-8) / 24):
//!
//! ```text
//! 0 sample ID low          1 f-num LSB + sample ID bit 8
//! 2 f-num MSB + octave     3 total level
//! 4 key/damp/LFO/pan       5 LFO speed + vibrato
//! 6 AR/D1R                 7 D1L/D2R
//! 8 RC/RR                  9 AM
//! ```
//!
//! The sample ID selects a 12-byte descriptor in the wave memory: IDs below 384
//! live in the YRW801 ROM (descriptor at `id * 12`), the rest in the card's RAM
//! (descriptor at `(id - 384) * 12`). A descriptor is
//!
//! ```text
//! byte 0   bits 7-6 sample format (0 = 8-bit, 1 = 12-bit, 2 = 16-bit),
//!          bits 5-0 start address bits 16-21
//! byte 1   start address bits 8-15
//! byte 2   start address bits 0-7
//! byte 3/4 loop address (16 bits, offset from the start)
//! byte 5/6 end address (16 bits, a *negative* length: len = 0x10000 - end)
//! byte 7-11 envelope/LFO defaults (not used here)
//! ```
//!
//! The wave memory is the ROM at 0x000000-0x1FFFFF and the RAM at 0x200000+.
//! Playback rate is `clock * step / (768 * 65536)` where
//! `step = (fnum + 1024) << (5 + octave)`.
//!
//! Furnace's OPL4 core does not load the YRW801 ROM itself, so this extracts the
//! sample bodies and embeds them as .fur samples.

use std::env;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

use flate2::read::DeflateDecoder;
use indexmap::IndexMap;

use crate::parity::{py_int, py_round};
use crate::vgm::VgmFile;

pub const OPL4_WAVE_SLOTS: usize = 24;
pub const OPL4_ROM_SAMPLES: i32 = 384; // sample IDs below this read the YRW801 ROM
pub const OPL4_RAM_BASE: i64 = 0x200000; // RAM window in the wave address space
pub const OPL4_DESCRIPTOR: usize = 12;
pub const OPL4_DEFAULT_CLOCK: u32 = 33868800;
// Some drivers (spc2vgm is one) key a wave voice with TL = 0x7F (silent) and
// pan 8, then write the real level, pan and sometimes the first pitch step one
// to two milliseconds later.  At a module's row resolution that is still the
// attack, so those writes belong to the hit's initial state instead of
// becoming a step on the next row.  A real mid-note move comes much later.
pub const OPL4_ATTACK_GAP: i64 = 128;
// A TL=0/pan-8 mute this close to a key-off is the driver's trailing silence;
// the note-off already stops the voice. A mute earlier than this is real
// silence and must survive to the pattern.
pub const OPL4_TAIL_MUTE: i64 = 2048;
pub const SMP_DEPTH_8BIT: i32 = 8;
pub const SMP_DEPTH_16BIT: i32 = 16;

/// Pan nibble -> Furnace `08xy` that comes back as the same nibble.
///
/// ymf278.cpp's table is 0 = centre, 1-7 = right (left falls), 8 = both off,
/// 9-15 = left (right falls). Furnace stores that nibble from the effect as
/// `8 ^ min(convertPanSplitToLinearLR(x, y, 15) + 1, 15)` (opl.cpp:2267),
/// which is not the naive gain split: pan 0 is centre, not hard left, and the
/// sides are reversed from the raw level order. Inverting the formula here
/// keeps the drum slots (pan 0) centred and every other slot on its
/// hardware side. Pan 8 has no `08xy` form (it would need convert == -1);
/// the caller mutes the note through the volume instead.
pub fn opl4_pan_fx(pan: i32) -> i32 {
    let pan = pan & 0x0F;
    if pan == 8 {
        return 0;
    }
    let n = 8 ^ pan;
    let convert = if n == 15 { 14 } else { n - 1 };
    let (left, right) = if convert <= 7 {
        (15, 2 * convert + i32::from(convert != 0))
    } else {
        (30 - 2 * convert, 15)
    };
    ((left & 15) << 4) | (right & 15)
}

/// The project's own furnace/ folder (next to furnace.exe), and the same
/// relative location under the current working directory.
fn _project_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn _yrw801_fallbacks() -> Vec<PathBuf> {
    let mut out = vec![_project_root().join("furnace").join("yrw801.rom")];
    if let Ok(cwd) = env::current_dir() {
        out.push(cwd.join("furnace").join("yrw801.rom"));
        out.push(cwd.join("yrw801.rom"));
    }
    out
}

fn u16_at(data: &[u8], pos: usize) -> Option<u16> {
    let b = data.get(pos..pos + 2)?;
    Some(u16::from_le_bytes([b[0], b[1]]))
}

fn u32_at(data: &[u8], pos: usize) -> Option<u32> {
    let b = data.get(pos..pos + 4)?;
    Some(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
}

fn u64_at(data: &[u8], pos: usize) -> Option<u64> {
    let b = data.get(pos..pos + 8)?;
    Some(u64::from_le_bytes([
        b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7],
    ]))
}

/// Find the end-of-central-directory record, ignoring a trailing comment.
fn zip_eocd(data: &[u8]) -> Option<usize> {
    if data.len() < 22 {
        return None;
    }
    let first = data.len().saturating_sub(22 + 0xFFFF);
    for pos in (first..=data.len() - 22).rev() {
        if u32_at(data, pos) == Some(0x0605_4B50) {
            let comment = u16_at(data, pos + 20).unwrap_or(0) as usize;
            if pos + 22 + comment <= data.len() {
                return Some(pos);
            }
        }
    }
    None
}

/// Central-directory start and entry count, including the ZIP64 records.
fn zip_directory(data: &[u8], eocd: usize) -> Option<(u64, u64)> {
    let mut count = u16_at(data, eocd + 10)? as u64;
    let mut offset = u32_at(data, eocd + 16)? as u64;
    if (count == 0xFFFF || offset == 0xFFFF_FFFF) && eocd >= 20 {
        // ZIP64 end-of-central-directory locator.
        if u32_at(data, eocd - 20) == Some(0x0706_4B50) {
            let z64 = u64_at(data, eocd - 20 + 8)? as usize;
            if u32_at(data, z64) == Some(0x0606_4B50) {
                count = u64_at(data, z64 + 32)?;
                offset = u64_at(data, z64 + 48)?;
            }
        }
    }
    Some((offset, count))
}

fn zip_entry_bytes(
    data: &[u8],
    local_offset: usize,
    compressed: usize,
    method: u16,
) -> Vec<u8> {
    if u32_at(data, local_offset) != Some(0x0403_4B50) {
        return Vec::new();
    }
    let Some(name_len) = u16_at(data, local_offset + 26) else {
        return Vec::new();
    };
    let Some(extra_len) = u16_at(data, local_offset + 28) else {
        return Vec::new();
    };
    let start = local_offset + 30 + name_len as usize + extra_len as usize;
    let Some(raw) = data.get(start..start.saturating_add(compressed)) else {
        return Vec::new();
    };
    match method {
        0 => raw.to_vec(),
        8 => {
            let mut out = Vec::new();
            let mut decoder = DeflateDecoder::new(raw);
            if decoder.read_to_end(&mut out).is_err() {
                return Vec::new();
            }
            out
        }
        _ => Vec::new(),
    }
}

/// `zipfile.ZipFile(candidate)`: the first entry with a 2 MB `.rom` name.
fn zip_read_rom(data: &[u8]) -> Vec<u8> {
    let Some(eocd) = zip_eocd(data) else {
        return Vec::new();
    };
    let Some((dir_offset, count)) = zip_directory(data, eocd) else {
        return Vec::new();
    };
    let mut pos = dir_offset as usize;
    for _ in 0..count {
        if u32_at(data, pos) != Some(0x0201_4B50) {
            break;
        }
        let Some(method) = u16_at(data, pos + 10) else {
            break;
        };
        let Some(compressed) = u32_at(data, pos + 20) else {
            break;
        };
        let Some(uncompressed) = u32_at(data, pos + 24) else {
            break;
        };
        let Some(name_len) = u16_at(data, pos + 28) else {
            break;
        };
        let Some(extra_len) = u16_at(data, pos + 30) else {
            break;
        };
        let Some(comment_len) = u16_at(data, pos + 32) else {
            break;
        };
        let Some(local_offset) = u32_at(data, pos + 42) else {
            break;
        };
        let name_start = pos + 46;
        let Some(name) = data.get(name_start..name_start + name_len as usize) else {
            break;
        };
        if uncompressed == 0x0020_0000 && name.to_ascii_lowercase().ends_with(b".rom") {
            return zip_entry_bytes(data, local_offset as usize, compressed as usize, method);
        }
        pos = name_start + name_len as usize + extra_len as usize + comment_len as usize;
    }
    Vec::new()
}

fn _read_yrw801(candidate: &Path) -> Vec<u8> {
    if candidate
        .extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("zip"))
    {
        let Ok(bytes) = fs::read(candidate) else {
            return Vec::new();
        };
        return zip_read_rom(&bytes);
    }
    let Ok(data) = fs::read(candidate) else {
        return Vec::new();
    };
    if data.len() == 0x200000 { data } else { Vec::new() }
}

/// Read the 2 MB YRW801 ROM from a `.rom` file or a `.zip`.
///
/// The order is: an explicit `path`, `VGM2151FUR_YRW801`, then the usual
/// in_vgm/Winamp location. Furnace does not load the ROM itself (as of this
/// version), so the converter extracts the ROM samples and embeds them.
pub fn load_yrw801(path: Option<&Path>) -> Vec<u8> {
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Some(path) = path {
        candidates.push(path.to_path_buf());
    }
    if let Some(raw) = env::var_os("VGM2151FUR_YRW801") {
        if !raw.is_empty() {
            candidates.push(PathBuf::from(raw));
        }
    }
    candidates.extend(_yrw801_fallbacks());
    for candidate in candidates {
        if !candidate.is_file() {
            continue;
        }
        let data = _read_yrw801(&candidate);
        if !data.is_empty() {
            return data;
        }
    }
    Vec::new()
}

/// The 16.16 sample increment for an octave/f-num pair (ymf278.cpp).
pub fn opl4_step(octave: i32, fnum: i32) -> i64 {
    if octave == -8 {
        return 0;
    }
    let step = (fnum + 1024) as i64;
    let shift = 5 + octave;
    // Octave -7..-6 shifts right; the reference core does the same. Python
    // raises on a negative left shift instead of wrapping like C.
    if shift < 0 {
        return step >> -shift;
    }
    (step << shift) & 0xFFFF_FFFF
}

/// Wave samples per second: output rate clock/768 times step/65536.
pub fn opl4_rate(clock: u32, step: i64) -> f64 {
    let clock = if clock != 0 {
        clock
    } else {
        OPL4_DEFAULT_CLOCK
    };
    (clock as f64) * ((step & 0xFFFF_FFFF) as f64) / (768.0 * 65536.0)
}

#[derive(Clone, Debug, PartialEq)]
pub struct Opl4WaveHit {
    pub sample_time: i64,
    pub slot: usize,
    pub wave: i32,
    pub step: i64,
    pub level: i32,
    pub pan: i32,
    pub end_time: i64,
    pub vol_pts: Vec<(i64, i32)>,
    pub pan_pts: Vec<(i64, i32)>,
    pub pitch_pts: Vec<(i64, i64)>,
}

impl Opl4WaveHit {
    /// Python's `Opl4WaveHit(...)` with every defaulted field left at its default.
    pub fn new(
        sample_time: i64,
        slot: usize,
        wave: i32,
        step: i64,
        level: i32,
        pan: i32,
    ) -> Self {
        Opl4WaveHit {
            sample_time,
            slot,
            wave,
            step,
            level,
            pan,
            end_time: 0,
            vol_pts: Vec::new(),
            pan_pts: Vec::new(),
            pitch_pts: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Opl4WaveSample {
    pub wave: i32,
    pub data: Vec<u8>, // signed 16-bit little-endian
    pub loop_start: Option<i64>,
    pub loop_end: Option<i64>,
    pub hits: Vec<Opl4WaveHit>,
}

impl Opl4WaveSample {
    pub fn frames(&self) -> i64 {
        (self.data.len() / 2) as i64
    }

    /// Most common nonzero key-on step (0 when unknown).
    pub fn mode_step(&self) -> i64 {
        let mut steps: IndexMap<i64, usize> = IndexMap::new();
        for h in &self.hits {
            if h.step != 0 {
                *steps.entry(h.step).or_insert(0) += 1;
            }
        }
        let mut best: Option<(i64, usize)> = None;
        // Counter.most_common(1) sorts by count only; a stable scan keeps the
        // first-seen step on ties.
        for (step, count) in steps {
            let replace = match best {
                None => true,
                Some((_, top)) => count > top,
            };
            if replace {
                best = Some((step, count));
            }
        }
        best.map_or(0, |(step, _)| step)
    }

    pub fn c4_rate(&self, clock: u32) -> i64 {
        i64::max(1000, py_int(py_round(opl4_rate(clock, self.mode_step()))))
    }
}

#[derive(Default)]
struct _Slot {
    wave: i32,
    fnum: i32,
    octave: i32,
    tl: i32,
    pan: i32,
    key: bool,
}

fn _descriptor(mem: &[u8], offset: usize) -> Option<(i32, i64, i64, i64)> {
    let data = mem.get(offset..offset.saturating_add(OPL4_DESCRIPTOR))?;
    if data.len() < OPL4_DESCRIPTOR {
        return None;
    }
    let bits = ((data[0] >> 6) & 0x03) as i32;
    if bits == 3 {
        return None;
    }
    let start = (data[2] as i64) | ((data[1] as i64) << 8) | (((data[0] & 0x3F) as i64) << 16);
    let loop_ = (data[4] as i64) | ((data[3] as i64) << 8);
    let end = (data[6] as i64) | ((data[5] as i64) << 8);
    Some((bits, start, loop_, end))
}

fn _s16(val: i32) -> i32 {
    let val = val & 0xFFFF;
    if val & 0x8000 != 0 { val - 0x10000 } else { val }
}

fn _byte(mem: &[u8], addr: i64) -> u8 {
    if addr >= 0 && (addr as usize) < mem.len() {
        mem[addr as usize]
    } else {
        0
    }
}

fn _put_s16(out: &mut [u8], pos: usize, val: i32) {
    out[pos * 2..pos * 2 + 2].copy_from_slice(&(val as i16).to_le_bytes());
}

/// Extract `frames` samples in the descriptor's format as int16 LE.
fn _body(mem: &[u8], start: i64, frames: i64, bits: i32) -> Vec<u8> {
    if frames <= 0 {
        return Vec::new();
    }
    let mut out = vec![0u8; frames as usize * 2];
    let n = mem.len() as i64;
    if bits == 0 {
        // 8-bit signed, one byte per sample.
        for pos in 0..frames as usize {
            let addr = start + pos as i64;
            let b = if addr < n { _byte(mem, addr) } else { 0 };
            let val: i32 = if b & 0x80 != 0 {
                ((b as i32) - 0x100) << 8
            } else {
                (b as i32) << 8
            };
            _put_s16(&mut out, pos, val);
        }
    } else if bits == 1 {
        // 12-bit, two samples per three bytes.
        for pos in 0..frames as usize {
            let addr = start + ((pos / 2) as i64) * 3;
            let b0 = if addr < n { _byte(mem, addr) } else { 0 };
            let b1 = if addr + 1 < n { _byte(mem, addr + 1) } else { 0 };
            let b2 = if addr + 2 < n { _byte(mem, addr + 2) } else { 0 };
            let val: i32 = if pos & 1 != 0 {
                ((b2 as i32) << 8) | ((b1 as i32) & 0xF0)
            } else {
                ((b0 as i32) << 8) | (((b1 as i32) << 4) & 0xF0)
            };
            _put_s16(&mut out, pos, _s16(val));
        }
    } else {
        // 16-bit big-endian.
        for pos in 0..frames as usize {
            let addr = start + pos as i64 * 2;
            let b0 = if addr < n { _byte(mem, addr) } else { 0 };
            let b1 = if addr + 1 < n { _byte(mem, addr + 1) } else { 0 };
            _put_s16(&mut out, pos, _s16(((b0 as i32) << 8) | (b1 as i32)));
        }
    }
    out
}

/// Keep a held slot's f-num/octave change as a pitch point.
fn record_step(
    hits: &mut [Opl4WaveHit],
    active: &[Option<usize>],
    slots: &[_Slot],
    slot: usize,
    t: i64,
) {
    let Some(idx) = active[slot] else { return };
    let hit = &mut hits[idx];
    let st = &slots[slot];
    let step = opl4_step(st.octave, st.fnum);
    let last = hit.pitch_pts.last().map_or(hit.step, |p| p.1);
    if step == last {
        return;
    }
    if t - hit.sample_time <= OPL4_ATTACK_GAP {
        hit.step = step;
        return;
    }
    if hit.pitch_pts.last().is_some_and(|p| p.0 == t) {
        let n = hit.pitch_pts.len();
        hit.pitch_pts[n - 1] = (t, step);
    } else {
        hit.pitch_pts.push((t, step));
    }
}

/// Keep a held slot's pan change as a point, or fold the attack.
fn record_pan(
    hits: &mut [Opl4WaveHit],
    active: &[Option<usize>],
    slot: usize,
    t: i64,
    pan: i32,
) {
    let Some(idx) = active[slot] else { return };
    let hit = &mut hits[idx];
    if pan == hit.pan {
        return;
    }
    if t - hit.sample_time <= OPL4_ATTACK_GAP && hit.pan == 8 {
        // Only a muted key-on is the two-stage attack; a pan move on an
        // already audible note is a real mid-note point.
        hit.pan = pan;
        return;
    }
    let last = hit.pan_pts.last().map_or(hit.pan, |p| p.1);
    if pan == last {
        return;
    }
    if hit.pan_pts.last().is_some_and(|p| p.0 == t) {
        let n = hit.pan_pts.len();
        hit.pan_pts[n - 1] = (t, pan);
    } else {
        hit.pan_pts.push((t, pan));
    }
}

/// Group the YMF278B wave plays into samples.
///
/// `rom` is the YRW801 image (2 MB) and `ram` the 0x87 RAM dump. ROM sample
/// IDs (0-383) need `rom`; RAM IDs (384-511) need `ram`.
pub fn collect_opl4_wave(
    vgm: &VgmFile,
    rom: &[u8],
    ram: &[u8],
) -> (Vec<Opl4WaveSample>, Vec<String>) {
    let mut warnings: Vec<String> = Vec::new();
    let mut slots: Vec<_Slot> = (0..OPL4_WAVE_SLOTS).map(|_| _Slot::default()).collect();
    let mut hits: Vec<Opl4WaveHit> = Vec::new();
    // The hit currently sounding on a slot, so a $5n level write inside the
    // note becomes a volume step instead of being lost. An index keeps the
    // reference's shared-object mutation.
    let mut active: Vec<Option<usize>> = vec![None; OPL4_WAVE_SLOTS];

    for w in &vgm.writes {
        if w.chip != "ymf278b" || (w.reg >> 8) != 2 {
            continue;
        }
        let r = (w.reg & 0xFF) as i32;
        if !(0x08..=0xF7).contains(&r) {
            continue;
        }
        let slot = ((r - 8) as usize) % OPL4_WAVE_SLOTS;
        let sub = ((r - 8) as usize) / OPL4_WAVE_SLOTS;
        let val = w.val as i32;
        let t = w.sample;
        if sub == 0 {
            slots[slot].wave = (slots[slot].wave & 0x100) | (val & 0xFF);
        } else if sub == 1 {
            slots[slot].wave = (slots[slot].wave & 0xFF) | ((val & 0x01) << 8);
            slots[slot].fnum = (slots[slot].fnum & 0x380) | (val >> 1);
            record_step(&mut hits, &active, &slots, slot, t);
        } else if sub == 2 {
            slots[slot].fnum = (slots[slot].fnum & 0x07F) | ((val & 0x07) << 7);
            let octave = (val >> 4) & 0x0F;
            slots[slot].octave = if octave & 0x08 != 0 { octave - 16 } else { octave };
            record_step(&mut hits, &active, &slots, slot, t);
        } else if sub == 3 {
            let level = (val >> 1) & 0x7F;
            if slots[slot].key {
                if let Some(idx) = active[slot] {
                    let hit = &mut hits[idx];
                    let last = hit.vol_pts.last().map_or(hit.level, |p| p.1);
                    if level != last {
                        if t - hit.sample_time <= OPL4_ATTACK_GAP && hit.level == 0x7F {
                            // Keyed muted, then the real attack follows a tick
                            // later: that is the note's opening level. An audible
                            // key-on keeps its level; a later write is a real
                            // decay step, not a replacement attack.
                            hit.level = level;
                        } else if hit.vol_pts.last().is_some_and(|p| p.0 == t) {
                            let n = hit.vol_pts.len();
                            hit.vol_pts[n - 1] = (t, level);
                        } else {
                            hit.vol_pts.push((t, level));
                        }
                    }
                }
            }
            slots[slot].tl = level;
        } else if sub == 4 {
            let pan = val & 0x0F;
            let key = (w.val & 0x80) != 0;
            if key && !slots[slot].key {
                let hit = Opl4WaveHit::new(
                    t,
                    slot,
                    slots[slot].wave,
                    opl4_step(slots[slot].octave, slots[slot].fnum),
                    slots[slot].tl,
                    pan,
                );
                hits.push(hit);
                active[slot] = Some(hits.len() - 1);
            } else if slots[slot].key && !key {
                // Attach the release now. Waiting for the next key-on (or
                // the end of the file) lost every intermediate note-off.
                if let Some(idx) = active[slot] {
                    if hits[idx].end_time == 0 {
                        hits[idx].end_time = t;
                    }
                }
                active[slot] = None;
            } else if slots[slot].key {
                record_pan(&mut hits, &active, slot, t, pan);
            }
            slots[slot].pan = pan;
            slots[slot].key = key;
        }
    }

    if hits.is_empty() {
        return (Vec::new(), warnings);
    }

    let mut by_wave: IndexMap<i32, Opl4WaveSample> = IndexMap::new();
    let mut rom_missing = false;
    for hit in hits {
        let wave = hit.wave;
        let (desc, mem, base): (Option<(i32, i64, i64, i64)>, &[u8], i64) =
            if wave < OPL4_ROM_SAMPLES {
                if rom.is_empty() {
                    rom_missing = true;
                    continue;
                }
                (
                    _descriptor(rom, wave as usize * OPL4_DESCRIPTOR),
                    rom,
                    0,
                )
            } else {
                if ram.is_empty() {
                    continue;
                }
                (
                    _descriptor(
                        ram,
                        (wave - OPL4_ROM_SAMPLES) as usize * OPL4_DESCRIPTOR,
                    ),
                    ram,
                    OPL4_RAM_BASE,
                )
            };
        let Some((bits, start, loop_, end)) = desc else {
            continue;
        };
        let mut frames = 0x10000 - end;
        if frames <= 2 {
            continue;
        }
        let offset = start - base;
        if offset < 0 || offset >= mem.len() as i64 {
            continue;
        }
        if let Some(sample) = by_wave.get_mut(&wave) {
            sample.hits.push(hit);
            continue;
        }
        // A loop point in the last two frames is the descriptor's loop
        // padding, not a sustain: the reference modules trim the body to
        // the loop point and play it once. A full 0x10000-frame body
        // wraps the slot's 16-bit position counter, so its loop address
        // cannot take effect either; those are one-shot too.
        let loop_start: Option<i64> = if frames >= 0x10000 {
            None
        } else if loop_ >= frames - 2 && loop_ > 0 {
            frames = loop_;
            None
        } else {
            // Loop address 0 is the start of the body, not "no loop".
            // spc2vgm uses it for single-cycle waveforms (Super Bomberman
            // 5's short sine bodies), which must repeat for the note.
            if loop_ < frames { Some(loop_) } else { None }
        };
        let data = _body(mem, offset, frames, bits);
        let sample = Opl4WaveSample {
            wave,
            data,
            loop_start,
            loop_end: if loop_start.is_some() { Some(frames) } else { None },
            hits: vec![hit],
        };
        by_wave.insert(wave, sample);
    }

    if rom_missing {
        warnings.push(
            "YMF278B wave notes use the YRW801 ROM; set VGM2151FUR_YRW801 to \
             its path to extract those samples"
                .to_string(),
        );
    }
    let mut samples: Vec<Opl4WaveSample> = by_wave.into_values().collect();
    samples.sort_by_key(|s| s.wave);
    (samples, warnings)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vgm::ChipWrite;

    const CLOCK: u32 = 33868800;

    fn descriptor(bits: i32, start: i64, loop_: i64, frames: i64) -> Vec<u8> {
        let end = (0x10000 - frames) & 0xFFFF;
        vec![
            (((bits << 6) as i64) | ((start >> 16) & 0x3F)) as u8,
            ((start >> 8) & 0xFF) as u8,
            (start & 0xFF) as u8,
            ((loop_ >> 8) & 0xFF) as u8,
            (loop_ & 0xFF) as u8,
            ((end >> 8) & 0xFF) as u8,
            (end & 0xFF) as u8,
            0,
            0,
            0,
            0,
            0,
        ]
    }

    fn rom(wave: i32, sample: &[u8]) -> Vec<u8> {
        let start = 0x1000usize;
        let mut mem = vec![0u8; start + sample.len() + 16];
        let off = wave as usize * 12;
        mem[off..off + 12].copy_from_slice(&descriptor(0, 0x1000, 0x10, sample.len() as i64));
        mem[start..start + sample.len()].copy_from_slice(sample);
        mem
    }

    fn wave_reg(slot: usize, sub: usize) -> u32 {
        (2 << 8) | (0x08 + slot + sub * 24) as u32
    }

    fn wave_note(t: i64, wave: i32, slot: usize, fnum: i32, octave: i32, tl: i32, off: i64) -> Vec<ChipWrite> {
        vec![
            ChipWrite::new(t, "ymf278b", 0, wave_reg(slot, 0), (wave & 0xFF) as u32),
            ChipWrite::new(
                t,
                "ymf278b",
                0,
                wave_reg(slot, 1),
                (((fnum & 0x7F) << 1) | ((wave >> 8) & 1)) as u32,
            ),
            ChipWrite::new(
                t,
                "ymf278b",
                0,
                wave_reg(slot, 2),
                (((octave & 0xF) << 4) | ((fnum >> 7) & 7)) as u32,
            ),
            ChipWrite::new(t, "ymf278b", 0, wave_reg(slot, 3), ((tl << 1) & 0xFE) as u32),
            ChipWrite::new(t, "ymf278b", 0, wave_reg(slot, 4), 0x8F),
            ChipWrite::new(off, "ymf278b", 0, wave_reg(slot, 4), 0x0F),
        ]
    }

    fn vgm_with(writes: Vec<ChipWrite>) -> VgmFile {
        let mut v = VgmFile::default();
        v.ymf278b_clock = CLOCK;
        v.writes = writes;
        v
    }

    #[test]
    fn step_matches_the_octave_formula() {
        assert_eq!(opl4_step(2, 0x200), (0x200 + 1024) << 7);
        assert_eq!(opl4_step(-4, 0x200), (0x200 + 1024) << 1);
        assert_eq!(opl4_step(-6, 0x200), (0x200 + 1024) >> 1);
        assert_eq!(opl4_step(-7, 0x200), (0x200 + 1024) >> 2);
        assert_eq!(opl4_step(-8, 0x200), 0);
        // The rate is the 768-divider output rate times step/65536.
        let step = opl4_step(2, 0x200);
        assert_eq!(opl4_rate(CLOCK, step), CLOCK as f64 * step as f64 / (768.0 * 65536.0));
        assert_eq!(opl4_rate(0, 65536), OPL4_DEFAULT_CLOCK as f64 / 768.0);
    }

    #[test]
    fn pan_inverts_the_furnace_conversion() {
        fn furnace_pan(fx: i32) -> i32 {
            let left = (fx >> 4) & 15;
            let right = fx & 15;
            let linear = if left == right {
                7
            } else {
                let frac = (right - left) as f64 / i32::max(left, right) as f64;
                ((1.0 + frac) * 0.5 * 15.0) as i32
            };
            8 ^ i32::min(linear + 1, 15)
        }
        assert_eq!(opl4_pan_fx(0), 0xFF);
        for pan in [0, 1, 2, 3, 4, 5, 6, 7, 9, 10, 11, 12, 13, 14, 15] {
            assert_eq!(furnace_pan(opl4_pan_fx(pan)), pan, "pan {pan}");
        }
        assert_eq!(opl4_pan_fx(8), 0);
    }

    #[test]
    fn key_on_extracts_the_descriptor_sample() {
        let sample: Vec<u8> = (1..=20).collect();
        let rom = rom(16, &sample);
        let v = vgm_with(wave_note(100, 16, 0, 0x200, 2, 0x20, 8000));
        let (samples, warn) = collect_opl4_wave(&v, &rom, b"");
        assert!(warn.is_empty());
        assert_eq!(samples.len(), 1);
        let smp = &samples[0];
        assert_eq!(smp.wave, 16);
        assert_eq!(smp.frames(), 20);
        assert_eq!((smp.loop_start, smp.loop_end), (Some(0x10), Some(20)));
        assert_eq!(smp.hits.len(), 1);
        // 8-bit samples become sign-extended 16-bit little-endian frames.
        assert_eq!(&smp.data[0..2], &[0x00, 0x01]);
        assert_eq!(smp.hits[0].step, opl4_step(2, 0x200));
        assert_eq!(smp.hits[0].end_time, 8000);
    }

    #[test]
    fn zero_loop_address_loops_the_whole_16bit_body() {
        let data: Vec<u8> = (0..16).collect();
        let mut mem = vec![0u8; 0x1000 + data.len() + 16];
        mem[16 * 12..16 * 12 + 12].copy_from_slice(&descriptor(2, 0x1000, 0, 8));
        mem[0x1000..0x1000 + data.len()].copy_from_slice(&data);
        let v = vgm_with(wave_note(100, 16, 0, 0x200, 2, 0x20, 9000));
        let (samples, _) = collect_opl4_wave(&v, &mem, b"");
        let smp = &samples[0];
        assert_eq!((smp.loop_start, smp.loop_end), (Some(0), Some(8)));
        // Big-endian 16-bit pairs: 0x0001, 0x0203, ...
        assert_eq!(&smp.data[0..4], &[0x01, 0x00, 0x03, 0x02]);
    }

    #[test]
    fn ram_samples_read_the_ram_table_and_missing_rom_warns() {
        let mut ram = vec![0u8; 0x400];
        ram[0..12].copy_from_slice(&descriptor(0, 0x200200, 0, 8));
        ram[0x200..0x208].copy_from_slice(&[1, 2, 3, 4, 5, 6, 7, 8]);
        let v = vgm_with(wave_note(100, 384, 0, 0x200, 2, 0x20, 9000));
        let (samples, warn) = collect_opl4_wave(&v, b"", &ram);
        assert!(warn.is_empty());
        assert_eq!(samples.len(), 1);
        assert_eq!(samples[0].wave, 384);
        assert_eq!(samples[0].frames(), 8);

        // A ROM sample without the YRW801 image is dropped with one warning.
        let v = vgm_with(wave_note(100, 16, 0, 0x200, 2, 0x20, 9000));
        let (samples, warn) = collect_opl4_wave(&v, b"", b"");
        assert!(samples.is_empty());
        assert!(warn.iter().any(|w| w.contains("YRW801")), "{warn:?}");
    }
}
