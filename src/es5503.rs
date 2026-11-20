//! Ensoniq ES5503 (Apple IIgs) -> Generic PCM DAC sample bodies.
//!
//! The ES5503 has 32 oscillators playing 8-bit unsigned wave data out of a
//! 128 KB RAM.  Each oscillator has a 16-bit frequency register, an 8-bit
//! volume, a wave-table pointer (256-byte aligned), a size class (256 to 32768
//! bytes) and a resolution class.  The chip reads one byte per output sample
//! step, so the source rate of a wave body is
//!
//! ```text
//! output_rate * freq / 2 ** (9 + resolution - size_class)
//! ```
//!
//! where `output_rate = (clock / 8) / (enabled_oscillators + 2)`.  Wave data
//! is terminated by a 0x00 byte: the chip halts an oscillator when it reads
//! one, which makes every body one-shot unless a driver avoids zero bytes and
//! leaves the oscillator in free-run mode.
//!
//! Furnace has no ES5503 platform, so each used oscillator becomes one
//! 1-channel Generic PCM DAC system.  The wave byte is stored as a signed
//! 8-bit sample (the chip's 0x80 centre line), and the oscillator rate becomes
//! the note/E5 pair relative to the body's most common rate.  Volume stays the
//! source's linear 0-255 value; no extra gain is applied.
//!
//! Port of `vgm2151fur/es5503.py` (v0.11.0).

use std::collections::BTreeMap;

use indexmap::IndexMap;

use crate::parity::{py_int, py_round};
use crate::vgm::{assemble_rom, VgmFile};

pub const ES5503_OSCILLATORS: usize = 32;
pub const ES5503_DEFAULT_CLOCK: u32 = 7_159_090;
pub const ES5503_RAM_SIZE: i64 = 0x20000;
pub const ES5503_MAX_BODY: i64 = 32768;

pub const MODE_FREE: i32 = 0;
pub const MODE_ONESHOT: i32 = 1;
pub const MODE_SYNCAM: i32 = 2;
pub const MODE_SWAP: i32 = 3;

/// Chip output (accumulator) rate for the enabled oscillator count.
pub fn es5503_out_rate(clock: u32, oscillators_enabled: i32) -> i64 {
    let clock = if clock == 0 {
        ES5503_DEFAULT_CLOCK
    } else {
        clock
    };
    let count = oscillators_enabled.clamp(1, ES5503_OSCILLATORS as i32);
    // Python floor divisions: both operands are non-negative.
    ((clock / 8) as i64 / (count as i64 + 2)).max(1000)
}

/// Wave frames per second for one oscillator setting.
pub fn es5503_play_rate(out_rate: i64, freq: i32, resolution: i32, size_class: i32) -> f64 {
    if freq <= 0 {
        return 0.0;
    }
    let step = (1i64 << (9 + (resolution & 7) - (size_class & 7))) as f64;
    out_rate as f64 * freq as f64 / step
}

/// The chip's linear volume register as a 0-255 Furnace level.
pub fn es5503_volume_fx(value: i32) -> i32 {
    value.clamp(0, 255)
}

/// Furnace 08xy for an oscillator's output-channel mask.
///
/// libvgm alternates the stereo channels L/R and sends a trailing odd output
/// channel to both.  With the usual two-channel header, mask 0 is left and
/// mask 1 right; masks past the outputs are silent.
pub fn es5503_pan_fx(mask: i32, output_channels: i32) -> i32 {
    let channels = if output_channels == 0 {
        2
    } else {
        output_channels
    };
    let mask = mask & 0x0F;
    if channels <= 1 {
        return 0xFF;
    }
    let stereo = channels & !1;
    if mask < stereo {
        return if (mask & 1) == 0 { 0xF0 } else { 0x0F };
    }
    if mask == stereo {
        return 0xFF;
    }
    0x00
}

