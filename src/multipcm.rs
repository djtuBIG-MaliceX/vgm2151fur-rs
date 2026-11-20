//! Yamaha YMW258-F (MultiPCM) ROM samples and voices.
//!
//! The VGM stream does not write channel registers directly. Command 0xB5 is
//! the chip's indirect port:
//!
//! ```text
//! 0xB5 0x01 dd   select a channel (value 0-31; every 8th selector is invalid)
//! 0xB5 0x02 dd   choose the channel register (0-10)
//! 0xB5 0x00 dd   write dd to that register
//! ```
//!
//! Registers 0x10/0x11/0x12 select a Sega ROM bank, and command 0xC3 asks the
//! player to write those same registers. Bit 7 of a 0xB5/0xC3 register byte
//! selects the second chip instance.
//!
//! The sample header sits at `(index * 12) & rom_mask`:
//!
//! ```text
//! 0-2   start address; bits 20-23 are the format (bit 2 = 12-bit)
//! 3-4   loop start, relative to the body
//! 5-6   0x10000 - body length
//! 7     LFO rate and vibrato depth
//! 8     attack rate and decay 1 rate
//! 9     decay level and decay 2 rate
//! 10    rate correction and release rate
//! 11    amplitude LFO depth
//! ```
//!
//! Only the bodies a song actually triggers are kept. The module uses
//! Furnace's native MultiPCM system, whose platform writes one instrument
//! header per module instrument; the converter therefore remaps the source's
//! 9-bit sample index to a module instrument index and keeps the source
//! body/loop/ADSR behind it.

use std::collections::{BTreeMap, BTreeSet, HashSet};

use indexmap::IndexMap;

use crate::parity::{py_int, py_round};
use crate::vgm::{assemble_rom, VgmFile};

/// libvgm's VALUE_TO_CHANNEL: a channel selector with every 8th entry invalid.
pub const VALUE_TO_CHANNEL: [i32; 32] = [
    0, 1, 2, 3, 4, 5, 6, -1, 7, 8, 9, 10, 11, 12, 13, -1, 14, 15, 16, 17, 18, 19, 20, -1, 21, 22,
    23, 24, 25, 26, 27, -1,
];
pub const MULTIPCM_VOICES: usize = 28;
/// VGM clock -> chip output rate.
pub const MULTIPCM_CLOCK_DIVIDER: i64 = 180;
/// The clock VGMPlay substitutes when the header logs none.
pub const MULTIPCM_DEFAULT_CLOCK: u32 = 8_053_975;

/// libvgm pow2_mask: the ROM address mask used by the chip.
fn pow2_mask(size: i64) -> i64 {
    if size <= 1 {
        return 0;
    }
    let mut mask: i64 = 1;
    while mask * 2 <= size {
        mask *= 2;
    }
    mask - 1
}

/// MultiPCM sample-step rate for a raw VGM clock.
///
/// VGM stores the clock with the old /180 divider. VGMPlay multiplies it by
/// 224/180 for the core, and the core divides by 224, so the effective rate is
/// simply `clock / 180`.
pub fn chip_rate(clock: u32) -> f64 {
    let clock = if clock != 0 {
        clock
    } else {
        MULTIPCM_DEFAULT_CLOCK
    };
    f64::max(1.0, clock as f64 / MULTIPCM_CLOCK_DIVIDER as f64)
}

/// Playback rate in samples/second for an octave/f-num pair.
///
/// The chip's octave is a signed 4-bit value. `octave = 8` is a negative
/// shift, so 0xF is -1 rather than 15.
pub fn played_rate(clock: u32, octave: i32, fnum: i32) -> f64 {
    let mut octave = octave;
    if octave & 8 != 0 {
        octave -= 16;
    }
    if octave <= -8 {
        return 0.0;
    }
    chip_rate(clock) * (1024 + fnum) as f64 / 1024.0 * 2.0f64.powi(octave - 1)
}

