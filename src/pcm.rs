//! K007232 ROM → 7-bit PCM samples → WAV + JSON.
//!
//! Port of `vgm2151fur/pcm.py` (v0.11.0).

use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::fs;
use std::path::Path;

use indexmap::IndexMap;
use serde_json::{json, Value};

use crate::error::Result;
use crate::parity::{py_int, py_round};
pub use crate::vgm::K007232_TRIGGER_REG;
use crate::vgm::{assemble_k007232_rom, ChipWrite, VgmFile};

// A key-on repeated within this many VGM samples is the same hit.  Drivers
// key the K007232 by reading register 0x05/0x0B, and several read it twice in a
// row; the logger puts the pair 0-1 samples apart.  libvgm and Furnace's core
// retrigger on every read, but the second restart lands before the chip can
// fetch another byte and is inaudible.  It also cannot be a separate row: the
// module's finest timing is one tick, tens of samples.
pub const KEYON_MERGE_SAMPLES: i64 = 2;

/// Hardware playback rate in Hz for a 12-bit pitch register.
///
/// Matches Furnace/MAME: chipClock/4 / (4096 - pitch).
pub fn k007232_rate(clock: u32, pitch: i32) -> f64 {
    let mut period = 4096 - (pitch & 0xFFF);
    if period <= 0 {
        period = 1;
    }
    (clock as f64 / 4.0) / period as f64
}

/// Bytes from start until (and not including) the first byte with bit 7 set.
///
/// Hardware stops on bit 7; that marker byte is not audible.
pub fn decode_7bit_pcm(rom: &[u8], start: i64) -> Vec<u8> {
    if start < 0 || start >= rom.len() as i64 {
        return Vec::new();
    }
    let mut out = Vec::new();
    for &b in &rom[start as usize..] {
        if b & 0x80 != 0 {
            break;
        }
        out.push(b);
    }
    out
}

/// 7-bit unsigned (0..127, center 64) → 16-bit signed little-endian.
pub fn pcm7_to_i16(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len() * 2);
    for &b in data {
        let v = (((b & 0x7F) as i32) - 64) << 9;
        let v = v.max(-32768).min(32767) as i16;
        out.extend_from_slice(&v.to_le_bytes());
    }
    out
}

/// 7-bit unsigned → 8-bit signed (Furnace 8-bit PCM).
pub fn pcm7_to_s8(data: &[u8]) -> Vec<u8> {
    data.iter()
        .map(|&b| {
            let v = (((b & 0x7F) as i32) - 64) * 2;
            v.max(-128).min(127) as i8 as u8
        })
        .collect()
}

/// Float −1..1 → 7-bit unsigned (center 64).
pub fn frames_to_pcm7(frames: &[f64]) -> Vec<u8> {
    frames
        .iter()
        .map(|&v| {
            let scaled = 64 + py_int(py_round(v * 64.0));
            scaled.max(0).min(127) as u8
        })
        .collect()
}

/// Scale 7-bit unsigned PCM about center 64. gain=1 leaves the body as-is.
pub fn scale_pcm7(data: &[u8], gain: f64) -> Vec<u8> {
    if (gain - 1.0).abs() < 1e-6 {
        return data.to_vec();
    }
    data.iter()
        .map(|&b| {
            let v = 64 + py_int(py_round((((b & 0x7F) as f64) - 64.0) * gain));
            v.max(0).min(127) as u8
        })
        .collect()
}

/// Mono 16-bit WAV of 7-bit PCM, matching Python `wave`'s canonical header.
pub fn write_wav(path: &Path, pcm7: &[u8], rate: i64) -> Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)?;
        }
    }
    let payload = pcm7_to_i16(pcm7);
    let rate = 1000.max(rate);
    let data_len = payload.len() as u32;
    let byte_rate = rate as u32 * 2;
    let mut out: Vec<u8> = Vec::with_capacity(44 + payload.len());
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + data_len).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes()); // PCM
    out.extend_from_slice(&1u16.to_le_bytes()); // mono
    out.extend_from_slice(&(rate as u32).to_le_bytes());
    out.extend_from_slice(&byte_rate.to_le_bytes());
    out.extend_from_slice(&2u16.to_le_bytes()); // block align
    out.extend_from_slice(&16u16.to_le_bytes()); // bits per sample
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data_len.to_le_bytes());
    out.extend_from_slice(&payload);
    fs::write(path, out)?;
    Ok(())
}

