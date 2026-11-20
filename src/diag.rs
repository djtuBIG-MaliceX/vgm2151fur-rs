//! Register-level diagnostics for a conversion.
//!
//! The source VGM and the Furnace export are the two register streams to
//! compare, channel by channel. Pitch is the sensor: place and trigger errors
//! show up as missing notes, while a drifting pitch stream shows as a run where
//! the export sits away from the source. `pitch_runs` measures those runs and
//! prints them with the register values at the worst point, which is enough to
//! tell a lost glide from a wrong base pitch.
//!
//! `export_fur` renders the .fur to a .vgm with the bundled Furnace build. The
//! export is the ground truth for what the .fur means to a player. Every
//! comparison here starts with it.

use std::collections::BTreeSet;
use std::fmt;
use std::path::{Path, PathBuf};

use indexmap::IndexMap;

use crate::ay8910::ay_min_period;
use crate::k005289::K005289_DEFAULT_CLOCK;
use crate::model::kc_kf_units;
use crate::opl::{self, OPLState, N as OPL_N, SLOT_ADDR};
use crate::parity::{py_floor_div, py_int, py_mod, py_round};
use crate::vgm::{ChipWrite, VgmFile, VGM_RATE};
use crate::ym2610::{CHAN_OFF_4, CHAN_OFF_6, KON_4, KON_6, YM2610_DEFAULT_CLOCK};
use crate::ym2608::{YM2608_DEFAULT_CLOCK, YM2608_SSG_DIVIDER};
use crate::ym2612::{ym2612_pitch_units, EXT_FREQ_OP};

// `src/ym2203.rs` (the YM2203 port) is not part of this tree yet. The formula
// is the shared OPN f-num/block pitch unit; `analyze::emit` carries the same
// private helper.
fn opn_pitch_units(block: i32, fnum: i32) -> f64 {
    let fnum = if fnum <= 0 { 1 } else { fnum };
    768.0 * ((fnum as f64).log2() + block as f64)
}

/// Python's `divmod` on module channel indexes.
fn divmod(a: i64, b: i64) -> (i64, i64) {
    (py_floor_div(a, b), py_mod(a, b))
}

/// Python list indexing, negative index included.
fn py_index(len: usize, i: i64) -> Option<usize> {
    let len = len as i64;
    let j = if i < 0 { len + i } else { i };
    if j >= 0 && j < len { Some(j as usize) } else { None }
}

#[derive(Clone, Debug, PartialEq)]
pub struct PitchRun {
    pub t0: i64,
    pub t1: i64,
    pub worst_st: f64,
}

impl fmt::Display for PitchRun {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{:7.3}-{:7.3}s ({:5.0} ms)  {:+.2} st",
            self.t0 as f64 / 44100.0,
            self.t1 as f64 / 44100.0,
            (self.t1 - self.t0) as f64 / 44.1,
            self.worst_st
        )
    }
}

/// `DiagError` in the reference. The comparison cannot be made: the exported
/// VGM has no such chip. It shares [`crate::Error`], whose `Msg` text the CLI
/// prints verbatim.
pub type DiagError = crate::Error;

fn has_chip(vgm: &VgmFile, name: &str) -> bool {
    vgm.writes.iter().any(|w| w.chip == name)
}

/// (sample, pitch units) after each KC/KF write on one YM2151 channel.
pub fn kc_kf_stream(vgm: &VgmFile, ch: i32) -> Vec<(i64, i32)> {
    let mut kc: Option<u32> = None;
    let mut kf: Option<u32> = None;
    let mut out: Vec<(i64, i32)> = Vec::new();
    for w in &vgm.writes {
        if w.chip != "ym2151" {
            continue;
        }
        if (0x28..=0x2F).contains(&w.reg) && (w.reg & 7) == ch as u32 {
            kc = Some(w.val);
        } else if (0x30..=0x37).contains(&w.reg) && (w.reg & 7) == ch as u32 {
            kf = Some(w.val);
        } else {
            continue;
        }
        if let (Some(kc), Some(kf)) = (kc, kf) {
            out.push((w.sample, kc_kf_units(kc as i32, kf as i32)));
        }
    }
    out
}

fn _opn_chip_ids(vgm: &VgmFile) -> Vec<u8> {
    let mut ids: Vec<u8> = vgm
        .writes
        .iter()
        .filter(|w| w.chip == "ym2203" || w.chip == "ym2203_ssg")
        .map(|w| w.chip_id)
        .collect();
    ids.sort();
    ids.dedup();
    ids
}

/// True for the YM2203's nine-channel channel-3-operator layout.
fn _opn_ext(vgm: &VgmFile) -> bool {
    vgm.writes
        .iter()
        .any(|w| w.chip == "ym2203" && (0xA8..=0xAE).contains(&w.reg))
}

/// Pitch for an AY/SSG tone period; larger is higher, differences are
/// meaningful (the clock/divider constant cancels between two logs).
pub fn opn_ssg_units(period: i32) -> f64 {
    -768.0 * (period.max(1) as f64).log2()
}

/// The OPN stream emit used by the YM2203 walk: a channel-3 operator column
/// follows its own Ext value only while special mode is on.
fn _emit_opn(
    out: &mut Vec<(i64, f64)>,
    ext_col: bool,
    special: bool,
    wanted: i32,
    value: &[Option<(i32, i32)>; 4],
    sample: i64,
) {
    let g = if ext_col && special { wanted } else { 3 };
    if let Some((b, f)) = value[g as usize] {
        out.push((sample, opn_pitch_units(b, f)));
    }
}

/// The two-port OPN emit: Ext columns read the split channel-3 values, a
/// normal FM channel reads its own latched f-num/block.
fn _emit_ym2612(
    out: &mut Vec<(i64, f64)>,
    kind: &str,
    mode: i32,
    group: i32,
    value: &[Option<(i32, i32)>; 4],
    voice_value: Option<(i32, i32)>,
    sample: i64,
) {
    if kind == "ext" {
        let g = if mode != 0 { group } else { 3 };
        if let Some((b, f)) = value[g as usize] {
            out.push((sample, ym2612_pitch_units(b, f)));
        }
    } else if let Some((b, f)) = voice_value {
        out.push((sample, ym2612_pitch_units(b, f)));
    }
}

fn _opn_ssg_stream(vgm: &VgmFile, cid: u8, ssg: i32) -> Vec<(i64, i32)> {
    let clock = if vgm.ym2203_clock == 0 {
        3_579_545
    } else {
        vgm.ym2203_clock
    };
    _ssg_stream(vgm, "ym2203_ssg", cid, ssg, clock, 16.0)
}

/// (sample, pitch units) for one YM2203 voice.
///
/// A standard YM2203 owns six Furnace channels: three FM then three SSG. The
/// channel-3-operator layout owns nine: FM1, FM2, four FM3 operator columns,
/// then the three SSG voices. The FM stream uses f-num/block units; the SSG
/// stream uses the tone period.
pub fn opn_pitch_stream(vgm: &VgmFile, ch: i32) -> Vec<(i64, f64)> {
    let ids = _opn_chip_ids(vgm);
    let ext = _opn_ext(vgm);
    let chunk = if ext { 9 } else { 6 };
    let (cidx, voice) = divmod(ch as i64, chunk);
    let cid = match py_index(ids.len(), cidx) {
        Some(i) => ids[i],
        None => return Vec::new(),
    };
    let ssg_at = if ext { 6 } else { 3 };
    if voice >= ssg_at {
        return _opn_ssg_stream(vgm, cid, (voice - ssg_at) as i32)
            .into_iter()
            .map(|(s, v)| (s, f64::from(v)))
            .collect();
    }
    let ext_col = ext && voice >= 2;
    let src_ch = if ext_col { 2 } else { voice };
    let group = if ext_col {
        EXT_FREQ_OP[(voice - 2) as usize] as i32
    } else {
        3
    };
    let mut out: Vec<(i64, f64)> = Vec::new();
    let mut lo: Option<u32> = None;
    let mut hi: Option<u32> = None;
    let mut slo = [0i32; 3];
    let mut shi = [0i32; 3];
    let mut value: [Option<(i32, i32)>; 4] = [None; 4];
    let mut special = false;

    for w in &vgm.writes {
        if w.chip != "ym2203" || w.chip_id != cid {
            continue;
        }
        let r = w.reg;
        if r == 0x27 {
            special = (w.val & 0xC0) != 0;
            if ext_col {
                _emit_opn(&mut out, ext_col, special, group, &value, w.sample);
            }
            continue;
        }
        if (0xA0..=0xA6).contains(&r) {
            if (r & 3) as i64 != src_ch {
                continue;
            }
            if r < 0xA4 {
                lo = Some(w.val);
                if let Some(hi) = hi {
                    value[3] = Some((
                        ((hi >> 3) & 7) as i32,
                        (lo.unwrap() | ((hi & 7) << 8)) as i32,
                    ));
                    _emit_opn(&mut out, ext_col, special, group, &value, w.sample);
                }
            } else {
                hi = Some(w.val);
                if let Some(lo) = lo {
                    value[3] = Some((
                        ((hi.unwrap() >> 3) & 7) as i32,
                        (lo | ((hi.unwrap() & 7) << 8)) as i32,
                    ));
                    _emit_opn(&mut out, ext_col, special, group, &value, w.sample);
                }
            }
            continue;
        }
        if ext && (0xA8..=0xAE).contains(&r) {
            let idx = (r & 3) as usize;
            if idx >= 3 {
                continue;
            }
            if r <= 0xAA {
                slo[idx] = w.val as i32;
            } else {
                shi[idx] = w.val as i32;
            }
            value[idx] = Some((
                (shi[idx] >> 3) & 7,
                slo[idx] | ((shi[idx] & 7) << 8),
            ));
            if ext_col && idx as i32 == group {
                _emit_opn(&mut out, ext_col, special, group, &value, w.sample);
            }
        }
    }
    out
}

