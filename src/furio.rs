//! Read Furnace .fur files: blocks, sample metadata, instruments, patterns.
//!
//! Works on vgm2151fur format-157 modules and on files Furnace re-saves
//! (INFO + SMP2 + PATN, optionally zlib-wrapped). INF2 files read too.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use indexmap::IndexMap;

use crate::error::{Error, Result};
use crate::furwrite::{
    CHIP_AY8910, CHIP_C140, CHIP_C219, CHIP_C352, CHIP_GA20, CHIP_GB, CHIP_K005289,
    CHIP_K007232, CHIP_K051649, CHIP_K052539, CHIP_K053260, CHIP_MSM5232, CHIP_MSM6258,
    CHIP_MSM6295, CHIP_MULTIPCM, CHIP_NES, CHIP_OPL, CHIP_OPL2, CHIP_OPL2_DRUMS, CHIP_OPL3,
    CHIP_OPL3_DRUMS, CHIP_OPL_DRUMS, CHIP_OPLL, CHIP_OPLL_DRUMS, CHIP_PCE, CHIP_PCM_DAC,
    CHIP_POKEY, CHIP_RF5C68, CHIP_SAA1099, CHIP_SEGAPCM, CHIP_VBOY, CHIP_VRC7, CHIP_YM2608,
    CHIP_YM2608_EXT, CHIP_YM2610, CHIP_YM2610B, CHIP_YM2612, CHIP_YM2612_CSM, CHIP_YM2612_EXT,
    CHIP_YMZ280B, FUR_MAGIC,
};

pub use crate::furwrite::decompress_fur;

/// Python: `CHIP_CHANNELS` (insertion order preserved).
pub const CHIP_CHANNELS: &[(u8, usize)] = &[
    (0x02, 10), // Sega Genesis: 6 FM + 4 PSG (system used by vgm2fur)
    (0x42, 13), // Sega Genesis Ext: 9 FM + 4 PSG
    (0x82, 8),  // YM2151
    (0x8D, 6),  // YM2203: 3 FM + 3 SSG voices
    (0xB6, 9),  // YM2203 Ext: 2 FM + 4 channel-3 operators + 3 SSG
    (CHIP_YM2612, 6), // YM2612: 6 FM voices (channel 6 may play samples)
    (CHIP_YM2612_EXT, 9), // YM2612 Ext: 2 FM + 4 channel-3 operators + FM4-6
    (CHIP_YM2612_CSM, 10), // YM2612 CSM: Ext + the CSM timer channel
    (CHIP_YM2610, 14), // YM2610: 4 FM + 3 SSG + 6 ADPCM-A + 1 ADPCM-B
    (CHIP_YM2610B, 16), // YM2610B: 6 FM + 3 SSG + 6 ADPCM-A + 1 ADPCM-B
    // YM2608 (OPNA): the same span with the rhythm kit where the ADPCM-A
    // voices were, and channel 3 split into four operator columns in Ext.
    (CHIP_YM2608, 16), // YM2608: 6 FM + 3 SSG + 6 rhythm + 1 ADPCM-B
    (CHIP_YM2608_EXT, 19), // YM2608 Ext: 2 FM + 4 operators + FM4-6 + the rest
    // OPL family. Rhythm mode is its own system: bank A's last FM channels
    // become the kit, so it shows five columns more on OPL/OPL2 and two on OPL3.
    (CHIP_OPL, 9),
    (CHIP_OPL2, 9),
    (CHIP_OPL3, 18),
    (CHIP_OPL_DRUMS, 11),
    (CHIP_OPL2_DRUMS, 11),
    (CHIP_OPL3_DRUMS, 20),
    // YM8950: 9 FM + 1 ADPCM, or the drums layout (11 FM) + 1 ADPCM.
    (0xB2, 10),
    (0xB3, 12),
    // YMF278B (OPL4): 18 FM + 24 wave, or 20 FM/drums + 24 wave.
    (0xAE, 42),
    (0xAF, 44),
    // YM2413 (OPLL): nine melodic channels; the drums layout adds BD, SD, TM,
    // TP and HH in place of the last three hardware channels.
    (CHIP_OPLL, 9),
    (CHIP_OPLL_DRUMS, 11),
    (CHIP_VRC7, 6),    // Konami VRC7 (OPLL core, six channels)
    (CHIP_NES, 5),     // NES APU: two pulses, triangle, noise, DPCM
    (CHIP_PCE, 6),     // HuC6280: six wavetable voices
    (CHIP_GB, 4),      // Game Boy DMG: two pulses, wavetable, noise
    (CHIP_MSM6295, 4), // MSM6295
    (CHIP_MSM6258, 1), // MSM6258
    (CHIP_K007232, 2), // K007232
    (CHIP_SEGAPCM, 16), // SegaPCM
    (CHIP_RF5C68, 8),  // Ricoh RF5C68 / RF5C164
    (CHIP_C140, 24),   // Namco C140
    (CHIP_C352, 32),   // Namco C352
    (CHIP_K053260, 4), // Konami K053260
    (CHIP_YMZ280B, 8), // Yamaha YMZ280B (PCMD8)
    (CHIP_MULTIPCM, 28), // Yamaha YMW258-F (MultiPCM)
    (CHIP_PCM_DAC, 1), // Generic PCM DAC (one system per uPD7759/PWM side)
    (CHIP_GA20, 4),    // Irem GA20
    (CHIP_MSM5232, 8), // OKI MSM5232
    (CHIP_VBOY, 6),    // Virtual Boy VSU: five wave voices + noise
    (CHIP_K051649, 5), // Konami SCC
    (CHIP_K052539, 5), // Konami SCC+
    (CHIP_K005289, 2), // Konami Bubble System WSG (two wavetable voices)
    (CHIP_AY8910, 3),  // AY-3-8910
    (CHIP_SAA1099, 6), // Philips SAA1099
    (CHIP_POKEY, 4),   // Atari POKEY
    (0xA8, 4), // Atari Lynx Mikey: four LFSR/DAC voices
    (0x8A, 1), // Famicom Disk System: one wavetable voice
    (0x96, 4), // Bandai WonderSwan: four wavetable/PCM voices
    (0xE0, 19), // Capcom QSound: sixteen PCM + three ADPCM voices
    (0xB0, 16), // Seta/Allumer X1-010: sixteen wavetable/PCM voices
    (0x03, 4),  // TI SN76489 (three tone + noise)
    (0xBF, 4),  // Toshiba T6W28 (stereo SN76489 derivative)
    (0xBB, 8),  // Namco C30 WSG
    (0xCF, 16), // Namco C219
];

