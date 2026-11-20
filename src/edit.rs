//! Edit `.fur` chip (system) volumes in place.
//!
//! Furnace stores one volume per chip, not per channel. An old INFO block keeps
//! the value twice: a byte per chip (64 = unity) that old readers use, and a
//! float triple behind the sample data (vol, panL, panR) that 0.6 reads. INF2
//! keeps only the floats. Both copies are patched together. Every reader
//! agrees.
//!
//! Volume above unity cannot ride in the byte field. INFO files cap at 1.0.
//! INF2 accepts up to 4.0.

use std::path::Path;
use std::sync::LazyLock;

use indexmap::IndexMap;

use crate::Error;
use crate::Result;
use crate::furwrite::{
    CHIP_AY8910, CHIP_C140, CHIP_C219, CHIP_C352, CHIP_GA20, CHIP_K005289, CHIP_K007232,
    CHIP_K051649, CHIP_K052539, CHIP_K053260, CHIP_MSM5232, CHIP_MSM6258, CHIP_MSM6295,
    CHIP_OPL, CHIP_OPL2, CHIP_OPL2_DRUMS, CHIP_OPL3, CHIP_OPL3_DRUMS, CHIP_OPL_DRUMS,
    CHIP_SEGAPCM, CHIP_YM2610, CHIP_YM2610B, CHIP_YM2612, CHIP_YM2612_CSM, CHIP_YM2612_EXT,
    FUR_MAGIC, compress_fur,
};
use crate::parity::{py_int, py_round};

pub static CHIP_NAMES: LazyLock<IndexMap<u32, &'static str>> = LazyLock::new(|| {
    let mut m: IndexMap<u32, &'static str> = IndexMap::new();
    m.insert(0x02, "Sega Genesis");
    m.insert(0x42, "Sega Genesis Ext");
    m.insert(0x82, "YM2151");
    m.insert(0x8D, "YM2203");
    m.insert(CHIP_YM2612 as u32, "YM2612");
    m.insert(CHIP_YM2612_EXT as u32, "YM2612 Ext");
    m.insert(CHIP_YM2612_CSM as u32, "YM2612 CSM");
    m.insert(CHIP_YM2610 as u32, "YM2610");
    m.insert(CHIP_YM2610B as u32, "YM2610B");
    m.insert(CHIP_MSM6295 as u32, "MSM6295");
    m.insert(CHIP_MSM6258 as u32, "MSM6258");
    m.insert(CHIP_K007232 as u32, "K007232");
    m.insert(CHIP_SEGAPCM as u32, "SegaPCM");
    m.insert(CHIP_C140 as u32, "C140");
    m.insert(CHIP_C219 as u32, "C219");
    m.insert(CHIP_C352 as u32, "C352");
    m.insert(CHIP_GA20 as u32, "GA20");
    m.insert(CHIP_MSM5232 as u32, "MSM5232");
    m.insert(CHIP_K053260 as u32, "K053260");
    m.insert(CHIP_K051649 as u32, "SCC");
    m.insert(CHIP_K052539 as u32, "SCC+");
    m.insert(CHIP_K005289 as u32, "Bubble System WSG");
    m.insert(0x8A, "FDS");
    m.insert(0x96, "WonderSwan");
    m.insert(0xE0, "QSound");
    m.insert(CHIP_AY8910 as u32, "AY8910");
    // OPL family, named as Furnace names the system. A rhythm module is not the
    // same instrument set: bank A's last channels are the kit.
    m.insert(CHIP_OPL as u32, "OPL (YM3526)");
    m.insert(CHIP_OPL2 as u32, "OPL2 (YM3812)");
    m.insert(CHIP_OPL3 as u32, "OPL3 (YMF262)");
    m.insert(CHIP_OPL_DRUMS as u32, "OPL (YM3526) + drums");
    m.insert(CHIP_OPL2_DRUMS as u32, "OPL2 (YM3812) + drums");
    m.insert(CHIP_OPL3_DRUMS as u32, "OPL3 (YMF262) + drums");
    m
});

/// Old readers take the byte field as a signed char. It stays <= 64.
pub const MAX_INFO_VOLUME: f64 = 1.0;
pub const MAX_INF2_VOLUME: f64 = 4.0;

#[derive(Clone, Debug, PartialEq)]
pub struct Chip {
    pub index: usize,
    pub id: u32,
    pub volume: f64,
    pub pan: f64,
    pub pan_fr: f64,
}

impl Chip {
    pub fn name(&self) -> String {
        match CHIP_NAMES.get(&self.id) {
            Some(name) => (*name).to_string(),
            None => format!("0x{:02x}", self.id),
        }
    }
}