pub fn state_at<T: Copy>(stream: &[(i64, T)], t: i64) -> Option<T> {
    let i = stream.partition_point(|(s, _v)| *s <= t);
    if i > 0 { Some(stream[i - 1].1) } else { None }
}

/// Samples where one YM2203 voice is sounding.
///
/// `ch` uses the module layout: six channels per chip (three FM then three
/// SSG) or nine when the source uses the channel-3-operator layout (FM1, FM2,
/// four FM3 columns, three SSG). FM uses the 0x28 operator mask (a column
/// follows its own mask bit); SSG uses its mixer tone bit and level register.
/// Chips the log does not contain return an empty list.
pub fn opn_keyed_intervals(vgm: &VgmFile, ch: i32) -> Vec<(i64, i64)> {
    let ids = _opn_chip_ids(vgm);
    let ext = _opn_ext(vgm);
    let chunk = if ext { 9 } else { 6 };
    let (chip, voice) = divmod(ch as i64, chunk);
    let cid = match py_index(ids.len(), chip) {
        Some(i) => ids[i],
        None => return Vec::new(),
    };
    let mut out: Vec<(i64, i64)> = Vec::new();
    let mut on = false;
    let mut start = 0i64;
    let ssg_at = if ext { 6 } else { 3 };
    if voice >= ssg_at {
        let clock = if vgm.ym2203_clock == 0 {
            3_579_545
        } else {
            vgm.ym2203_clock
        };
        // Noise-only percussion has no meaningful tone period, and a period
        // below the module's representable range is the driver's silence gate:
        // both are handled by the shared SSG walker, which gates on a tone-only
        // mixer and an in-range period.
        return _ssg_keyed_intervals(
            vgm,
            "ym2203_ssg",
            cid,
            (voice - ssg_at) as i32,
            clock,
            16.0,
        );
    } else {
        let ext_col = ext && voice >= 2;
        let target_ch = if ext_col { 2 } else { voice };
        let bit: Option<u32> = if ext_col { Some(1 << (voice - 2)) } else { None };
        for w in &vgm.writes {
            if w.chip != "ym2203"
                || w.chip_id != cid
                || w.reg != 0x28
                || (w.val & 3) as i64 != target_ch
            {
                continue;
            }
            let mask = (w.val >> 4) & 0xF;
            let now = match bit {
                Some(bit) => mask & bit != 0,
                None => mask != 0,
            };
            if now && !on {
                start = w.sample;
                on = true;
            } else if !now && on {
                out.push((start, w.sample));
                on = false;
            }
        }
    }
    if on {
        let end = if vgm.total_samples != 0 {
            vgm.total_samples
        } else {
            vgm.writes
                .iter()
                .map(|w| w.sample)
                .max()
                .unwrap_or(start)
                + 1
        };
        out.push((start, end));
    }
    out
}

fn _ym2612_chip_ids(vgm: &VgmFile, chip: &str) -> Vec<u8> {
    let mut ids: Vec<u8> = vgm
        .writes
        .iter()
        .filter(|w| w.chip == chip)
        .map(|w| w.chip_id)
        .collect();
    ids.sort();
    ids.dedup();
    ids
}

/// True when the source ever selects channel-3 special/CSM mode.
///
/// The mode register is on port 0 for every two-port OPN chip; the YM2608's
/// second FM block answers the same `$27` on its own port.
fn _ym2612_ext(vgm: &VgmFile, chip: &str) -> bool {
    vgm.writes.iter().any(|w| {
        w.chip == chip && (w.reg & 0x100) == 0 && (w.reg & 0xFF) == 0x27 && (w.val & 0xC0) != 0
    })
}

/// (chip id, kind, voice/column) for one module channel.
///
/// `kind` is "fm" for a normal channel, "ext" for a channel-3 operator column.
/// Returns None when the module layout has no such channel.
fn _ym2612_channel_map(vgm: &VgmFile, ch: i32, chip: &str) -> Option<(u8, &'static str, i32)> {
    let ids = _ym2612_chip_ids(vgm, chip);
    let chunk = if _ym2612_ext(vgm, chip) { 9 } else { 6 };
    let (chip_i, voice) = divmod(ch as i64, chunk);
    let cid = ids[py_index(ids.len(), chip_i)?];
    if voice < 2 {
        return Some((cid, "fm", voice as i32));
    }
    if chunk == 9 {
        if voice < 6 {
            return Some((cid, "ext", (voice - 2) as i32));
        }
        return Some((cid, "fm", (voice - 3) as i32));
    }
    Some((cid, "fm", voice as i32))
}

/// (sample, pitch units) for one two-port OPN module channel.
///
/// The normal layout follows the channel's latched f-num/block. The Ext
/// layout follows the four channel-3 operator pairs for its columns. Every
/// two-port OPN chip - YM2612 and YM2608 alike - uses this one mapping, because
/// the frequency registers and their formula are shared.
pub fn ym2612_pitch_stream(vgm: &VgmFile, ch: i32, chip: &str) -> Vec<(i64, f64)> {
    let (cid, kind, voice) = match _ym2612_channel_map(vgm, ch, chip) {
        Some(mapped) => mapped,
        None => return Vec::new(),
    };
    let group = if kind == "ext" {
        EXT_FREQ_OP[voice as usize] as i32
    } else {
        3
    };
    let mut out: Vec<(i64, f64)> = Vec::new();
    let mut hi: [Option<u32>; 6] = [None; 6];
    let mut shi: [Option<u32>; 3] = [None; 3];
    let mut value: [Option<(i32, i32)>; 4] = [None; 4];
    let mut voice_value: Option<(i32, i32)> = None;
    let mut mode = 0i32;

    for w in &vgm.writes {
        if w.chip != chip || w.chip_id != cid {
            continue;
        }
        let r = w.reg & 0xFF;
        let bank = (w.reg & 0x100) != 0;
        if !bank && r == 0x27 {
            if kind == "ext" {
                mode = ((w.val >> 6) & 3) as i32;
                _emit_ym2612(
                    &mut out, kind, mode, group, &value, voice_value, w.sample,
                );
            }
            continue;
        }
        if !bank && (0xA8..=0xAE).contains(&r) {
            let idx = (r & 3) as usize;
            if idx == 3 {
                continue;
            }
            if r < 0xAC {
                if let Some(shi_v) = shi[idx] {
                    let block = ((shi_v >> 3) & 7) as i32;
                    let fnum = (w.val | ((shi_v & 7) << 8)) as i32;
                    value[idx] = Some((block, fnum));
                    shi[idx] = None;
                    _emit_ym2612(
                        &mut out, kind, mode, group, &value, voice_value, w.sample,
                    );
                }
            } else {
                shi[idx] = Some(w.val);
            }
            continue;
        }
        if (0xA0..=0xA6).contains(&r) {
            let src_ch = ((r & 3) + if bank { 3 } else { 0 }) as usize;
            if (r & 3) == 3 {
                continue;
            }
            if r < 0xA4 {
                if let Some(hi_v) = hi[src_ch] {
                    let block = ((hi_v >> 3) & 7) as i32;
                    let fnum = (w.val | ((hi_v & 7) << 8)) as i32;
                    if src_ch == 2 {
                        value[3] = Some((block, fnum));
                        if kind == "ext" && (mode == 0 || group == 3) {
                            _emit_ym2612(
                                &mut out, kind, mode, group, &value, voice_value,
                                w.sample,
                            );
                        } else if kind == "fm" && voice == 2 {
                            voice_value = Some((block, fnum));
                            _emit_ym2612(
                                &mut out, kind, mode, group, &value, voice_value,
                                w.sample,
                            );
                        }
                    } else if kind == "fm" && src_ch as i32 == voice {
                        voice_value = Some((block, fnum));
                        _emit_ym2612(
                            &mut out, kind, mode, group, &value, voice_value, w.sample,
                        );
                    }
                    hi[src_ch] = None;
                }
            } else {
                hi[src_ch] = Some(w.val);
            }
        }
    }
    out
}