#[derive(Clone, Debug, PartialEq)]
pub struct Es5503Hit {
    pub sample_time: i64,
    pub chip_id: i32,
    pub osc: usize,
    pub freq: i32,
    pub vol: i32,
    pub ptr: i32,
    pub size_class: i32,
    pub resolution: i32,
    pub mode: i32,
    pub chan_mask: i32,
    pub out_rate: i64,
    pub control: i32,
    pub off_sample: i64,
    pub sample_index: i32,
    pub vol_pts: Vec<(i64, i32)>,
    pub freq_pts: Vec<(i64, i32)>,
}

impl Default for Es5503Hit {
    fn default() -> Self {
        Es5503Hit {
            sample_time: 0,
            chip_id: 0,
            osc: 0,
            freq: 0,
            vol: 0,
            ptr: 0,
            size_class: 0,
            resolution: 0,
            mode: 0,
            chan_mask: 0,
            out_rate: 0,
            control: 0,
            off_sample: 0,
            sample_index: -1,
            vol_pts: Vec::new(),
            freq_pts: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Es5503Off {
    pub sample_time: i64,
    pub chip_id: i32,
    pub osc: usize,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Es5503Sample {
    pub chip_id: i32,
    pub ptr: i32,
    pub size_class: i32,
    pub data: Vec<u8>,
    pub c4_rate: i64,
    pub loop_: bool,
    pub hits: Vec<Es5503Hit>,
}

impl Default for Es5503Sample {
    fn default() -> Self {
        Es5503Sample {
            chip_id: 0,
            ptr: 0,
            size_class: 0,
            data: Vec::new(),
            c4_rate: 0,
            loop_: false,
            hits: Vec::new(),
        }
    }
}

impl Es5503Sample {
    pub fn length(&self) -> usize {
        self.data.len()
    }
}

/// The most common key of a Python `Counter`, ties broken by insertion order.
fn most_common<K: Copy>(counts: &IndexMap<K, i64>) -> Option<K> {
    let mut best: Option<(K, i64)> = None;
    for (&key, &n) in counts {
        match best {
            Some((_, bn)) if n <= bn => {}
            _ => best = Some((key, n)),
        }
    }
    best.map(|(key, _)| key)
}

/// Wave bytes from `ptr` up to the 0x00 terminator or the size limit.
fn signed_body(ram: &[u8], ptr: i32, size: i64) -> Vec<u8> {
    if ptr < 0 || ptr as usize >= ram.len() {
        return Vec::new();
    }
    let mut end = ram.len().min((ptr as i64 + size) as usize);
    if let Some(pos) = ram[ptr as usize..end].iter().position(|&b| b == 0) {
        end = ptr as usize + pos;
    }
    ram[ptr as usize..end]
        .iter()
        .map(|&value| value.wrapping_sub(0x80))
        .collect()
}

/// Collect ES5503 oscillator hits and their 8-bit wave bodies.
pub fn collect_es5503(vgm: &VgmFile) -> (Vec<Es5503Sample>, Vec<Es5503Off>, Vec<String>) {
    let mut warnings: Vec<String> = Vec::new();
    let mut hits: Vec<Es5503Hit> = Vec::new();
    let mut offs: Vec<Es5503Off> = Vec::new();

    let mut freq = [0i32; ES5503_OSCILLATORS];
    let mut vol = [0i32; ES5503_OSCILLATORS];
    let mut ptr = [0i32; ES5503_OSCILLATORS];
    let mut size_class = [0i32; ES5503_OSCILLATORS];
    let mut resolution = [0i32; ES5503_OSCILLATORS];
    let mut control: [Option<i32>; ES5503_OSCILLATORS] = [None; ES5503_OSCILLATORS];
    let mut active: [Option<usize>; ES5503_OSCILLATORS] = [None; ES5503_OSCILLATORS];
    let mut changed_wave = [false; ES5503_OSCILLATORS];
    let mut oscillators_enabled = 1i32;

    let clock = if vgm.es5503_clock == 0 {
        ES5503_DEFAULT_CLOCK
    } else {
        vgm.es5503_clock
    };

    for w in &vgm.writes {
        if w.chip != "es5503" {
            continue;
        }
        let reg = w.reg & 0xFF;
        let value = (w.val & 0xFF) as i32;
        if reg == 0xE1 {
            // Global oscillator-enable count, (data >> 1) + 1.
            oscillators_enabled = ((value >> 1) & 0x1F) + 1;
            continue;
        }
        if reg >= 0xE0 {
            continue;
        }
        let osc = (reg & 0x1F) as usize;
        let cls = reg & 0xE0;
        let current = active[osc];
        if cls == 0x00 {
            freq[osc] = (freq[osc] & 0xFF00) | value;
            if let Some(idx) = current {
                hits[idx].freq_pts.push((w.sample, freq[osc]));
            }
        } else if cls == 0x20 {
            freq[osc] = (freq[osc] & 0x00FF) | (value << 8);
            if let Some(idx) = current {
                hits[idx].freq_pts.push((w.sample, freq[osc]));
            }
        } else if cls == 0x40 {
            vol[osc] = value;
            if let Some(idx) = current {
                hits[idx].vol_pts.push((w.sample, value));
            }
        } else if cls == 0x80 {
            ptr[osc] = (ptr[osc] & 0x10000) | (value << 8);
            if current.is_some() {
                changed_wave[osc] = true;
            }
        } else if cls == 0xA0 {
            let old = control[osc];
            control[osc] = Some(value);
            let running = (value & 1) == 0;
            let was_running = old.is_some_and(|old| (old & 1) == 0);
            if running && !was_running {
                if let Some(idx) = current {
                    hits[idx].off_sample = w.sample;
                }
                let hit = Es5503Hit {
                    sample_time: w.sample,
                    chip_id: w.chip_id as i32,
                    osc,
                    freq: freq[osc],
                    vol: vol[osc],
                    ptr: ptr[osc],
                    size_class: size_class[osc],
                    resolution: resolution[osc],
                    mode: (value >> 1) & 3,
                    chan_mask: (value >> 4) & 0x0F,
                    out_rate: es5503_out_rate(clock, oscillators_enabled),
                    control: value,
                    ..Default::default()
                };
                hits.push(hit);
                active[osc] = Some(hits.len() - 1);
            } else if !running && was_running {
                if let Some(idx) = current {
                    hits[idx].off_sample = w.sample;
                    offs.push(Es5503Off {
                        sample_time: w.sample,
                        chip_id: w.chip_id as i32,
                        osc,
                    });
                    active[osc] = None;
                }
            }
        } else if cls == 0xC0 {
            ptr[osc] = (ptr[osc] & 0xFFFF) | (if value & 0x40 != 0 { 0x10000 } else { 0 });
            size_class[osc] = (value >> 3) & 7;
            resolution[osc] = value & 7;
            if current.is_some() {
                changed_wave[osc] = true;
            }
        }
    }

    for (osc, hit) in active.iter().enumerate() {
        if let Some(idx) = hit {
            offs.push(Es5503Off {
                sample_time: vgm.total_samples,
                chip_id: hits[*idx].chip_id,
                osc,
            });
        }
    }

    for (osc, changed) in changed_wave.iter().enumerate() {
        if *changed {
            warnings.push(format!(
                "ES5503 oscillator {}: wave table changed mid-note; only the key-on table is used",
                osc + 1
            ));
        }
    }

    if !hits.is_empty() && vgm.roms.is_empty() {
        warnings.push(
            "ES5503 writes present but no ES5503 RAM data block; the wave bodies are empty"
                .to_string(),
        );
    }

    let chip_ids: std::collections::BTreeSet<i32> = hits.iter().map(|hit| hit.chip_id).collect();
    let mut ram_by_chip: BTreeMap<i32, Vec<u8>> = BTreeMap::new();
    for chip_id in chip_ids {
        ram_by_chip.insert(chip_id, assemble_rom(vgm, "es5503", chip_id as u8, 0, false, true));
    }

    let mut by_body: IndexMap<(i32, i32, i32), Vec<usize>> = IndexMap::new();
    for (index, hit) in hits.iter().enumerate() {
        by_body
            .entry((hit.chip_id, hit.ptr, hit.size_class))
            .or_default()
            .push(index);
    }

    let mut samples: Vec<Es5503Sample> = Vec::new();
    let mut kept: Vec<bool> = vec![false; hits.len()];
    for ((chip_id, body_ptr, body_size_class), body_hits) in &by_body {
        let empty: &[u8] = &[];
        let ram = ram_by_chip.get(chip_id).map(Vec::as_slice).unwrap_or(empty);
        let size = 256i64 << body_size_class;
        let data = signed_body(ram, *body_ptr, size);
        if data.len() < 2 {
            warnings.push(format!(
                "ES5503 {chip_id}: wave at {body_ptr:05x} is empty or one byte; its notes are silent"
            ));
            continue;
        }
        let mut rates: IndexMap<i64, i64> = IndexMap::new();
        for &idx in body_hits {
            let hit = &hits[idx];
            if hit.freq > 0 {
                let rate = py_int(py_round(es5503_play_rate(
                    hit.out_rate,
                    hit.freq,
                    hit.resolution,
                    hit.size_class,
                )));
                *rates.entry(rate).or_insert(0) += 1;
            }
        }
        let c4_rate = match most_common(&rates) {
            Some(rate) => rate,
            None => {
                warnings.push(format!(
                    "ES5503 {chip_id}: wave at {body_ptr:05x} has no pitched hits; its notes are silent"
                ));
                continue;
            }
        };
        let mut modes: IndexMap<i32, i64> = IndexMap::new();
        for &idx in body_hits {
            *modes.entry(hits[idx].mode).or_insert(0) += 1;
        }
        let loop_ = most_common(&modes) == Some(MODE_FREE) && data.len() as i64 >= size;
        let mut smp = Es5503Sample {
            chip_id: *chip_id,
            ptr: *body_ptr,
            size_class: *body_size_class,
            data,
            c4_rate: c4_rate.max(1),
            loop_,
            hits: Vec::new(),
        };
        for &idx in body_hits {
            let mut hit = hits[idx].clone();
            hit.sample_index = samples.len() as i32;
            smp.hits.push(hit);
            kept[idx] = true;
        }
        samples.push(smp);
    }

    // Drop hits whose body was silent; their notes must not reach the module.
    if hits
        .iter()
        .zip(kept.iter())
        .any(|(hit, &keep)| keep && (hit.mode == MODE_SYNCAM || hit.mode == MODE_SWAP))
    {
        warnings.push(
            "ES5503 sync/AM and swap oscillator modes are approximated as one-shot".to_string(),
        );
    }

    (samples, offs, warnings)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vgm::{ChipWrite, RomSlice};

    fn write(sample: i64, reg: u32, val: u32) -> ChipWrite {
        ChipWrite::new(sample, "es5503", 0, reg, val)
    }

    fn vgm_with(ram: Vec<u8>, writes: Vec<ChipWrite>) -> VgmFile {
        let mut vgm = VgmFile::default();
        vgm.es5503_clock = ES5503_DEFAULT_CLOCK;
        vgm.total_samples = 5000;
        if !ram.is_empty() {
            vgm.roms.push(RomSlice {
                chip: 0,
                rom_size: ram.len() as i64,
                start: 0,
                data: ram,
                kind: "es5503",
                time: 0,
            });
        }
        vgm.writes = writes;
        vgm
    }

    /// Key-on oscillator 0 with ptr, size class, freq, vol and control.
    fn keyon(sample: i64, ptr: u32, size_res: u32, freq: u32, control: u32) -> Vec<ChipWrite> {
        vec![
            write(sample, 0x00, freq & 0xFF),
            write(sample, 0x20, (freq >> 8) & 0xFF),
            write(sample, 0x40, 0xFF),
            write(sample, 0x80, (ptr >> 8) & 0xFFFF),
            write(sample, 0xC0, size_res),
            write(sample, 0xA0, control),
        ]
    }

    #[test]
    fn rates_pan_and_volume_match_the_chip() {
        assert_eq!(es5503_out_rate(0, 1), 298_295);
        assert_eq!(es5503_out_rate(ES5503_DEFAULT_CLOCK, 0), 298_295);
        assert_eq!(es5503_out_rate(ES5503_DEFAULT_CLOCK, 2), 223_721);
        // The count clamps to 1..32.
        assert_eq!(es5503_out_rate(ES5503_DEFAULT_CLOCK, 100), es5503_out_rate(ES5503_DEFAULT_CLOCK, 32));
        assert_eq!(es5503_out_rate(8_000, 1), 1000);

        assert_eq!(es5503_play_rate(298_295, 0, 0, 0), 0.0);
        assert_eq!(es5503_play_rate(298_295, 4096, 0, 0), 2_386_360.0);
        assert_eq!(es5503_play_rate(298_295, 1000, 3, 0), 72_825.927_734_375);

        assert_eq!(es5503_volume_fx(-1), 0);
        assert_eq!(es5503_volume_fx(300), 255);
        assert_eq!(es5503_volume_fx(0x80), 0x80);

        assert_eq!(es5503_pan_fx(0, 2), 0xF0);
        assert_eq!(es5503_pan_fx(1, 2), 0x0F);
        assert_eq!(es5503_pan_fx(2, 2), 0xFF);
        assert_eq!(es5503_pan_fx(3, 2), 0x00);
        assert_eq!(es5503_pan_fx(0, 0), 0xF0);
        assert_eq!(es5503_pan_fx(0, 1), 0xFF);
        assert_eq!(es5503_pan_fx(3, 4), 0x0F);
        assert_eq!(es5503_pan_fx(4, 4), 0xFF);
        assert_eq!(es5503_pan_fx(5, 4), 0x00);
        // Only the low nibble of the mask is used.
        assert_eq!(es5503_pan_fx(0x13, 2), 0x00);
    }

    #[test]
    fn signed_body_stops_at_zero_and_centres_on_0x80() {
        let ram = [0x80u8, 0x81, 0x00, 0x82];
        assert_eq!(signed_body(&ram, 0, 4), vec![0x00, 0x01]);
        assert_eq!(signed_body(&ram, 1, 4), vec![0x01]);
        assert_eq!(signed_body(&ram, 0, 2), vec![0x00, 0x01]);
        assert_eq!(signed_body(&ram, 3, 400), vec![0x02]);
        assert!(signed_body(&ram, 4, 4).is_empty());
        assert!(signed_body(&ram, -1, 4).is_empty());
        assert!(signed_body(&[], 0, 4).is_empty());
    }

    #[test]
    fn collect_extracts_a_free_running_loop_and_its_hit() {
        let mut ram = vec![0x80u8; 0x300];
        for (i, b) in ram[0x100..0x200].iter_mut().enumerate() {
            *b = 0x80u8.wrapping_add((i & 0x7F) as u8);
        }
        let mut writes = keyon(10, 0x100, 0x00, 0x1000, 0x00);
        // A wave-table change while the note runs is reported, not applied.
        writes.push(write(20, 0x80, 0x02));
        // Pitch and volume motion while the note runs.
        writes.push(write(30, 0x00, 0x00));
        writes.push(write(31, 0x20, 0x20));
        writes.push(write(32, 0x40, 0x40));
        writes.push(write(40, 0xA0, 0x01));
        let vgm = vgm_with(ram, writes);

        let (samples, offs, warnings) = collect_es5503(&vgm);
        assert_eq!(samples.len(), 1);
        let smp = &samples[0];
        assert_eq!((smp.chip_id, smp.ptr, smp.size_class), (0, 0x100, 0));
        assert_eq!(smp.length(), 0x100);
        assert!(smp.loop_);
        assert_eq!(smp.c4_rate, 2_386_360);
        assert_eq!(smp.data[0], 0x00);
        assert_eq!(smp.data[1], 0x01);
        assert_eq!(smp.hits.len(), 1);
        let hit = &smp.hits[0];
        assert_eq!(hit.sample_time, 10);
        assert_eq!(hit.freq, 0x1000);
        assert_eq!(hit.vol, 255);
        assert_eq!(hit.mode, MODE_FREE);
        assert_eq!(hit.chan_mask, 0);
        assert_eq!(hit.out_rate, 298_295);
        assert_eq!(hit.sample_index, 0);
        assert_eq!(hit.off_sample, 40);
        assert_eq!(hit.freq_pts, vec![(30, 0x1000), (31, 0x2000)]);
        assert_eq!(hit.vol_pts, vec![(32, 0x40)]);

        assert_eq!(offs.len(), 1);
        assert_eq!(
            (offs[0].sample_time, offs[0].chip_id, offs[0].osc),
            (40, 0, 0)
        );
        assert_eq!(
            warnings,
            vec![
                "ES5503 oscillator 1: wave table changed mid-note; only the key-on table is used"
                    .to_string()
            ]
        );
    }

    #[test]
    fn collect_warns_about_empty_and_unpitched_bodies() {
        // Body at 0x00 is a 256-byte window whose first byte is the 0x00
        // terminator: empty body, hit dropped, key-off kept.
        let mut ram = vec![0x80u8; 0x200];
        ram[0] = 0x00;
        let writes = keyon(0, 0x00, 0x00, 0x800, 0x00);
        let vgm = vgm_with(ram, writes);
        let (samples, offs, warnings) = collect_es5503(&vgm);
        assert!(samples.is_empty());
        // The oscillator was left running, so the end of the song closes it.
        assert_eq!(offs.len(), 1);
        assert_eq!(offs[0].sample_time, 5000);
        assert_eq!(
            warnings,
            vec![
                "ES5503 0: wave at 00000 is empty or one byte; its notes are silent".to_string()
            ]
        );

        // A body with no nonzero frequency never produces a rate.
        let ram = vec![0x81u8; 0x200];
        let writes = keyon(0, 0x00, 0x00, 0x0000, 0x00);
        let vgm = vgm_with(ram, writes);
        let (samples, _offs, warnings) = collect_es5503(&vgm);
        assert!(samples.is_empty());
        assert_eq!(
            warnings,
            vec!["ES5503 0: wave at 00000 has no pitched hits; its notes are silent".to_string()]
        );
    }

    #[test]
    fn collect_without_ram_and_with_sync_modes_warns() {
        let writes = keyon(0, 0x00, 0x00, 0x1000, 0x00);
        let vgm = vgm_with(Vec::new(), writes);
        let (samples, _offs, warnings) = collect_es5503(&vgm);
        assert!(samples.is_empty());
        assert_eq!(
            warnings,
            vec![
                "ES5503 writes present but no ES5503 RAM data block; the wave bodies are empty"
                    .to_string(),
                "ES5503 0: wave at 00000 is empty or one byte; its notes are silent".to_string(),
            ]
        );

        // A SYNC/AM oscillator (mode 2) still yields a body but is flagged.
        let ram = vec![0x81u8; 0x300];
        let writes = keyon(0, 0x01, 0x00, 0x1000, (MODE_SYNCAM as u32) << 1);
        let vgm = vgm_with(ram, writes);
        let (samples, _offs, warnings) = collect_es5503(&vgm);
        assert_eq!(samples.len(), 1);
        assert!(!samples[0].loop_);
        assert_eq!(
            warnings,
            vec![
                "ES5503 sync/AM and swap oscillator modes are approximated as one-shot".to_string()
            ]
        );
    }
}