/// Every region that starts after an end marker (or at 0).
pub fn split_rom_on_end_markers(rom: &[u8]) -> Vec<(i64, Vec<u8>)> {
    let mut samples: Vec<(i64, Vec<u8>)> = Vec::new();
    let n = rom.len() as i64;
    let mut i: i64 = 0;
    while i < n {
        while i < n && (rom[i as usize] & 0x80) != 0 {
            i += 1;
        }
        if i >= n {
            break;
        }
        let start = i;
        let body = decode_7bit_pcm(rom, start);
        if !body.is_empty() {
            i = start + body.len() as i64 + 1;
            samples.push((start, body));
        } else {
            i += 1;
        }
    }
    samples
}

#[derive(Clone, Debug, PartialEq)]
pub struct PcmHit {
    pub sample_time: i64,
    pub ch: i32,
    pub start: i64,
    pub pitch: i32,
    pub bank: i32,
    pub vol_l: i32,
    pub vol_r: i32,
    pub chip_id: u8,
    /// Loop-enable register value at the trigger; looped bodies have no natural
    /// end marker to convert into a note-off.
    pub loop_: bool,
    /// Live volume-register changes while this hit is active:
    /// (sample, left, right), excluding the trigger values.
    pub vol_pts: Vec<(i64, i32, i32)>,
}