/// Samples where one two-port OPN module channel is keyed.
///
/// In the Ext layout channel-3 columns follow the 0x28 mask bits; the other
/// channels use their own key state. DAC playback on channel 6 is not FM, so a
/// keyed FM6 under DAC enable is treated as silent - that test is the chip's
/// own, since `$2B` is an IRQ register on the YM2608.
pub fn ym2612_keyed_intervals(
    vgm: &VgmFile,
    ch: i32,
    chip: &str,
    dac: bool,
) -> Vec<(i64, i64)> {
    let (cid, kind, voice) = match _ym2612_channel_map(vgm, ch, chip) {
        Some(mapped) => mapped,
        None => return Vec::new(),
    };
    let (target_ch, bit) = if kind == "ext" {
        (2i32, Some(1u32 << voice))
    } else {
        (voice, None)
    };
    let mut out: Vec<(i64, i64)> = Vec::new();
    let mut on = false;
    let mut start = 0i64;
    let mut dac_on = false;
    for w in &vgm.writes {
        if w.chip != chip || w.chip_id != cid {
            continue;
        }
        let r = w.reg & 0xFF;
        if dac && (w.reg & 0x100) == 0 && r == 0x2B {
            dac_on = (w.val & 0x80) != 0;
            continue;
        }
        if r != 0x28 || (w.reg & 0x100) != 0 {
            continue;
        }
        let raw_ch = (w.val & 7) as i32;
        if raw_ch == 3 || raw_ch == 7 {
            continue;
        }
        let src_ch = if raw_ch < 3 { raw_ch } else { raw_ch - 1 };
        if src_ch != target_ch {
            continue;
        }
        let mask = (w.val >> 4) & 0x0F;
        let mut now = match bit {
            Some(bit) => mask & bit != 0,
            None => mask != 0,
        };
        if dac && target_ch == 5 && dac_on {
            now = false;
        }
        if now && !on {
            start = w.sample;
            on = true;
        } else if !now && on {
            out.push((start, w.sample));
            on = false;
        }
    }
    if on {
        let end = if vgm.total_samples != 0 {
            vgm.total_samples
        } else {
            vgm.writes
                .iter()
                .map(|w| w.sample)
                .max()
                .unwrap_or(start)
                + 1
        };
        out.push((start, end));
    }
    out
}

/// The OPL family this log drives, if any.
fn _opl_kind(vgm: &VgmFile) -> Option<&'static str> {
    for kind in ["opl3", "opl2", "opl"] {
        if has_chip(vgm, kind) {
            return Some(kind);
        }
    }
    None
}

/// (kind, chip_id, column) of a module channel.
///
/// Returns None when the channel is not an OPL voice it can measure: past the
/// last chip, or one of a rhythm module's kit columns. Two percussion voices
/// share one frequency register there - snare with hi-hat, tom and cymbal with
/// each other. The byte at a hit belongs to whichever was written last and
/// says nothing about the voice being compared.
fn _opl_channel(vgm: &VgmFile, ch: i32) -> Option<(&'static str, u8, i32)> {
    let kind = _opl_kind(vgm)?;
    let drums = vgm
        .writes
        .iter()
        .any(|w| w.chip == kind && (w.reg & 0xFF) == 0xBD && (w.val & 0x20) != 0);
    let chunk = opl::channel_count(kind, drums);
    let mut ids: Vec<u8> = vgm
        .writes
        .iter()
        .filter(|w| w.chip == kind)
        .map(|w| w.chip_id)
        .collect();
    ids.sort();
    ids.dedup();
    if ids.is_empty() {
        ids.push(0);
    }
    let (index, col) = divmod(ch as i64, chunk as i64);
    let cid = ids[py_index(ids.len(), index)?];
    let col = col as i32;
    if drums && opl::is_drum_channel(kind, true, col) {
        return None;
    }
    Some((kind, cid, col))
}

/// (sample, pitch units) for one OPL-family voice channel.
///
/// The unit is `fnum << block` expressed in 1/64 semitone. A comparison
/// between two streams of the same chip is therefore a pitch difference. Only
/// the channel that owns the frequency register is followed: on an OPL3
/// four-operator pair that is the head, whose partner stays silent.
pub fn opl_pitch_stream(vgm: &VgmFile, ch: i32) -> Vec<(i64, i32)> {
    let (kind, cid, col) = match _opl_channel(vgm, ch) {
        Some(mapped) => mapped,
        None => return Vec::new(),
    };
    let st = OPLState::new(kind, false);
    let cm = st.chan_map[col as usize];
    let port = if st.opl3 { i32::from((cm >> 8) & 1) } else { 0 };
    let off = i32::from(cm & 0xFF);
    let mut lo: Option<u32> = None;
    let mut out: Vec<(i64, i32)> = Vec::new();
    for w in &vgm.writes {
        if w.chip != kind || w.chip_id != cid {
            continue;
        }
        if st.opl3 && (((w.reg >> 8) & 1) as i32) != port {
            continue;
        }
        let r = w.reg & 0xFF;
        if r == (0xA0 + off) as u32 {
            lo = Some(w.val);
        } else if r == (0xB0 + off) as u32 {
            if let Some(lo) = lo {
                let block = ((w.val >> 2) & 7) as i32;
                let fnum = (((w.val & 3) << 8) | lo) as i32;
                out.push((
                    w.sample,
                    py_int(py_round(opl::opl_pitch_units(block, fnum))) as i32,
                ));
            }
        }
    }
    out
}

/// Spans where one OPL-family voice holds its key bit.
pub fn opl_keyed_intervals(vgm: &VgmFile, ch: i32) -> Vec<(i64, i64)> {
    let (kind, cid, col) = match _opl_channel(vgm, ch) {
        Some(mapped) => mapped,
        None => return Vec::new(),
    };
    let st = OPLState::new(kind, false);
    let cm = st.chan_map[col as usize];
    let port = if st.opl3 { i32::from((cm >> 8) & 1) } else { 0 };
    let target = (port, 0xB0 + i32::from(cm & 0xFF));
    let mut out: Vec<(i64, i64)> = Vec::new();
    let mut on = false;
    let mut start = 0i64;
    for w in &vgm.writes {
        if w.chip != kind || w.chip_id != cid {
            continue;
        }
        let addr = (
            if st.opl3 {
                ((w.reg >> 8) & 1) as i32
            } else {
                0
            },
            (w.reg & 0xFF) as i32,
        );
        if addr != target {
            continue;
        }
        let now = (w.val & 0x20) != 0;
        if now && !on {
            start = w.sample;
            on = true;
        } else if !now && on {
            out.push((start, w.sample));
            on = false;
        }
    }
    if on {
        let end = if vgm.total_samples != 0 {
            vgm.total_samples
        } else {
            vgm.writes
                .iter()
                .map(|w| w.sample)
                .max()
                .unwrap_or(start)
                + 1
        };
        out.push((start, end));
    }
    out
}

fn _ym2610_chip_ids(vgm: &VgmFile) -> Vec<u8> {
    let mut ids: Vec<u8> = vgm
        .writes
        .iter()
        .filter(|w| w.chip == "ym2610" || w.chip == "ym2610_ssg")
        .map(|w| w.chip_id)
        .collect();
    ids.sort();
    ids.dedup();
    ids
}

fn _ym2610_fm_count(vgm: &VgmFile) -> i32 {
    if vgm.ym2610_b { 6 } else { 4 }
}

/// (chip id, kind, voice) for one YM2610 module channel.
fn _ym2610_channel_map(vgm: &VgmFile, ch: i32) -> Option<(u8, &'static str, i32)> {
    let ids = _ym2610_chip_ids(vgm);
    let fm_count = _ym2610_fm_count(vgm);
    let chunk = fm_count + 10;
    let (chip, voice) = divmod(ch as i64, chunk as i64);
    let cid = ids[py_index(ids.len(), chip)?];
    let voice = voice as i32;
    if voice < fm_count {
        return Some((cid, "fm", voice));
    }
    if voice < fm_count + 3 {
        return Some((cid, "ssg", voice - fm_count));
    }
    if voice < fm_count + 9 {
        return Some((cid, "adpcm_a", voice - (fm_count + 3)));
    }
    if voice == fm_count + 9 {
        return Some((cid, "adpcm_b", 0));
    }
    None
}

/// (sample, pitch units) from one SSG voice's tone period.
fn _ssg_stream(
    vgm: &VgmFile,
    ssg_chip: &str,
    cid: u8,
    voice: i32,
    clock: u32,
    divider: f64,
) -> Vec<(i64, i32)> {
    let mut out: Vec<(i64, i32)> = Vec::new();
    let mut lo: Option<u32> = None;
    let mut hi: Option<u32> = None;
    let min_period = ay_min_period(clock, divider);
    for w in &vgm.writes {
        if w.chip != ssg_chip || w.chip_id != cid {
            continue;
        }
        if w.reg == (voice * 2) as u32 {
            lo = Some(w.val);
        } else if w.reg == (voice * 2 + 1) as u32 {
            hi = Some(w.val);
        } else {
            continue;
        }
        if let (Some(lo), Some(hi)) = (lo, hi) {
            let period = ((lo & 0xFF) | ((hi & 0x0F) << 8)) as i32;
            if period >= min_period {
                out.push((w.sample, py_round(opn_ssg_units(period)) as i32));
            }
        }
    }
    out
}