/// Chips whose samples carry playable 8-bit bodies (VOX depths stay out).
pub const _PCM_BODY_CHIPS: &[u8] = &[
    CHIP_K007232,
    CHIP_SEGAPCM,
    CHIP_RF5C68,
    CHIP_C140,
    CHIP_C219,
    CHIP_C352,
    CHIP_K053260,
    CHIP_YMZ280B,
    CHIP_MULTIPCM,
    CHIP_PCM_DAC,
    CHIP_GA20,
];

/// `CHIP_CHANNELS.get(chip, 1)`.
pub fn chip_channels(chip: u8) -> usize {
    CHIP_CHANNELS
        .iter()
        .find(|(c, _)| *c == chip)
        .map(|(_, n)| *n)
        .unwrap_or(1)
}

fn short_buf(width: usize, off: usize) -> Error {
    Error::Msg(format!(
        "unpack_from requires a buffer of at least {} bytes for unpacking {} bytes at offset {}",
        off.saturating_add(width),
        width,
        off
    ))
}

fn u16le(data: &[u8], off: usize) -> Result<u16> {
    let end = off.checked_add(2).ok_or_else(|| short_buf(2, off))?;
    let bytes = data.get(off..end).ok_or_else(|| short_buf(2, off))?;
    Ok(u16::from_le_bytes([bytes[0], bytes[1]]))
}

