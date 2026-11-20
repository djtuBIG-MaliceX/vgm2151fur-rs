//! Yamaha YMZ280B (PCMD8) register stream → sample bodies and notes.
//!
//! The YMZ280B has eight sample voices. Each voice has a start, loop-start,
//! loop-end and stop address, a 9-bit frequency, an 8-bit linear level, a
//! 0-15 pan value and a control byte selecting 4-bit ADPCM, 8-bit PCM or
//! 16-bit PCM.
//!
//! Register model (MAME `ymz280b.cpp`):
//!
//! ```text
//! ch*4 + 0x00  frequency low
//! ch*4 + 0x01  control: bit 7 key-on, bit 4 loop, bit 0 frequency bit 8,
//!                     bits 5-6 mode (1 ADPCM, 2 PCM8, 3 PCM16)
//! ch*4 + 0x02  total level (linear, 255 = full scale)
//! ch*4 + 0x03  pan (0/1 hard left, 8 centre, 15 hard right)
//! ch*4 + 0x20  start high       0x40 start mid       0x60 start low
//! ch*4 + 0x21  loop start high  0x41 loop start mid  0x61 loop start low
//! ch*4 + 0x22  loop end high    0x42 loop end mid    0x62 loop end low
//! ch*4 + 0x23  stop high        0x43 stop mid        0x63 stop low
//! ```
//!
//! The address registers are byte addresses into the external sample ROM. A
//! key-on latches the address/mode state; rewriting start/stop while the voice
//! is held does not affect the current note. A control write with bit 7
//! already set is a pitch/loop update, not a retrigger.
//!
//! Playback rate:
//!
//! ```text
//! source samples/second = clock * (fnum + 1) / 98304
//! ```
//!
//! 4-bit ADPCM uses only the low eight frequency bits (maximum clock/384),
//! while 8-bit and 16-bit PCM use all nine (maximum clock/192).
//!
//! The ROM is big-endian 16-bit PCM and high-nibble-first ADPCM. Furnace's
//! 16-bit SMP2 bodies are little-endian, so each 16-bit pair is swapped while
//! slicing. ADPCM bodies are stored raw as Furnace depth 3 (YMZ/YMU ADPCM).

use std::collections::BTreeSet;

use indexmap::IndexMap;

use crate::parity::{py_floor_div, py_int, py_round};
use crate::vgm::{assemble_rom, VgmFile, YMZ280B_DEFAULT_CLOCK};

pub const YMZ280B_VOICES: usize = 8;
pub const YMZ280B_RATE_DIVISOR: f64 = 98304.0;
/// Furnace sample depths we store.
pub const YMZ_DEPTH_ADPCM: i32 = 3;
pub const YMZ_DEPTH_PCM8: i32 = 8;
pub const YMZ_DEPTH_PCM16: i32 = 16;

/// Clock for one YMZ280B instance.
pub fn ymz280b_clock(vgm: &VgmFile, chip_id: i32) -> u32 {
    if chip_id == 1 && vgm.ymz280b_clock_2 != 0 {
        vgm.ymz280b_clock_2
    } else if vgm.ymz280b_clock != 0 {
        vgm.ymz280b_clock
    } else {
        YMZ280B_DEFAULT_CLOCK
    }
}

/// Source frames per second for a frequency register and sample depth.
pub fn ymz280b_rate(clock: u32, fnum: i32, depth: i32) -> f64 {
    let mask = if depth == YMZ_DEPTH_ADPCM { 0xFF } else { 0x1FF };
    let n = (fnum & mask) + 1;
    let clock = if clock == 0 {
        YMZ280B_DEFAULT_CLOCK
    } else {
        clock
    };
    f64::max(1.0, (clock as f64) * (n as f64) / YMZ280B_RATE_DIVISOR)
}

/// Control mode bits → Furnace sample depth (0 when no mode was set).
pub fn ymz_depth(mode: i32) -> i32 {
    match mode & 0x03 {
        1 => YMZ_DEPTH_ADPCM,
        2 => YMZ_DEPTH_PCM8,
        3 => YMZ_DEPTH_PCM16,
        _ => 0,
    }
}