fn fur_edit(msg: impl Into<String>) -> Error {
    Error::FurEdit(msg.into())
}

fn find_bytes(hay: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() {
        return Some(0);
    }
    if hay.len() < needle.len() {
        return None;
    }
    hay.windows(needle.len()).position(|w| w == needle)
}

fn read_u16(data: &[u8], off: usize) -> Result<u16> {
    let b = data
        .get(off..off + 2)
        .ok_or_else(|| Error::Msg(format!("unpack_from requires a buffer of at least 2 bytes at offset {off}")))?;
    Ok(u16::from_le_bytes([b[0], b[1]]))
}

fn read_u32(data: &[u8], off: usize) -> Result<u32> {
    let b = data
        .get(off..off + 4)
        .ok_or_else(|| Error::Msg(format!("unpack_from requires a buffer of at least 4 bytes at offset {off}")))?;
    Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
}

/// `struct.unpack_from("<fff", data, off)`.
fn read_fff(data: &[u8], off: usize) -> Result<(f64, f64, f64)> {
    let b = data
        .get(off..off + 12)
        .ok_or_else(|| Error::Msg(format!("unpack_from requires a buffer of at least 12 bytes at offset {off}")))?;
    let a = f32::from_le_bytes([b[0], b[1], b[2], b[3]]) as f64;
    let c = f32::from_le_bytes([b[4], b[5], b[6], b[7]]) as f64;
    let d = f32::from_le_bytes([b[8], b[9], b[10], b[11]]) as f64;
    Ok((a, c, d))
}

fn write_f32(data: &mut [u8], off: usize, value: f32) -> Result<()> {
    let slot = data
        .get_mut(off..off + 4)
        .ok_or_else(|| Error::Msg(format!("pack_into requires a buffer of at least 4 bytes at offset {off}")))?;
    slot.copy_from_slice(&value.to_le_bytes());
    Ok(())
}

fn byte_at(data: &[u8], off: usize) -> Result<u8> {
    data.get(off)
        .copied()
        .ok_or_else(|| Error::Msg(format!("index out of range: {off}")))
}

pub fn _cstring(data: &[u8], pos: usize) -> Result<(String, usize)> {
    let tail = data
        .get(pos..)
        .ok_or_else(|| Error::Msg("subsection not found".to_string()))?;
    let end = tail
        .iter()
        .position(|&b| b == 0)
        .ok_or_else(|| Error::Msg("subsection not found".to_string()))?;
    let text = String::from_utf8_lossy(&tail[..end]).into_owned();
    Ok((text, pos + end + 1))
}

/// (payload offset, payload size) of the INFO or INF2 block.
pub fn _block(data: &[u8], magic: &[u8]) -> Result<(usize, usize)> {
    if data.len() < FUR_MAGIC.len() + 12 {
        return Err(fur_edit("not a Furnace module"));
    }
    let mut ptr = read_u32(data, FUR_MAGIC.len() + 4)? as usize;
    if data.get(ptr..ptr + 4) != Some(magic) {
        ptr = find_bytes(data, magic).ok_or_else(|| {
            fur_edit(format!("no {} block", String::from_utf8_lossy(magic)))
        })?;
    }
    let size = read_u32(data, ptr + 4)? as usize;
    Ok((ptr + 8, size))
}

/// Offset of the float triples, or `None` when the tail does not check out.
///
/// The payload tail is `[triples][patchbay 4][auto 1][pad 8][speed 17][groove 1]`
/// followed by three asset-directory pointers. Anchor on those: the first has
/// to resolve to an ADIR block.
pub fn _info_triples(
    data: &[u8],
    payload: usize,
    payload_end: usize,
    n_chips: usize,
) -> Result<Option<usize>> {
    let start = payload_end as i64 - 12;
    let stop = (payload as i64).max(payload_end as i64 - 4096) - 1;
    let len = data.len() as i64;
    let mut off = start;
    while off > stop {
        let o = usize::try_from(off)
            .map_err(|_| Error::Msg(format!("unpack_from requires a non-negative offset: {off}")))?;
        let ptr = read_u32(data, o)? as i64;
        if 0 < ptr && ptr <= len - 4 {
            let p = ptr as usize;
            if data.get(p..p + 4) == Some(&b"ADIR"[..]) {
                let triples = off - 31 - 12 * n_chips as i64;
                if triples >= payload as i64 {
                    return Ok(Some(triples as usize));
                }
                return Ok(None);
            }
        }
        off -= 4;
    }
    Ok(None)
}