/// Spans where one SSG voice has its tone bit on and a level to say so.
fn _ssg_keyed_intervals(
    vgm: &VgmFile,
    ssg_chip: &str,
    cid: u8,
    voice: i32,
    clock: u32,
    divider: f64,
) -> Vec<(i64, i64)> {
    let mut out: Vec<(i64, i64)> = Vec::new();
    let mut mixer = 0i32;
    let mut vol = [0i32; 3];
    let mut lo = 0i32;
    let mut hi = 0i32;
    let mut on = false;
    let mut start = 0i64;
    let min_period = ay_min_period(clock, divider);
    for w in &vgm.writes {
        if w.chip != ssg_chip || w.chip_id != cid {
            continue;
        }
        if w.reg == 7 {
            mixer = w.val as i32;
        } else if (8..=10).contains(&w.reg) {
            vol[(w.reg - 8) as usize] = w.val as i32;
        } else if w.reg == (voice * 2) as u32 {
            lo = w.val as i32;
        } else if w.reg == (voice * 2 + 1) as u32 {
            hi = w.val as i32;
        } else {
            continue;
        }
        let level = vol[voice as usize];
        let period = (lo & 0xFF) | ((hi & 0x0F) << 8);
        let now = (mixer & (1 << voice)) == 0
            && (mixer & (1 << (voice + 3))) != 0
            && ((level & 0x10) != 0 || (level & 0x0F) != 0)
            && period >= min_period;
        if now && !on {
            start = w.sample;
            on = true;
        } else if !now && on {
            out.push((start, w.sample));
            on = false;
        }
    }
    if on {
        let end = if vgm.total_samples != 0 {
            vgm.total_samples
        } else {
            vgm.writes
                .iter()
                .map(|w| w.sample)
                .max()
                .unwrap_or(start)
                + 1
        };
        out.push((start, end));
    }
    out
}

fn _ym2610_ssg_stream(vgm: &VgmFile, cid: u8, ssg: i32) -> Vec<(i64, i32)> {
    let clock = if vgm.ym2610_clock == 0 {
        YM2610_DEFAULT_CLOCK
    } else {
        vgm.ym2610_clock
    };
    _ssg_stream(vgm, "ym2610_ssg", cid, ssg, clock, 32.0)
}

/// Pitch for a WSG counter value (`0xFFF - register`); larger is higher.
///
/// The voice sounds at `clock / (32 * (period + 1))`, so the clock and divider
/// cancel between two logs (source and Furnace export).
pub fn k005289_units(period: i32) -> f64 {
    -768.0 * (period.max(0) as f64 + 1.0).log2()
}

/// (sample, pitch units) after each TG write on one WSG voice.
pub fn k005289_pitch_stream(vgm: &VgmFile, cid: u8, voice: i32) -> Vec<(i64, i32)> {
    let mut out: Vec<(i64, i32)> = Vec::new();
    let mut latch = 0i32;
    let mut have = false;
    for w in &vgm.writes {
        if w.chip != "k005289" || w.chip_id != cid {
            continue;
        }
        let off = (w.reg & 0x0F) as i32;
        if off == 2 + voice {
            latch = (w.val & 0xFFF) as i32;
            have = true;
        } else if off == 4 + voice && have {
            out.push((
                w.sample,
                py_round(k005289_units(0xFFF - latch)) as i32,
            ));
        }
    }
    out
}

/// Spans where one WSG voice has a non-zero control volume.
///
/// The chip has no key-on bit; the 4-bit volume is the only gate.
pub fn k005289_keyed_intervals(vgm: &VgmFile, cid: u8, voice: i32) -> Vec<(i64, i64)> {
    let mut out: Vec<(i64, i64)> = Vec::new();
    let mut on = false;
    let mut start = 0i64;
    for w in &vgm.writes {
        if w.chip != "k005289" || w.chip_id != cid {
            continue;
        }
        if (w.reg & 0x0F) as i32 != voice {
            continue;
        }
        let now = (w.val & 0x0F) > 0;
        if now && !on {
            start = w.sample;
            on = true;
        } else if !now && on {
            out.push((start, w.sample));
            on = false;
        }
    }
    if on {
        let end = if vgm.total_samples != 0 {
            vgm.total_samples
        } else {
            vgm.writes
                .iter()
                .map(|w| w.sample)
                .max()
                .unwrap_or(start)
                + 1
        };
        out.push((start, end));
    }
    out
}