fn u32le(data: &[u8], off: usize) -> Result<u32> {
    let end = off.checked_add(4).ok_or_else(|| short_buf(4, off))?;
    let bytes = data.get(off..end).ok_or_else(|| short_buf(4, off))?;
    Ok(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
}

fn i32le(data: &[u8], off: usize) -> Result<i32> {
    Ok(u32le(data, off)? as i32)
}

fn f32le(data: &[u8], off: usize) -> Result<f32> {
    Ok(f32::from_bits(u32le(data, off)?))
}

fn byte_at(data: &[u8], off: usize) -> Result<u8> {
    data.get(off)
        .copied()
        .ok_or_else(|| Error::Msg("index out of range".to_string()))
}

/// Python slice `data[start:start+n]` (clamped, empty past the end).
fn slice_n(data: &[u8], start: usize, n: usize) -> &[u8] {
    if start >= data.len() {
        return &[];
    }
    let end = start.saturating_add(n).min(data.len());
    &data[start..end]
}

/// `round -> py_round` is not needed here; all fields stay integers.
pub fn load_fur_bytes(path: &Path) -> Result<Vec<u8>> {
    let raw = std::fs::read(path).map_err(|e| Error::io(path, e))?;
    if raw.get(..16) == Some(FUR_MAGIC) {
        return Ok(raw);
    }
    let name = path
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let dec = {
        use std::io::Read;
        let mut dec = flate2::read::ZlibDecoder::new(raw.as_slice());
        let mut out = Vec::new();
        match dec.read_to_end(&mut out) {
            Ok(_) => out,
            Err(_) => return Err(Error::Msg(format!("{name}: not a Furnace module"))),
        }
    };
    if dec.get(..16) != Some(FUR_MAGIC) {
        return Err(Error::Msg(format!("{name}: not a Furnace module")));
    }
    Ok(dec)
}

pub fn iter_furs(path: &Path) -> Vec<PathBuf> {
    if path.is_file() {
        return vec![path.to_path_buf()];
    }
    let mut files: Vec<PathBuf> = Vec::new();
    if let Ok(rd) = std::fs::read_dir(path) {
        let mut upper: Vec<PathBuf> = Vec::new();
        for entry in rd.flatten() {
            let p = entry.path();
            let ext = p
                .extension()
                .map(|e| e.to_string_lossy().to_string())
                .unwrap_or_default();
            // `*.fur` + `*.FUR`, the two patterns pathlib.glob tries.
            if ext == "fur" {
                files.push(p);
            } else if ext == "FUR" {
                upper.push(p);
            }
        }
        files.extend(upper);
    }
    let key = |p: &PathBuf| {
        p.file_name()
            .map(|s| s.to_string_lossy().to_lowercase())
            .unwrap_or_default()
    };
    files.sort_by(|a, b| key(a).cmp(&key(b)));
    let mut seen: HashSet<String> = HashSet::new();
    let mut out: Vec<PathBuf> = Vec::new();
    for p in files {
        let k = std::fs::canonicalize(&p)
            .unwrap_or_else(|_| p.clone())
            .to_string_lossy()
            .to_lowercase();
        if seen.insert(k) {
            out.push(p);
        }
    }
    out
}

fn _read_strz(data: &[u8], pos: usize) -> (String, usize) {
    if pos >= data.len() {
        return (String::new(), data.len());
    }
    match data[pos..].iter().position(|&b| b == 0) {
        Some(i) => {
            let end = pos + i;
            (
                String::from_utf8_lossy(&data[pos..end]).into_owned(),
                end + 1,
            )
        }
        None => (
            String::from_utf8_lossy(&data[pos..]).into_owned(),
            data.len(),
        ),
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Block<'a> {
    pub off: usize,
    pub magic: [u8; 4],
    pub size: u32,
    pub payload: &'a [u8],
}

pub fn iter_blocks(data: &[u8]) -> Result<Vec<Block<'_>>> {
    if data.get(..16) != Some(FUR_MAGIC) {
        return Err(Error::Msg("not a Furnace module".to_string()));
    }
    let info_ptr = u32le(data, 20)? as usize;
    let mut out = Vec::new();
    let mut pos = info_ptr;
    while pos.saturating_add(8) <= data.len() {
        let mut magic = [0u8; 4];
        magic.copy_from_slice(&data[pos..pos + 4]);
        let size = u32le(data, pos + 4)?;
        let end = pos as u64 + 8 + size as u64;
        if end > data.len() as u64 + 8 {
            break;
        }
        let payload_end = (end as usize).min(data.len());
        let payload = &data[pos + 8..payload_end];
        out.push(Block {
            off: pos,
            magic,
            size,
            payload,
        });
        pos += 8 + size as usize;
    }
    Ok(out)
}

#[derive(Clone, Debug, PartialEq)]
pub struct FurSample {
    pub index: usize,
    pub name: String,
    pub file_off: usize,
    pub block_size: usize, // magic + size field + payload
    pub n_frames: u32,
    pub compat_rate: u32,
    pub c4_rate: u32,
    pub depth: u8,
    pub loop_mode: u8,
    pub flags: u8,
    pub flags2: u8,
    pub loop_start: i32,
    pub loop_end: i32,
    pub presence: Vec<u8>,
    pub data: Vec<u8>,
}

/// Python dict returned by `parse_smp2_payload`.
#[derive(Clone, Debug, PartialEq)]
pub struct Smp2Payload {
    pub name: String,
    pub n_frames: u32,
    pub compat_rate: u32,
    pub c4_rate: u32,
    pub depth: u8,
    pub loop_mode: u8,
    pub flags: u8,
    pub flags2: u8,
    pub loop_start: i32,
    pub loop_end: i32,
    pub presence: Vec<u8>,
    pub data: Vec<u8>,
}

pub fn parse_smp2_payload(payload: &[u8]) -> Result<Smp2Payload> {
    let (name, mut pos) = _read_strz(payload, 0);
    let n_frames = u32le(payload, pos)?;
    let compat = u32le(payload, pos + 4)?;
    let c4 = u32le(payload, pos + 8)?;
    pos += 12;
    let depth = byte_at(payload, pos)?;
    let loop_mode = byte_at(payload, pos + 1)?;
    let flags = byte_at(payload, pos + 2)?;
    let flags2 = byte_at(payload, pos + 3)?;
    pos += 4;
    let loop_start = i32le(payload, pos)?;
    let loop_end = i32le(payload, pos + 4)?;
    pos += 8;
    let presence = slice_n(payload, pos, 16).to_vec();
    pos += 16;
    let width: usize = if depth == 16 { 2 } else { 1 };
    let rest = payload.len() as i64 - pos as i64;
    let nbytes: usize = if matches!(depth, 3 | 5 | 6 | 7 | 10) {
        // ADPCM depths store a nibble count in the samples field; the payload
        // holds (count+1)/2 bytes. VOX also has the legacy byte-count form.
        if depth == 10 && rest == n_frames as i64 {
            n_frames as usize
        } else {
            ((n_frames as u64 + 1) / 2) as usize
        }
    } else {
        n_frames as usize * width
    };
    let pcm = slice_n(payload, pos, nbytes).to_vec();
    Ok(Smp2Payload {
        name,
        n_frames,
        compat_rate: compat,
        c4_rate: c4,
        depth,
        loop_mode,
        flags,
        flags2,
        loop_start,
        loop_end,
        presence,
        data: pcm,
    })
}

pub fn list_fur_samples(data: &[u8]) -> Result<Vec<FurSample>> {
    let mut out: Vec<FurSample> = Vec::new();
    let mut i = 0usize;
    for block in iter_blocks(data)? {
        if block.magic != *b"SMP2" {
            continue;
        }
        let p = parse_smp2_payload(block.payload)?;
        out.push(FurSample {
            index: i,
            name: p.name,
            file_off: block.off,
            block_size: 8 + block.size as usize,
            n_frames: p.n_frames,
            compat_rate: p.compat_rate,
            c4_rate: p.c4_rate,
            depth: p.depth,
            loop_mode: p.loop_mode,
            flags: p.flags,
            flags2: p.flags2,
            loop_start: p.loop_start,
            loop_end: p.loop_end,
            presence: p.presence,
            data: p.data,
        });
        i += 1;
    }
    Ok(out)
}

#[derive(Clone, Debug, PartialEq)]
pub struct FurRow {
    pub note: i32,
    pub ins: i32,
    pub vol: i32,
    pub fx: Vec<(u8, u8)>,
}

impl Default for FurRow {
    fn default() -> Self {
        FurRow {
            note: -1,
            ins: -1,
            vol: -1,
            fx: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct FurModule {
    pub version: u16,
    pub speed: i32,
    pub hz: f64,
    pub pat_len: u16,
    pub n_ord: u16,
    pub n_ch: usize,
    pub chips: Vec<u8>,
    pub pcm_channels: Vec<usize>,
    pub orders: Vec<Vec<u16>>,
    pub samples: Vec<FurSample>,
    pub ins_sample: Vec<i32>,
    pub patterns: IndexMap<(u16, u16), Vec<FurRow>>,
    pub name: String,
    pub effect_cols: Vec<u8>,
    pub master_vol: f64,
}

pub fn _decode_patn_rows(block: &[u8], version: u16) -> Result<(u16, u16, Vec<FurRow>)> {
    let mut pos = 0usize;
    let _subsong = byte_at(block, pos)?;
    pos += 1;
    let ch: u16 = if version >= 240 {
        let v = u16le(block, pos)?;
        pos += 2;
        v
    } else {
        let v = byte_at(block, pos)? as u16;
        pos += 1;
        v
    };
    let idx = u16le(block, pos)?;
    pos += 2;
    let _name: String;
    (_name, pos) = _read_strz(block, pos);
    let mut rows: Vec<FurRow> = Vec::new();
    while pos < block.len() {
        let mask = byte_at(block, pos)?;
        pos += 1;
        if mask == 0xFF {
            break;
        }
        if mask == 0 {
            rows.push(FurRow::default());
            continue;
        }
        if mask & 0x80 != 0 {
            let skip = ((mask & 0x7F) as usize) + 2;
            rows.extend(std::iter::repeat_with(FurRow::default).take(skip));
            continue;
        }
        let mut extra: u16 = 0;
        if mask & 0x20 != 0 {
            extra |= byte_at(block, pos)? as u16;
            pos += 1;
        }
        if mask & 0x40 != 0 {
            extra |= (byte_at(block, pos)? as u16) << 8;
            pos += 1;
        }
        let mut effect_mask = extra;
        if mask & 0x08 != 0 {
            effect_mask |= 1;
        }
        if mask & 0x10 != 0 {
            effect_mask |= 2;
        }
        let mut row = FurRow::default();
        if mask & 1 != 0 {
            row.note = byte_at(block, pos)? as i32;
            pos += 1;
        }
        if mask & 2 != 0 {
            row.ins = byte_at(block, pos)? as i32;
            pos += 1;
        }
        if mask & 4 != 0 {
            row.vol = byte_at(block, pos)? as i32;
            pos += 1;
        }
        let mut fx_bytes: Vec<u8> = Vec::new();
        for bit in 0..16 {
            if effect_mask & (1 << bit) != 0 {
                fx_bytes.push(byte_at(block, pos)?);
                pos += 1;
            }
        }
        let mut col = 0usize;
        while col + 1 < fx_bytes.len() {
            row.fx.push((fx_bytes[col], fx_bytes[col + 1]));
            col += 2;
        }
        rows.push(row);
    }
    Ok((ch, idx, rows))
}

pub fn _parse_ins_sample_index(payload: &[u8]) -> Result<i32> {
    // INS2: walk NA/SM/FM/MA features for the SM sample index. -1 if none.
    if payload.len() < 4 {
        return Ok(-1);
    }
    let mut pos = 4usize; // version + type
    while pos + 2 <= payload.len() {
        let feat = &payload[pos..pos + 2];
        if feat == b"EN" {
            break;
        }
        if pos + 4 > payload.len() {
            break;
        }
        let size = u16le(payload, pos + 2)? as usize;
        let body = pos + 4;
        if feat == b"SM" && size >= 2 {
            return Ok(u16le(payload, body)? as i32);
        }
        pos = body + size;
    }
    Ok(-1)
}

pub fn _n_ch_from_chips(chips: &[u8]) -> usize {
    let mut n = 0usize;
    for &c in chips {
        if c == 0 {
            continue;
        }
        n += chip_channels(c);
    }
    n
}

pub fn _pcm_channels(chips: &[u8]) -> Vec<usize> {
    // Module channels of the chips that carry playable sample bodies.
    //
    // A module with no such chip (a YM2151-only transcription, or one whose
    // sample chip is a VOX one) has no PCM channels: the old "last two" guess
    // read the FM tail as PCM on every melody-only file.
    let mut ch = 0usize;
    let mut pcm: Vec<usize> = Vec::new();
    for &c in chips {
        if c == 0 {
            continue;
        }
        let n = chip_channels(c);
        if _PCM_BODY_CHIPS.contains(&c) {
            pcm.extend(ch..ch + n);
        }
        ch += n;
    }
    pcm
}

pub fn parse_fur(data: &[u8]) -> Result<FurModule> {
    let data = decompress_fur(data);
    let ver = u16le(&data, 16)?;
    let info_ptr = u32le(&data, 20)? as usize;
    let mag = slice_n(&data, info_ptr, 4);
    if mag != b"INFO" && mag != b"INF2" {
        return Err(Error::Msg("INFO/INF2 missing".to_string()));
    }
    let mut mag4 = [0u8; 4];
    mag4.copy_from_slice(mag);
    let size = u32le(&data, info_ptr + 4)? as usize;
    let info_start = info_ptr + 8;
    let payload = slice_n(&data, info_start, size);

    let samples = list_fur_samples(&data)?;
    let mut ins_sample: Vec<i32> = Vec::new();
    let mut patterns: IndexMap<(u16, u16), Vec<FurRow>> = IndexMap::new();
    for block in iter_blocks(&data)? {
        if block.magic == *b"INS2" {
            ins_sample.push(_parse_ins_sample_index(block.payload)?);
        } else if block.magic == *b"PATN" {
            let (ch, idx, rows) = _decode_patn_rows(block.payload, ver)?;
            patterns.insert((ch, idx), rows);
        }
    }

    if mag4 == *b"INF2" {
        return _parse_inf2(ver, payload, samples, ins_sample, patterns, &data);
    }
    _parse_info(ver, payload, samples, ins_sample, patterns)
}

#[derive(Clone, Debug, PartialEq)]
pub struct InfoWalk {
    pub speed: i32,
    pub hz: f64,
    pub pat_len: u16,
    pub n_ord: u16,
    pub n_ch: usize,
    pub chips: Vec<u8>,
    pub orders: Vec<Vec<u16>>,
    pub effect_cols: Vec<u8>,
    pub name: String,
    pub master_vol: f64,
    pub master_off: i64,
}

pub fn _info_walk(payload: &[u8], ver: u16) -> Result<InfoWalk> {
    // Walk an INFO payload to its channel state and master volume.
    //
    // `master_off` is payload-relative (Furnace stores the song volume after
    // the comment string); -1 when the version predates it.
    let speed = {
        let v = byte_at(payload, 1)?;
        if v != 0 { v as i32 } else { 1 }
    };
    let hz = {
        let v = f32le(payload, 4)?;
        if v != 0.0 { v as f64 } else { 60.0 }
    };
    let pat_len = {
        let v = u16le(payload, 8)?;
        if v != 0 { v } else { 64 }
    };
    let n_ord = {
        let v = u16le(payload, 10)?;
        if v != 0 { v } else { 1 }
    };
    let n_ins = u16le(payload, 14)? as usize;
    let n_wav = u16le(payload, 16)? as usize;
    let n_smp = u16le(payload, 18)? as usize;
    let n_pat = u32le(payload, 20)? as usize;
    let chips: Vec<u8> = slice_n(payload, 24, 32)
        .iter()
        .copied()
        .filter(|&c| c != 0)
        .collect();
    let n_ch = {
        let v = _n_ch_from_chips(&chips);
        if v != 0 { v } else { 2 }
    };
    let mut pos = 24usize + 32 + 32 + 32 + 128;
    let name: String;
    (name, pos) = _read_strz(payload, pos);
    let _author: String;
    (_author, pos) = _read_strz(payload, pos);
    pos += 4; // tuning
    pos += 20; // compat phase 1
    pos += (n_ins + n_wav + n_smp) * 4;
    pos += n_pat * 4;
    let mut orders: Vec<Vec<u16>> = Vec::new();
    for _ch in 0..n_ch {
        orders.push(
            slice_n(payload, pos, n_ord as usize)
                .iter()
                .map(|&b| b as u16)
                .collect(),
        );
        pos += n_ord as usize;
    }
    let effect_cols: Vec<u8> = slice_n(payload, pos, n_ch).to_vec();
    pos += n_ch;
    let mut master_vol = 1.0f64;
    let mut master_off: i64 = -1;
    if ver >= 39 {
        pos += 2 * n_ch; // channel show + collapse bytes
        for _ in 0..2 * n_ch {
            // long and short names
            let _s: String;
            (_s, pos) = _read_strz(payload, pos);
        }
        let _notes: String;
        (_notes, pos) = _read_strz(payload, pos);
        if ver >= 59 && pos + 4 <= payload.len() {
            master_vol = f32le(payload, pos)? as f64;
            master_off = pos as i64;
        }
    }
    Ok(InfoWalk {
        speed,
        hz,
        pat_len,
        n_ord,
        n_ch,
        chips,
        orders,
        effect_cols,
        name,
        master_vol,
        master_off,
    })
}

pub fn _parse_info(
    ver: u16,
    payload: &[u8],
    samples: Vec<FurSample>,
    ins_sample: Vec<i32>,
    patterns: IndexMap<(u16, u16), Vec<FurRow>>,
) -> Result<FurModule> {
    let w = _info_walk(payload, ver)?;
    Ok(FurModule {
        version: ver,
        speed: w.speed.max(1),
        hz: w.hz,
        pat_len: w.pat_len,
        n_ord: w.n_ord,
        n_ch: w.n_ch,
        chips: w.chips.clone(),
        pcm_channels: _pcm_channels(&w.chips),
        orders: w.orders,
        samples,
        ins_sample,
        patterns,
        name: w.name,
        effect_cols: w.effect_cols,
        master_vol: w.master_vol,
    })
}

pub fn _parse_inf2(
    ver: u16,
    payload: &[u8],
    samples: Vec<FurSample>,
    ins_sample: Vec<i32>,
    patterns: IndexMap<(u16, u16), Vec<FurRow>>,
    data: &[u8],
) -> Result<FurModule> {
    let mut pos = 0usize;
    for _ in 0..8 {
        let _s: String;
        (_s, pos) = _read_strz(payload, pos);
    }
    pos += 4; // tuning
    pos += 1; // auto system name
    let master_vol = f32le(payload, pos)? as f64;
    pos += 4; // master vol
    let n_ch_read = u16le(payload, pos)? as usize;
    pos += 2;
    let n_chips = u16le(payload, pos)? as usize;
    pos += 2;
    let mut chips: Vec<u8> = Vec::new();
    for _ in 0..n_chips {
        let cid = u16le(payload, pos)?;
        pos += 2;
        let _cc = u16le(payload, pos)?;
        pos += 2;
        pos += 12; // vol, pan, rear
        chips.push(cid as u8);
    }
    let n_conn = u32le(payload, pos)? as usize;
    pos += 4 + 4 * n_conn;
    pos += 1; // auto patchbay
    let mut sng2_off: Option<usize> = None;
    while pos < payload.len() {
        let etype = byte_at(payload, pos)?;
        pos += 1;
        if etype == 0 {
            break;
        }
        let n_el = u32le(payload, pos)? as usize;
        pos += 4;
        let mut ptrs: Vec<u32> = Vec::new();
        for i in 0..n_el {
            ptrs.push(u32le(payload, pos + 4 * i)?);
        }
        pos += 4 * n_el;
        if etype == 1 && !ptrs.is_empty() {
            sng2_off = Some(ptrs[0] as usize);
        }
    }
    let mut speed: i32 = 6;
    let mut hz: f64 = 60.0;
    let mut pat_len: u16 = 64;
    let mut n_ord: u16 = 1;
    let mut orders: Vec<Vec<u16>> = Vec::new();
    let mut name = String::new();
    let mut effect_cols: Vec<u8> = Vec::new();
    if let Some(off) = sng2_off {
        if slice_n(data, off, 4) == b"SNG2" {
            let sz = u32le(data, off + 4)? as usize;
            let sng = slice_n(data, off + 8, sz);
            hz = {
                let v = f32le(sng, 0)?;
                if v != 0.0 { v as f64 } else { 60.0 }
            };
            pat_len = {
                let v = u16le(sng, 6)?;
                if v != 0 { v } else { 64 }
            };
            n_ord = {
                let v = u16le(sng, 8)?;
                if v != 0 { v } else { 1 }
            };
            let _sp_len = byte_at(sng, 16)?;
            // speed pattern starts at 17, 16 unsigned shorts
            speed = {
                let v = u16le(sng, 17)?;
                if v != 0 { v as i32 } else { 1 }
            };
            let mut p = 17 + 32;
            (name, p) = _read_strz(sng, p);
            let _cmt: String;
            (_cmt, p) = _read_strz(sng, p);
            for _ch in 0..n_ch_read {
                orders.push(
                    slice_n(sng, p, n_ord as usize)
                        .iter()
                        .map(|&b| b as u16)
                        .collect(),
                );
                p += n_ord as usize;
            }
            effect_cols = slice_n(sng, p, n_ch_read).to_vec();
        }
    }
    if orders.is_empty() {
        return Err(Error::Msg(
            "INF2 module has no SNG2 orders (cannot render)".to_string(),
        ));
    }
    Ok(FurModule {
        version: ver,
        speed: speed.max(1),
        hz,
        pat_len,
        n_ord,
        n_ch: if n_ch_read != 0 {
            n_ch_read
        } else {
            _n_ch_from_chips(&chips)
        },
        chips: chips.clone(),
        pcm_channels: _pcm_channels(&chips),
        orders,
        samples,
        ins_sample,
        patterns,
        name,
        effect_cols,
        master_vol,
    })
}

pub fn _block(data: &[u8]) -> Result<([u8; 4], &[u8], usize)> {
    // (magic, payload, payload offset) of the INFO or INF2 block.
    let info_ptr = u32le(data, 20)? as usize;
    let mag = slice_n(data, info_ptr, 4);
    if mag != b"INFO" && mag != b"INF2" {
        return Err(Error::Msg("INFO/INF2 missing".to_string()));
    }
    let mut mag4 = [0u8; 4];
    mag4.copy_from_slice(mag);
    let size = u32le(data, info_ptr + 4)? as usize;
    let start = info_ptr + 8;
    Ok((mag4, slice_n(data, start, size), start))
}

pub fn _inf2_master_off(payload: &[u8]) -> usize {
    let mut pos = 0usize;
    for _ in 0..8 {
        let _s: String;
        (_s, pos) = _read_strz(payload, pos);
    }
    pos + 5 // tuning f32 + auto system name byte
}

pub fn master_volume(data: &[u8]) -> Result<f64> {
    // The song master volume (what Furnace shows as the module volume).
    let ver = u16le(data, 16)?;
    let (mag, payload, _off) = _block(data)?;
    if mag == *b"INF2" {
        return Ok(f32le(payload, _inf2_master_off(payload))? as f64);
    }
    Ok(_info_walk(payload, ver)?.master_vol)
}

pub fn set_master_volume(data: &[u8], volume: f64) -> Result<Vec<u8>> {
    // Patch the master volume in place. Both formats keep it as one f32.
    let ver = u16le(data, 16)?;
    let (mag, payload, off) = _block(data)?;
    let off = if mag == *b"INF2" {
        off + _inf2_master_off(payload)
    } else {
        let master_off = _info_walk(payload, ver)?.master_off;
        if master_off < 0 {
            return Err(Error::Msg(
                "INFO block does not carry a master volume".to_string(),
            ));
        }
        off + master_off as usize
    };
    let mut buf = data.to_vec();
    let end = off.checked_add(4).ok_or_else(|| short_buf(4, off))?;
    let dst = buf.get_mut(off..end).ok_or_else(|| short_buf(4, off))?;
    dst.copy_from_slice(&(volume as f32).to_le_bytes());
    Ok(buf)
}

pub fn fx_pan_amps(val: i32) -> (f64, f64) {
    // 08xy to (left, right). x and y are independent levels, 0-F.
    //
    // Furnace 0.6.8 effect 0x08. 0xFF is both full, 0xF0 is left, 0x0F is
    // right, 0x80 is left at 8/15 and silent on the right.
    let v = (val & 0xFF) as u32;
    (
        ((v >> 4) & 15) as f64 / 15.0,
        (v & 15) as f64 / 15.0,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header(info_ptr: u32) -> Vec<u8> {
        let mut d = vec![0u8; 24];
        d[..16].copy_from_slice(FUR_MAGIC);
        d[16..18].copy_from_slice(&157u16.to_le_bytes());
        d[20..24].copy_from_slice(&info_ptr.to_le_bytes());
        d
    }

    #[test]
    fn read_strz_handles_missing_terminator() {
        assert_eq!(_read_strz(b"abc\0xyz", 0), ("abc".to_string(), 4));
        assert_eq!(_read_strz(b"abc", 0), ("abc".to_string(), 3));
        assert_eq!(_read_strz(b"abc", 9), (String::new(), 3));
    }

    #[test]
    fn smp2_vox_nibble_count_is_halved() {
        let mut p = Vec::new();
        p.extend_from_slice(b"vox\0");
        p.extend_from_slice(&16u32.to_le_bytes());
        p.extend_from_slice(&11111u32.to_le_bytes());
        p.extend_from_slice(&15152u32.to_le_bytes());
        p.extend_from_slice(&[10, 0, 0, 0]);
        p.extend_from_slice(&(-1i32).to_le_bytes());
        p.extend_from_slice(&(-1i32).to_le_bytes());
        p.extend_from_slice(&[0xAA; 16]);
        p.extend_from_slice(&[0x11; 8]);
        let s = parse_smp2_payload(&p).unwrap();
        assert_eq!(s.name, "vox");
        assert_eq!(s.n_frames, 16);
        assert_eq!(s.c4_rate, 15152);
        assert_eq!(s.depth, 10);
        assert_eq!(s.presence, vec![0xAA; 16]);
        assert_eq!(s.data, vec![0x11; 8]);
    }

    #[test]
    fn smp2_pcm_body_is_width_times_frames() {
        let mut p = Vec::new();
        p.extend_from_slice(b"pcm\0");
        p.extend_from_slice(&3u32.to_le_bytes());
        p.extend_from_slice(&0u32.to_le_bytes());
        p.extend_from_slice(&55930u32.to_le_bytes());
        p.extend_from_slice(&[16, 0, 0, 0]);
        p.extend_from_slice(&0i32.to_le_bytes());
        p.extend_from_slice(&0i32.to_le_bytes());
        p.extend_from_slice(&[0; 16]);
        p.extend_from_slice(&[1, 2, 3, 4, 5, 6]);
        let s = parse_smp2_payload(&p).unwrap();
        assert_eq!(s.data, vec![1, 2, 3, 4, 5, 6]);
    }

    #[test]
    fn iter_blocks_rejects_non_furnace_data() {
        let err = iter_blocks(b"not a module").unwrap_err();
        assert_eq!(err.to_string(), "not a Furnace module");
    }

    #[test]
    fn iter_blocks_stops_at_truncated_size() {
        let mut d = header(24);
        d.extend_from_slice(b"SMP2");
        d.extend_from_slice(&1000u32.to_le_bytes()); // runs past the end by > 8
        d.extend_from_slice(b"x");
        let blocks = iter_blocks(&d).unwrap();
        assert!(blocks.is_empty());
    }

    #[test]
    fn patn_rows_decode_note_ins_vol_and_empty_rows() {
        let mut b = Vec::new();
        b.push(0); // subsong
        b.push(3); // ch (version < 240)
        b.extend_from_slice(&5u16.to_le_bytes());
        b.extend_from_slice(b"pat\0");
        b.push(0x07); // note + ins + vol
        b.extend_from_slice(&[60, 2, 15]);
        b.push(0);
        b.push(0xFF);
        let (ch, idx, rows) = _decode_patn_rows(&b, 157).unwrap();
        assert_eq!((ch, idx), (3, 5));
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].note, 60);
        assert_eq!(rows[0].ins, 2);
        assert_eq!(rows[0].vol, 15);
        assert_eq!(rows[1], FurRow::default());
    }

    #[test]
    fn patn_effect_mask_pulls_pairs() {
        let mut b = Vec::new();
        b.push(0);
        b.push(0);
        b.extend_from_slice(&0u16.to_le_bytes());
        b.push(0);
        b.push(0x18); // effect bits 1 and 2 -> one fx pair
        b.extend_from_slice(&[0x08, 0x80]);
        b.push(0xFF);
        let (_, _, rows) = _decode_patn_rows(&b, 157).unwrap();
        assert_eq!(rows[0].fx, vec![(0x08, 0x80)]);
    }

    #[test]
    fn ins_sample_index_walks_features() {
        let mut p = vec![0u8; 4];
        p.extend_from_slice(b"NA");
        p.extend_from_slice(&1u16.to_le_bytes());
        p.push(0);
        p.extend_from_slice(b"SM");
        p.extend_from_slice(&2u16.to_le_bytes());
        p.extend_from_slice(&7u16.to_le_bytes());
        p.extend_from_slice(b"EN");
        assert_eq!(_parse_ins_sample_index(&p).unwrap(), 7);
        assert_eq!(_parse_ins_sample_index(&[0, 0, 0, 0]).unwrap(), -1);
    }

    #[test]
    fn chip_channel_spans_and_pcm_channels() {
        assert_eq!(chip_channels(CHIP_K005289), 2);
        assert_eq!(chip_channels(0xFE), 1);
        assert_eq!(
            _pcm_channels(&[CHIP_K007232, CHIP_MSM6295]),
            vec![0, 1]
        );
    }

    #[test]
    fn master_volume_roundtrip_on_info_payload() {
        // 2 channels (chips all zero), format 157, no comment, one order row.
        let mut payload = vec![0u8; 291];
        payload[1] = 6;
        payload[4..8].copy_from_slice(&60.0f32.to_le_bytes());
        payload[8..10].copy_from_slice(&64u16.to_le_bytes());
        payload[10..12].copy_from_slice(&1u16.to_le_bytes());
        // name/author empty at 248/249; master volume at 287
        payload[287..291].copy_from_slice(&1.0f32.to_le_bytes());
        let mut d = header(24);
        d.extend_from_slice(b"INFO");
        d.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        d.extend_from_slice(&payload);
        assert_eq!(master_volume(&d).unwrap(), 1.0);
        let patched = set_master_volume(&d, 2.5).unwrap();
        assert_eq!(master_volume(&patched).unwrap(), 2.5);
        let m = parse_fur(&patched).unwrap();
        assert_eq!(m.master_vol, 2.5);
        assert_eq!(m.n_ch, 2);
        assert_eq!(m.speed, 6);
        assert_eq!(m.pat_len, 64);
        assert_eq!(m.n_ord, 1);
    }

    #[test]
    fn pan_amps_are_independent_levels() {
        assert_eq!(fx_pan_amps(0xFF), (1.0, 1.0));
        assert_eq!(fx_pan_amps(0xF0), (1.0, 0.0));
        assert_eq!(fx_pan_amps(0x0F), (0.0, 1.0));
        let (l, r) = fx_pan_amps(0x80);
        assert!((l - 8.0 / 15.0).abs() < 1e-12);
        assert_eq!(r, 0.0);
    }
}