/// The chip's 0-15 pan law as a 0-15 left/right pair.
///
/// Pan 8 is centre. Pan 1 is hard left (pan 0 is the same as 1); pan 15 is
/// hard right. The ratios ignore the level, which the volume column carries.
pub fn ymz_pan_ratio(pan: i32) -> (i32, i32) {
    let p = pan & 0x0F;
    if p == 8 {
        return (15, 15);
    }
    if p < 8 {
        let right = if p == 0 {
            0
        } else {
            py_floor_div(((p - 1) * 15 + 3) as i64, 7) as i32
        };
        return (15, right);
    }
    let left = py_floor_div(((15 - p) * 15 + 3) as i64, 7) as i32;
    (left, 15)
}

/// Furnace 08xy split for a YMZ pan value, loud side at 15.
pub fn ymz_pan_fx(pan: i32) -> i32 {
    let (left, right) = ymz_pan_ratio(pan);
    ((left & 0x0F) << 4) | (right & 0x0F)
}

/// ROM big-endian 16-bit PCM → little-endian SMP2 body.
fn swap16(data: &[u8]) -> Vec<u8> {
    let mut out = vec![0u8; data.len()];
    let pairs = data.len() & !1;
    let mut i = 0;
    while i < pairs {
        out[i] = data[i + 1];
        out[i + 1] = data[i];
        i += 2;
    }
    out
}

#[derive(Clone, Debug, PartialEq)]
pub struct YmzHit {
    pub sample_time: i64,
    pub chip_id: i32,
    pub voice: usize,
    pub start: i32,
    pub stop: i32,
    pub loop_start: i32,
    pub loop_end: i32,
    pub loop_on: bool,
    pub depth: i32,
    pub fnum: i32,
    pub vol: i32,
    pub pan: i32,
    /// Key-off sample (0 = still playing at the end).
    pub off_sample: i64,
    /// Mid-note writes while the voice stays keyed.
    pub vol_pts: Vec<(i64, i32)>,
    pub pan_pts: Vec<(i64, i32)>,
    pub freq_pts: Vec<(i64, i32)>,
}

