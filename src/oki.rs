//! MSM6295 phrases and MSM6258 bursts for the Furnace writer.
//!
//! MSM6295 plays a phrase at one chip-wide rate. VGM command 0xB8 is the
//! command port plus the bank registers VGMPlay uses for an NMK112-style
//! mapper (regs 0x0E and 0x10-0x13). The phrase ADPCM is stored as Furnace
//! VOX and triggered on a fixed note; pitch comes from the chip clock and
//! pin 7, volume from the OKI nibble.
//!
//! MSM6258 is a single ADPCM stream. X68000 logs play it through DAC stream
//! control (data block 0x04 plus 0x90-0x95) aimed at chip type 0x17. A log
//! with no stream falls back to 0xB7 data-port bursts. Direct bursts shorter
//! than 4 bytes are the dummy bytes those stream logs also write, and are
//! dropped.
//!
//! Port of `vgm2151fur/oki.py` (v0.11.0).

use std::collections::{BTreeSet, HashMap};

use indexmap::IndexMap;

use crate::parity::{py_int, py_round};
use crate::vgm::{Msm6258Play, VGM_RATE, VgmFile};

// Furnace format 157: note 0 = C-(-5), 108 = C-4.
pub const FURNACE_C4: i32 = 108;
pub const OKI_VOICES: usize = 4;
// Nibbles 0..8 are the nine hardware steps (~3 dB). 9..15 are silent.
// Furnace's MSM6295 volume column is 0..8 and the platform writes nibble
// (8 - volume): column 8 is nibble 0 (full scale).
pub const OKI_SILENT_NIBBLE: i32 = 9;

// furnace/src/engine/platform/msm6295.cpp setFlags, v0.6.8.
// case 8 is 4_000_000/2. rateSel false -> divisor 132 (pin 7 high);
// rateSel true -> divisor 165 (pin 7 low). Effect 20xx uses the same pair:
// 0 = /132, 1 = /165.
//
// Python keeps this as a dict and only ever calls `.get`, so the port stores
// the insertion-ordered pairs and looks them up linearly.
pub const OKI_CLOCK_SEL: &[(u32, i32)] = &[
    (1_000_000, 0),
    (1_056_000, 1),
    (4_000_000, 2),
    (4_224_000, 3),
    (3_579_545, 4),
    (1_789_773, 5),
    (1_022_727, 6),
    (894_886, 7),
    (2_000_000, 8),
    (2_112_000, 9),
    (875_000, 10),
    (937_500, 11),
    (1_500_000, 12),
    (3_000_000, 13),
    (1_193_182, 14),
    (3_200_000, 15),
];
// Unbanked Furnace ROM is 256 KB with a 1 KB phrase book and 127 phrases.
pub const OKI_PLAIN_BYTES: usize = 250_000;
pub const OKI_PLAIN_PHRASES: usize = 120;

// Header 0x94 bits 0-1. Furnace effect 20xx is not this order: the platform
// sets msmDivider = max(2, 4 - (val & 3)), so 0=/1024, 1=/768, 2=/512.
pub const MSM6258_DIVIDERS: [i32; 4] = [1024, 768, 512, 512];
// One note-map instrument holds this many samples (Furnace notes 60..179).
pub const ADPCM_MAP_SLOTS: i32 = 120;
pub const MSM6258_MAP_NOTE: i32 = 60;
// A 1- or 2-byte 0xB7 write is the dummy the stream logs emit beside 0x95.
pub const MSM6258_MIN_BURST: usize = 4;
pub const MSM6258_CLOCK_SEL: &[(u32, i32)] = &[
    (4_000_000, 0),
    (4_096_000, 1),
    (8_000_000, 2),
    (8_192_000, 3),
];

/// Python `OKI_CLOCK_SEL.get(int(clock))`.
fn oki_clock_sel(clock: u32) -> Option<i32> {
    OKI_CLOCK_SEL
        .iter()
        .find(|(key, _)| *key == clock)
        .map(|(_, value)| *value)
}

/// Python `MSM6258_CLOCK_SEL.get(int(clock))`.
fn msm6258_clock_sel(clock: u32) -> Option<i32> {
    MSM6258_CLOCK_SEL
        .iter()
        .find(|(key, _)| *key == clock)
        .map(|(_, value)| *value)
}

/// OKI volume nibble -> Furnace column, or None when the chip stays silent.
pub fn oki_furnace_vol(nibble: i32) -> Option<i32> {
    let n = nibble & 0x0F;
    if n >= OKI_SILENT_NIBBLE {
        return None;
    }
    Some(8 - n)
}

/// Playback Hz. Pin 7 high divides the master clock by 132, low by 165.
pub fn oki_rate(clock: u32, pin7_high: bool) -> i32 {
    if clock == 0 {
        return 1;
    }
    let div = if pin7_high { 132 } else { 165 };
    (py_int(py_round(clock as f64 / div as f64)) as i32).max(1)
}