#[derive(Clone, Debug, PartialEq)]
pub struct MultipcmHit {
    pub sample_time: i64,
    pub chip_id: i32,
    pub voice: i32,
    pub sample: i32,
    pub octave: i32,
    pub fnum: i32,
    pub tl: i32,
    pub pan: i32,
    pub direct: bool,
    pub clock: u32,
    /// Mid-note (time, octave, fnum) writes.
    pub freq_pts: Vec<(i64, i32, i32)>,
    /// Mid-note (time, TL, pan) writes.
    pub vol_pts: Vec<(i64, i32, i32)>,
    /// LFO/VIB/AM register writes while held: (time, code, value), where code
    /// is 0x20 = LFO rate, 0x21 = PM depth and 0x22 = AM depth.
    pub lfo_pts: Vec<(i64, i32, i32)>,
    pub off_sample: i64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct MultipcmSample {
    pub chip_id: i32,
    pub index: i32,
    pub start: i32,
    pub data: Vec<u8>,
    pub frames: i64,
    /// 8 or 12.
    pub depth: i32,
    pub loop_start: Option<i64>,
    pub loop_end: Option<i64>,
    pub ar: i32,
    pub d1r: i32,
    pub dl: i32,
    pub d2r: i32,
    pub rr: i32,
    pub rc: i32,
    pub lfo: i32,
    pub vib: i32,
    pub am: i32,
    pub direct: bool,
    pub clock: u32,
    pub hits: Vec<MultipcmHit>,
}

impl MultipcmSample {
    pub fn length(&self) -> i64 {
        self.frames
    }

    /// Most common nonzero (octave, fnum) among the hits.
    pub fn mode_pitch(&self) -> (i32, i32) {
        if self.hits.is_empty() {
            return (1, 0);
        }
        let mut freqs: IndexMap<(i32, i32), i64> = IndexMap::new();
        for h in &self.hits {
            if h.fnum != 0 || h.octave != 8 {
                *freqs.entry((h.octave, h.fnum)).or_insert(0) += 1;
            }
        }
        if freqs.is_empty() {
            return (1, 0);
        }
        let mut best: Option<((i32, i32), i64)> = None;
        for (&key, &n) in freqs.iter() {
            match best {
                Some((_, bn)) if n <= bn => {}
                _ => best = Some((key, n)),
            }
        }
        best.map(|(key, _)| key).unwrap_or((1, 0))
    }