impl YmzHit {
    /// Python's `YmzHit(...)` with every defaulted field left at its default.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        sample_time: i64,
        chip_id: i32,
        voice: usize,
        start: i32,
        stop: i32,
        loop_start: i32,
        loop_end: i32,
        loop_on: bool,
        depth: i32,
        fnum: i32,
        vol: i32,
        pan: i32,
    ) -> Self {
        YmzHit {
            sample_time,
            chip_id,
            voice,
            start,
            stop,
            loop_start,
            loop_end,
            loop_on,
            depth,
            fnum,
            vol,
            pan,
            off_sample: 0,
            vol_pts: Vec::new(),
            pan_pts: Vec::new(),
            freq_pts: Vec::new(),
        }
    }

    /// Source frames this hit plays (before the loop wraps).
    pub fn length(&self) -> i64 {
        if self.depth == YMZ_DEPTH_PCM16 {
            return i64::max(0, py_floor_div((self.stop - self.start) as i64, 2));
        }
        if self.depth == YMZ_DEPTH_ADPCM {
            return i64::max(0, ((self.stop - self.start) as i64) * 2);
        }
        i64::max(0, (self.stop - self.start) as i64)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct YmzOff {
    pub sample_time: i64,
    pub chip_id: i32,
    pub voice: usize,
}

/// The frequency a hit actually starts on.
///
/// A few drivers write the frequency just after the key-on byte at the same
/// sample; the collector folds those into the hit already. If the register
/// was still zero, accept the first later nonzero write as the note's own
/// pitch instead of pinning the sample reference to 172 Hz.
pub fn hit_played_fnum(hit: &YmzHit) -> i32 {
    if hit.fnum != 0 {
        return hit.fnum;
    }
    for &(_t, fnum) in &hit.freq_pts {
        if fnum != 0 {
            return fnum;
        }
    }
    0
}

#[derive(Clone, Debug, PartialEq)]
pub struct YmzSample {
    pub chip_id: i32,
    pub start: i32,
    pub stop: i32,
    pub data: Vec<u8>,
    pub depth: i32,
    pub loop_start: Option<i64>,
    pub loop_end: Option<i64>,
    pub hits: Vec<YmzHit>,
    pub clock: u32,
}

impl YmzSample {
    /// Frame count in the sample's own units.
    ///
    /// 8-bit: bytes. 16-bit: 16-bit frames. ADPCM: nibbles.
    pub fn length(&self) -> i64 {
        if self.depth == YMZ_DEPTH_PCM16 {
            return (self.data.len() / 2) as i64;
        }
        if self.depth == YMZ_DEPTH_ADPCM {
            return (self.data.len() * 2) as i64;
        }
        self.data.len() as i64
    }

    /// Most common nonzero key-on frequency (0 when unknown).
    pub fn mode_fnum(&self) -> i32 {
        if self.hits.is_empty() {
            return 0;
        }
        let mut freqs: IndexMap<i32, usize> = IndexMap::new();
        for h in &self.hits {
            let f = hit_played_fnum(h);
            if f != 0 {
                *freqs.entry(f).or_insert(0) += 1;
            }
        }
        let mut best: Option<(i32, usize)> = None;
        // Counter.most_common(1) sorts by count only; a stable sort keeps the
        // first-seen frequency on ties.
        for (&f, &n) in &freqs {
            if best.map_or(true, |(_, bn)| n > bn) {
                best = Some((f, n));
            }
        }
        best.map_or(0, |(f, _)| f)
    }

    pub fn c4_rate(&self) -> i64 {
        i64::max(
            1,
            py_int(py_round(ymz280b_rate(
                if self.clock == 0 {
                    YMZ280B_DEFAULT_CLOCK
                } else {
                    self.clock
                },
                self.mode_fnum(),
                self.depth,
            ))),
        )
    }
}

#[derive(Clone, Debug, PartialEq, Default)]
struct Voice {
    start: i32,
    stop: i32,
    loop_start: i32,
    loop_end: i32,
    loop_: bool,
    mode: i32,
    fnum: i32,
    ctrl: i32,
    vol: i32,
    pan: i32,
}

fn record_freq(hit: Option<&mut YmzHit>, when: i64, fnum: i32, old: i32) {
    let Some(hit) = hit else { return };
    if fnum == old {
        return;
    }
    if when <= hit.sample_time {
        hit.fnum = fnum;
    } else {
        hit.freq_pts.push((when, fnum));
    }
}

fn record_volume(hit: Option<&mut YmzHit>, when: i64, vol: i32, old: i32) {
    let Some(hit) = hit else { return };
    if vol == old {
        return;
    }
    if when <= hit.sample_time {
        hit.vol = vol;
    } else {
        hit.vol_pts.push((when, vol));
    }
}

fn record_pan(hit: Option<&mut YmzHit>, when: i64, pan: i32, old: i32) {
    let Some(hit) = hit else { return };
    if pan == old {
        return;
    }
    if when <= hit.sample_time {
        hit.pan = pan;
    } else {
        hit.pan_pts.push((when, pan));
    }
}

/// Collect YMZ280B key-ons, key-offs and sample bodies.
pub fn collect_ymz280b(vgm: &VgmFile) -> (Vec<YmzSample>, Vec<YmzOff>, Vec<String>) {
    let writes: Vec<&crate::vgm::ChipWrite> =
        vgm.writes.iter().filter(|w| w.chip == "ymz280b").collect();
    let mut warnings: Vec<String> = Vec::new();
    if writes.is_empty() {
        return (Vec::new(), Vec::new(), warnings);
    }

    let mut voices: Vec<Voice> = (0..YMZ280B_VOICES * 2).map(|_| Voice::default()).collect();
    // The reference keeps the same YmzHit object in `hits` and `active`, so a
    // mid-note write is visible in both. Indices preserve that sharing.
    let mut active: Vec<Option<usize>> = vec![None; YMZ280B_VOICES * 2];
    let mut hits: Vec<YmzHit> = Vec::new();
    let mut offs: Vec<YmzOff> = Vec::new();
    let mut keyon_enable = [false, false];
    let mut missed_keyons = 0i64;
    let mut bad_mode = 0i64;
    let mut midnote_mode_loop = 0i64;
    let mut ram_writes = 0i64;
    // 0x84/0x85 latch the high/middle RAM-write address; 0x87 writes one byte
    // and increments it. These logs normally ship a type-0x86 ROM instead.
    let mut ram_addr: i64 = 0;

    for w in writes {
        let reg = w.reg & 0xFF;
        let val = w.val & 0xFF;
        let t = w.sample;
        let cid = (w.chip_id & 1) as i32;

        if reg < 0x80 {
            let vi = (cid as usize) * YMZ280B_VOICES + (((reg >> 2) & 7) as usize);
            let base = vi % YMZ280B_VOICES;
            let kind = reg & 0xE3;
            let hit_idx = active[vi];

            if kind == 0x00 {
                let old = voices[vi].fnum;
                voices[vi].fnum = (voices[vi].fnum & 0x100) | (val as i32);
                record_freq(
                    hit_idx.map(|i| &mut hits[i]),
                    t,
                    voices[vi].fnum,
                    old,
                );
            } else if kind == 0x01 {
                let voice = &mut voices[vi];
                let old_fnum = voice.fnum;
                let old_ctrl = voice.ctrl;
                let old_loop = voice.loop_;
                let old_mode = voice.mode;
                voice.fnum = (voice.fnum & 0xFF) | (((val & 1) as i32) << 8);
                voice.loop_ = (val & 0x10) != 0;
                if val & 0x60 != 0 {
                    voice.mode = ((val & 0x60) >> 5) as i32;
                }
                let key = (val & 0x80) != 0;
                let was = (old_ctrl & 0x80) != 0;

                if key && !was {
                    if !keyon_enable[cid as usize] {
                        missed_keyons += 1;
                    } else {
                        let depth = ymz_depth(voice.mode);
                        if depth == 0 {
                            bad_mode += 1;
                        } else {
                            let new_hit = YmzHit::new(
                                t,
                                cid,
                                base,
                                voice.start,
                                voice.stop,
                                voice.loop_start,
                                voice.loop_end,
                                voice.loop_,
                                depth,
                                voice.fnum,
                                voice.vol,
                                voice.pan & 0x0F,
                            );
                            hits.push(new_hit);
                            active[vi] = Some(hits.len() - 1);
                        }
                    }
                } else if key && was && hit_idx.is_some() {
                    record_freq(
                        hit_idx.map(|i| &mut hits[i]),
                        t,
                        voice.fnum,
                        old_fnum,
                    );
                    if voice.loop_ != old_loop || voice.mode != old_mode {
                        // The chip changes these live; the module's sample
                        // loop/depth is fixed at key-on.
                        midnote_mode_loop += 1;
                    }
                } else if !key && was {
                    if let Some(i) = hit_idx {
                        hits[i].off_sample = t;
                        offs.push(YmzOff {
                            sample_time: t,
                            chip_id: cid,
                            voice: base,
                        });
                        active[vi] = None;
                    }
                }
                voices[vi].ctrl = val as i32;
            } else if kind == 0x02 {
                let old = voices[vi].vol;
                voices[vi].vol = val as i32;
                record_volume(
                    hit_idx.map(|i| &mut hits[i]),
                    t,
                    val as i32,
                    old,
                );
            } else if kind == 0x03 {
                let old = voices[vi].pan;
                voices[vi].pan = (val & 0x0F) as i32;
                record_pan(
                    hit_idx.map(|i| &mut hits[i]),
                    t,
                    voices[vi].pan,
                    old,
                );
            } else if kind == 0x20 {
                voices[vi].start = (voices[vi].start & 0x00FFFF) | ((val as i32) << 16);
            } else if kind == 0x21 {
                voices[vi].loop_start =
                    (voices[vi].loop_start & 0x00FFFF) | ((val as i32) << 16);
            } else if kind == 0x22 {
                voices[vi].loop_end =
                    (voices[vi].loop_end & 0x00FFFF) | ((val as i32) << 16);
            } else if kind == 0x23 {
                voices[vi].stop = (voices[vi].stop & 0x00FFFF) | ((val as i32) << 16);
            } else if kind == 0x40 {
                voices[vi].start = (voices[vi].start & 0xFF00FF) | ((val as i32) << 8);
            } else if kind == 0x41 {
                voices[vi].loop_start =
                    (voices[vi].loop_start & 0xFF00FF) | ((val as i32) << 8);
            } else if kind == 0x42 {
                voices[vi].loop_end =
                    (voices[vi].loop_end & 0xFF00FF) | ((val as i32) << 8);
            } else if kind == 0x43 {
                voices[vi].stop = (voices[vi].stop & 0xFF00FF) | ((val as i32) << 8);
            } else if kind == 0x60 {
                voices[vi].start = (voices[vi].start & 0xFFFF00) | (val as i32);
            } else if kind == 0x61 {
                voices[vi].loop_start = (voices[vi].loop_start & 0xFFFF00) | (val as i32);
            } else if kind == 0x62 {
                voices[vi].loop_end = (voices[vi].loop_end & 0xFFFF00) | (val as i32);
            } else if kind == 0x63 {
                voices[vi].stop = (voices[vi].stop & 0xFFFF00) | (val as i32);
            }
            continue;
        }

        if reg == 0xFF {
            let old_enable = keyon_enable[cid as usize];
            keyon_enable[cid as usize] = (val & 0x80) != 0;
            if old_enable && !keyon_enable[cid as usize] {
                // Turning this chip off stops its voices. A later re-enable
                // only resumes looping voices; the module already gets an off.
                for vi in 0..active.len() {
                    if let Some(i) = active[vi] {
                        if vi / YMZ280B_VOICES == cid as usize {
                            hits[i].off_sample = t;
                            offs.push(YmzOff {
                                sample_time: t,
                                chip_id: hits[i].chip_id,
                                voice: vi % YMZ280B_VOICES,
                            });
                            active[vi] = None;
                        }
                    }
                }
            }
        } else if reg == 0x84 {
            ram_addr = (ram_addr & 0x00FFFF) | ((val as i64) << 16);
        } else if reg == 0x85 {
            ram_addr = (ram_addr & 0xFF00FF) | ((val as i64) << 8);
        } else if reg == 0x86 {
            ram_addr = (ram_addr & 0xFFFF00) | (val as i64);
        } else if reg == 0x87 {
            ram_writes += 1;
            ram_addr = (ram_addr + 1) & 0xFFFFFF;
        }
    }

    if missed_keyons != 0 {
        warnings.push(format!(
            "{missed_keyons} YMZ280B key-on(s) while 0xFF bit 7 was clear; \
             the chip would stay silent"
        ));
    }
    if bad_mode != 0 {
        warnings.push(format!(
            "{bad_mode} YMZ280B key-on(s) had no sample mode set; \
             those notes stay silent"
        ));
    }
    if midnote_mode_loop != 0 {
        warnings.push(format!(
            "{midnote_mode_loop} YMZ280B held-note loop/mode change(s) kept \
             the key-on body"
        ));
    }
    if ram_writes != 0 {
        warnings.push(
            "YMZ280B register RAM writes (0x87) are not replayed; sample \
             bodies come from the type-0x86 ROM block"
                .to_string(),
        );
    }
    if !vgm.writes.iter().any(|w| w.chip == "ymz280b") {
        return (Vec::new(), offs, warnings);
    }

    // Slice the ROM once per chip. Bodies are deduplicated by their full
    // address/loop/depth identity, so repeated drum hits share one instrument.
    let mut by_key: IndexMap<(i32, i32, i32, i32, i32, i32), YmzSample> = IndexMap::new();
    let mut chips: BTreeSet<i32> = BTreeSet::new();
    for h in &hits {
        chips.insert(h.chip_id);
    }
    let mut roms: IndexMap<i32, Vec<u8>> = IndexMap::new();
    for &cid in &chips {
        roms.insert(cid, assemble_rom(vgm, "ymz280b", cid as u8, 0, false, true));
    }
    let missing_rom: Vec<i32> = chips
        .iter()
        .copied()
        .filter(|cid| roms.get(cid).map_or(true, |r| r.is_empty()))
        .collect();
    if !missing_rom.is_empty() {
        let names = missing_rom
            .iter()
            .map(|cid| (cid + 1).to_string())
            .collect::<Vec<String>>()
            .join(", ");
        warnings.push(format!(
            "no YMZ280B ROM block (0x86) for chip {names}; \
             those samples will be silent"
        ));
    }
    let mut clamped = 0i64;
    let mut bad_bodies = 0i64;
    for h in &hits {
        let rom: &[u8] = roms.get(&h.chip_id).map(|r| r.as_slice()).unwrap_or(&[]);
        let clock = ymz280b_clock(vgm, h.chip_id);
        if rom.is_empty() {
            continue;
        }
        let start = (h.start as i64).clamp(0, rom.len() as i64);
        let stop = (h.stop as i64).clamp(0, rom.len() as i64);
        if stop <= start {
            bad_bodies += 1;
            continue;
        }
        if stop != h.stop as i64 || start != h.start as i64 {
            clamped += 1;
        }

        let loop_key_start = if h.loop_on { h.loop_start } else { 0 };
        let loop_key_end = if h.loop_on { h.loop_end } else { 0 };
        let key = (
            h.chip_id,
            h.depth,
            start as i32,
            stop as i32,
            loop_key_start,
            loop_key_end,
        );
        if !by_key.contains_key(&key) {
            let body: Vec<u8> = if h.depth == YMZ_DEPTH_PCM16 {
                let even_stop = start + ((stop - start) & !1);
                swap16(&rom[start as usize..even_stop as usize])
            } else {
                rom[start as usize..stop as usize].to_vec()
            };
            let mut smp = YmzSample {
                chip_id: h.chip_id,
                start: start as i32,
                stop: stop as i32,
                data: body,
                depth: h.depth,
                loop_start: None,
                loop_end: None,
                hits: Vec::new(),
                clock,
            };
            if h.loop_on {
                let ls = h.loop_start as i64;
                let le = h.loop_end as i64;
                if ls < start || le > stop || le <= ls {
                    warnings.push(format!(
                        "YMZ280B loop {ls:06x}-{le:06x} outside body \
                         {start:06x}-{stop:06x}; loop disabled"
                    ));
                } else {
                    let (loop_start, loop_end) = if h.depth == YMZ_DEPTH_PCM16 {
                        (
                            py_floor_div(ls - start, 2),
                            py_floor_div((le - start) + 1, 2),
                        )
                    } else if h.depth == YMZ_DEPTH_ADPCM {
                        ((ls - start) * 2, (le - start) * 2)
                    } else {
                        (ls - start, le - start)
                    };
                    if 0 <= loop_start && loop_start < loop_end && loop_end <= smp.length()
                    {
                        smp.loop_start = Some(loop_start);
                        smp.loop_end = Some(loop_end);
                    } else {
                        warnings.push(format!(
                            "YMZ280B loop {ls:06x}-{le:06x} does not fit body \
                             {start:06x}-{stop:06x}; loop disabled"
                        ));
                    }
                }
            }
            by_key.insert(key, smp);
        }
        by_key.get_mut(&key).unwrap().hits.push(h.clone());
    }

    let mut samples: Vec<YmzSample> = by_key.into_values().collect();
    samples.sort_by(|a, b| {
        (a.chip_id, a.start, a.depth).cmp(&(b.chip_id, b.start, b.depth))
    });
    if clamped != 0 {
        warnings.push(format!(
            "{clamped} YMZ280B key-on(s) addressed past the end of the \
             dumped ROM; the body was clamped"
        ));
    }
    if bad_bodies != 0 {
        warnings.push(format!(
            "{bad_bodies} YMZ280B key-on(s) had an empty start/stop range"
        ));
    }
    (samples, offs, warnings)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn depth_from_mode_bits() {
        assert_eq!(ymz_depth(0x01), YMZ_DEPTH_ADPCM);
        assert_eq!(ymz_depth(0x02), YMZ_DEPTH_PCM8);
        assert_eq!(ymz_depth(0x03), YMZ_DEPTH_PCM16);
        assert_eq!(ymz_depth(0x00), 0);
        // Only the low two bits select a mode; bit 2 is ignored.
        assert_eq!(ymz_depth(0x05), YMZ_DEPTH_ADPCM);
        assert_eq!(ymz_depth(0x07), YMZ_DEPTH_PCM16);
    }

    #[test]
    fn rate_masks_adpcm_to_eight_bits() {
        // 16934400 * (0x1FF + 1) / 98304 = 172.265625 kHz for 9-bit PCM.
        assert_eq!(ymz280b_rate(16934400, 0x1FF, YMZ_DEPTH_PCM8), 88200.0);
        // ADPCM ignores bit 8, so 0x1FF behaves like 0xFF.
        assert_eq!(
            ymz280b_rate(16934400, 0x1FF, YMZ_DEPTH_ADPCM),
            ymz280b_rate(16934400, 0xFF, YMZ_DEPTH_ADPCM)
        );
        // A zero clock uses the default clock, and the rate never goes below 1.
        assert_eq!(ymz280b_rate(0, 0, YMZ_DEPTH_PCM8), 172.265625);
        assert_eq!(ymz280b_rate(1, -2, YMZ_DEPTH_PCM8), 1.0);
    }

    #[test]
    fn pan_law_endpoints_and_centre() {
        assert_eq!(ymz_pan_ratio(0), (15, 0));
        assert_eq!(ymz_pan_ratio(1), (15, 0));
        assert_eq!(ymz_pan_ratio(8), (15, 15));
        assert_eq!(ymz_pan_ratio(9), (13, 15));
        assert_eq!(ymz_pan_ratio(15), (0, 15));
        assert_eq!(ymz_pan_fx(8), 0xFF);
        assert_eq!(ymz_pan_fx(15), 0x0F);
        assert_eq!(ymz_pan_fx(1), 0xF0);
        // Only the low four bits of the register matter.
        assert_eq!(ymz_pan_ratio(0x18), (15, 15));
    }

    #[test]
    fn hit_and_sample_lengths_follow_depth() {
        let mut hit = YmzHit::new(100, 0, 0, 0x10, 0x20, 0, 0, false, YMZ_DEPTH_PCM16, 1, 1, 8);
        assert_eq!(hit.length(), 8);
        hit.depth = YMZ_DEPTH_ADPCM;
        assert_eq!(hit.length(), 32);
        hit.depth = YMZ_DEPTH_PCM8;
        assert_eq!(hit.length(), 16);
        hit.start = 0x20;
        assert_eq!(hit.length(), 0);

        let mut smp = YmzSample {
            chip_id: 0,
            start: 0,
            stop: 32,
            data: vec![0; 32],
            depth: YMZ_DEPTH_PCM16,
            loop_start: None,
            loop_end: None,
            hits: Vec::new(),
            clock: 16934400,
        };
        assert_eq!(smp.length(), 16);
        smp.depth = YMZ_DEPTH_ADPCM;
        assert_eq!(smp.length(), 64);
        smp.depth = YMZ_DEPTH_PCM8;
        assert_eq!(smp.length(), 32);
    }

    #[test]
    fn played_fnum_c4_rate_and_mode_fnum() {
        let mut hit = YmzHit::new(100, 0, 0, 0, 32, 0, 0, false, YMZ_DEPTH_PCM8, 0, 0, 8);
        hit.freq_pts = vec![(90, 0), (100, 0x1FF)];
        assert_eq!(hit_played_fnum(&hit), 0x1FF);
        hit.fnum = 0x100;
        assert_eq!(hit_played_fnum(&hit), 0x100);

        let mut a = hit.clone();
        a.fnum = 0x0FF;
        let mut b = hit.clone();
        b.fnum = 0x200;
        b.freq_pts.clear();
        let smp = YmzSample {
            chip_id: 0,
            start: 0,
            stop: 0,
            data: Vec::new(),
            depth: YMZ_DEPTH_PCM8,
            loop_start: None,
            loop_end: None,
            hits: vec![a, b],
            clock: 0,
        };
        // Tie on count: the first frequency seen wins.
        assert_eq!(smp.mode_fnum(), 0x0FF);
        // 16934400 * 0x100 / 98304 = 44100 Hz.
        assert_eq!(smp.c4_rate(), 44100);
    }

    #[test]
    fn collect_slices_loops_and_warns() {
        let mut vgm = VgmFile::default();
        vgm.ymz280b_clock = 16_934_400;
        vgm.roms.push(crate::vgm::RomSlice {
            chip: 0,
            rom_size: 16,
            start: 0,
            data: (0u8..16).collect(),
            kind: "ymz280b",
            time: 0,
        });
        let mut push = |t: i64, chip_id: u8, reg: u32, val: u32| {
            vgm.writes.push(crate::vgm::ChipWrite {
                sample: t,
                chip: "ymz280b",
                chip_id,
                reg,
                val,
            });
        };
        push(0, 0, 0xFF, 0x80); // enable chip 0
        // Voice 0: stop 16, loop end 8, 16-bit PCM looping.
        push(10, 0, 0x63, 16);
        push(10, 0, 0x62, 8);
        push(10, 0, 0x00, 0xFF);
        push(10, 0, 0x01, 0x80 | 0x60 | 0x10);
        push(50, 0, 0x01, 0x70); // key off
        // Voice 1: stop 16, 8-bit PCM.
        push(20, 0, 0x67, 16);
        push(20, 0, 0x04, 0x7F);
        push(20, 0, 0x05, 0x80 | 0x40);
        push(60, 0, 0x05, 0x40); // key off
        // Voice 2: key-on with no mode set.
        push(70, 0, 0x09, 0x80);
        drop(push);

        let (samples, offs, warnings) = collect_ymz280b(&vgm);
        assert_eq!(warnings, vec![
            "1 YMZ280B key-on(s) had no sample mode set; those notes stay silent"
        ]);
        assert_eq!(samples.len(), 2);
        // Sorted by (chip_id, start, depth): the 8-bit body comes first.
        assert_eq!(samples[0].depth, YMZ_DEPTH_PCM8);
        assert_eq!(samples[0].data, (0u8..16).collect::<Vec<u8>>());
        assert_eq!(samples[0].c4_rate(), 22050);
        assert_eq!(samples[0].length(), 16);
        assert_eq!(samples[1].depth, YMZ_DEPTH_PCM16);
        assert_eq!(
            samples[1].data,
            (0u8..16)
                .map(|b| if b % 2 == 0 { b + 1 } else { b - 1 })
                .collect::<Vec<u8>>()
        );
        assert_eq!(samples[1].loop_start, Some(0));
        assert_eq!(samples[1].loop_end, Some(4));
        assert_eq!(samples[1].length(), 8);
        assert_eq!(samples[1].hits.len(), 1);
        assert_eq!(samples[1].hits[0].off_sample, 50);
        assert_eq!(offs.len(), 2);
        assert_eq!((offs[0].sample_time, offs[0].voice), (50, 0));
        assert_eq!((offs[1].sample_time, offs[1].voice), (60, 1));
    }
}
