//! Yamaha Y8950: OPL1 FM plus one delta-T (ADPCM-B) voice.
//!
//! The FM half is an OPL1/OPL2 register map and is collected by the shared OPL
//! path (`KIND_Y8950` in opl.py). This module owns the ADPCM half, which is the
//! same delta-T engine the YM2608 carries, exposed at registers `$07`-`$12`:
//!
//!     0x07 control      bit 7 execute, bit 4 repeat, bit 0 reset
//!     0x08 pan/control  bit 7 left, bit 6 right, low bits ROM/RAM select
//!     0x09/0x0A start  low/high, in 32-byte units
//!     0x0B/0x0C end    low/high, in 32-byte units
//!     0x10/0x11 pitch  low/high, the ADPCM-B delta
//!     0x12 level       8-bit
//!
//! The ROM is data block type `0x88`. Address shift, rate divider, byte
//! slicing and the 256-byte body padding are the YM2608's (`ADPCM_B_DIV`,
//! `_deltat_slice`, `deltat_store_body`).

use std::borrow::Cow;

use indexmap::IndexMap;

use crate::vgm::{assemble_rom, VgmFile, VGM_RATE};
use crate::ym2608::_deltat_slice;
use crate::ym2610::{AdpcmBHit, AdpcmBSample, ADPCM_B_DIV};

pub const Y8950_DEFAULT_CLOCK: u32 = 3_579_545;
// 32-byte address units.
pub const ADPCM_B_ADDR_SHIFT: i32 = 5;
// How soon after the start bit a driver writes the sample's own pan/level/rate.
pub const _Y8950_GATE: i32 = 64;

#[derive(Clone, Debug, PartialEq)]
pub struct _State {
    pub start: i32,
    pub end: i32,
    pub freq: i32,
    pub vol: i32,
    pub pan: i32,
}

impl Default for _State {
    fn default() -> Self {
        _State {
            start: 0,
            end: 0,
            freq: 0,
            vol: 0,
            pan: 0,
        }
    }
}

/// The engine's 2-bit L/R field as a Furnace 08xy byte.
pub fn _pan_byte(pan_reg: i32) -> i32 {
    let left = pan_reg & 0x80 != 0;
    let right = pan_reg & 0x40 != 0;
    if left && right {
        return 0xFF;
    }
    if left {
        return 0xF0;
    }
    if right {
        return 0x0F;
    }
    // The Y8950's ADPCM is mono; the platform forces bit 7. A cleared field
    // still plays, so treat it as centred rather than silent.
    0xFF
}