    pub fn c4_rate(&self) -> i64 {
        let (octave, fnum) = self.mode_pitch();
        let rate = played_rate(self.clock, octave, fnum);
        let rate = py_int(py_round(rate));
        rate.max(1).min(192000)
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct Header {
    start: i32,
    depth: i32,
    r#loop: i32,
    end: i64,
    ar: i32,
    d1r: i32,
    dl: i32,
    d2r: i32,
    rr: i32,
    rc: i32,
    lfo: i32,
    vib: i32,
    am: i32,
}

struct Channel {
    regs: [i32; 11],
    active: bool,
    /// (sample key, index into that sample's `hits`), the Rust equivalent of
    /// holding a reference to the open `MultipcmHit`.
    hit: Option<((i32, i32), usize)>,
    sample_index: i32,
}

impl Default for Channel {
    fn default() -> Self {
        Channel {
            regs: [0; 11],
            active: false,
            hit: None,
            sample_index: 0,
        }
    }
}

fn read_header(rom: &[u8], mask: i64, index: i32) -> Option<Header> {
    let off = ((index as i64) * 12) & mask;
    let off = off as usize;
    let h = rom.get(off..off + 12)?;
    let mut start = ((h[0] as i32) << 16) | ((h[1] as i32) << 8) | h[2] as i32;
    let fmt = (start >> 20) & 0xFE;
    start &= 0x3FFFFF;
    let r#loop = ((h[3] as i32) << 8) | h[4] as i32;
    let end = 0x10000 - (((h[5] as i32) << 8) | h[6] as i32);
    Some(Header {
        start,
        depth: if fmt & 4 != 0 { 12 } else { 8 },
        r#loop,
        end: end as i64,
        ar: ((h[8] >> 4) & 0xF) as i32,
        d1r: (h[8] & 0xF) as i32,
        dl: ((h[9] >> 4) & 0xF) as i32,
        d2r: (h[9] & 0xF) as i32,
        rc: ((h[10] >> 4) & 0xF) as i32,
        rr: (h[10] & 0xF) as i32,
        lfo: ((h[7] >> 3) & 7) as i32,
        vib: (h[7] & 7) as i32,
        am: (h[11] & 0xF) as i32,
    })
}

/// Apply libvgm's Sega MultiPCM bank mapping to a sample start address.
fn resolve_base(start: i64, banking: bool, bank0: i64, bank1: i64) -> i64 {
    let mut base = start & 0x1FFFFF;
    if banking && (base & 0x100000) != 0 {
        if (base & 0x080000) != 0 {
            base = (base & 0x07FFFF) | bank1;
        } else {
            base = (base & 0x07FFFF) | bank0;
        }
    }
    base
}

struct Collector {
    roms: BTreeMap<i32, Vec<u8>>,
    masks: BTreeMap<i32, i64>,
    headers: BTreeMap<(i32, i32), Option<Header>>,
    channels: BTreeMap<i32, Vec<Channel>>,
    /// banking, bank0, bank1
    banks: BTreeMap<i32, [i64; 3]>,
    cur: BTreeMap<i32, i32>,
    addr: BTreeMap<i32, u32>,
    samples: BTreeMap<(i32, i32), MultipcmSample>,
    warnings: Vec<String>,
    clock: u32,
}

impl Collector {
    fn apply_bank(&mut self, cid: i32, mask: u32, number: u32) {
        let st = self.banks.get_mut(&cid).unwrap();
        if mask == 3 && (number & 0x08) == 0 {
            // 1 MB banking (Sega Model 1): VGMPlay writes register 0x10 with
            // number / 0x10, and the core shifts that by 20.
            st[0] = 1;
            let bank = ((number / 0x10) as i64) << 20;
            st[1] = bank;
            st[2] = bank | 0x080000;
        } else {
            // 512 KB banking (Sega Multi 32): registers 0x11/0x12 take
            // number / 8, shifted by 19 by the core.
            st[0] = 1;
            let bank = ((number / 0x08) as i64) << 19;
            if mask & 0x02 != 0 {
                st[1] = bank;
            }
            if mask & 0x01 != 0 {
                st[2] = bank;
            }
        }
    }

    fn header_for(&mut self, cid: i32, index: i32) -> Option<Header> {
        let key = (cid, index);
        if let Some(h) = self.headers.get(&key) {
            return *h;
        }
        let rom = self.roms.get(&cid)?;
        let mask = *self.masks.get(&cid).unwrap_or(&0);
        let header = read_header(rom, mask, index);
        self.headers.insert(key, header);
        header
    }

    /// Ensure the sample for `(cid, index)` exists. Returns false when it
    /// cannot be built (missing ROM, header past the end, empty body).
    fn ensure_sample(&mut self, cid: i32, index: i32) -> bool {
        let key = (cid, index);
        if self.samples.contains_key(&key) {
            return true;
        }
        if self.roms.get(&cid).map(|r| r.is_empty()).unwrap_or(true) {
            return false;
        }
        let Some(header) = self.header_for(cid, index) else {
            self.warnings.push(format!(
                "MultiPCM chip {} sample {}: header lies past the ROM",
                cid + 1,
                index
            ));
            return false;
        };
        let banks = self.banks[&cid];
        let base = resolve_base(
            header.start as i64,
            banks[0] != 0,
            banks[1],
            banks[2],
        );
        let byte_len = if header.depth == 12 {
            (header.end * 3 + 1) / 2
        } else {
            header.end
        };
        let rom = &self.roms[&cid];
        let rom_len = rom.len() as i64;
        let body_start = base.min(rom_len.max(0));
        let body_stop = (base + byte_len).min(rom_len.max(0)).max(body_start);
        let mut body = if body_start < body_stop {
            rom[body_start as usize..body_stop as usize].to_vec()
        } else {
            Vec::new()
        };
        if body.is_empty() {
            self.warnings.push(format!(
                "MultiPCM chip {} sample {}: body at {:#x} is empty",
                cid + 1,
                index,
                base
            ));
            return false;
        }
        if (body.len() as i64) < byte_len {
            self.warnings.push(format!(
                "MultiPCM chip {} sample {}: body truncated ({} of {} bytes)",
                cid + 1,
                index,
                body.len(),
                byte_len
            ));
            body.resize(byte_len as usize, 0);
        }
        let mut frames = header.end;
        // Furnace's 8-bit MultiPCM buffer tops out at 65535 frames.
        if header.depth == 8 && frames > 65535 {
            self.warnings.push(format!(
                "MultiPCM chip {} sample {}: {} frames, trimmed to Furnace's 65535-frame limit",
                cid + 1,
                index,
                frames
            ));
            frames = 65535;
            body.truncate(frames as usize);
        }
        let mut loop_start: Option<i64> = None;
        let mut loop_end: Option<i64> = None;
        if 0 <= header.r#loop as i64 && (header.r#loop as i64) < frames - 1 {
            loop_start = Some(header.r#loop as i64);
            loop_end = Some(frames);
        }
        let sample = MultipcmSample {
            chip_id: cid,
            index,
            start: base as i32,
            data: body,
            frames,
            depth: header.depth,
            loop_start,
            loop_end,
            ar: header.ar,
            d1r: header.d1r,
            dl: header.dl,
            d2r: header.d2r,
            rr: header.rr,
            rc: header.rc,
            lfo: header.lfo,
            vib: header.vib,
            am: header.am,
            direct: true,
            clock: self.clock,
            hits: Vec::new(),
        };
        self.samples.insert(key, sample);
        true
    }

    fn open_hit(&mut self, cid: i32, voice: usize, t: i64) {
        let index;
        let prev_active;
        let prev_hit;
        let octave;
        let fnum;
        let tl;
        let pan;
        let direct;
        {
            let ch = &self.channels[&cid][voice];
            index = ch.regs[1] | ((ch.regs[2] & 1) << 8);
            prev_active = ch.active;
            prev_hit = ch.hit;
            octave = (ch.regs[3] >> 4) & 0xF;
            fnum = ((ch.regs[3] & 0xF) << 6) | (ch.regs[2] >> 2);
            tl = (ch.regs[5] >> 1) & 0x7F;
            pan = (ch.regs[0] >> 4) & 0xF;
            direct = ch.regs[5] & 1 != 0;
        }
        if !self.ensure_sample(cid, index) {
            return;
        }
        if prev_active {
            if let Some((key, hit_index)) = prev_hit {
                let hit = &mut self.samples.get_mut(&key).unwrap().hits[hit_index];
                if hit.off_sample == 0 {
                    hit.off_sample = t;
                }
            }
        }
        let key = (cid, index);
        let hit = MultipcmHit {
            sample_time: t,
            chip_id: cid,
            voice: voice as i32,
            sample: index,
            octave,
            fnum,
            tl,
            pan,
            direct,
            clock: self.clock,
            freq_pts: Vec::new(),
            vol_pts: Vec::new(),
            lfo_pts: Vec::new(),
            off_sample: 0,
        };
        let hits = &mut self.samples.get_mut(&key).unwrap().hits;
        hits.push(hit);
        let hit_index = hits.len() - 1;
        let ch = &mut self.channels.get_mut(&cid).unwrap()[voice];
        ch.hit = Some((key, hit_index));
        ch.active = true;
        ch.sample_index = index;
    }
}

/// Decode MultiPCM register writes into sample bodies and hits.
pub fn collect_multipcm(vgm: &VgmFile) -> (Vec<MultipcmSample>, Vec<String>) {
    let mut warnings: Vec<String> = Vec::new();
    let ids: BTreeSet<i32> = vgm
        .writes
        .iter()
        .filter(|w| w.chip == "multipcm" || w.chip == "multipcm_bank")
        .map(|w| w.chip_id as i32)
        .collect();
    if ids.is_empty() {
        return (Vec::new(), warnings);
    }

    let mut roms: BTreeMap<i32, Vec<u8>> = BTreeMap::new();
    let mut masks: BTreeMap<i32, i64> = BTreeMap::new();
    for &cid in &ids {
        let rom = assemble_rom(vgm, "multipcm", cid as u8, 0, false, true);
        if rom.is_empty() {
            warnings.push(format!(
                "MultiPCM chip {} has no type-0x89 ROM block; its voices will be silent",
                cid + 1
            ));
        }
        masks.insert(cid, if rom.is_empty() { 0 } else { pow2_mask(rom.len() as i64) });
        roms.insert(cid, rom);
    }

    let mut channels: BTreeMap<i32, Vec<Channel>> = BTreeMap::new();
    let mut banks: BTreeMap<i32, [i64; 3]> = BTreeMap::new();
    let mut cur: BTreeMap<i32, i32> = BTreeMap::new();
    let mut addr: BTreeMap<i32, u32> = BTreeMap::new();
    for &cid in &ids {
        channels.insert(cid, (0..MULTIPCM_VOICES).map(|_| Channel::default()).collect());
        banks.insert(cid, [0, 0, 0]);
        cur.insert(cid, -1);
        addr.insert(cid, 0);
    }
    let clock = if vgm.multipcm_clock != 0 {
        vgm.multipcm_clock
    } else {
        MULTIPCM_DEFAULT_CLOCK
    };
    let mut collector = Collector {
        roms,
        masks,
        headers: BTreeMap::new(),
        channels,
        banks,
        cur,
        addr,
        samples: BTreeMap::new(),
        warnings,
        clock,
    };

    for w in &vgm.writes {
        let cid = w.chip_id as i32;
        if !collector.channels.contains_key(&cid) {
            continue;
        }
        if w.chip == "multipcm_bank" {
            collector.apply_bank(cid, w.reg & 3, w.val);
            continue;
        }
        if w.chip != "multipcm" {
            continue;
        }
        let reg = w.reg;
        if reg == 1 {
            collector
                .cur
                .insert(cid, VALUE_TO_CHANNEL[(w.val & 0x1F) as usize]);
            continue;
        }
        if reg == 2 {
            collector.addr.insert(cid, w.val);
            continue;
        }
        if reg == 0x10 || reg == 0x11 || reg == 0x12 {
            let st = collector.banks.get_mut(&cid).unwrap();
            st[0] = 1;
            if reg == 0x10 {
                st[1] = ((w.val as i64) << 20) & 0xFFFFFF;
                st[2] = (((w.val as i64) << 20) & 0xFFFFFF) | 0x080000;
            } else if reg == 0x11 {
                st[1] = ((w.val as i64) << 19) & 0xFFFFFF;
            } else {
                st[2] = ((w.val as i64) << 19) & 0xFFFFFF;
            }
            continue;
        }
        if reg != 0 {
            continue;
        }
        let voice = *collector.cur.get(&cid).unwrap();
        if voice < 0 {
            continue;
        }
        let a = *collector.addr.get(&cid).unwrap();
        if a >= 11 {
            continue;
        }
        let voice = voice as usize;
        let (active, hit) = {
            let ch = &mut collector.channels.get_mut(&cid).unwrap()[voice];
            let old = ch.regs[a as usize];
            ch.regs[a as usize] = w.val as i32;
            if a == 1 {
                // Selecting a sample re-inits the header and retriggers a
                // playing voice. The source almost always writes it before
                // key-on.
                if ch.active && old as u32 != w.val {
                    (true, None)
                } else {
                    (false, None)
                }
            } else {
                (ch.active, ch.hit)
            }
        };
        if a == 1 {
            if active {
                collector.open_hit(cid, voice, w.sample);
            }
        } else if a == 2 || a == 3 {
            if active {
                if let Some((key, hit_index)) = hit {
                    let ch = &collector.channels[&cid][voice];
                    let octave = (ch.regs[3] >> 4) & 0xF;
                    let fnum = ((ch.regs[3] & 0xF) << 6) | (ch.regs[2] >> 2);
                    let hit = &mut collector.samples.get_mut(&key).unwrap().hits[hit_index];
                    hit.freq_pts.push((w.sample, octave, fnum));
                }
            }
        } else if a == 4 {
            if w.val & 0x80 != 0 {
                collector.open_hit(cid, voice, w.sample);
            } else if active {
                if let Some((key, hit_index)) = hit {
                    let target = &mut collector.samples.get_mut(&key).unwrap().hits[hit_index];
                    if target.off_sample == 0 {
                        target.off_sample = w.sample;
                    }
                    collector.channels.get_mut(&cid).unwrap()[voice].active = false;
                }
            }
        } else if a == 5 {
            if active {
                if let Some((key, hit_index)) = hit {
                    let tl = ((w.val >> 1) & 0x7F) as i32;
                    let pan = (collector.channels[&cid][voice].regs[0] >> 4) & 0xF;
                    let target = &mut collector.samples.get_mut(&key).unwrap().hits[hit_index];
                    target.vol_pts.push((w.sample, tl, pan));
                    target.direct = w.val & 1 != 0;
                }
            }
        } else if a == 0 {
            if active {
                if let Some((key, hit_index)) = hit {
                    let tl = (collector.channels[&cid][voice].regs[5] >> 1) & 0x7F;
                    let pan = ((w.val >> 4) & 0xF) as i32;
                    let target = &mut collector.samples.get_mut(&key).unwrap().hits[hit_index];
                    target.vol_pts.push((w.sample, tl, pan));
                }
            }
        } else if a == 6 {
            if active {
                if let Some((key, hit_index)) = hit {
                    let target = &mut collector.samples.get_mut(&key).unwrap().hits[hit_index];
                    target
                        .lfo_pts
                        .push((w.sample, 0x20, ((w.val >> 3) & 7) as i32));
                    target.lfo_pts.push((w.sample, 0x21, (w.val & 7) as i32));
                }
            }
        } else if a == 10 {
            if active {
                if let Some((key, hit_index)) = hit {
                    let target = &mut collector.samples.get_mut(&key).unwrap().hits[hit_index];
                    target.lfo_pts.push((w.sample, 0x22, (w.val & 7) as i32));
                }
            }
        }
    }

    warnings = collector.warnings;
    let mut audible: Vec<MultipcmSample> = collector
        .samples
        .into_values()
        .filter(|s| !s.hits.is_empty() && s.length() > 0)
        .collect();
    audible.sort_by(|a, b| (a.chip_id, a.index).cmp(&(b.chip_id, b.index)));
    for sample in &mut audible {
        let true_n = sample.hits.iter().filter(|h| h.direct).count();
        let false_n = sample.hits.len() - true_n;
        sample.direct = if true_n > false_n {
            true
        } else if false_n > true_n {
            false
        } else {
            // Counter.most_common keeps the first-inserted key on a tie.
            sample.hits[0].direct
        };
    }
    let mut seen: HashSet<String> = HashSet::new();
    warnings.retain(|w| seen.insert(w.clone()));
    (audible, warnings)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vgm::{ChipWrite, RomSlice, VgmFile};

    fn write(sample: i64, reg: u32, val: u32) -> ChipWrite {
        ChipWrite::new(sample, "multipcm".into(), 0, reg, val)
    }

    #[test]
    fn pow2_mask_matches_libvgm() {
        assert_eq!(pow2_mask(0), 0);
        assert_eq!(pow2_mask(1), 0);
        assert_eq!(pow2_mask(2), 1);
        assert_eq!(pow2_mask(3), 1);
        assert_eq!(pow2_mask(4), 3);
        assert_eq!(pow2_mask(100), 63);
        assert_eq!(pow2_mask(0x200), 0x1FF);
    }

    #[test]
    fn chip_and_played_rates() {
        assert_eq!(chip_rate(0), MULTIPCM_DEFAULT_CLOCK as f64 / 180.0);
        assert!((chip_rate(0) - 44744.305555555555).abs() < 1e-9);
        // Octave 1: one step per chip clock (2^(1-1)).
        assert!((played_rate(8_053_975, 1, 0) - chip_rate(0)).abs() < 1e-9);
        // 0xF is the signed value -1.
        assert!((played_rate(8_053_975, 0xF, 0) - chip_rate(0) / 4.0).abs() < 1e-9);
        // 0x8 is -8, the chip's mute octave.
        assert_eq!(played_rate(8_053_975, 8, 0), 0.0);
    }

    #[test]
    fn resolve_base_applies_sega_banking() {
        assert_eq!(resolve_base(0x100000, false, 0x200000, 0x280000), 0x100000);
        assert_eq!(resolve_base(0x100100, true, 0x200000, 0x280000), 0x200100);
        assert_eq!(resolve_base(0x180100, true, 0x200000, 0x280000), 0x280100);
        assert_eq!(resolve_base(0x0FFFFF, true, 0x200000, 0x280000), 0x0FFFFF);
    }

    fn basic_rom() -> Vec<u8> {
        let mut rom = vec![0u8; 0x200];
        // start = 0x100, 8-bit, loop = 2, frames = 8.
        let header = [
            0x00, 0x01, 0x00, 0x00, 0x02, 0xFF, 0xF8, 0x00, 0x5A, 0x3C, 0x27, 0x0D,
        ];
        rom[..12].copy_from_slice(&header);
        rom[0x100..0x108].copy_from_slice(&[1, 2, 3, 4, 5, 6, 7, 8]);
        rom
    }

    fn basic_writes() -> Vec<ChipWrite> {
        vec![
            write(0, 1, 0),    // channel 0
            write(0, 2, 1),    // register address 1
            write(0, 0, 0),    // sample index 0
            write(0, 2, 3),    // register 3
            write(0, 0, 0x10), // octave 1, fnum high nibble 0
            write(0, 2, 2),    // register 2
            write(0, 0, 0),    // fnum low bits 0
            write(0, 2, 5),    // register 5: TL/direct
            write(0, 0, 0x40), // TL 32, direct 0
            write(0, 2, 0),    // register 0: pan
            write(0, 0, 0x30), // pan 3
            write(0, 2, 4),    // register 4: key on
            write(0, 0, 0x80),
        ]
    }

    #[test]
    fn collects_sample_and_hit() {
        let vgm = VgmFile {
            multipcm_clock: 8_053_975,
            writes: basic_writes(),
            roms: vec![RomSlice {
                chip: 0,
                rom_size: 0,
                start: 0,
                data: basic_rom(),
                kind: "multipcm",
                time: 0,
            }],
            ..Default::default()
        };
        let (samples, warnings) = collect_multipcm(&vgm);
        assert!(warnings.is_empty());
        assert_eq!(samples.len(), 1);
        let s = &samples[0];
        assert_eq!((s.chip_id, s.index, s.start), (0, 0, 0x100));
        assert_eq!(s.data, vec![1, 2, 3, 4, 5, 6, 7, 8]);
        assert_eq!(s.frames, 8);
        assert_eq!(s.length(), 8);
        assert_eq!(s.depth, 8);
        assert_eq!(s.loop_start, Some(2));
        assert_eq!(s.loop_end, Some(8));
        assert_eq!((s.ar, s.d1r, s.dl, s.d2r, s.rr, s.rc), (5, 0xA, 3, 0xC, 7, 2));
        assert_eq!((s.lfo, s.vib, s.am), (0, 0, 0x0D));
        assert_eq!(s.hits.len(), 1);
        let h = &s.hits[0];
        assert_eq!((h.voice, h.sample, h.octave, h.fnum), (0, 0, 1, 0));
        assert_eq!((h.tl, h.pan, h.direct), (32, 3, false));
        assert_eq!(s.mode_pitch(), (1, 0));
        assert_eq!(s.c4_rate(), 44744);
    }

    #[test]
    fn collects_midnote_points_and_keyoff() {
        let mut writes = basic_writes();
        writes.push(write(100, 2, 3));
        writes.push(write(100, 0, 0x20)); // octave 2
        writes.push(write(200, 2, 5));
        writes.push(write(200, 0, 0x41)); // TL 32 -> 32? (0x41>>1)&0x7F = 32, direct 1
        writes.push(write(300, 2, 4));
        writes.push(write(300, 0, 0x00)); // key off
        let vgm = VgmFile {
            multipcm_clock: 8_053_975,
            writes,
            roms: vec![RomSlice {
                chip: 0,
                rom_size: 0,
                start: 0,
                data: basic_rom(),
                kind: "multipcm",
                time: 0,
            }],
            ..Default::default()
        };
        let (samples, _) = collect_multipcm(&vgm);
        let h = &samples[0].hits[0];
        assert_eq!(h.freq_pts, vec![(100, 2, 0)]);
        assert_eq!(h.vol_pts, vec![(200, 32, 3)]);
        assert_eq!(h.direct, true);
        assert_eq!(h.off_sample, 300);
        assert_eq!(h.sample_time, 0);
    }

    #[test]
    fn warns_when_rom_is_missing() {
        let vgm = VgmFile {
            multipcm_clock: 0,
            writes: basic_writes(),
            ..Default::default()
        };
        let (samples, warnings) = collect_multipcm(&vgm);
        assert!(samples.is_empty());
        assert_eq!(
            warnings,
            vec!["MultiPCM chip 1 has no type-0x89 ROM block; its voices will be silent".to_string()]
        );
    }
}