/// (clock Hz, pin 7 high) for one MSM6295. Chip 1 uses the extra header.
pub fn oki_chip_setup(vgm: &VgmFile, chip_id: i32) -> (u32, bool) {
    if chip_id == 0 {
        return (vgm.msm6295_clock as u32, vgm.msm6295_pin7);
    }
    let raw = vgm.extra_clocks.get(&0x17).copied().unwrap_or(0) as i64;
    if raw != 0 {
        return ((raw & 0x3FFFFFFF) as u32, (raw & 0x80000000) != 0);
    }
    (vgm.msm6295_clock as u32, vgm.msm6295_pin7)
}

/// Furnace FLAG body for one MSM6295.
pub fn oki_flag_text(clock: u32, pin7_high: bool, banked: bool) -> String {
    let mut lines: Vec<String> = Vec::new();
    if let Some(sel) = oki_clock_sel(clock) {
        lines.push(format!("clockSel={sel}"));
    } else if clock > 0 {
        lines.push(format!("customClock={clock}"));
    }
    if !pin7_high {
        lines.push("rateSel=true".to_string());
    }
    if banked {
        lines.push("isBanked=true".to_string());
    }
    lines.join("\n")
}

pub fn oki_needs_banked(samples: &[OkiSample]) -> bool {
    let total: usize = samples.iter().map(|s| s.data.len()).sum();
    samples.len() > OKI_PLAIN_PHRASES || total > OKI_PLAIN_BYTES
}

pub fn msm6258_flag_text(clock: u32) -> String {
    if let Some(sel) = msm6258_clock_sel(clock) {
        return format!("clockSel={sel}");
    }
    if clock > 0 {
        return format!("customClock={clock}");
    }
    String::new()
}

pub fn msm6258_divider(flags: i32) -> i32 {
    MSM6258_DIVIDERS[(flags & 3) as usize]
}

/// Furnace 20xx value, or None for the platform default (/512).
///
/// The C++ core does `msmDivider = max(2, 4 - (val & 3))`: effect 0 is
/// /1024, effect 1 is /768, effect 2 is /512.
pub fn msm6258_div_fx(divider: i32) -> Option<i32> {
    if divider == 1024 {
        return Some(0);
    }
    if divider == 768 {
        return Some(1);
    }
    None
}

/// Divider whose byte rate (clock/divider/2) is closest to the stream.
///
/// Header flags win a tie. `stream_hz` is the 0x92 value, in bytes per
/// second. With no stream, the header divider is used as-is.
pub fn msm6258_best_divider(clock: i32, flags: i32, stream_hz: i32) -> i32 {
    let header = msm6258_divider(flags);
    if stream_hz <= 0 || clock <= 0 {
        return header;
    }
    let mut best = header;
    let mut best_err = (clock as f64 / header as f64 / 2.0 - stream_hz as f64).abs();
    for div in [1024, 768, 512] {
        let err = (clock as f64 / div as f64 / 2.0 - stream_hz as f64).abs();
        if err < best_err {
            best = div;
            best_err = err;
        }
    }
    best
}

pub fn msm6258_ins_count(n_samples: i32) -> i32 {
    if n_samples <= 0 {
        return 0;
    }
    (n_samples + ADPCM_MAP_SLOTS - 1) / ADPCM_MAP_SLOTS
}