/// Chips plus (byte-volume offset, triples offset) for an old INFO block.
pub fn _read_info(data: &[u8]) -> Result<(Vec<Chip>, usize, i64)> {
    let (payload, size) = _block(data, b"INFO")?;
    let payload_end = payload + size;
    let mut n_chips = 0usize;
    for i in 0..32 {
        if byte_at(data, payload + 24 + i)? != 0 {
            n_chips = i + 1;
        }
    }
    if n_chips < 1 {
        return Err(fur_edit("INFO block declares no chips"));
    }
    let vol_off = payload + 56;
    let triples = _info_triples(data, payload, payload_end, n_chips)?;
    let mut chips = Vec::with_capacity(n_chips);
    for i in 0..n_chips {
        let chip_id = byte_at(data, payload + 24 + i)?;
        let legacy = byte_at(data, vol_off + i)?;
        let (volume, pan, pan_fr) = match triples {
            Some(t) => read_fff(data, t + 12 * i)?,
            None => (legacy as f64 / 64.0, 0.0, 0.0),
        };
        chips.push(Chip {
            index: i,
            id: chip_id as u32,
            volume,
            pan,
            pan_fr,
        });
    }
    Ok((chips, vol_off, triples.map_or(-1, |t| t as i64)))
}

/// Chips from an INF2 header plus the offset of the first volume float.
pub fn _read_inf2(data: &[u8]) -> Result<(Vec<Chip>, usize)> {
    let (payload, _size) = _block(data, b"INF2")?;
    let mut pos = payload;
    for _ in 0..8 {
        let (_s, next) = _cstring(data, pos)?;
        pos = next;
    }
    pos += 4 + 1 + 4 + 2 + 2; // tuning, autoSystem, master vol, chans, systems
    let mut chips = Vec::new();
    let mut first: Option<usize> = None;
    for i in 0..32 {
        if pos + 16 > data.len() {
            break;
        }
        let chip_id = read_u16(data, pos)?;
        if chip_id == 0 {
            break;
        }
        if first.is_none() {
            first = Some(pos + 4);
        }
        let (volume, pan, pan_fr) = read_fff(data, pos + 4)?;
        chips.push(Chip {
            index: i,
            id: chip_id as u32,
            volume,
            pan,
            pan_fr,
        });
        pos += 16;
    }
    if chips.is_empty() || first.is_none() {
        return Err(fur_edit("INF2 block declares no chips"));
    }
    Ok((chips, first.unwrap()))
}

fn has_inf2(data: &[u8]) -> bool {
    find_bytes(&data[..data.len().min(0x2000)], b"INF2").is_some()
}

pub fn read_chips(data: &[u8]) -> Result<Vec<Chip>> {
    if has_inf2(data) {
        if let Ok((chips, _first)) = _read_inf2(data) {
            return Ok(chips);
        }
    }
    Ok(_read_info(data)?.0)
}

pub fn read_chips_file(path: &Path) -> Result<Vec<Chip>> {
    let data = crate::furio::load_fur_bytes(path)?;
    read_chips(&data)
}

/// Scale or set chip volumes. Returns the patched bytes and the new chips.
///
/// `targets` holds chip indexes as listed by `read_chips`. `None` means every
/// chip. Give exactly one of `factor` and `value`.
pub fn set_chip_volume(
    data: &[u8],
    factor: Option<f64>,
    value: Option<f64>,
    targets: Option<&[usize]>,
) -> Result<(Vec<u8>, Vec<Chip>)> {
    if factor.is_none() == value.is_none() {
        return Err(Error::Msg(
            "give exactly one of factor and value".to_string(),
        ));
    }
    let (chips, first, limit, vol_off) = if has_inf2(data) {
        let (chips, first) = _read_inf2(data)?;
        (chips, first, MAX_INF2_VOLUME, None)
    } else {
        let (chips, vol_off, triples) = _read_info(data)?;
        if triples < 0 {
            return Err(fur_edit(
                "INFO tail not recognised, refusing to patch volumes",
            ));
        }
        (chips, triples as usize, MAX_INFO_VOLUME, Some(vol_off))
    };
    let mut buf = data.to_vec();
    let chosen: std::collections::BTreeSet<usize> = match targets {
        None => (0..chips.len()).collect(),
        Some(t) => t.iter().copied().collect(),
    };
    for chip in &chips {
        if !chosen.contains(&chip.index) {
            continue;
        }
        let mut new = match value {
            Some(v) => v,
            None => chip.volume * factor.unwrap(),
        };
        new = new.min(limit).max(0.0);
        write_f32(&mut buf, first + 12 * chip.index, new as f32)?;
        if let Some(vol_off) = vol_off {
            let q = py_int(py_round(new * 64.0)).max(0).min(64) as u8;
            *buf.get_mut(vol_off + chip.index).ok_or_else(|| {
                Error::Msg(format!(
                    "index out of range: {}",
                    vol_off + chip.index
                ))
            })? = q;
        }
    }
    let chips = read_chips(&buf)?;
    Ok((buf, chips))
}