/// Group the Y8950's delta-T plays into samples.
pub fn collect_y8950_delta_t(
    vgm: &VgmFile,
    rom: Option<&[u8]>,
) -> (Vec<AdpcmBSample>, Vec<String>) {
    let mut warnings: Vec<String> = Vec::new();
    #[allow(unused_variables)]
    let clock = if vgm.y8950_clock != 0 {
        vgm.y8950_clock
    } else {
        Y8950_DEFAULT_CLOCK
    };
    let rom: Cow<'_, [u8]> = match rom {
        Some(r) => Cow::Borrowed(r),
        None => Cow::Owned(assemble_rom(vgm, "y8950_deltat", 0, 0, false, true)),
    };
    let mut state = _State::default();
    let mut active: Option<usize> = None;
    let mut hits: Vec<AdpcmBHit> = Vec::new();

    // The reference defines this helper but never calls it: the hit list keeps
    // the play position it was started with.
    #[allow(dead_code)]
    fn advance(hit: &mut AdpcmBHit, t: i64, clock: u32) -> bool {
        if hit.ended {
            return false;
        }
        let dt = t - hit.play_t;
        if dt > 0 && !hit.r#loop {
            let rate_hz =
                clock as f64 / ADPCM_B_DIV as f64 * (hit.play_rate & 0xFFFF) as f64 / 65535.0;
            hit.play_pos += dt as f64 * rate_hz / VGM_RATE as f64;
        }
        hit.play_t = t;
        if !hit.r#loop && hit.play_pos >= (hit.end - hit.start + 1) as f64 {
            hit.ended = true;
            return false;
        }
        true
    }

    for w in &vgm.writes {
        if w.chip != "y8950" {
            continue;
        }
        let r = (w.reg & 0xFF) as i32;
        let val = (w.val & 0xFF) as i32;
        if r == 0x09 {
            state.start = (state.start & 0xFF00) | val;
        } else if r == 0x0A {
            state.start = (state.start & 0x00FF) | (val << 8);
        } else if r == 0x0B {
            state.end = (state.end & 0xFF00) | val;
        } else if r == 0x0C {
            state.end = (state.end & 0x00FF) | (val << 8);
        } else if r == 0x10 {
            state.freq = (state.freq & 0xFF00) | val;
        } else if r == 0x11 {
            state.freq = (state.freq & 0x00FF) | (val << 8);
        } else if r == 0x12 {
            state.vol = val;
        } else if r == 0x08 {
            state.pan = val;
        }
        if r != 0x07 {
            continue;
        }
        // Control write: start, stop, or reset.
        let start = val & 0x80 != 0;
        let reset = val & 0x01 != 0;
        let old = active;
        if !start || reset {
            if let Some(idx) = old {
                hits[idx].end_time = w.sample;
                active = None;
            }
            continue;
        }
        if let Some(idx) = old {
            hits[idx].end_time = w.sample;
        }
        let lo = state.start << ADPCM_B_ADDR_SHIFT;
        let mut hi = ((state.end + 1) << ADPCM_B_ADDR_SHIFT) - 1;
        if hi < lo {
            hi = lo + (1 << ADPCM_B_ADDR_SHIFT) - 1;
        }
        let mut hit = AdpcmBHit::new(
            w.sample,
            0,
            lo,
            hi,
            state.freq,
            state.vol,
            _pan_byte(state.pan),
            val & 0x10 != 0,
        );
        hit.play_t = w.sample;
        hit.play_rate = state.freq;
        hits.push(hit);
        active = Some(hits.len() - 1);
    }

    if let Some(idx) = active {
        if hits[idx].end_time == 0 {
            hits[idx].end_time = vgm.total_samples;
        }
    }

    // A level rewrite while the sample sounds is a mid-note step.
    for hit in hits.iter_mut() {
        for w in &vgm.writes {
            if w.chip != "y8950" || (w.reg & 0xFF) != 0x12 {
                continue;
            }
            let end_time = if hit.end_time != 0 {
                hit.end_time
            } else {
                hit.sample_time
            };
            if !(hit.sample_time < w.sample && w.sample <= end_time) {
                continue;
            }
            let level = (w.val & 0xFF) as i32;
            let last = hit.vol_pts.last().map_or(hit.vol, |pt| pt.1);
            if level != last {
                hit.vol_pts.push((w.sample, level));
            }
        }
    }

    let mut samples: IndexMap<(u8, i32, i32, i32), AdpcmBSample> = IndexMap::new();
    for hit in hits.iter() {
        let key = (hit.chip_id, hit.start, hit.end, hit.rate);
        let smp = samples.entry(key).or_insert_with(|| {
            let data = _deltat_slice(&rom, hit.start, hit.end);
            AdpcmBSample::new(hit.chip_id, hit.start, hit.end, hit.rate, data)
        });
        smp.hits.push(hit.clone());
    }

    if !hits.is_empty() && rom.is_empty() {
        warnings.push(
            "Y8950 ADPCM key-ons but no delta-T data block (0x88); the ADPCM channel is silent"
                .to_string(),
        );
    }
    (samples.into_values().collect(), warnings)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vgm::{ChipWrite, RomSlice};

    const CLOCK: u32 = 3_579_545;

    fn vgm_with(writes: Vec<ChipWrite>, roms: Vec<RomSlice>) -> VgmFile {
        VgmFile {
            version: 0x161,
            total_samples: 400_000,
            ym2151_clock: CLOCK,
            y8950_clock: CLOCK,
            roms,
            writes,
            ..VgmFile::default()
        }
    }

    fn reg(t: i64, reg: u32, val: i32) -> ChipWrite {
        ChipWrite::new(t, "y8950", 0, reg, (val & 0xFF) as u32)
    }

    fn adpcm_note(t: i64, start: i32, end: i32, pitch: i32, level: i32, r#loop: bool) -> Vec<ChipWrite> {
        vec![
            reg(t, 0x09, start & 0xFF),
            reg(t, 0x0A, (start >> 8) & 0xFF),
            reg(t, 0x0B, end & 0xFF),
            reg(t, 0x0C, (end >> 8) & 0xFF),
            reg(t, 0x10, pitch & 0xFF),
            reg(t, 0x11, (pitch >> 8) & 0xFF),
            reg(t, 0x12, level),
            reg(t, 0x07, if r#loop { 0xB0 } else { 0xA0 }),
        ]
    }

    fn rom_slice(start: i64, data: Vec<u8>) -> RomSlice {
        RomSlice {
            chip: 0,
            rom_size: 0,
            start,
            data,
            kind: "y8950_deltat",
            time: 0,
        }
    }

    fn body(n: usize) -> Vec<u8> {
        (0..n).map(|i| (0x80 | ((i * 7) % 126)) as u8).collect()
    }

    /// The Python fixture spaces its writes 5000 samples apart.
    fn lattice(n: i64) -> Vec<ChipWrite> {
        (0..n)
            .map(|k| ChipWrite::new(k * 5000, "ym2151", 0, 0x08, 0x78))
            .collect()
    }

    #[test]
    fn pan_byte_maps_the_two_bits() {
        assert_eq!(_pan_byte(0x80), 0xF0);
        assert_eq!(_pan_byte(0x40), 0x0F);
        assert_eq!(_pan_byte(0xC0), 0xFF);
        assert_eq!(_pan_byte(0x00), 0xFF);
    }

    #[test]
    fn one_control_write_is_one_hit() {
        let mut writes = lattice(60);
        writes.extend(adpcm_note(1000, 0x100, 0x1FF, 0x1000, 0x80, false));
        let (samples, _warn) = collect_y8950_delta_t(
            &vgm_with(writes, vec![rom_slice(0x2000, body(512))]),
            None,
        );
        assert_eq!(samples.len(), 1);
        let smp = &samples[0];
        assert_eq!((smp.start, smp.end), ((0x100 << 5), ((0x1FF + 1) << 5) - 1));
        assert_eq!(smp.rate, 0x1000);
        assert_eq!(smp.hits.len(), 1);
        assert_eq!(smp.hits[0].vol, 0x80);
        assert!(smp.hits[0].end_time == 400_000);
        assert!(!smp.data.is_empty());
    }

    #[test]
    fn reset_stops_the_voice() {
        let mut writes = lattice(60);
        writes.extend(adpcm_note(1000, 0x100, 0x1FF, 0x1000, 0x80, false));
        writes.push(reg(5000, 0x07, 0x01));
        let (samples, _warn) = collect_y8950_delta_t(
            &vgm_with(writes, vec![rom_slice(0x2000, body(512))]),
            None,
        );
        assert_eq!(samples[0].hits[0].end_time, 5000);
    }

    #[test]
    fn a_second_start_closes_the_first() {
        let mut writes = lattice(60);
        writes.extend(adpcm_note(1000, 0x100, 0x1FF, 0x1000, 0x80, false));
        writes.extend(adpcm_note(2000, 0x100, 0x1FF, 0x2000, 0x80, false));
        let (samples, _warn) = collect_y8950_delta_t(
            &vgm_with(writes, vec![rom_slice(0x2000, body(512))]),
            None,
        );
        assert_eq!(samples.len(), 2);
        assert_eq!(samples[0].hits[0].end_time, 2000);
    }

    #[test]
    fn loop_bit_is_kept_and_missing_rom_warns() {
        let mut writes = lattice(60);
        writes.extend(adpcm_note(1000, 0x100, 0x1FF, 0x1000, 0x80, true));
        let (samples, _warn) =
            collect_y8950_delta_t(&vgm_with(writes, vec![rom_slice(0x2000, body(512))]), None);
        assert!(samples[0].hits[0].r#loop);

        let mut writes = lattice(60);
        writes.extend(adpcm_note(1000, 0x100, 0x1FF, 0x1000, 0x80, false));
        let (samples, warn) =
            collect_y8950_delta_t(&vgm_with(writes, Vec::new()), Some(&[]));
        assert_eq!(samples.len(), 1);
        assert!(samples[0].data.is_empty());
        assert_eq!(
            warn,
            vec![
                "Y8950 ADPCM key-ons but no delta-T data block (0x88); the ADPCM channel is silent"
                    .to_string()
            ]
        );
    }
}