/// (family, chip id, voices) in module order for a Bubble System module.
/// The writer puts the AY systems first and the WSG last. Only that shape is
/// mapped here; a module with another tone family keeps the existing
/// single-family comparison path.
type ToneLayout = Vec<(&'static str, u8, i32)>;

fn _tone_layout(vgm: &VgmFile) -> Option<ToneLayout> {
    let mut wsg: Vec<u8> = vgm
        .writes
        .iter()
        .filter(|w| w.chip == "k005289")
        .map(|w| w.chip_id)
        .collect();
    wsg.sort();
    wsg.dedup();
    if wsg.is_empty() {
        return None;
    }
    let other_tones = [
        "k051649",
        "nes",
        "huc6280",
        "gb",
        "vsu",
        "saa1099",
        "pokey",
        "mikey",
        "x1_010",
        "fds",
        "wonderswan",
        "qsound",
    ];
    if vgm
        .writes
        .iter()
        .any(|w| other_tones.contains(&w.chip))
    {
        return None;
    }
    let mut ay: Vec<u8> = vgm
        .writes
        .iter()
        .filter(|w| w.chip == "ay8910")
        .map(|w| w.chip_id)
        .collect();
    ay.sort();
    ay.dedup();
    if !ay.is_empty() && vgm.ay8910_dual {
        ay.push(0);
        ay.push(1);
        ay.sort();
        ay.dedup();
    }
    let mut layout: ToneLayout = Vec::new();
    for cid in ay {
        layout.push(("ay8910", cid, 3));
    }
    for cid in wsg {
        layout.push(("k005289", cid, 2));
    }
    Some(layout)
}

/// Pitch stream for one module channel of a tone-only module.
pub fn tone_pitch_stream(vgm: &VgmFile, ch: i32, layout: &[(&str, u8, i32)]) -> Vec<(i64, i32)> {
    let mut ch = ch;
    for (family, cid, voices) in layout {
        if ch < *voices {
            if *family == "ay8910" {
                let clock = if vgm.ay8910_clock == 0 {
                    3_579_545
                } else {
                    vgm.ay8910_clock
                };
                return _ssg_stream(vgm, "ay8910", *cid, ch, clock, 8.0);
            }
            return k005289_pitch_stream(vgm, *cid, ch);
        }
        ch -= *voices;
    }
    Vec::new()
}

/// Sounding spans for one module channel of a tone-only module.
pub fn tone_keyed_intervals(
    vgm: &VgmFile,
    ch: i32,
    layout: &[(&str, u8, i32)],
) -> Vec<(i64, i64)> {
    let mut ch = ch;
    for (family, cid, voices) in layout {
        if ch < *voices {
            if *family == "ay8910" {
                let clock = if vgm.ay8910_clock == 0 {
                    3_579_545
                } else {
                    vgm.ay8910_clock
                };
                return _ssg_keyed_intervals(vgm, "ay8910", *cid, ch, clock, 8.0);
            }
            return k005289_keyed_intervals(vgm, *cid, ch);
        }
        ch -= *voices;
    }
    Vec::new()
}

#[derive(Clone, Debug)]
struct MacroPitch {
    values: Vec<i32>,
    speed: i32,
    delay: i32,
    loop_at: u8,
    mode: u8,
}

/// (values, speed, delay, loop, mode) of an INS2 block's pitch macro.
fn _macro_pitch_values(block: &[u8]) -> Option<MacroPitch> {
    let pos = block.windows(2).position(|w| w == b"MA")?;
    let byte = |i: usize| block.get(i).copied().unwrap_or(0);
    let size = u16::from_le_bytes([byte(pos + 2), byte(pos + 3)]) as usize;
    let mut cursor = pos + 6; // skip the macro header length field
    let end = pos + 4 + size;
    while cursor + 8 <= end && byte(cursor) != 0xFF {
        let code = byte(cursor);
        let length = byte(cursor + 1);
        let loop_at = byte(cursor + 2);
        let _rel = byte(cursor + 3);
        let mode = byte(cursor + 4);
        let word = byte(cursor + 5);
        let delay = byte(cursor + 6);
        let speed = byte(cursor + 7);
        cursor += 8;
        let width = word >> 6;
        let mut values: Vec<i32> = Vec::new();
        for _ in 0..length {
            if width == 2 {
                let raw = u16::from_le_bytes([byte(cursor), byte(cursor + 1)]);
                values.push(raw as i16 as i32);
                cursor += 2;
            } else if width == 3 {
                let raw = u32::from_le_bytes([
                    byte(cursor),
                    byte(cursor + 1),
                    byte(cursor + 2),
                    byte(cursor + 3),
                ]);
                values.push(raw as i32);
                cursor += 4;
            } else {
                values.push(byte(cursor) as i32);
                cursor += 1;
            }
        }
        if code == 4 {
            return Some(MacroPitch {
                values,
                speed: i32::from(speed).max(1),
                delay: i32::from(delay),
                loop_at,
                mode,
            });
        }
    }
    None
}

/// The pitch-macro value at one tick, following Furnace's delay/loop.
fn _macro_at(macro_pitch: Option<&MacroPitch>, tick: i64) -> i32 {
    let Some(m) = macro_pitch else { return 0 };
    if m.values.is_empty() {
        return 0;
    }
    let mut seq: Vec<i32> = vec![m.values[0]; (m.delay + 1).max(0) as usize];
    for value in &m.values[1..] {
        for _ in 0..m.speed {
            seq.push(*value);
        }
    }
    if m.loop_at != 255 && (m.loop_at as usize) < m.values.len() {
        let mut tail: Vec<i32> = Vec::new();
        for value in &m.values[m.loop_at as usize..] {
            for _ in 0..m.speed {
                tail.push(*value);
            }
        }
        while !tail.is_empty() && seq.len() as i64 <= tick {
            seq.extend_from_slice(&tail);
        }
    }
    let value = if (tick as usize) < seq.len() {
        seq[tick as usize]
    } else {
        *seq.last().unwrap()
    };
    if m.mode != 0 {
        // Relative macro: each step adds to the previous offset.
        let last = (seq.len() as i64 - 1).max(0);
        let hi = tick.min(last).max(0) as usize;
        return seq[..=hi].iter().sum();
    }
    value
}

/// (pitch stream, keyed intervals) for one WSG channel of a .fur.
///
/// Furnace's VGM export has no Bubble System WSG case, so the module's own
/// patterns are the independent record of what the .fur asks the chip to
/// play. This walks them with the same pitch table the platform uses (linear
/// pitch, 32x divider) and folds in each instrument's pitch macro.
pub fn _fur_wsg_stream(
    fur: &Path,
    ch: i32,
    clock: u32,
) -> crate::Result<(Vec<(i64, i32)>, Vec<(i64, i64)>)> {
    let data = crate::furio::load_fur_bytes(fur)?;
    let mod_ = crate::furio::parse_fur(&data)?;
    let mut start = 0i32;
    let mut voice: Option<i32> = None;
    for chip in &mod_.chips {
        let span = crate::furio::chip_channels(*chip) as i32;
        if start <= ch && ch < start + span {
            if *chip != crate::furwrite::CHIP_K005289 {
                return Ok((Vec::new(), Vec::new()));
            }
            voice = Some(ch - start);
            break;
        }
        start += span;
    }
    let Some(_voice) = voice else {
        return Ok((Vec::new(), Vec::new()));
    };

    let mut macros: Vec<Option<MacroPitch>> = Vec::new();
    for block in crate::furio::iter_blocks(&data)? {
        if block.magic == *b"INS2" {
            macros.push(_macro_pitch_values(block.payload));
        }
    }
    let hz = if mod_.hz != 0.0 { mod_.hz } else { 60.0 };
    let tick = 44100.0 / hz.max(1e-6);
    let row_samples = i64::max(1, py_int(py_round(tick * i32::max(1, mod_.speed) as f64)));
    let table = crate::pitch::PeriodPitchTable::new(
        440.0,
        if clock != 0 {
            f64::from(clock)
        } else {
            f64::from(K005289_DEFAULT_CLOCK)
        },
        32.0,
        0xFFF,
    );

    let mut notes: Vec<(i64, i64, i32, i32, i32)> = Vec::new();
    let mut intervals: Vec<(i64, i64)> = Vec::new();
    let mut open_note: Option<(i64, i32, i32, i32)> = None;
    let mut cur_ins = 0i32;
    let orders_len = mod_.orders.len();
    let order_0: Vec<u16> = if (ch as usize) < orders_len {
        mod_.orders[ch as usize].clone()
    } else {
        Vec::new()
    };
    let orders = &order_0;
    let mut order: i64 = 0;
    let mut row: i64 = 0;
    let mut time_row: i64 = 0;
    let mut steps = 0i32;
    // Dxx breaks to the next order and Bxx is the loop jump at the end; a
    // hard cap keeps a malformed module from looping forever here.
    while order >= 0 && (order as usize) < orders.len() && steps < 8192 {
        let pidx = orders[order as usize];
        let empty: Vec<crate::furio::FurRow> = Vec::new();
        let rows = mod_
            .patterns
            .get(&(ch as u16, pidx))
            .unwrap_or(&empty);
        if row < 0 || row as usize >= rows.len() {
            order += 1;
            row = 0;
            continue;
        }
        let cell = &rows[row as usize];
        let row_sample = time_row * row_samples;
        if cell.ins >= 0 {
            cur_ins = cell.ins;
        }
        if cell.note == 180 {
            if let Some(open) = open_note {
                intervals.push((open.0, row_sample));
                notes.push((open.0, row_sample, open.1, open.2, open.3));
                open_note = None;
            }
        } else if (0..180).contains(&cell.note) {
            if let Some(open) = open_note {
                intervals.push((open.0, row_sample));
                notes.push((open.0, row_sample, open.1, open.2, open.3));
            }
            let delay = cell
                .fx
                .iter()
                .find(|(c, _v)| *c == 0xED)
                .map(|(_c, v)| i32::from(*v))
                .unwrap_or(0);
            let e5 = cell
                .fx
                .iter()
                .find(|(c, _v)| *c == 0xE5)
                .map(|(_c, v)| i32::from(*v))
                .unwrap_or(0x80);
            open_note = Some((
                py_int(py_round(row_sample as f64 + f64::from(delay) * tick)),
                cell.note,
                cur_ins,
                e5,
            ));
        }
        let brk = cell.fx.iter().find(|(c, _v)| *c == 0x0D).map(|(_, v)| *v);
        let jump = cell.fx.iter().find(|(c, _v)| *c == 0x0B).map(|(_, v)| *v);
        time_row += 1;
        steps += 1;
        if let Some(brk) = brk {
            order += 1;
            row = i64::from(brk);
        } else if jump.is_some() {
            // The loop jump sits on the last content row; the first pass is
            // complete here.
            break;
        } else {
            row += 1;
            if row as usize >= rows.len() {
                order += 1;
                row = 0;
            }
        }
    }
    let last_sample = time_row * row_samples;
    if let Some(open) = open_note {
        intervals.push((open.0, last_sample));
        notes.push((open.0, last_sample, open.1, open.2, open.3));
    }

    let mut stream: Vec<(i64, i32)> = Vec::new();
    for (start_sample, end_sample, note, ins, e5) in notes {
        let macro_p = if ins >= 0 && (ins as usize) < macros.len() {
            macros[ins as usize].as_ref()
        } else {
            None
        };
        let end_sample = i64::max(start_sample + 1, end_sample);
        let ticks = i64::max(
            1,
            py_int(py_round((end_sample - start_sample) as f64 / tick)),
        );
        let mut last: Option<i32> = None;
        for i in 0..=ticks {
            let sample = py_int(py_round(start_sample as f64 + i as f64 * tick));
            let fine = (e5 - 0x80) + _macro_at(macro_p, i);
            let value = i32::max(1, table.value(note, fine));
            let units = py_round(-768.0 * (value as f64).log2()) as i32;
            if last != Some(units) {
                stream.push((sample, units));
                last = Some(units);
            }
        }
    }
    Ok((stream, intervals))
}

/// (sample, pitch units) for one YM2610 module channel.
///
/// FM follows the latched f-num/block. The SSG follows its tone period. ADPCM
/// channels have no comparable pitch and return nothing.
pub fn ym2610_pitch_stream(vgm: &VgmFile, ch: i32) -> Vec<(i64, f64)> {
    let (cid, kind, voice) = match _ym2610_channel_map(vgm, ch) {
        Some(mapped) => mapped,
        None => return Vec::new(),
    };
    if kind == "ssg" {
        return _ym2610_ssg_stream(vgm, cid, voice)
            .into_iter()
            .map(|(s, v)| (s, f64::from(v)))
            .collect();
    }
    if kind != "fm" {
        return Vec::new();
    }
    let fm_count = _ym2610_fm_count(vgm);
    let (bank, off) = if fm_count == 6 {
        CHAN_OFF_6[voice as usize]
    } else {
        CHAN_OFF_4[voice as usize]
    };
    let base: u32 = if bank != 0 { 0x100 } else { 0 };
    let lo_reg = base | (0xA0 + off as u32);
    let hi_reg = base | (0xA4 + off as u32);
    let mut out: Vec<(i64, f64)> = Vec::new();
    let mut lo: Option<u32> = None;
    let mut hi: Option<u32> = None;
    for w in &vgm.writes {
        if w.chip != "ym2610" || w.chip_id != cid {
            continue;
        }
        if w.reg == lo_reg {
            lo = Some(w.val);
            if let Some(hi) = hi {
                out.push((
                    w.sample,
                    opn_pitch_units(
                        ((hi >> 3) & 7) as i32,
                        (lo.unwrap() | ((hi & 7) << 8)) as i32,
                    ),
                ));
            }
        } else if w.reg == hi_reg {
            hi = Some(w.val);
            if let Some(lo) = lo {
                out.push((
                    w.sample,
                    opn_pitch_units(((hi.unwrap() >> 3) & 7) as i32, (lo | ((hi.unwrap() & 7) << 8)) as i32),
                ));
            }
        }
    }
    out
}

/// Samples where one YM2610 module voice is sounding.
///
/// FM uses its 0x28 channel field; the SSG uses the mixer tone bit and level
/// register. ADPCM channels are not pitch-compared.
pub fn ym2610_keyed_intervals(vgm: &VgmFile, ch: i32) -> Vec<(i64, i64)> {
    let (cid, kind, voice) = match _ym2610_channel_map(vgm, ch) {
        Some(mapped) => mapped,
        None => return Vec::new(),
    };
    let mut out: Vec<(i64, i64)> = Vec::new();
    let mut on = false;
    let mut start = 0i64;
    if kind == "ssg" {
        let clock = if vgm.ym2610_clock == 0 {
            YM2610_DEFAULT_CLOCK
        } else {
            vgm.ym2610_clock
        };
        return _ssg_keyed_intervals(vgm, "ym2610_ssg", cid, voice, clock, 32.0);
    } else if kind == "fm" {
        let fm_count = _ym2610_fm_count(vgm);
        let kon: &[(i32, i32)] = if fm_count == 6 { &KON_6 } else { &KON_4 };
        for w in &vgm.writes {
            if w.chip != "ym2610"
                || w.chip_id != cid
                || (w.reg & 0x100) != 0
                || (w.reg & 0xFF) != 0x28
            {
                continue;
            }
            let mapped = kon
                .iter()
                .find(|(k, _v)| *k == (w.val & 7) as i32)
                .map(|(_k, v)| *v);
            if mapped != Some(voice) {
                continue;
            }
            let now = ((w.val >> 4) & 0x0F) != 0;
            if now && !on {
                start = w.sample;
                on = true;
            } else if !now && on {
                out.push((start, w.sample));
                on = false;
            }
        }
    } else {
        return Vec::new();
    }

    if on {
        let end = if vgm.total_samples != 0 {
            vgm.total_samples
        } else {
            vgm.writes
                .iter()
                .map(|w| w.sample)
                .max()
                .unwrap_or(start)
                + 1
        };
        out.push((start, end));
    }
    out
}

/// (chip id, kind, walk channel) for one YM2608 module channel.
///
/// `kind` is "fm" for any FM voice or channel-3 operator column - the walk
/// channel then indexes a run of six per chip, or nine when channel 3 is
/// split, which is what [`ym2612_pitch_stream`] expects - "ssg" for one of the
/// three tone voices, and "rhythm" or "delta_t" for the sample columns.
fn _ym2608_channel_map(vgm: &VgmFile, ch: i32) -> Option<(u8, &'static str, i32)> {
    let ids = _ym2612_chip_ids(vgm, "ym2608");
    let fm_count = if _ym2612_ext(vgm, "ym2608") { 9 } else { 6 };
    let chunk = fm_count + 3 + 6 + 1;
    let (idx, voice) = divmod(ch as i64, chunk as i64);
    let cid = ids[py_index(ids.len(), idx)?];
    let voice = voice as i32;
    if voice < fm_count {
        return Some((cid, "fm", idx as i32 * fm_count + voice));
    }
    if voice < fm_count + 3 {
        return Some((cid, "ssg", voice - fm_count));
    }
    if voice < fm_count + 9 {
        return Some((cid, "rhythm", voice - (fm_count + 3)));
    }
    Some((cid, "delta_t", 0))
}

/// (sample, pitch units) for one YM2608 module channel.
///
/// The OPNA's FM voices - split channel 3 included - are the same two-port OPN
/// registers, so the stream comes from that walk with the chip swapped. Its
/// SSG voices follow tone periods; rhythm and delta-T have no pitch to compare,
/// exactly as on the YM2610.
pub fn ym2608_pitch_stream(vgm: &VgmFile, ch: i32) -> Vec<(i64, f64)> {
    let (cid, kind, voice) = match _ym2608_channel_map(vgm, ch) {
        Some(mapped) => mapped,
        None => return Vec::new(),
    };
    if kind == "fm" {
        return ym2612_pitch_stream(vgm, voice, "ym2608");
    }
    if kind == "ssg" {
        let clock = if vgm.ym2608_clock == 0 {
            YM2608_DEFAULT_CLOCK
        } else {
            vgm.ym2608_clock
        };
        return _ssg_stream(
            vgm,
            "ym2608_ssg",
            cid,
            voice,
            clock,
            YM2608_SSG_DIVIDER as f64,
        )
        .into_iter()
        .map(|(s, v)| (s, f64::from(v)))
        .collect();
    }
    Vec::new()
}

/// Samples where one YM2608 module voice is sounding.
pub fn ym2608_keyed_intervals(vgm: &VgmFile, ch: i32) -> Vec<(i64, i64)> {
    let (cid, kind, voice) = match _ym2608_channel_map(vgm, ch) {
        Some(mapped) => mapped,
        None => return Vec::new(),
    };
    if kind == "fm" {
        return ym2612_keyed_intervals(vgm, voice, "ym2608", false);
    }
    if kind == "ssg" {
        let clock = if vgm.ym2608_clock == 0 {
            YM2608_DEFAULT_CLOCK
        } else {
            vgm.ym2608_clock
        };
        return _ssg_keyed_intervals(
            vgm,
            "ym2608_ssg",
            cid,
            voice,
            clock,
            YM2608_SSG_DIVIDER as f64,
        );
    }
    Vec::new()
}

/// [(chip_id, column, sample, register image)] at each key-on, folded per
/// sample.
///
/// A driver that re-attacks quickly writes the new voice's operator levels and
/// its key bit in one sample, interleaved with the previous note's release. The
/// chip applies those in stream order, and a tracker row holds one value. The
/// state that matters - and the state `analyze` snapshots when it opens a note -
/// is the register set once that sample is finished. This walk gives exactly
/// that: every write of a sample is applied, then its key edges emit an image.
/// Comparing two streams this way says whether a converted module hands the
/// chip the same registers the rip did.
///
/// Bank B of an OPL3 keeps its number in bit 8, and the keys it reports are
/// columns of Furnace's own table. A four-operator voice and the kit's shared
/// frequency registers are named as the engine names them.
pub fn opl_attacks(
    vgm: &VgmFile,
    kind: Option<&str>,
    drums: Option<bool>,
) -> Vec<(i32, i32, i64, IndexMap<(i32, i32), u32>)> {
    let kind = match kind {
        Some(kind) => kind,
        None => match _opl_kind(vgm) {
            Some(kind) => kind,
            None => return Vec::new(),
        },
    };
    let drums = drums.unwrap_or_else(|| {
        vgm.writes
            .iter()
            .any(|w| w.chip == kind && (w.reg & 0xFF) == 0xBD && (w.val & 0x20) != 0)
    });
    let st = OPLState::new(kind, drums);
    let mut chan_of: IndexMap<(i32, i32), Vec<i32>> = IndexMap::new();
    for (fch, cm) in st.chan_map.iter().enumerate() {
        let cm = *cm;
        let port = if st.opl3 {
            i32::from((cm >> 8) & 1)
        } else {
            0
        };
        chan_of
            .entry((port, i32::from(cm & 0xFF)))
            .or_default()
            .push(fch as i32);
    }
    let mut per: IndexMap<u8, Vec<&ChipWrite>> = IndexMap::new();
    for w in &vgm.writes {
        if w.chip == kind {
            per.entry(w.chip_id).or_default().push(w);
        }
    }
    let mut out: Vec<(i32, i32, i64, IndexMap<(i32, i32), u32>)> = Vec::new();
    for (cid, ws) in per {
        let mut image: IndexMap<(i32, i32), u32> = IndexMap::new();
        let mut keyed: IndexMap<(i32, i32), bool> = IndexMap::new();
        let mut trig = 0u32;
        let mut i = 0usize;
        let n = ws.len();
        while i < n {
            let sample = ws[i].sample;
            let mut j = i + 1;
            while j < n && ws[j].sample == sample {
                j += 1;
            }
            let mut edges: Vec<(i32, i32)> = Vec::new();
            for w in &ws[i..j] {
                let port = if st.opl3 {
                    ((w.reg >> 8) & 1) as i32
                } else {
                    0
                };
                let r = (w.reg & 0xFF) as i32;
                image.insert((port, r), w.val);
                if (0xB0..=0xB8).contains(&r) {
                    let on = (w.val & 0x20) != 0;
                    if on && !keyed.get(&(port, r)).copied().unwrap_or(false) {
                        if let Some(fchs) = chan_of.get(&(port, r - 0xB0)) {
                            for fch in fchs {
                                if !st.drums || *fch < st.melodic {
                                    edges.push((*fch, -1));
                                }
                            }
                        }
                    }
                    keyed.insert((port, r), on);
                } else if r == 0xBD && !st.opl3 {
                    let bits = if (w.val & 0x20) != 0 {
                        w.val & 0x1F
                    } else {
                        0
                    };
                    for k in 0..5 {
                        let bit = 1u32 << (4 - k);
                        if (bits & bit) != 0 && (trig & bit) == 0 {
                            edges.push((st.melodic + k, bit as i32));
                        }
                    }
                    trig = bits;
                }
            }
            for (fch, _bit) in edges {
                out.push((i32::from(cid), fch, sample, image.clone()));
            }
            i = j;
        }
    }
    out
}

/// Operator addresses of the voice a column owns in one register image.
///
/// Four when the sample's `0x104` pairs the column with its neighbour: such a
/// voice takes two operators from each hardware channel of the pair.
pub fn opl_voice_addrs(
    kind: &str,
    drums: bool,
    col: i32,
    image: &IndexMap<(i32, i32), u32>,
) -> Vec<u16> {
    let st = OPLState::new(kind, drums);
    let four = st.opl3
        && col < 12
        && (col & 1) == 0
        && ((image.get(&(1, 0x04)).copied().unwrap_or(0) >> (col / 2)) & 1) != 0;
    let mut out: Vec<u16> = Vec::new();
    for row in 0..(if four { 4 } else { 2 }) {
        let slots = st.slots[row];
        if (col as usize) < slots.len() && slots[col as usize] != OPL_N {
            out.push(SLOT_ADDR[slots[col as usize] as usize]);
        }
    }
    out
}

fn _stream_for(
    selector: u8,
    vgm: &VgmFile,
    ch: i32,
    tone_layout: Option<&ToneLayout>,
) -> Vec<(i64, f64)> {
    let as_f64 = |stream: Vec<(i64, i32)>| -> Vec<(i64, f64)> {
        stream.into_iter().map(|(s, v)| (s, f64::from(v))).collect()
    };
    match selector {
        1 => ym2610_pitch_stream(vgm, ch),
        2 => ym2608_pitch_stream(vgm, ch),
        3 => ym2612_pitch_stream(vgm, ch, "ym2612"),
        4 => opn_pitch_stream(vgm, ch),
        5 => as_f64(opl_pitch_stream(vgm, ch)),
        6 => as_f64(tone_pitch_stream(vgm, ch, tone_layout.unwrap())),
        _ => as_f64(kc_kf_stream(vgm, ch)),
    }
}

/// Stretches where the two pitch streams differ by more than `threshold_st`.
///
/// The comparison walks every write time of the two streams in order and
/// samples both states. A short mismatch inside a glide is ignored while a
/// held wrong pitch is reported with its full span. For YM2203 chips the
/// driver keeps writing f-num during the gaps between notes, and the
/// comparison therefore only counts samples where the source has a keyed
/// operator. An OPL-family voice additionally trims its key, because a
/// row-tick note and the sample the driver used do not coincide.
pub fn pitch_runs(
    src_vgm: &VgmFile,
    other_vgm: &VgmFile,
    ch: i32,
    threshold_st: f64,
    min_run_ms: f64,
) -> Vec<PitchRun> {
    let opnb = src_vgm
        .writes
        .iter()
        .any(|w| w.chip == "ym2610" || w.chip == "ym2610_ssg");
    let opna = src_vgm
        .writes
        .iter()
        .any(|w| w.chip == "ym2608" || w.chip == "ym2608_ssg");
    let opn = has_chip(src_vgm, "ym2203");
    let opn2 = has_chip(src_vgm, "ym2612");
    let opl = _opl_kind(src_vgm).is_some();
    let tone_layout = _tone_layout(src_vgm);
    let selector = if opnb {
        1
    } else if opna {
        2
    } else if opn2 {
        3
    } else if opn {
        4
    } else if opl {
        5
    } else if tone_layout.is_some() {
        6
    } else {
        0
    };
    let a = _stream_for(selector, src_vgm, ch, tone_layout.as_ref());
    let b = _stream_for(selector, other_vgm, ch, tone_layout.as_ref());
    // Loop finding cuts the source's post-loop tail from the module. The
    // export's declared length is the content it really plays; comparing past
    // it would score the module's loop restart against the source's tail.
    let mut limit: Option<i64> = None;
    if other_vgm.total_samples != 0 {
        limit = Some(other_vgm.total_samples);
        if src_vgm.total_samples != 0 {
            limit = Some(limit.unwrap().min(src_vgm.total_samples));
        }
    }
    let mut intervals: Option<Vec<(i64, i64)>> = None;
    if opnb || opna {
        intervals = Some(if opnb {
            ym2610_keyed_intervals(src_vgm, ch)
        } else {
            ym2608_keyed_intervals(src_vgm, ch)
        });
    } else if opn2 {
        intervals = Some(ym2612_keyed_intervals(src_vgm, ch, "ym2612", true));
    } else if opn {
        intervals = Some(opn_keyed_intervals(src_vgm, ch));
    } else if opl {
        // The OPL keeps the last f-num in its register after a release, and a
        // rhythm column shares one register with another drum. Only the spans
        // where this voice itself held the key are compared. A module places a
        // note on a row tick while the driver wrote it at an exact sample, and
        // the two agree only a few milliseconds apart at each boundary: comparing
        // there would measure that drift as wrong pitch. Trim the ends, and a
        // note shorter than the trim drops out - grace notes carry no evidence.
        let trim = py_int(0.035 * VGM_RATE as f64);
        intervals = Some(
            opl_keyed_intervals(src_vgm, ch)
                .into_iter()
                .filter(|(s, e)| e - s > 2 * trim)
                .map(|(s, e)| (s + trim, e - trim))
                .collect(),
        );
    } else if let Some(layout) = tone_layout.as_ref() {
        // AY uses its mixer/level gate; the WSG uses its control volume.
        intervals = Some(tone_keyed_intervals(src_vgm, ch, layout));
    }
    _runs_between(
        &a,
        &b,
        intervals.as_deref(),
        limit,
        threshold_st,
        min_run_ms,
    )
}

/// Compare two (sample, pitch units) streams over keyed spans.
///
/// Every write time of either stream and every interval edge is sampled. A
/// short mismatch inside a glide is ignored while a held wrong pitch is
/// reported with its full span.
pub fn _runs_between(
    a: &[(i64, f64)],
    b: &[(i64, f64)],
    intervals: Option<&[(i64, i64)]>,
    limit: Option<i64>,
    threshold_st: f64,
    min_run_ms: f64,
) -> Vec<PitchRun> {
    let mut times: BTreeSet<i64> = BTreeSet::new();
    for (s, _v) in a {
        times.insert(*s);
    }
    for (s, _v) in b {
        times.insert(*s);
    }
    if let Some(intervals) = intervals {
        for (s, _e) in intervals {
            times.insert(*s);
        }
        for (_s, e) in intervals {
            times.insert(*e);
        }
    }
    let times: Vec<i64> = match limit {
        Some(limit) => times.into_iter().filter(|t| *t <= limit).collect(),
        None => times.into_iter().collect(),
    };
    let threshold = threshold_st * 64.0;
    let min_run = py_int(min_run_ms * 44.1);

    let keyed_at = |t: i64| -> bool {
        match intervals {
            None => true,
            Some(intervals) => intervals.iter().any(|(s, e)| *s <= t && t < *e),
        }
    };

    let mut runs: Vec<PitchRun> = Vec::new();
    let mut cur: Option<PitchRun> = None;
    let mut ia = 0usize;
    let mut ib = 0usize;
    for t in times {
        if !keyed_at(t) {
            if let Some(mut c) = cur.take() {
                c.t1 = t;
                if c.t1 - c.t0 >= min_run {
                    runs.push(c);
                }
            }
            continue;
        }
        while ia < a.len() && a[ia].0 <= t {
            ia += 1;
        }
        while ib < b.len() && b[ib].0 <= t {
            ib += 1;
        }
        if ia == 0 || ib == 0 {
            continue;
        }
        let delta = b[ib - 1].1 - a[ia - 1].1;
        if delta.abs() > threshold {
            match cur.as_mut() {
                None => {
                    cur = Some(PitchRun {
                        t0: t,
                        t1: t,
                        worst_st: delta / 64.0,
                    })
                }
                Some(c) => {
                    c.t1 = t;
                    if delta.abs() > c.worst_st.abs() * 64.0 {
                        c.worst_st = delta / 64.0;
                    }
                }
            }
        } else if let Some(mut c) = cur.take() {
            // The state only moves at write times, and every write time is in
            // `times`. The first non-divergent sample is the moment the pitch
            // came back, and the run ends there.
            c.t1 = t;
            if c.t1 - c.t0 >= min_run {
                runs.push(c);
            }
        }
    }
    if let Some(c) = cur {
        if c.t1 - c.t0 >= min_run {
            runs.push(c);
        }
    }
    runs
}

/// (sample, register, value) rows for one chip channel.
///
/// Registers below 0x20 are chip-wide (the LFO block) and are not offset by
/// the channel; the rest are matched per channel.
pub fn register_writes(
    vgm: &VgmFile,
    chip: &str,
    ch: i32,
    regs: &[i32],
    t0: i64,
    t1: Option<i64>,
    only_changes: bool,
) -> Vec<(i64, i32, u32)> {
    let mut last: IndexMap<i32, u32> = IndexMap::new();
    let mut out: Vec<(i64, i32, u32)> = Vec::new();
    for w in &vgm.writes {
        if w.chip != chip || w.sample < t0 || (t1.is_some() && w.sample > t1.unwrap()) {
            continue;
        }
        for &r in regs {
            let hit = if r < 0x20 {
                w.reg == r as u32
            } else {
                w.reg == (r + ch) as u32
            };
            if hit {
                if only_changes && last.get(&r) == Some(&w.val) {
                    break;
                }
                last.insert(r, w.val);
                out.push((w.sample, r, w.val));
                break;
            }
        }
    }
    out
}

pub fn export_fur(fur: &Path, out: Option<&Path>) -> crate::Result<PathBuf> {
    let out = match out {
        Some(path) => path.to_path_buf(),
        None => {
            let stem = fur
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default();
            fur.with_file_name(format!("{stem}.export.vgm"))
        }
    };
    crate::furnace::export_vgm(fur, &out)
}

/// (chip id, voice) of one module channel from the .fur's system list.
fn _module_chip_voice(fur: &Path, ch: i32) -> Option<(u32, i32)> {
    let chips = match crate::edit::read_chips_file(fur) {
        Ok(chips) => chips,
        Err(_) => return None,
    };
    let mut start = 0i32;
    for chip in chips {
        let span = if chip.id <= 0xFF {
            crate::furio::chip_channels(chip.id as u8) as i32
        } else {
            1
        };
        if start <= ch && ch < start + span {
            return Some((chip.id, ch - start));
        }
        start += span;
    }
    None
}

/// Export the .fur and return the pitch divergences on one channel.
///
/// Furnace's VGM exporter has no case for the Bubble System WSG, so the export
/// carries no K005289 writes. A WSG channel is therefore compared against the
/// .fur's own pattern/instrument data, which is the module's independent record
/// of the pitch it asks the chip to play.
pub fn compare_fur_to_source(
    src_vgm: &Path,
    fur: &Path,
    ch: i32,
    threshold_st: f64,
    min_run_ms: f64,
    export_to: Option<&Path>,
) -> crate::Result<(Vec<PitchRun>, PathBuf)> {
    let exported = export_fur(fur, export_to)?;
    let src = crate::vgm::load_vgm(src_vgm)?;
    let other = crate::vgm::load_vgm(&exported)?;
    let info = _module_chip_voice(fur, ch);
    if let Some((chip, voice)) = info {
        if chip == u32::from(crate::furwrite::CHIP_K005289)
            && !other.writes.iter().any(|w| w.chip == "k005289")
        {
            let clock = if src.k005289_clock != 0 {
                src.k005289_clock
            } else {
                K005289_DEFAULT_CLOCK
            };
            let (mod_stream, _mod_intervals) =
                _fur_wsg_stream(fur, ch, clock).map_err(|exc| {
                    crate::Error::Msg(format!(
                        "could not read the .fur's WSG patterns: {exc}"
                    ))
                })?;
            let mut cids: Vec<u8> = src
                .writes
                .iter()
                .filter(|w| w.chip == "k005289")
                .map(|w| w.chip_id)
                .collect();
            cids.sort();
            cids.dedup();
            let cid = cids.first().copied().unwrap_or(0);
            let src_stream = k005289_pitch_stream(&src, cid, voice);
            let intervals = k005289_keyed_intervals(&src, cid, voice);
            let mut limit = if src.total_samples != 0 {
                Some(src.total_samples)
            } else {
                None
            };
            if other.total_samples != 0 {
                limit = Some(match limit {
                    None => other.total_samples,
                    Some(l) => l.min(other.total_samples),
                });
            }
            let src_stream: Vec<(i64, f64)> = src_stream
                .into_iter()
                .map(|(s, v)| (s, f64::from(v)))
                .collect();
            let mod_stream: Vec<(i64, f64)> = mod_stream
                .into_iter()
                .map(|(s, v)| (s, f64::from(v)))
                .collect();
            return Ok((
                _runs_between(
                    &src_stream,
                    &mod_stream,
                    Some(&intervals),
                    limit,
                    threshold_st,
                    min_run_ms,
                ),
                exported,
            ));
        }
    }
    Ok((
        pitch_runs(&src, &other, ch, threshold_st, min_run_ms),
        exported,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    const PATCH: [(u32, u32); 24] = [
        (0x20, 0xC7),
        (0x28, 0x3E),
        (0x30, 0x00),
        (0x38, 0x00),
        (0x60, 0x00),
        (0x68, 0x00),
        (0x70, 0x00),
        (0x78, 0x00),
        (0x80, 0x1F),
        (0x88, 0x1F),
        (0x90, 0x1F),
        (0x98, 0x1F),
        (0xA0, 0x00),
        (0xA8, 0x00),
        (0xB0, 0x00),
        (0xB8, 0x00),
        (0xC0, 0x00),
        (0xC8, 0x00),
        (0xD0, 0x00),
        (0xD8, 0x00),
        (0xE0, 0x0F),
        (0xE8, 0x0F),
        (0xF0, 0x0F),
        (0xF8, 0x0F),
    ];

    fn lattice(n: i64) -> Vec<ChipWrite> {
        (0..n)
            .map(|k| ChipWrite::new(k * 5000, "ym2151", 0, 0x08, 0x78))
            .collect()
    }

    fn note_song(key_on: i64, key_off: i64, kc: u32, kf: u32) -> VgmFile {
        let mut writes = lattice(key_off / 5000 + 6);
        for (reg, val) in PATCH {
            writes.push(ChipWrite::new(key_on - 1000, "ym2151", 0, reg, val));
        }
        writes.push(ChipWrite::new(key_on, "ym2151", 0, 0x28, kc));
        writes.push(ChipWrite::new(key_on, "ym2151", 0, 0x30, kf));
        writes.push(ChipWrite::new(key_on, "ym2151", 0, 0x08, 0x78));
        writes.push(ChipWrite::new(key_off, "ym2151", 0, 0x08, 0));
        writes.sort_by_key(|w| w.sample);
        let mut vgm = VgmFile::default();
        vgm.path = PathBuf::from("synthetic.vgm");
        vgm.version = 0x161;
        vgm.total_samples = 400_000;
        vgm.ym2151_clock = 3_579_545;
        vgm.writes = writes;
        vgm
    }

    #[test]
    fn identical_streams_have_no_runs() {
        let song = note_song(60000, 160000, 0, 0);
        assert_eq!(pitch_runs(&song, &song, 0, 0.5, 50.0), Vec::new());
    }

    #[test]
    fn held_divergence_is_reported_with_span() {
        let a = note_song(60000, 160000, 0, 0);
        let mut writes = a.writes.clone();
        writes.push(ChipWrite::new(100000, "ym2151", 0, 0x28, 0x02));
        writes.push(ChipWrite::new(120000, "ym2151", 0, 0x28, 0x00));
        writes.sort_by_key(|w| w.sample);
        let b = VgmFile { writes, ..a.clone() };
        let runs = pitch_runs(&a, &b, 0, 0.5, 100.0);
        assert_eq!(runs.len(), 1);
        assert!(crate::parity::py_almost_eq(runs[0].worst_st, 2.0, 2));
        assert!(runs[0].t1 - runs[0].t0 > 19000);
        assert_eq!(
            runs[0].to_string(),
            "  2.268-  2.721s (  454 ms)  +2.00 st"
        );
    }

    #[test]
    fn short_glitch_is_ignored() {
        let a = note_song(60000, 160000, 0, 0);
        let mut writes = a.writes.clone();
        writes.push(ChipWrite::new(100000, "ym2151", 0, 0x28, 0x02));
        writes.sort_by_key(|w| w.sample);
        let b = VgmFile { writes, ..a.clone() };
        assert_eq!(pitch_runs(&a, &b, 0, 0.5, 500.0), Vec::new());
    }

    #[test]
    fn kc_kf_stream_tracks_each_write() {
        let song = note_song(60000, 160000, 0x42, 0x14);
        let stream = kc_kf_stream(&song, 0);
        let first = state_at(&stream, 60000);
        let second = state_at(&stream, 100000);
        assert_eq!(first, second);
    }

    #[test]
    fn register_writes_filter_changes() {
        let song = note_song(60000, 160000, 0, 0);
        let rows = register_writes(&song, "ym2151", 0, &[0x28, 0x30], 0, None, true);
        assert!(!rows.is_empty());
        assert!(rows.iter().all(|(_t, reg, _v)| *reg == 0x28 || *reg == 0x30));
    }

}