pub fn set_chip_volume_file(
    path: &Path,
    factor: Option<f64>,
    value: Option<f64>,
    targets: Option<&[usize]>,
    dry_run: bool,
) -> Result<Vec<Chip>> {
    let raw = std::fs::read(path).map_err(|e| Error::io(path, e))?;
    let data = crate::furio::load_fur_bytes(path)?;
    let (mut patched, chips) = set_chip_volume(&data, factor, value, targets)?;
    if !dry_run && patched != data {
        if raw.get(..FUR_MAGIC.len()) != Some(FUR_MAGIC) {
            patched = compress_fur(&patched, 5);
        }
        std::fs::write(path, &patched).map_err(|e| Error::io(path, e))?;
    }
    Ok(chips)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The INF2 fixture from `tests/test_edit.py`: one YM2151 chip.
    fn inf2(volume: f32) -> Vec<u8> {
        fn s(text: &str) -> Vec<u8> {
            let mut out = text.as_bytes().to_vec();
            out.push(0);
            out
        }

        let mut payload = Vec::new();
        for text in ["name", "author", "system", "", "", "", "", ""] {
            payload.extend_from_slice(&s(text));
        }
        payload.extend_from_slice(&440.0f32.to_le_bytes());
        payload.push(0);
        payload.extend_from_slice(&1.0f32.to_le_bytes());
        payload.extend_from_slice(&8u16.to_le_bytes());
        payload.extend_from_slice(&1u16.to_le_bytes());
        payload.extend_from_slice(&0x82u16.to_le_bytes());
        payload.extend_from_slice(&8u16.to_le_bytes());
        payload.extend_from_slice(&volume.to_le_bytes());
        payload.extend_from_slice(&0.0f32.to_le_bytes());
        payload.extend_from_slice(&0.0f32.to_le_bytes());

        let mut buf = vec![0u8; 16];
        buf[0..16].copy_from_slice(FUR_MAGIC);
        buf.extend_from_slice(&157u16.to_le_bytes());
        buf.extend_from_slice(&0u16.to_le_bytes());
        buf.extend_from_slice(&32u32.to_le_bytes());
        buf.extend_from_slice(&[0u8; 8]);
        buf.extend_from_slice(b"INF2");
        buf.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        buf.extend_from_slice(&payload);
        buf
    }

    fn assert_almost(a: f64, b: f64, places: i32) {
        assert_eq!(py_round((a - b) * 10f64.powi(places)), 0.0, "{a} != {b}");
    }

    #[test]
    fn reads_inf2_chips() {
        let chips = read_chips(&inf2(0.8)).unwrap();
        assert_eq!(chips.len(), 1);
        assert_eq!(chips[0].id, 0x82);
        assert_eq!(chips[0].name(), "YM2151");
        assert_almost(chips[0].volume, 0.8, 3);
    }

    #[test]
    fn inf2_scales_and_caps() {
        let (patched, chips) = set_chip_volume(&inf2(0.8), Some(1.25), None, None).unwrap();
        assert_almost(chips[0].volume, 1.0, 3);
        let (patched2, chips2) = set_chip_volume(&patched, None, Some(2.0), None).unwrap();
        assert_almost(chips2[0].volume, 2.0, 3);
        assert_eq!(read_chips(&patched2).unwrap()[0].volume as f32, 2.0f32);
    }

    #[test]
    fn inf2_targets_keep_other_chips() {
        let data = inf2(0.8);
        let (patched, chips) = set_chip_volume(&data, Some(0.5), None, Some(&[])).unwrap();
        assert_eq!(patched, data);
        assert_almost(chips[0].volume, 0.8, 3);
    }

    #[test]
    fn needs_exactly_one_of_factor_value() {
        let data = inf2(1.0);
        assert!(matches!(
            set_chip_volume(&data, None, None, None),
            Err(Error::Msg(m)) if m == "give exactly one of factor and value"
        ));
        assert!(set_chip_volume(&data, Some(1.0), Some(1.0), None).is_err());
    }

    #[test]
    fn rejects_a_foreign_buffer() {
        assert!(matches!(
            read_chips(b"not a fur at all"),
            Err(Error::FurEdit(m)) if m == "not a Furnace module"
        ));
        assert_eq!(Chip { index: 0, id: 0x99, volume: 1.0, pan: 0.0, pan_fr: 0.0 }.name(), "0x99");
        assert_eq!(_cstring(b"ab\0cd", 0).unwrap(), ("ab".to_string(), 3));
    }
}