#[derive(Clone, Debug, PartialEq)]
pub struct OkiHit {
    pub sample_time: i64,
    pub voice: i32,
    pub vol: i32,
    pub end: i64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct OkiSample {
    pub chip_id: i32,
    pub phrase: i32,
    pub data: Vec<u8>,
    pub rate: i32,
    pub clock: u32,
    pub pin7: bool,
    pub name: String,
    pub hits: Vec<OkiHit>,
}

impl OkiSample {
    pub fn length(&self) -> usize {
        self.data.len()
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct AdpcmHit {
    pub sample_time: i64,
    pub end: i64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct AdpcmSample {
    pub chip_id: i32,
    pub data: Vec<u8>,
    pub rate: i32,
    pub clock: u32,
    pub divider: i32,
    pub name: String,
    pub hits: Vec<AdpcmHit>,
}

impl AdpcmSample {
    pub fn length(&self) -> usize {
        self.data.len()
    }
}

/// OKI address -> ROM offset.
///
/// Unbanked reads are 18-bit. Banked reads follow Furnace's NMK112 map:
/// the phrase book (addresses below 0x400) takes its page from
/// bank[(addr >> 8) & 3], and every other 64 KB window takes its page
/// from bank[(addr >> 16) & 3].
fn _chip_to_phys(addr: i64, banks: &[i32; 4], banked: bool) -> i64 {
    let addr = addr & 0x3FFFF;
    if !banked {
        return addr;
    }
    if addr < 0x400 {
        let page = (banks[((addr >> 8) & 3) as usize] & 0xFF) as i64;
        return (page << 16) | (addr & 0x3FF);
    }
    let page = (banks[((addr >> 16) & 3) as usize] & 0xFF) as i64;
    (page << 16) | (addr & 0xFFFF)
}

/// Read a bank-relative range from a flat 256 KB MSM6295 bank.
///
/// Register 0x0F selects one 256 KB bank (the VGMPlay player convention for
/// a 512 KB or larger ROM). Phrase headers and their sample data are both
/// relative to that bank, so a 512 KB ROM uses base 0 or 0x40000.
fn _read_flat(rom: &[u8], start: i64, stop: i64, base: i64) -> Vec<u8> {
    if stop < start {
        return Vec::new();
    }
    let begin = base + start;
    let end = base + stop + 1;
    let want = (end - begin).max(0) as usize;
    let mut piece: Vec<u8> = if begin < 0 || begin >= rom.len() as i64 {
        Vec::new()
    } else {
        let first = begin as usize;
        let last = (end.max(0) as usize).min(rom.len());
        rom[first..last].to_vec()
    };
    if piece.len() < want {
        piece.resize(want, 0);
    }
    piece
}

fn _read_mapped(rom: &[u8], start: i64, stop: i64, banks: &[i32; 4], banked: bool) -> Vec<u8> {
    if stop < start {
        return Vec::new();
    }
    let mut out: Vec<u8> = Vec::new();
    let mut addr = start & 0x3FFFF;
    let end = stop & 0x3FFFF;
    while addr <= end {
        let phys = _chip_to_phys(addr, banks, banked);
        let n: i64 = if !banked {
            end - addr + 1
        } else if addr < 0x400 {
            (end - addr + 1).min(0x400 - addr)
        } else {
            let window_end = (addr & !0xFFFF) + 0xFFFF;
            (end - addr + 1).min(window_end - addr + 1)
        };
        let n_usize = n as usize;
        if phys >= rom.len() as i64 {
            out.resize(out.len() + n_usize, 0);
        } else {
            let first = phys as usize;
            let last = (phys + n).max(0) as usize;
            let last = last.min(rom.len());
            let mut piece = rom[first..last].to_vec();
            if piece.len() < n_usize {
                piece.resize(n_usize, 0);
            }
            out.extend_from_slice(&piece);
        }
        addr += n;
    }
    out
}

pub fn assemble_oki_rom(vgm: &VgmFile, chip_id: i32) -> Vec<u8> {
    let slices: Vec<_> = vgm
        .roms
        .iter()
        .filter(|s| s.kind == "msm6295" && s.chip as i32 == chip_id)
        .collect();
    if slices.is_empty() {
        return Vec::new();
    }
    let rom_size = slices.iter().map(|s| s.rom_size as i64).max().unwrap_or(0);
    let end = slices
        .iter()
        .map(|s| s.start as i64 + s.data.len() as i64)
        .max()
        .unwrap_or(0);
    let len = rom_size.max(end).max(0) as usize;
    let mut rom = vec![0u8; len];
    for s in slices {
        let start = s.start as usize;
        let bytes = s.data.len();
        if start + bytes <= rom.len() {
            rom[start..start + bytes].copy_from_slice(&s.data);
        }
    }
    rom
}

/// Pair 0xB8 command bytes into phrase starts and early stops.
pub fn collect_oki(vgm: &VgmFile) -> (Vec<OkiSample>, Vec<String>) {
    let mut warnings: Vec<String> = Vec::new();
    let mut chip_id_set: BTreeSet<i32> = BTreeSet::new();
    for w in &vgm.writes {
        if w.chip == "msm6295" {
            chip_id_set.insert(w.chip_id as i32);
        }
    }
    for s in &vgm.roms {
        if s.kind == "msm6295" {
            chip_id_set.insert(s.chip as i32);
        }
    }
    let chip_ids: Vec<i32> = chip_id_set.into_iter().collect();
    let mut samples: Vec<OkiSample> = Vec::new();
    let mut by_body: IndexMap<(i32, Vec<u8>), usize> = IndexMap::new();
    let mut names: HashMap<String, i32> = HashMap::new();

    for chip_id in chip_ids {
        let rom = assemble_oki_rom(vgm, chip_id);
        let (mut clock, pin7) = oki_chip_setup(vgm, chip_id);
        if clock == 0 {
            clock = 1_056_000;
            warnings.push(format!(
                "MSM6295 chip {chip_id}: clock missing, using {clock} Hz"
            ));
        }
        let rate = oki_rate(clock, pin7);
        if rom.is_empty() {
            warnings.push(format!("MSM6295 chip {chip_id}: no ROM block (0x8B)"));
        }
        let mut banks: [i32; 4] = [0, 0, 0, 0];
        let mut banked = false;
        // Register 0x0F is the single 256 KB bank select some drivers use
        // instead of the NMK112 map (0x0E + 0x10-0x13). None means "not seen";
        // an explicit 0 selects the first bank, which is different from the
        // unbanked read.
        let mut flat_bank: Option<i64> = None;
        let mut latch: Option<i32> = None;
        let mut active: Vec<Option<(usize, usize)>> = vec![None; OKI_VOICES];
        let mut empty_phrases: i32 = 0;

        for w in &vgm.writes {
            if w.chip != "msm6295" || w.chip_id as i32 != chip_id {
                continue;
            }
            let reg = w.reg as i32;
            let val = w.val as i32;
            if reg == 0x0E {
                banked = (val & 0x80) != 0;
                continue;
            }
            if reg == 0x0F {
                flat_bank = Some(((val & 0xFF) as i64) * 0x40000);
                continue;
            }
            if (0x10..=0x13).contains(&reg) {
                banks[(reg - 0x10) as usize] = val;
                banked = true;
                continue;
            }
            if reg != 0 {
                continue;
            }
            if let Some(phrase) = latch {
                latch = None;
                let mask = (val >> 4) & 0x0F;
                let vol = oki_furnace_vol(val);
                let vol = match vol {
                    Some(v) if mask != 0 && !rom.is_empty() => v,
                    _ => continue,
                };
                let hdr = if let Some(base) = flat_bank {
                    _read_flat(&rom, phrase as i64 * 8, phrase as i64 * 8 + 5, base)
                } else {
                    _read_mapped(
                        &rom,
                        phrase as i64 * 8,
                        phrase as i64 * 8 + 5,
                        &banks,
                        banked,
                    )
                };
                let start =
                    (((hdr[0] as i64) << 16) | ((hdr[1] as i64) << 8) | hdr[2] as i64) & 0x3FFFF;
                let stop =
                    (((hdr[3] as i64) << 16) | ((hdr[4] as i64) << 8) | hdr[5] as i64) & 0x3FFFF;
                if stop < start || (stop == start && start == 0) {
                    empty_phrases += 1;
                    continue;
                }
                if stop - start > 0x40000 {
                    empty_phrases += 1;
                    continue;
                }
                let data = if let Some(base) = flat_bank {
                    _read_flat(&rom, start, stop, base)
                } else {
                    _read_mapped(&rom, start, stop, &banks, banked)
                };
                if data.is_empty() || !data.iter().any(|&b| b != 0) {
                    empty_phrases += 1;
                    continue;
                }
                let body_key = (chip_id, data.clone());
                let existing = by_body.get(&body_key).copied();
                let idx = if let Some(i) = existing {
                    i
                } else {
                    let mut label = format!("{phrase:02X}");
                    if chip_id != 0 {
                        label = format!("c{} {label}", chip_id + 1);
                    }
                    let seen = names.get(&label).copied().unwrap_or(0) + 1;
                    names.insert(label.clone(), seen);
                    let label = if seen > 1 {
                        format!("{label} {seen}")
                    } else {
                        label
                    };
                    samples.push(OkiSample {
                        chip_id,
                        phrase,
                        data,
                        rate,
                        clock,
                        pin7,
                        name: label,
                        hits: Vec::new(),
                    });
                    let i = samples.len() - 1;
                    by_body.insert(body_key, i);
                    i
                };
                for voice in 0..OKI_VOICES {
                    if mask & (1 << voice) == 0 {
                        continue;
                    }
                    let hit = OkiHit {
                        sample_time: w.sample,
                        voice: voice as i32,
                        vol,
                        end: 0,
                    };
                    samples[idx].hits.push(hit);
                    let hi = samples[idx].hits.len() - 1;
                    active[voice] = Some((idx, hi));
                }
                continue;
            }
            if val & 0x80 != 0 {
                latch = Some(val & 0x7F);
                continue;
            }
            // 0abcd000: bit (3 + voice) stops that voice.
            for voice in 0..OKI_VOICES {
                if (val >> (3 + voice)) & 1 != 0 {
                    if let Some((si, hi)) = active[voice] {
                        if samples[si].hits[hi].end == 0 {
                            samples[si].hits[hi].end = w.sample;
                        }
                    }
                    active[voice] = None;
                }
            }
        }
        if empty_phrases != 0 {
            warnings.push(format!(
                "MSM6295 chip {chip_id}: skipped {empty_phrases} empty phrase starts"
            ));
        }
    }

    samples.sort_by(|a, b| a.chip_id.cmp(&b.chip_id));
    (samples, warnings)
}

fn _msm_clock(vgm: &VgmFile, chip_id: i32, warnings: &mut Vec<String>) -> u32 {
    let mut clock = vgm.msm6258_clock as i64;
    if chip_id != 0 {
        let extra = vgm.extra_clocks.get(&0x16).copied().unwrap_or(0) as i64;
        if extra != 0 {
            clock = extra & 0x3FFFFFFF;
        }
    }
    if clock <= 0 {
        clock = 4_000_000;
        warnings.push(format!(
            "MSM6258 chip {chip_id}: clock missing, using {clock} Hz"
        ));
    }
    clock as u32
}

/// Bytes a stream writes, walking `step` bytes per command from `offset`.
fn _stride(buf: &[u8], offset: i64, nbytes: i64, step: i32, base: i64) -> Vec<u8> {
    if step <= 1 && base == 0 {
        let start = offset.max(0) as usize;
        if start >= buf.len() {
            return Vec::new();
        }
        if nbytes <= 0 {
            return buf[start..].to_vec();
        }
        let end = (start as i64 + nbytes) as usize;
        return buf[start..end.min(buf.len())].to_vec();
    }
    let mut out: Vec<u8> = Vec::new();
    let mut cmd = offset.max(0);
    let limit = if nbytes > 0 {
        nbytes
    } else {
        buf.len() as i64 + 1
    };
    let mut got: i64 = 0;
    while got < limit {
        let idx = cmd * step as i64 + base;
        if idx < 0 || idx >= buf.len() as i64 {
            break;
        }
        out.push(buf[idx as usize]);
        cmd += 1;
        got += 1;
    }
    out
}

/// Reverse OKIM6258 nibble order. The chip plays the low nibble first.
fn _reverse_nibbles(data: &[u8]) -> Vec<u8> {
    let mut nibs: Vec<u8> = Vec::new();
    for &byte in data {
        nibs.push(byte & 0x0F);
        nibs.push(byte >> 4);
    }
    nibs.reverse();
    if nibs.len() % 2 != 0 {
        nibs.push(0);
    }
    let mut out = Vec::new();
    let mut i = 0;
    while i < nibs.len() {
        out.push(nibs[i] | (nibs[i + 1] << 4));
        i += 2;
    }
    out
}

fn _play_bytes(blocks: &[Vec<u8>], play: &Msm6258Play) -> Vec<u8> {
    let mut data: Vec<u8>;
    if play.length_mode == 3 && play.block >= 0 {
        if play.block as usize >= blocks.len() {
            return Vec::new();
        }
        data = _stride(
            &blocks[play.block as usize],
            0,
            0,
            play.step as i32,
            play.step_base as i64,
        );
    } else {
        let mut bank: Vec<u8> = Vec::new();
        for block in blocks {
            bank.extend_from_slice(block);
        }
        let mut nbytes = play.nbytes as i64;
        if play.length_mode == 2 && play.rate > 0 {
            nbytes = 0.max(py_int(py_round(
                play.rate as f64 * play.nbytes as f64 / 1000.0,
            )));
        }
        data = _stride(
            &bank,
            play.offset as i64,
            nbytes,
            play.step as i32,
            play.step_base as i64,
        );
    }
    if play.reverse && !data.is_empty() {
        data = _reverse_nibbles(&data);
    }
    if play.loop_ && play.end > play.sample && !data.is_empty() && play.rate > 0 {
        let need = py_int((play.end - play.sample) as f64 / VGM_RATE as f64 * play.rate as f64) + 1;
        if need > data.len() as i64 {
            if !data.is_empty() {
                let reps = (need + data.len() as i64 - 1) / data.len() as i64;
                let original = data.clone();
                data.clear();
                for _ in 0..reps {
                    data.extend_from_slice(&original);
                }
                data.truncate(need as usize);
            }
        }
    }
    data
}

/// One sample per distinct block body. Hits share it; a stop is the end.
fn _collect_msm6258_stream(vgm: &VgmFile, warnings: &mut Vec<String>) -> Vec<AdpcmSample> {
    let mut samples: Vec<AdpcmSample> = Vec::new();
    let mut by_body: IndexMap<(i32, i32, Vec<u8>), usize> = IndexMap::new();
    let mut clocks: IndexMap<i32, u32> = IndexMap::new();
    let mut missing: i32 = 0;
    for play in &vgm.msm6258_plays {
        let data = _play_bytes(&vgm.adpcm_blocks, play);
        if data.is_empty() {
            missing += 1;
            continue;
        }
        let chip_id = play.chip_id as i32;
        let clock = match clocks.get(&chip_id).copied() {
            Some(clock) => clock,
            None => {
                let clock = _msm_clock(vgm, chip_id, warnings);
                clocks.insert(chip_id, clock);
                clock
            }
        };
        let divider =
            msm6258_best_divider(clock as i32, vgm.msm6258_flags as i32, play.rate as i32);
        let rate = (py_int(py_round(clock as f64 / divider as f64)) as i32).max(1);
        let key = (chip_id, divider, data.clone());
        let existing = by_body.get(&key).copied();
        let idx = if let Some(i) = existing {
            i
        } else {
            samples.push(AdpcmSample {
                chip_id,
                data,
                rate,
                clock,
                divider,
                name: String::new(),
                hits: Vec::new(),
            });
            let i = samples.len() - 1;
            by_body.insert(key, i);
            i
        };
        samples[idx].hits.push(AdpcmHit {
            sample_time: play.sample,
            end: play.end,
        });
    }
    for (i, smp) in samples.iter_mut().enumerate() {
        smp.name = if smp.chip_id == 0 {
            format!("adpcm {i:02}")
        } else {
            format!("adpcm{} {i:02}", smp.chip_id + 1)
        };
    }
    if missing != 0 {
        warnings.push(format!(
            "MSM6258: skipped {missing} stream plays with no ADPCM block"
        ));
    }
    samples
}

/// OKIM6258 phrases: DAC-stream blocks, else 0xB7 bursts of 4 bytes or more.
pub fn collect_msm6258(vgm: &VgmFile) -> (Vec<AdpcmSample>, Vec<String>) {
    let mut warnings: Vec<String> = Vec::new();
    if !vgm.msm6258_plays.is_empty() {
        let samples = _collect_msm6258_stream(vgm, &mut warnings);
        return (samples, warnings);
    }
    let samples = _collect_msm6258_bytes(vgm, &mut warnings);
    (samples, warnings)
}

/// Turn MSM6258 data-port bytes into one VOX sample per burst.
fn _collect_msm6258_bytes(vgm: &VgmFile, warnings: &mut Vec<String>) -> Vec<AdpcmSample> {
    let mut chip_id_set: BTreeSet<i32> = BTreeSet::new();
    for w in &vgm.writes {
        if w.chip == "msm6258" {
            chip_id_set.insert(w.chip_id as i32);
        }
    }
    let mut chip_ids: Vec<i32> = chip_id_set.into_iter().collect();
    if vgm.msm6258_clock != 0 && chip_ids.is_empty() {
        chip_ids = vec![0];
    }
    if chip_ids.is_empty() {
        return Vec::new();
    }
    let mut samples: Vec<AdpcmSample> = Vec::new();
    let divider = msm6258_divider(vgm.msm6258_flags as i32);
    for chip_id in chip_ids {
        let clock = _msm_clock(vgm, chip_id, warnings);
        let rate = (py_int(py_round(clock as f64 / divider as f64)) as i32).max(1);
        // Two nibbles per written byte. A gap of several sample-periods ends a burst.
        let byte_samples = 1.max(py_int(py_round(VGM_RATE as f64 * 2.0 / rate as f64)));
        let mut _playing = false;
        let mut buf: Vec<u8> = Vec::new();
        let mut start_t: i64 = 0;
        let mut last_t: i64 = 0;
        let mut bursts: Vec<(i64, i64, Vec<u8>)> = Vec::new();

        for w in &vgm.writes {
            if w.chip != "msm6258" || w.chip_id as i32 != chip_id {
                continue;
            }
            if w.reg as i32 == 0 {
                if w.val as i32 & 0x01 != 0 {
                    _playing = false;
                    if !buf.is_empty() {
                        bursts.push((start_t, w.sample, std::mem::take(&mut buf)));
                    }
                }
                if w.val as i32 & 0x02 != 0 {
                    _playing = true;
                }
                continue;
            }
            if w.reg as i32 != 1 {
                continue;
            }
            if !buf.is_empty() && w.sample - last_t > byte_samples as i64 * 4 {
                bursts.push((start_t, last_t, std::mem::take(&mut buf)));
            }
            if buf.is_empty() {
                start_t = w.sample;
                _playing = true;
            }
            buf.push(w.val as u8);
            last_t = w.sample;
        }
        if !buf.is_empty() {
            bursts.push((start_t, last_t, std::mem::take(&mut buf)));
        }
        let mut kept: i32 = 0;
        let mut short: i32 = 0;
        for (t0, t1, data) in bursts {
            if data.len() < MSM6258_MIN_BURST || !data.iter().any(|&b| b != 0) {
                short += 1;
                continue;
            }
            samples.push(AdpcmSample {
                chip_id,
                data,
                rate,
                clock,
                divider,
                name: if chip_id == 0 {
                    format!("adpcm {kept:02}")
                } else {
                    format!("adpcm{} {kept:02}", chip_id + 1)
                },
                hits: vec![AdpcmHit {
                    sample_time: t0,
                    end: t1,
                }],
            });
            kept += 1;
        }
        if short != 0 && kept == 0 {
            warnings.push(format!(
                "MSM6258 chip {chip_id}: ignored {short} data burst(s) shorter than \
                 {MSM6258_MIN_BURST} bytes"
            ));
        }
    }
    samples
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_oki_volume_nibbles() {
        assert_eq!(oki_furnace_vol(0), Some(8));
        assert_eq!(oki_furnace_vol(8), Some(0));
        assert_eq!(oki_furnace_vol(9), None);
        assert_eq!(oki_furnace_vol(15), None);
        // Python masks with 0x0F first.
        assert_eq!(oki_furnace_vol(0x18), Some(0));
        assert_eq!(oki_furnace_vol(-1), None);
    }

    #[test]
    fn computes_playback_rate_and_zero_clock() {
        // 1.056 MHz / 132 = 8000 Hz, / 165 = 6400 Hz.
        assert_eq!(oki_rate(1_056_000, true), 8000);
        assert_eq!(oki_rate(1_056_000, false), 6400);
        assert_eq!(oki_rate(0, true), 1);
        // 1 MHz / 132 = 7575.75... -> half-even round 7576.
        assert_eq!(oki_rate(1_000_000, true), 7576);
    }

    #[test]
    fn formats_flag_bodies() {
        assert_eq!(oki_flag_text(1_056_000, true, false), "clockSel=1");
        assert_eq!(
            oki_flag_text(1_056_000, false, true),
            "clockSel=1\nrateSel=true\nisBanked=true"
        );
        assert_eq!(oki_flag_text(1234, true, false), "customClock=1234");
        assert_eq!(oki_flag_text(0, true, false), "");

        assert_eq!(msm6258_flag_text(4_000_000), "clockSel=0");
        assert_eq!(msm6258_flag_text(8_192_000), "clockSel=3");
        assert_eq!(msm6258_flag_text(4_096_001), "customClock=4096001");
        assert_eq!(msm6258_flag_text(0), "");
    }

    #[test]
    fn picks_msm6258_divider_and_instrument_count() {
        assert_eq!(msm6258_divider(0), 1024);
        assert_eq!(msm6258_divider(1), 768);
        assert_eq!(msm6258_divider(2), 512);
        assert_eq!(msm6258_divider(7), 512);
        assert_eq!(msm6258_div_fx(1024), Some(0));
        assert_eq!(msm6258_div_fx(768), Some(1));
        assert_eq!(msm6258_div_fx(512), None);
        // Header (1024) wins only when no alternative is strictly closer.
        assert_eq!(msm6258_best_divider(4_000_000, 0, 0), 1024);
        assert_eq!(msm6258_best_divider(4_000_000, 0, 3900), 512);
        assert_eq!(msm6258_best_divider(0, 1, 3900), 768);

        assert_eq!(msm6258_ins_count(0), 0);
        assert_eq!(msm6258_ins_count(-5), 0);
        assert_eq!(msm6258_ins_count(1), 1);
        assert_eq!(msm6258_ins_count(120), 1);
        assert_eq!(msm6258_ins_count(121), 2);
        assert_eq!(msm6258_ins_count(240), 2);
    }

    #[test]
    fn strides_nibbles_and_maps_banked_addresses() {
        let buf = b"abcdef";
        assert_eq!(_stride(buf, 1, 3, 1, 0), b"bcd");
        assert_eq!(_stride(buf, 1, 0, 1, 0), b"bcdef");
        assert_eq!(_stride(buf, 0, 3, 2, 1), b"bdf");
        assert_eq!(_stride(buf, 99, 2, 1, 0), Vec::<u8>::new());

        assert_eq!(_reverse_nibbles(&[0x21, 0x43]), vec![0x34, 0x12]);
        assert_eq!(_reverse_nibbles(&[]), Vec::<u8>::new());

        let banks = [1, 2, 3, 4];
        assert_eq!(_chip_to_phys(0x25000, &banks, false), 0x25000);
        assert_eq!(_chip_to_phys(0x100, &banks, true), 0x20100);
        assert_eq!(_chip_to_phys(0x25000, &banks, true), 0x35000);
    }

    #[test]
    fn collects_phrases_and_stream_plays_like_the_reference() {
        use crate::vgm::{ChipWrite, Msm6258Play, RomSlice, VgmFile};

        let mut rom = vec![0u8; 0x300];
        rom[0..3].copy_from_slice(&[0x00, 0x01, 0x00]);
        rom[3..6].copy_from_slice(&[0x00, 0x01, 0x0F]);
        for (i, byte) in rom.iter_mut().enumerate().take(0x110).skip(0x100) {
            *byte = (i * 7) as u8;
        }
        rom[8..11].copy_from_slice(&[0x00, 0x02, 0x00]);
        rom[11..14].copy_from_slice(&[0x00, 0x02, 0x0F]);

        let mut vgm = VgmFile::default();
        vgm.roms = vec![RomSlice {
            chip: 0,
            rom_size: 0x300,
            start: 0,
            data: rom,
            kind: "msm6295",
            time: 0,
        }];
        vgm.writes = vec![
            ChipWrite::new(0, "msm6295", 0, 0, 0x80),
            ChipWrite::new(10, "msm6295", 0, 0, 0xF0),
            ChipWrite::new(20, "msm6295", 0, 0x10, 0x03),
            ChipWrite::new(30, "msm6295", 0, 0, 0x81),
            ChipWrite::new(40, "msm6295", 0, 0, 0xF0),
            ChipWrite::new(500, "msm6295", 0, 0, 0x28),
        ];
        let (samples, warnings) = collect_oki(&vgm);
        assert_eq!(samples.len(), 1);
        let smp = &samples[0];
        assert_eq!(smp.chip_id, 0);
        assert_eq!(smp.phrase, 0);
        assert_eq!(smp.name, "00");
        assert_eq!(smp.rate, 6400);
        assert_eq!(smp.clock, 1_056_000);
        assert!(!smp.pin7);
        assert_eq!(smp.data.len(), 16);
        assert_eq!(smp.hits.len(), 4);
        assert_eq!(smp.hits[0].sample_time, 10);
        assert_eq!(smp.hits[0].voice, 0);
        assert_eq!(smp.hits[0].vol, 8);
        assert_eq!(smp.hits[0].end, 500);
        assert_eq!(smp.hits[1].end, 0);
        assert_eq!(smp.hits[2].end, 500);
        assert_eq!(
            warnings,
            vec![
                "MSM6295 chip 0: clock missing, using 1056000 Hz".to_string(),
                "MSM6295 chip 0: skipped 1 empty phrase starts".to_string(),
            ]
        );

        let mut stream = VgmFile::default();
        stream.msm6258_clock = 4_000_000;
        stream.adpcm_blocks = vec![vec![1, 2, 3, 4, 5, 6, 7, 8]];
        stream.msm6258_plays = vec![Msm6258Play {
            sample: 100,
            block: 0,
            rate: 3900,
            end: 200,
            length_mode: 3,
            ..Default::default()
        }];
        let (adpcm, warnings) = collect_msm6258(&stream);
        assert!(warnings.is_empty());
        assert_eq!(adpcm.len(), 1);
        assert_eq!(adpcm[0].name, "adpcm 00");
        assert_eq!(adpcm[0].rate, 7812);
        assert_eq!(adpcm[0].clock, 4_000_000);
        assert_eq!(adpcm[0].divider, 512);
        assert_eq!(adpcm[0].data, vec![1, 2, 3, 4, 5, 6, 7, 8]);
        assert_eq!(
            adpcm[0].hits,
            vec![AdpcmHit {
                sample_time: 100,
                end: 200,
            }]
        );

        let mut bursts = VgmFile::default();
        bursts.msm6258_clock = 4_000_000;
        bursts.writes = vec![
            ChipWrite::new(0, "msm6258", 0, 0, 0x02),
            ChipWrite::new(10, "msm6258", 0, 1, 0x11),
            ChipWrite::new(20, "msm6258", 0, 1, 0x22),
            ChipWrite::new(30, "msm6258", 0, 1, 0x33),
            ChipWrite::new(40, "msm6258", 0, 1, 0x44),
            ChipWrite::new(50, "msm6258", 0, 0, 0x01),
        ];
        let (adpcm, warnings) = collect_msm6258(&bursts);
        assert!(warnings.is_empty());
        assert_eq!(adpcm.len(), 1);
        assert_eq!(adpcm[0].name, "adpcm 00");
        assert_eq!(adpcm[0].rate, 3906);
        assert_eq!(adpcm[0].divider, 1024);
        assert_eq!(adpcm[0].data, vec![0x11, 0x22, 0x33, 0x44]);
        assert_eq!(
            adpcm[0].hits,
            vec![AdpcmHit {
                sample_time: 10,
                end: 50,
            }]
        );

        let mut too_short = VgmFile::default();
        too_short.msm6258_clock = 4_000_000;
        too_short.writes = vec![
            ChipWrite::new(0, "msm6258", 0, 0, 0x02),
            ChipWrite::new(10, "msm6258", 0, 1, 0x11),
            ChipWrite::new(20, "msm6258", 0, 0, 0x01),
        ];
        let (adpcm, warnings) = collect_msm6258(&too_short);
        assert!(adpcm.is_empty());
        assert_eq!(
            warnings,
            vec!["MSM6258 chip 0: ignored 1 data burst(s) shorter than 4 bytes".to_string()]
        );
    }
}