impl Default for PcmHit {
    fn default() -> Self {
        PcmHit {
            sample_time: 0,
            ch: 0,
            start: 0,
            pitch: 0,
            bank: 0,
            vol_l: 0,
            vol_r: 0,
            chip_id: 0,
            loop_: false,
            vol_pts: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct PcmSample {
    pub start: i64,
    pub pcm7: Vec<u8>,
    pub hits: Vec<PcmHit>,
    pub chip_id: u8,
    /// Multiply hardware Hz (Furnace C-4 and the synth path).
    pub rate_scale: f64,
    /// If set, C-4 / pattern notes use this pitch instead of mode_pitch().
    pub c4_pitch: Option<i32>,
    /// Linear amplitude correction against the recorded ROM body.
    pub volume_scale: f64,
}

impl Default for PcmSample {
    fn default() -> Self {
        PcmSample {
            start: 0,
            pcm7: Vec::new(),
            hits: Vec::new(),
            chip_id: 0,
            rate_scale: 1.0,
            c4_pitch: None,
            volume_scale: 1.0,
        }
    }
}

impl PcmSample {
    /// Python's `PcmSample(start=..., pcm7=...)`, every defaulted field left
    /// at its default.
    pub fn new(start: i64, pcm7: Vec<u8>) -> Self {
        PcmSample { start, pcm7, ..Default::default() }
    }

    pub fn length(&self) -> usize {
        self.pcm7.len()
    }

    pub fn mode_pitch(&self) -> i32 {
        if self.hits.is_empty() {
            return 4051;
        }
        let mut counts: IndexMap<i32, i64> = IndexMap::new();
        for h in &self.hits {
            *counts.entry(h.pitch).or_insert(0) += 1;
        }
        let mut best: Option<(i32, i64)> = None;
        for (&pitch, &n) in counts.iter() {
            match best {
                Some((_, bn)) if n <= bn => {}
                _ => best = Some((pitch, n)),
            }
        }
        best.map(|(p, _)| p).unwrap_or(4051)
    }

    pub fn effective_c4_pitch(&self) -> i32 {
        match self.c4_pitch {
            Some(p) => p,
            None => self.mode_pitch(),
        }
    }

    pub fn c4_rate(&self, clock: u32) -> i64 {
        let hz = k007232_rate(clock, self.effective_c4_pitch()) * self.rate_scale;
        1000.max(py_int(py_round(hz)))
    }
}

pub fn collect_pcm_hits(vgm: &VgmFile) -> Vec<PcmHit> {
    let ids: BTreeSet<u8> = vgm
        .writes
        .iter()
        .filter(|w| w.chip == "k007232")
        .map(|w| w.chip_id)
        .collect();
    let mut hits: Vec<PcmHit> = Vec::new();
    for cid in ids {
        let writes: Vec<&ChipWrite> = vgm
            .writes
            .iter()
            .filter(|w| w.chip == "k007232" && w.chip_id == cid)
            .collect();
        hits.extend(collect_pcm_chip(&writes, cid));
    }
    hits
}

fn collect_pcm_chip(writes: &[&ChipWrite], chip_id: u8) -> Vec<PcmHit> {
    let mut ch_pitch = [0i32, 0];
    let mut ch_start = [0i32, 0];
    let mut ch_bank = [0i32, 0];
    let mut vol = [0i32, 0, 0, 0]; // 0x10..0x13
    let mut loop_en = 0i32;
    let mut hits: Vec<PcmHit> = Vec::new();
    let mut active: [Option<usize>; 2] = [None, None];
    for w in writes {
        let r = w.reg as i32;
        let v = w.val as i32;
        if r == 0 {
            ch_pitch[0] = (ch_pitch[0] & 0xF00) | v;
        } else if r == 1 {
            ch_pitch[0] = (ch_pitch[0] & 0x0FF) | ((v & 0x0F) << 8);
        } else if r == 2 {
            ch_start[0] = (ch_start[0] & 0x1FF00) | v;
        } else if r == 3 {
            ch_start[0] = (ch_start[0] & 0x100FF) | (v << 8);
        } else if r == 4 {
            ch_start[0] = (ch_start[0] & 0x0FFFF) | ((v & 1) << 16);
        } else if r == 6 {
            ch_pitch[1] = (ch_pitch[1] & 0xF00) | v;
        } else if r == 7 {
            ch_pitch[1] = (ch_pitch[1] & 0x0FF) | ((v & 0x0F) << 8);
        } else if r == 8 {
            ch_start[1] = (ch_start[1] & 0x1FF00) | v;
        } else if r == 9 {
            ch_start[1] = (ch_start[1] & 0x100FF) | (v << 8);
        } else if r == 10 {
            ch_start[1] = (ch_start[1] & 0x0FFFF) | ((v & 1) << 16);
        } else if r == 0x14 {
            ch_bank[0] = v;
        } else if r == 0x15 {
            ch_bank[1] = v;
        } else if (0x10..=0x13).contains(&r) {
            vol[(r - 0x10) as usize] = v;
            let ch = ((r - 0x10) >> 1) as usize;
            if let Some(hi) = active[ch] {
                let hit = &mut hits[hi];
                let left = vol[ch * 2];
                let right = vol[ch * 2 + 1];
                let last = match hit.vol_pts.last() {
                    Some(&(_, l, r)) => (l, r),
                    None => (hit.vol_l, hit.vol_r),
                };
                if (left, right) != last {
                    hit.vol_pts.push((w.sample, left, right));
                }
            }
        } else if r == 0x0D {
            loop_en = v;
        } else if r == 0x05 || r == 0x0B || r == K007232_TRIGGER_REG as i32 {
            // Registers 0x05 and 0x0B are the real per-voice key-on triggers.
            // 0x1F is how the parser records the VGM read command: libvgm
            // passes its value as the read offset, and only reading 0x05/0x0B
            // keys a voice.  Any other read returns register data and is not a
            // trigger.
            let ch: usize = if r == 0x05 {
                0
            } else if r == 0x0B {
                1
            } else {
                match v {
                    0x05 => 0,
                    0x0B => 1,
                    _ => continue,
                }
            };
            let addr = ((ch_start[ch] & 0x1FFFF) | ((ch_bank[ch] & 0xFF) << 17)) as i64;
            let pitch = ch_pitch[ch] & 0xFFF;
            let vol_l = vol[ch * 2];
            let vol_r = vol[ch * 2 + 1];
            if let Some(pi) = active[ch] {
                if w.sample - hits[pi].sample_time <= KEYON_MERGE_SAMPLES {
                    // A key-on this close is one hit.  The later write owns the
                    // playback state; the first read is the audible onset.
                    let prev = &mut hits[pi];
                    prev.start = addr;
                    prev.pitch = pitch;
                    prev.bank = ch_bank[ch];
                    prev.vol_l = vol_l;
                    prev.vol_r = vol_r;
                    prev.loop_ = loop_en != 0;
                    continue;
                }
            }
            hits.push(PcmHit {
                sample_time: w.sample,
                ch: ch as i32,
                start: addr,
                pitch,
                bank: ch_bank[ch],
                vol_l,
                vol_r,
                chip_id,
                loop_: loop_en != 0,
                vol_pts: Vec::new(),
            });
            active[ch] = Some(hits.len() - 1);
        }
    }
    hits
}

pub fn build_pcm_bank(vgm: &VgmFile) -> (Vec<u8>, Vec<PcmSample>) {
    let ids: BTreeSet<u8> = vgm
        .writes
        .iter()
        .filter(|w| w.chip == "k007232")
        .map(|w| w.chip_id)
        .collect();
    let mut roms: IndexMap<u8, Vec<u8>> = IndexMap::new();
    if ids.is_empty() {
        roms.insert(0, assemble_k007232_rom(vgm, 0));
    } else {
        for cid in ids {
            roms.insert(cid, assemble_k007232_rom(vgm, cid));
        }
    }
    let rom = roms.get(&0).cloned().unwrap_or_default();
    let hits = collect_pcm_hits(vgm);
    let mut by_start: IndexMap<(u8, i64), PcmSample> = IndexMap::new();
    for h in hits {
        let key = (h.chip_id, h.start);
        let smp = by_start.entry(key).or_insert_with(|| {
            let chip_rom = roms.get(&h.chip_id).map(|r| r.as_slice()).unwrap_or(&[]);
            let body = if chip_rom.is_empty() {
                Vec::new()
            } else {
                decode_7bit_pcm(chip_rom, h.start)
            };
            PcmSample {
                start: h.start,
                pcm7: body,
                chip_id: h.chip_id,
                ..Default::default()
            }
        });
        smp.hits.push(h);
    }
    let mut samples: Vec<PcmSample> = by_start.into_values().collect();
    samples.sort_by(|a, b| (a.chip_id, a.start).cmp(&(b.chip_id, b.start)));
    (rom, samples)
}

/// Write one WAV per triggered start address plus a JSON index.
pub fn dump_pcm(
    vgm: &VgmFile,
    out_dir: &Path,
    min_length: i64,
) -> Result<IndexMap<String, Value>> {
    fs::create_dir_all(out_dir)?;
    let (rom, samples) = build_pcm_bank(vgm);
    let clock = if vgm.k007232_clock != 0 { vgm.k007232_clock } else { 3579545 };
    let source = vgm.path.display().to_string();
    let mut index: IndexMap<String, Value> = IndexMap::new();
    index.insert("source".to_string(), Value::String(source.clone()));
    index.insert("clock".to_string(), json!(clock));
    index.insert("rom_size".to_string(), json!(rom.len()));
    let mut entries: Vec<Value> = Vec::new();
    let mut blocks: Vec<String> = Vec::new();
    for (i, smp) in samples.iter().enumerate() {
        if (smp.length() as i64) < min_length {
            continue;
        }
        let rate = smp.c4_rate(clock);
        let name = format!("smp_{:02}_{:05x}.wav", i, smp.start);
        write_wav(&out_dir.join(&name), &smp.pcm7, rate)?;
        let mut pitch_hist: IndexMap<i32, i64> = IndexMap::new();
        for h in &smp.hits {
            *pitch_hist.entry(h.pitch).or_insert(0) += 1;
        }
        let mut most: Vec<(i32, i64)> = pitch_hist.into_iter().collect();
        most.sort_by(|a, b| b.1.cmp(&a.1));
        let channels: Vec<i32> = smp
            .hits
            .iter()
            .map(|h| h.ch)
            .collect::<BTreeSet<i32>>()
            .into_iter()
            .collect();
        let mode_pitch = smp.mode_pitch();
        let mut pitches_json = serde_json::Map::new();
        for &(k, n) in &most {
            pitches_json.insert(k.to_string(), json!(n));
        }
        entries.push(json!({
            "id": i,
            "file": name,
            "start": smp.start,
            "length": smp.length(),
            "c4_rate": rate,
            "mode_pitch": mode_pitch,
            "triggers": smp.hits.len(),
            "channels": channels,
            "pitches": pitches_json,
        }));
        blocks.push(sample_block(
            i,
            &name,
            smp.start,
            smp.length(),
            rate,
            mode_pitch,
            smp.hits.len(),
            &channels,
            &most,
        ));
    }
    index.insert("samples".to_string(), Value::Array(entries));
    let mut text = String::new();
    text.push_str("{\n");
    let _ = writeln!(text, "  \"source\": {},", py_json_str(&source));
    let _ = writeln!(text, "  \"clock\": {},", clock);
    let _ = writeln!(text, "  \"rom_size\": {},", rom.len());
    if blocks.is_empty() {
        text.push_str("  \"samples\": []\n}\n");
    } else {
        text.push_str("  \"samples\": [\n");
        for (i, block) in blocks.iter().enumerate() {
            text.push_str(block);
            if i + 1 < blocks.len() {
                text.push(',');
            }
            text.push('\n');
        }
        text.push_str("  ]\n}\n");
    }
    fs::write(out_dir.join("pcm_index.json"), text)?;
    Ok(index)
}

#[allow(clippy::too_many_arguments)]
fn sample_block(
    id: usize,
    file: &str,
    start: i64,
    length: usize,
    rate: i64,
    mode_pitch: i32,
    triggers: usize,
    channels: &[i32],
    pitches: &[(i32, i64)],
) -> String {
    let mut s = String::new();
    s.push_str("    {\n");
    let _ = writeln!(s, "      \"id\": {},", id);
    let _ = writeln!(s, "      \"file\": {},", py_json_str(file));
    let _ = writeln!(s, "      \"start\": {},", start);
    let _ = writeln!(s, "      \"length\": {},", length);
    let _ = writeln!(s, "      \"c4_rate\": {},", rate);
    let _ = writeln!(s, "      \"mode_pitch\": {},", mode_pitch);
    let _ = writeln!(s, "      \"triggers\": {},", triggers);
    if channels.is_empty() {
        s.push_str("      \"channels\": [],\n");
    } else {
        s.push_str("      \"channels\": [\n");
        for (i, c) in channels.iter().enumerate() {
            let _ = write!(s, "        {}", c);
            if i + 1 < channels.len() {
                s.push(',');
            }
            s.push('\n');
        }
        s.push_str("      ],\n");
    }
    if pitches.is_empty() {
        s.push_str("      \"pitches\": {}\n");
    } else {
        s.push_str("      \"pitches\": {\n");
        for (i, (k, n)) in pitches.iter().enumerate() {
            let _ = write!(s, "        {}: {}", py_json_str(&k.to_string()), n);
            if i + 1 < pitches.len() {
                s.push(',');
            }
            s.push('\n');
        }
        s.push_str("      }\n");
    }
    s.push_str("    }");
    s
}

/// Python `json.dumps(s, ensure_ascii=True)` string escaping.
fn py_json_str(s: &str) -> String {
    let mut out = String::from("\"");
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c if (c as u32) < 0x7F => out.push(c),
            c => {
                let cp = c as u32;
                if cp <= 0xFFFF {
                    let _ = write!(out, "\\u{:04x}", cp);
                } else {
                    let v = cp - 0x10000;
                    let _ = write!(
                        out,
                        "\\u{:04x}\\u{:04x}",
                        0xD800 + (v >> 10),
                        0xDC00 + (v & 0x3FF)
                    );
                }
            }
        }
    }
    out.push('"');
    out
}

/// Return (furnace_note, e5xx) relative to a sample whose C-4 is c4_pitch.
///
/// Furnace C-4 = 108. E5 80 is center; 00 is -1 semitone, FF is almost +1.
pub fn pitch_to_note(pitch: i32, c4_pitch: i32, clock: u32) -> (i32, i32) {
    let r = k007232_rate(clock, pitch);
    let r0 = k007232_rate(clock, c4_pitch);
    rate_to_note(r, r0)
}

/// (furnace_note, e5xx) for a playback rate relative to a sample's C-4.
pub fn rate_to_note(rate: f64, ref_rate: f64) -> (i32, i32) {
    if rate <= 0.0 || ref_rate <= 0.0 {
        return (108, 0x80);
    }
    let semis = 12.0 * (rate / ref_rate).log2();
    let note = py_int(py_round(semis)) as i32;
    let frac = semis - note as f64;
    let furnace = (108 + note).max(0).min(179);
    let e5 = (py_int(py_round(128.0 + frac * 128.0)) as i32).max(0).min(255);
    (furnace, e5)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cw(sample: i64, reg: u16, val: u16) -> ChipWrite {
        ChipWrite::new(sample, "k007232".into(), 0, reg as u32, val as u32)
    }

    fn hits(writes: &[ChipWrite]) -> Vec<PcmHit> {
        let refs: Vec<&ChipWrite> = writes.iter().collect();
        collect_pcm_chip(&refs, 0)
    }

    fn setup(t0: i64, triggers: &[ChipWrite]) -> Vec<ChipWrite> {
        let mut v = vec![
            cw(t0, 0x00, 0xD3),
            cw(t0, 0x01, 0x0F),
            cw(t0, 0x02, 0x00),
            cw(t0, 0x03, 0x01),
            cw(t0, 0x04, 0x00),
            cw(t0, 0x10, 0x55),
            cw(t0, 0x11, 0x55),
        ];
        v.extend_from_slice(triggers);
        v
    }

    #[test]
    fn test_k007232_rate_and_pitch_note() {
        let hz = k007232_rate(3579545, 4051);
        assert!(hz > 15000.0 && hz < 25000.0);
        // Pitch 0xFFF is the shortest period: clock/4.
        assert_eq!(k007232_rate(4000, 0xFFF), 1000.0);
        // Same pitch both sides: C-4, centered E5.
        assert_eq!(pitch_to_note(4000, 4000, 3579545), (108, 0x80));
    }

    #[test]
    fn test_pcm7_to_s8_and_scale_pcm7() {
        assert_eq!(pcm7_to_s8(&[0x40])[0], 0);
        assert_eq!(pcm7_to_s8(&[0x7F])[0], 126);
        assert_eq!(pcm7_to_s8(&[0x00])[0], (-128i32) as i8 as u8);
        assert_eq!(scale_pcm7(&[0x40, 0x7F], 1.0), vec![0x40, 0x7F]);
        let scaled = scale_pcm7(&[0x40, 0x7F], 0.72);
        assert_eq!(scaled[0], 0x40);
        assert!(scaled[1] < 0x7F);
        assert!(scaled[1] > 0x40);
    }

    #[test]
    fn test_two_trigger_reads_are_one_keyon() {
        for gap in 0..=2 {
            let writes = setup(
                877,
                &[cw(877, 0x1F, 0x05), cw(877 + gap, 0x1F, 0x05)],
            );
            let h = hits(&writes);
            assert_eq!(h.len(), 1, "gap {gap}");
            assert_eq!(h[0].pitch, 0xFD3);
            assert_eq!(h[0].start, 0x100);
        }
        // A later key-on beyond the window is kept.
        let writes = setup(877, &[cw(877, 0x1F, 0x05), cw(1000, 0x1F, 0x05)]);
        assert_eq!(hits(&writes).len(), 2);
    }

    #[test]
    fn test_reads_of_other_offsets_do_not_trigger() {
        let writes = setup(877, &[cw(877, 0x1F, 0x00), cw(878, 0x1F, 0x0A)]);
        assert!(hits(&writes).is_empty());
        // Register 0x05 keys voice 0 directly.
        let writes = setup(0, &[cw(10, 0x05, 0x00)]);
        let h = hits(&writes);
        assert_eq!(h.len(), 1);
        assert_eq!(h[0].ch, 0);
    }

    #[test]
    fn test_volume_changes_while_active_and_split_regions() {
        let mut writes = setup(10, &[cw(10, 0x1F, 0x05)]);
        writes.push(cw(20, 0x10, 0x22));
        writes.push(cw(21, 0x11, 0x33));
        let h = hits(&writes);
        assert_eq!(h.len(), 1);
        assert_eq!(h[0].vol_pts, vec![(20, 0x22, 0x55), (21, 0x22, 0x33)]);
        assert_eq!(
            split_rom_on_end_markers(&[0x80, 0x10, 0x20, 0x80, 0x30, 0x40, 0xFF]),
            vec![
                (1, vec![0x10, 0x20]),
                (4, vec![0x30, 0x40]),
            ]
        );
    }

    #[test]
    fn test_rate_to_note_clamps_and_rounds() {
        assert_eq!(rate_to_note(1000.0, 1000.0), (108, 0x80));
        assert_eq!(rate_to_note(2000.0, 1000.0), (120, 0x80));
        assert_eq!(rate_to_note(0.0, 1000.0), (108, 0x80));
        assert_eq!(rate_to_note(1000.0, 0.0), (108, 0x80));
        assert_eq!(rate_to_note(1e9, 1.0).0, 179);
    }
}
