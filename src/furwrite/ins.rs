//! Instrument (`INS2`) features and sample (`SMP2`) bodies.
//!
//! Port of `furwrite.py` lines 571-2201: the FM feature (`FM`/`MA`/`O1`..`O4`)
//! for every FM family, the per-family instrument writers, the sample bodies
//! for every PCM chip, the `WAVE`/`ADIR` helpers and the system `FLAG` text.
//!
//! Block size fields are patched in place after the payload is written, the
//! way the Python writer does it; `patch_u16` is the two-byte sibling of
//! `Buf::patch_u32`.

use indexmap::IndexMap;

use crate::fm::{CARRIERS, OPN_CARRIERS};
use crate::model::ToneIns;
use crate::parity::{py_int, py_round};
use crate::pcm::{pcm7_to_s8, scale_pcm7, PcmSample};

use super::buf::{
    Buf, C140_FURNACE_DEFAULT_CLOCK, FUR_VERSION, INS_AY, INS_FDS, INS_FM, INS_GB,
    INS_MIKEY, INS_NES, INS_OPL, INS_OPL_DRUMS, INS_OPLL, INS_OPM, INS_PCE,
    INS_POKEY, INS_QSOUND, INS_SAA, INS_SCC,
    INS_SWAN, INS_VBOY, INS_X1_010, SEGAPCM_CLOCK_X, SEGAPCM_CLOCK_Y, SMP_DEPTH_12BIT,
    SMP_DEPTH_16BIT, SMP_DEPTH_4BIT, SMP_DEPTH_8BIT, SMP_DEPTH_ADPCM_K, SMP_DEPTH_DPCM, SMP_DEPTH_QSOUND_ADPCM, SMP_DEPTH_VOX,
    SMP_DEPTH_YMZ_ADPCM, pack_dt_mul,
};

const DEFAULT_RENDER_MASKS: [u32; 4] = [0xFFFF_FFFF; 4];
// Furnace's POKEY clock list: NTSC (default) and PAL.  COLOR_PAL*2/5 is
// 1773447.5 Hz.
const POKEY_PAL_CLOCK: f64 = 1_773_448.0;
const POKEY_DEFAULT_CLOCK: f64 = 1_789_772.0;

/// `b.b[size_off:size_off + 2] = int(n).to_bytes(2, "little")`.
fn patch_u16(b: &mut Buf, off: usize, v: i32) {
    let bytes = (v as u16).to_le_bytes();
    b.data[off..off + 2].copy_from_slice(&bytes);
}

/// INS2 'FM' feature.
///
/// Operator order matches YM2151/YM2203 register order and Furnace's internal
/// order.  An OPL patch takes the same feature with its own bit layout.
pub fn _write_fm_feature(b: &mut Buf, patch: &crate::fm::FMPatch) {
    b.raw(b"FM");
    let size_off = b.len();
    b.u16(0);
    let start = b.len();
    if patch.opll {
        _write_opll_fm_feature_body(b, patch);
        let n = b.len() - start;
        patch_u16(b, size_off, n as i32);
        return;
    }
    if patch.opl {
        _write_opl_fm_feature_body(b, patch);
        let n = b.len() - start;
        patch_u16(b, size_off, n as i32);
        return;
    }
    let flags = 4 | 0x10 | 0x20 | 0x40 | 0x80;
    b.u8(flags as i64);
    b.u8((((patch.alg & 7) << 4) | (patch.fb & 7)) as i64);
    b.u8((((patch.ams & 3) << 3) | (patch.pms & 7)) as i64);
    b.u8(0x20); // 4-op bit; version < 224 has no Block byte
    let opn = patch.opn;
    let table: &[&[i32]] = if opn { &OPN_CARRIERS } else { &CARRIERS };
    let carriers: &[i32] = table
        .get((patch.alg & 7) as usize)
        .copied()
        .unwrap_or(&[3i32][..]);
    for idx in 0..4usize {
        let op = &patch.ops[idx];
        // Keep the logged TL. At vol 127 Furnace uses TL as written. OPN sets
        // it on every operator like the reference converter.
        let tl = op.tl & 0x7F;
        let kvs: i32 = if opn || carriers.contains(&(idx as i32)) { 2 } else { 0 };
        b.u8(pack_dt_mul(op.dt_mul as i32, opn) as i64);
        b.u8(tl as i64);
        b.u8((op.ks_ar & 0xDF) as i64);
        b.u8(op.am_d1r as i64);
        if opn {
            // 0x70 is the 5-bit sustain rate (SR) and 0x90 is SSG-EG.
            b.u8((((kvs & 3) << 5) | ((op.sr & 0x1F) as i32)) as i64);
            b.u8(op.d1l_rr as i64);
            b.u8((op.ssg_eg & 0x0F) as i64);
            b.u8(0);
        } else {
            b.u8((((kvs & 3) << 5) | ((op.dt2_d2r & 0x1F) as i32)) as i64);
            b.u8(op.d1l_rr as i64);
            b.u8(0);
            let dt2 = ((op.dt2_d2r as i32) >> 6) & 3;
            b.u8(((dt2 & 3) << 3) as i64);
        }
    }
    let n = b.len() - start;
    patch_u16(b, size_off, n as i32);
}

/// The 'FM' feature payload of a DIV_INS_OPL or DIV_INS_OPL_DRUMS voice.
pub fn _write_opl_fm_feature_body(b: &mut Buf, patch: &crate::fm::FMPatch) {
    let four = patch.four || patch.drum;
    // Four operator entries always: the reader takes its count from opCount,
    // and a drums instrument needs all four.
    b.u8((4 | 0x10 | 0x20 | (if four { 0x40 | 0x80 } else { 0 })) as i64);
    b.u8((((patch.alg & 3) << 4) | (patch.fb & 7)) as i64);
    // No channel LFO depth: the OPL sets AM and vibrato per operator.
    b.u8(0);
    b.u8((if patch.four { 32 } else { 0 }) as i64);
    let carriers = patch.carriers();
    for idx in 0..4usize {
        let op = &patch.ops[idx];
        let kvs: i32 = if carriers.contains(&(idx as i32)) { 2 } else { 0 };
        b.u8((((op.ksr & 1) as i32) << 7 | ((op.mult & 15) as i32)) as i64);
        b.u8((((op.sus & 1) as i32) << 7 | ((op.tl & 63) as i32)) as i64);
        b.u8((((op.vib & 1) as i32) << 5 | ((op.ar & 31) as i32)) as i64);
        b.u8((((op.am & 1) as i32) << 7
            | (((op.ksl & 3) as i32) << 5)
            | ((op.dr & 31) as i32)) as i64);
        b.u8((kvs << 5) as i64);
        b.u8(((((op.sl & 15) as i32) << 4) | ((op.rr & 15) as i32)) as i64);
        b.u8(0); // deep vibrato and SSG-EG are not OPL fields
        b.u8((op.ws & 7) as i64);
    }
}

/// The 'FM' feature payload of a DIV_INS_OPLL voice.
pub fn _write_opll_fm_feature_body(b: &mut Buf, patch: &crate::fm::FMPatch) {
    b.u8((2 | 0x10 | 0x20) as i64);
    b.u8((((patch.alg & 1) << 4) | (patch.fb & 7)) as i64);
    // The chip's $03 bits 4 and 3 are the vibrato and tremolo depth.
    b.u8((((patch.ams & 3) << 3) | (patch.pms & 7)) as i64);
    b.u8((patch.preset & 31) as i64);
    for idx in 0..2usize {
        let op = &patch.ops[idx];
        b.u8((((op.ksr & 1) as i32) << 7 | ((op.mult & 15) as i32)) as i64);
        b.u8((((op.sus & 1) as i32) << 7 | ((op.tl & 63) as i32)) as i64);
        b.u8((((op.vib & 1) as i32) << 5 | ((op.ar & 15) as i32)) as i64);
        b.u8((((op.am & 1) as i32) << 7
            | (((op.ksl & 3) as i32) << 5)
            | ((op.dr & 15) as i32)) as i64);
        b.u8(0);
        b.u8(((((op.sl & 15) as i32) << 4) | ((op.rr & 15) as i32)) as i64);
        // Bit 3 of ssgEnv maps back to $00/$01 bit 5 (ET).
        b.u8((op.ssg_eg & 0x0F) as i64);
        b.u8(0);
    }
}

/// One macro inside a `MA` feature. `width` picks the value encoding the
/// format calls wordSize: 0 = 8-bit unsigned, 1 = 8-bit signed, 2 = 16-bit
/// signed, 3 = 32-bit.
pub fn _write_macro(
    b: &mut Buf,
    code: i32,
    values: &[i32],
    // Python default speed=1
    speed: i32,
    // Python default loop=255
    r#loop: i32,
    // Python default release=255
    release: i32,
    // Python default delay=0
    delay: i32,
    // Python default width=0
    width: i32,
) {
    if values.is_empty() {
        return;
    }
    let values = &values[..values.len().min(255)];
    b.u8((code & 0xFF) as i64);
    b.u8((values.len() & 0xFF) as i64);
    b.u8((r#loop & 0xFF) as i64);
    b.u8((release & 0xFF) as i64);
    b.u8(0); // mode: sequence
    b.u8((1 | (width << 6)) as i64); // open, 8-bit unsigned
    b.u8((delay & 0xFF) as i64);
    b.u8((speed.max(1) & 0xFF) as i64);
    match width {
        0 | 1 => {
            for &v in values {
                b.u8((v & 0xFF) as i64);
            }
        }
        2 => {
            for &v in values {
                b.u16((v & 0xFFFF) as i64);
            }
        }
        _ => {
            for &v in values {
                b.u32((v as i64) & 0xFFFF_FFFF);
            }
        }
    }
}

pub fn _write_ma(
    b: &mut Buf,
    // Python default None
    patch: Option<&crate::fm::FMPatch>,
) {
    b.raw(b"MA");
    let size_off = b.len();
    b.u16(0);
    let start = b.len();
    b.u16(8); // macro header length (Furnace always writes 8)
    if let Some(patch) = patch {
        let speed = if patch.macro_speed != 0 { patch.macro_speed } else { 1 };
        // 10 = FMS/PMS, 11 = AMS. Loop 255 = no loop (hold last).
        _write_macro(b, 10, &patch.fms_macro, speed, 255, 255, 0, 0);
        _write_macro(b, 11, &patch.ams_macro, speed, 255, 255, 0, 0);
        // 4 = pitch, added to the note in 1/128-semitone units.
        if !patch.pitch_macro.is_empty() {
            let pspeed = if patch.pitch_speed != 0 { patch.pitch_speed } else { 1 };
            _write_macro(b, 4, &patch.pitch_macro, pspeed, patch.pitch_loop, 255, 0, 2);
        }
        // OPLL tick-rate automation: volume (code 0), preset wave (code 3) and
        // repeated key-on retrigger (code 14).
        if !patch.vol_macro.is_empty() {
            let vspeed = if patch.vol_speed != 0 { patch.vol_speed } else { 1 };
            _write_macro(b, 0, &patch.vol_macro, vspeed, patch.vol_loop, 255, 0, 0);
        }
        if !patch.wave_macro.is_empty() {
            let wspeed = if patch.wave_speed != 0 { patch.wave_speed } else { 1 };
            _write_macro(b, 3, &patch.wave_macro, wspeed, patch.wave_loop, 255, 0, 0);
        }
        if !patch.phase_macro.is_empty() {
            let pspeed = if patch.phase_speed != 0 { patch.phase_speed } else { 1 };
            _write_macro(b, 14, &patch.phase_macro, pspeed, patch.phase_loop, 255, 0, 0);
        }
    }
    b.u8(255); // stop
    let n = b.len() - start;
    patch_u16(b, size_off, n as i32);
}

/// An `O1`..`O4` feature: the macro list of one operator.
///
/// Each entry is (macro code, values, speed, invert_to_level, value mask).
pub fn _write_op_group(
    b: &mut Buf,
    op_index: usize,
    entries: &[(i32, Vec<i32>, i32, bool, i32)],
) {
    let entries: Vec<&(i32, Vec<i32>, i32, bool, i32)> =
        entries.iter().filter(|e| !e.1.is_empty()).collect();
    if entries.is_empty() {
        return;
    }
    b.raw(format!("O{}", op_index + 1).as_bytes());
    let size_off = b.len();
    b.u16(0);
    let start = b.len();
    // if you update the macro header, update this length as well (Furnace: 8)
    b.u16(8);
    for entry in entries {
        let (code, values, speed, to_level, mask) = entry;
        let stored: Vec<i32> = if *to_level {
            values.iter().map(|&v| 127 - (v & mask)).collect()
        } else {
            values.iter().map(|&v| v & mask).collect()
        };
        _write_macro(b, *code, &stored, *speed, 255, 255, 0, 0);
    }
    b.u8(255); // stop
    let n = b.len() - start;
    patch_u16(b, size_off, n as i32);
}

/// INS2 for an FM instrument of any family.
pub fn _write_ins_opm(b: &mut Buf, name: &str, patch: &crate::fm::FMPatch) {
    let opl = patch.opl;
    let ins_type = if opl {
        if patch.opll {
            INS_OPLL
        } else if patch.drum {
            INS_OPL_DRUMS
        } else {
            INS_OPL
        }
    } else if patch.opn {
        INS_FM
    } else {
        INS_OPM
    };
    let (size_off, start) = b.begin_block(b"INS2");
    b.u16(FUR_VERSION as i64);
    b.u16(ins_type as i64);
    b.raw(b"NA");
    let na_size = b.len();
    b.u16(0);
    let na_start = b.len();
    b.strz(name);
    let n = b.len() - na_start;
    patch_u16(b, na_size, n as i32);
    _write_fm_feature(b, patch);
    _write_ma(b, Some(patch));
    // One O1..O4 feature per operator carries every macro that operator needs.
    let mut op_entries: IndexMap<i32, Vec<(i32, Vec<i32>, i32, bool, i32)>> =
        IndexMap::new();
    for (oi, values, speed) in &patch.op_tl_macro {
        op_entries
            .entry(*oi)
            .or_default()
            .push((6, values.clone(), *speed, true, 0x3F));
    }
    for (oi, values, speed) in &patch.op_mul_macro {
        op_entries
            .entry(*oi)
            .or_default()
            .push((3, values.clone(), *speed, false, 0x0F));
    }
    for (oi, values, speed) in &patch.op_dt_macro {
        op_entries
            .entry(*oi)
            .or_default()
            .push((9, values.clone(), *speed, false, 0x07));
    }
    for (oi, values, speed) in &patch.op_dt2_macro {
        op_entries
            .entry(*oi)
            .or_default()
            .push((7, values.clone(), *speed, false, 0x03));
    }
    let mut order: Vec<i32> = op_entries.keys().copied().collect();
    order.sort();
    for oi in order {
        let entries = op_entries.get(&oi).unwrap();
        _write_op_group(b, oi as usize, entries);
    }
    b.raw(b"EN");
    b.end_block(size_off, start);
}

/// 120 note slots, one sample per slot.
pub fn _write_ins_sample_map(
    b: &mut Buf,
    name: &str,
    first_sample: i32,
    maps: &[i32],
    // Python default ins_type=INS_C352
    ins_type: i32,
    // Python default flags=0x01
    flags: i32,
) {
    let (size_off, start) = b.begin_block(b"INS2");
    b.u16(FUR_VERSION as i64);
    b.u16(ins_type as i64);
    b.raw(b"NA");
    let na_size = b.len();
    b.u16(0);
    let na_start = b.len();
    b.strz(name);
    let n = b.len() - na_start;
    patch_u16(b, na_size, n as i32);
    b.raw(b"SM");
    let sm_size = b.len();
    b.u16(0);
    let sm_start = b.len();
    b.u16((first_sample & 0xFFFF) as i64);
    b.u8((flags & 0xFF) as i64);
    b.u8(31); // wave length field (unused for samples)
    for slot in 0..120usize {
        b.u16(48); // note 60 + 48 = C-4 in the pre-246 encoding
        let m = if slot < maps.len() { maps[slot] } else { -1 };
        b.u16((m & 0xFFFF) as i64);
    }
    let n = b.len() - sm_start;
    patch_u16(b, sm_size, n as i32);
    _write_ma(b, None);
    b.raw(b"EN");
    b.end_block(size_off, start);
}

/// MultiPCM 12-bit PCM: SMP2 stores a frame count and packed 3-byte pairs.
pub fn _write_sample_s12(
    b: &mut Buf,
    name: &str,
    data: &[u8],
    frames: i32,
    c4_rate: i32,
    // Python default loop=(None, None)
    r#loop: (Option<i32>, Option<i32>),
    // Python default None
    render_masks: Option<&[u32]>,
) {
    let render_masks = render_masks.unwrap_or(&DEFAULT_RENDER_MASKS);
    let frames = frames.max(0).min(98303);
    let (size_off, start) = b.begin_block(b"SMP2");
    b.strz(name);
    b.u32(frames as i64);
    b.u32(c4_rate as i64);
    b.u32(c4_rate as i64);
    b.u8(SMP_DEPTH_12BIT as i64);
    b.u8(0);
    b.u8(0);
    b.u8(0);
    let (lo, hi) = r#loop;
    if let (Some(lo), Some(hi)) = (lo, hi) {
        if 0 <= lo && lo < hi && hi <= frames {
            b.s32(lo as i64);
            b.s32(hi as i64);
        } else {
            b.s32(-1);
            b.s32(-1);
        }
    } else {
        b.s32(-1);
        b.s32(-1);
    }
    for &mask in render_masks {
        b.u32(mask as i64);
    }
    let byte_len = ((frames as i64 * 3 + 1) / 2) as usize;
    b.raw(&data[..data.len().min(byte_len)]);
    b.end_block(size_off, start);
}

pub fn _write_ins_pcm(
    b: &mut Buf,
    name: &str,
    sample_index: i32,
    // Python default ins_type=INS_K007232
    ins_type: i32,
    // Python default None
    mp: Option<&[i32]>,
) {
    let (size_off, start) = b.begin_block(b"INS2");
    b.u16(FUR_VERSION as i64);
    b.u16(ins_type as i64);
    b.raw(b"NA");
    let na_size = b.len();
    b.u16(0);
    let na_start = b.len();
    b.strz(name);
    let n = b.len() - na_start;
    patch_u16(b, na_size, n as i32);
    b.raw(b"SM");
    let sm_size = b.len();
    b.u16(0);
    let sm_start = b.len();
    b.u16((sample_index & 0xFFFF) as i64);
    b.u8(0x02); // use sample
    b.u8(0);
    let n = b.len() - sm_start;
    patch_u16(b, sm_size, n as i32);
    _write_ma(b, None);
    if let Some(mp) = mp {
        // DIV_INS_MULTIPCM's MP feature.
        b.raw(b"MP");
        let mp_size = b.len();
        b.u16(0);
        let mp_start = b.len();
        for &value in mp.iter().take(10) {
            b.u8((value & 0xFF) as i64);
        }
        let n = b.len() - mp_start;
        patch_u16(b, mp_size, n as i32);
    }
    b.raw(b"EN");
    b.end_block(size_off, start);
}

/// Short stable id for an SCC waveform, for instrument names.
pub fn _wave_label(wave: &[u8]) -> String {
    let mut h: i32 = 0;
    for &val in wave {
        h = (h * 31 + val as i32) & 0xFFFF;
    }
    format!("{:04x}", h)
}

/// DIV_INS_SCC: the note starts on wavetable `ins.wave`.
pub fn _write_ins_scc(
    b: &mut Buf,
    name: &str,
    ins: &ToneIns,
    // Python default wave_base=0
    wave_base: i32,
) {
    let (size_off, start) = b.begin_block(b"INS2");
    b.u16(FUR_VERSION as i64);
    b.u16(INS_SCC as i64);
    b.raw(b"NA");
    let na_size = b.len();
    b.u16(0);
    let na_start = b.len();
    b.strz(name);
    let n = b.len() - na_start;
    patch_u16(b, na_size, n as i32);
    b.raw(b"WS");
    let ws_size = b.len();
    b.u16(0);
    let ws_start = b.len();
    let wave_index = wave_base + ins.wave.max(0);
    b.u32(wave_index as i64); // wave1
    b.u32(wave_index as i64); // wave2, only used by the dual-wave effects
    b.u8(1); // rateDivider
    b.u8(0); // effect: none
    b.u8(1); // enabled
    b.u8(0); // global: restart the wave on every note
    b.u8(0); // speed
    b.u8(0); // param1
    b.u8(0); // param2
    b.u8(0); // param3
    b.u8(0); // param4
    let n = b.len() - ws_start;
    patch_u16(b, ws_size, n as i32);
    _write_tone_macros(b, ins, true, false, true, wave_base);
    b.raw(b"EN");
    b.end_block(size_off, start);
}

/// DIV_INS_PCE: the note starts on wavetable `ins.wave`.
pub fn _write_ins_pce(b: &mut Buf, name: &str, ins: &ToneIns) {
    let (size_off, start) = b.begin_block(b"INS2");
    b.u16(FUR_VERSION as i64);
    b.u16(INS_PCE as i64);
    b.raw(b"NA");
    let na_size = b.len();
    b.u16(0);
    let na_start = b.len();
    b.strz(name);
    let n = b.len() - na_start;
    patch_u16(b, na_size, n as i32);
    b.raw(b"WS");
    let ws_size = b.len();
    b.u16(0);
    let ws_start = b.len();
    b.u32(ins.wave as i64); // wave1
    b.u32(ins.wave as i64); // wave2, only used by the dual-wave effects
    b.u8(1); // rateDivider
    b.u8(0); // effect: none
    b.u8(1); // enabled
    b.u8(0); // global: restart the wave on every note
    b.u8(0); // speed
    b.u8(0); // param1
    b.u8(0); // param2
    b.u8(0); // param3
    b.u8(0); // param4
    let n = b.len() - ws_start;
    patch_u16(b, ws_size, n as i32);
    _write_tone_macros(b, ins, true, false, true, 0);
    b.raw(b"EN");
    b.end_block(size_off, start);
}

/// MA feature for an SCC/AY/SAA instrument.
///
/// File codes: 0 volume, 2 duty, 3 wave, 4 pitch, 5 ex1, 6 ex2, 12 panL,
/// 13 panR, 14 phase reset, 16 ex5.
pub fn _write_tone_macros(
    b: &mut Buf,
    ins: &ToneIns,
    wave: bool,
    // Python default reset=false
    reset: bool,
    // Python default ay_compat=true
    ay_compat: bool,
    // Python default wave_base=0
    wave_base: i32,
) {
    let moving = !ins.vol.is_empty()
        || !ins.pitch.is_empty()
        || (wave && !ins.waves.is_empty())
        || !ins.noise.is_empty()
        || !ins.mode.is_empty()
        || !ins.env_shape.is_empty()
        || !ins.env_period.is_empty()
        || !ins.panl.is_empty()
        || !ins.panr.is_empty()
        || !ins.env.is_empty()
        || !ins.duty.is_empty()
        || reset;
    if !moving {
        _write_ma(b, None);
        return;
    }
    b.raw(b"MA");
    let ma_size = b.len();
    b.u16(0);
    let ma_start = b.len();
    b.u16(8); // macro header length (Furnace always writes 8)
    if !ins.vol.is_empty() {
        _write_macro(
            b,
            0,
            &ins.vol,
            ins.vol_speed,
            ins.vol_loop,
            255,
            ins.vol_delay,
            0,
        );
    }
    if !ins.noise.is_empty() {
        // ay.cpp writes register 6 as ``31 - duty``.
        _write_macro(
            b,
            2,
            &ins.noise,
            ins.noise_speed,
            ins.noise_loop,
            255,
            0,
            0,
        );
    }
    if !ins.duty.is_empty() {
        _write_macro(
            b,
            2,
            &ins.duty,
            ins.duty_speed,
            ins.duty_loop,
            255,
            0,
            0,
        );
    }
    if wave && !ins.waves.is_empty() {
        let wave_values: Vec<i32> = ins.waves.iter().map(|&v| wave_base + v).collect();
        let width = if *wave_values.iter().max().unwrap() > 127 { 2 } else { 1 };
        _write_macro(
            b,
            3,
            &wave_values,
            ins.wave_speed,
            ins.wave_loop,
            255,
            0,
            width,
        );
    }
    if !ins.mode.is_empty() {
        // AY mode bits (tone 1, noise 2, envelope 4). Before file version 193
        // Furnace bumps every DIV_INS_AY wave-macro value by one on load.
        let mode: Vec<i32> = if ay_compat && FUR_VERSION < 193 {
            ins.mode.iter().map(|&value| (value - 1) & 0xFF).collect()
        } else {
            ins.mode.clone()
        };
        _write_macro(
            b,
            3,
            &mode,
            ins.mode_speed,
            ins.mode_loop,
            255,
            0,
            0,
        );
    }
    if !ins.panl.is_empty() {
        _write_macro(
            b,
            12,
            &ins.panl,
            ins.panl_speed,
            ins.panl_loop,
            255,
            ins.panl_delay,
            0,
        );
    }
    if !ins.panr.is_empty() {
        _write_macro(
            b,
            13,
            &ins.panr,
            ins.panr_speed,
            ins.panr_loop,
            255,
            ins.panr_delay,
            0,
        );
    }
    if !ins.env.is_empty() {
        _write_macro(
            b,
            5,
            &ins.env,
            ins.env_speed,
            ins.env_loop,
            255,
            ins.env_delay,
            0,
        );
    }
    if !ins.pitch.is_empty() {
        _write_macro(
            b,
            4,
            &ins.pitch,
            ins.pitch_speed,
            ins.pitch_loop,
            255,
            ins.pitch_delay,
            2,
        );
    }
    if !ins.env_shape.is_empty() {
        _write_macro(
            b,
            6,
            &ins.env_shape,
            ins.env_shape_speed,
            ins.env_shape_loop,
            255,
            0,
            0,
        );
    }
    if !ins.env_period.is_empty() {
        let width = if *ins.env_period.iter().max().unwrap() <= 32767 { 2 } else { 3 };
        _write_macro(
            b,
            16,
            &ins.env_period,
            ins.env_period_speed,
            ins.env_period_loop,
            255,
            0,
            width,
        );
    }
    if reset {
        // The AY restarts its envelope only on a shape write, and Furnace
        // skips same-value writes. The phase reset clears that cache.
        _write_macro(b, 14, &[1, 0], 1, 255, 255, 0, 0);
    }
    b.u8(255); // stop
    let n = b.len() - ma_start;
    patch_u16(b, ma_size, n as i32);
}

/// Plain SN76489/T6W28 instrument: only the pitch macro is needed.
pub fn _write_ins_psg(
    b: &mut Buf,
    name: &str,
    // Python default ins=None
    ins: Option<&ToneIns>,
    // Python default ins_type=INS_STD
    ins_type: i32,
) {
    let default = ToneIns::default();
    let ins = ins.unwrap_or(&default);
    let (size_off, start) = b.begin_block(b"INS2");
    b.u16(FUR_VERSION as i64);
    b.u16(ins_type as i64);
    b.raw(b"NA");
    let na_size = b.len();
    b.u16(0);
    let na_start = b.len();
    b.strz(name);
    let n = b.len() - na_start;
    patch_u16(b, na_size, n as i32);
    _write_tone_macros(b, ins, false, false, true, 0);
    b.raw(b"EN");
    b.end_block(size_off, start);
}

/// DIV_INS_AY with an optional phase-reset and pitch macro.
pub fn _write_ins_ay(
    b: &mut Buf,
    name: &str,
    // Python default ins=None
    ins: Option<&ToneIns>,
) {
    let default = ToneIns::default();
    let ins = ins.unwrap_or(&default);
    let (size_off, start) = b.begin_block(b"INS2");
    b.u16(FUR_VERSION as i64);
    b.u16(INS_AY as i64);
    b.raw(b"NA");
    let na_size = b.len();
    b.u16(0);
    let na_start = b.len();
    b.strz(name);
    let n = b.len() - na_start;
    patch_u16(b, na_size, n as i32);
    _write_tone_macros(b, ins, false, ins.env_hw, true, 0);
    b.raw(b"EN");
    b.end_block(size_off, start);
}

/// DIV_INS_POKEY: volume, AUDCTL, waveform and pitch macros.
pub fn _write_ins_pokey(
    b: &mut Buf,
    name: &str,
    // Python default ins=None
    ins: Option<&ToneIns>,
) {
    let default = ToneIns::default();
    let ins = ins.unwrap_or(&default);
    let (size_off, start) = b.begin_block(b"INS2");
    b.u16(FUR_VERSION as i64);
    b.u16(INS_POKEY as i64);
    b.raw(b"NA");
    let na_size = b.len();
    b.u16(0);
    let na_start = b.len();
    b.strz(name);
    let n = b.len() - na_start;
    patch_u16(b, na_size, n as i32);
    _write_tone_macros(b, ins, true, false, true, 0);
    b.raw(b"EN");
    b.end_block(size_off, start);
}

/// DIV_INS_MIKEY: LFSR tone macros for the Atari Lynx.
pub fn _write_ins_mikey(
    b: &mut Buf,
    name: &str,
    // Python default ins=None
    ins: Option<&ToneIns>,
) {
    let default = ToneIns::default();
    let ins = ins.unwrap_or(&default);
    let (size_off, start) = b.begin_block(b"INS2");
    b.u16(FUR_VERSION as i64);
    b.u16(INS_MIKEY as i64);
    b.raw(b"NA");
    let na_size = b.len();
    b.u16(0);
    let na_start = b.len();
    b.strz(name);
    let n = b.len() - na_start;
    patch_u16(b, na_size, n as i32);
    if !ins.vol.is_empty()
        || !ins.duty.is_empty()
        || !ins.pitch.is_empty()
        || !ins.lfsr.is_empty()
        || !ins.panl.is_empty()
        || !ins.panr.is_empty()
    {
        b.raw(b"MA");
        let ma_size = b.len();
        b.u16(0);
        let ma_start = b.len();
        b.u16(8);
        if !ins.vol.is_empty() {
            _write_macro(
                b,
                0,
                &ins.vol,
                ins.vol_speed,
                ins.vol_loop,
                255,
                ins.vol_delay,
                0,
            );
        }
        // Code 2 is Duty/Int: the tap bitmask is 10 bits wide.
        if !ins.duty.is_empty() {
            _write_macro(
                b,
                2,
                &ins.duty,
                ins.duty_speed,
                ins.duty_loop,
                255,
                0,
                2,
            );
        }
        if !ins.pitch.is_empty() {
            _write_macro(
                b,
                4,
                &ins.pitch,
                ins.pitch_speed,
                ins.pitch_loop,
                255,
                ins.pitch_delay,
                2,
            );
        }
        // Code 5 is Load LFSR; the seed is 12 bits.
        if !ins.lfsr.is_empty() {
            _write_macro(
                b,
                5,
                &ins.lfsr,
                ins.lfsr_speed,
                ins.lfsr_loop,
                255,
                0,
                2,
            );
        }
        if !ins.panl.is_empty() {
            _write_macro(
                b,
                12,
                &ins.panl,
                ins.panl_speed,
                ins.panl_loop,
                255,
                0,
                0,
            );
        }
        if !ins.panr.is_empty() {
            _write_macro(
                b,
                13,
                &ins.panr,
                ins.panr_speed,
                ins.panr_loop,
                255,
                0,
                0,
            );
        }
        b.u8(255);
        let n = b.len() - ma_start;
        patch_u16(b, ma_size, n as i32);
    } else {
        _write_ma(b, None);
    }
    b.raw(b"EN");
    b.end_block(size_off, start);
}

/// DIV_INS_X1_010 wavetable voice: waveform plus hardware envelope.
pub fn _write_ins_x1(
    b: &mut Buf,
    name: &str,
    // Python default ins=None
    ins: Option<&ToneIns>,
    // Python default wave_base=0
    wave_base: i32,
    // Python default env_base=0
    env_base: i32,
) {
    let default = ToneIns::default();
    let ins = ins.unwrap_or(&default);
    let (size_off, start) = b.begin_block(b"INS2");
    b.u16(FUR_VERSION as i64);
    b.u16(INS_X1_010 as i64);
    b.raw(b"NA");
    let na_size = b.len();
    b.u16(0);
    let na_start = b.len();
    b.strz(name);
    let n = b.len() - na_start;
    patch_u16(b, na_size, n as i32);
    b.raw(b"WS");
    let ws_size = b.len();
    b.u16(0);
    let ws_start = b.len();
    let wave1 = wave_base + ins.wave.max(0);
    b.u32(wave1 as i64); // wave1
    b.u32(wave1 as i64); // wave2
    b.u8(1); // rateDivider
    b.u8(0); // effect: none
    b.u8(1); // enabled
    b.u8(0); // global: restart the wave on every note
    b.u8(0); // speed
    b.u8(0); // param1
    b.u8(0); // param2
    b.u8(0); // param3
    b.u8(0); // param4
    let n = b.len() - ws_start;
    patch_u16(b, ws_size, n as i32);
    b.raw(b"MA");
    let ma_size = b.len();
    b.u16(0);
    let ma_start = b.len();
    b.u16(8);
    if !ins.vol.is_empty() {
        _write_macro(
            b,
            0,
            &ins.vol,
            ins.vol_speed,
            ins.vol_loop,
            255,
            ins.vol_delay,
            0,
        );
    }
    if !ins.waves.is_empty() {
        let macro_waves: Vec<i32> = ins.waves.iter().map(|&v| wave_base + v).collect();
        let width = if *macro_waves.iter().max().unwrap() > 127 { 2 } else { 0 };
        _write_macro(
            b,
            3,
            &macro_waves,
            ins.wave_speed,
            ins.wave_loop,
            255,
            0,
            width,
        );
    }
    if !ins.pitch.is_empty() {
        _write_macro(
            b,
            4,
            &ins.pitch,
            ins.pitch_speed,
            ins.pitch_loop,
            255,
            ins.pitch_delay,
            2,
        );
    }
    let env_vals: &[i32] = if ins.env.is_empty() { &[1] } else { &ins.env };
    _write_macro(
        b,
        5,
        env_vals,
        ins.env_speed,
        ins.env_loop,
        255,
        0,
        0,
    );
    let shape_src: &[i32] = if ins.env_shape.is_empty() { &[0] } else { &ins.env_shape };
    let macro_env: Vec<i32> = shape_src.iter().map(|&v| env_base + v).collect();
    let width = if *macro_env.iter().max().unwrap() > 127 { 2 } else { 0 };
    _write_macro(
        b,
        6,
        &macro_env,
        ins.env_shape_speed,
        ins.env_shape_loop,
        255,
        0,
        width,
    );
    let period_vals: &[i32] = if ins.env_period.is_empty() { &[0] } else { &ins.env_period };
    _write_macro(
        b,
        16,
        period_vals,
        ins.env_period_speed,
        ins.env_period_loop,
        255,
        0,
        0,
    );
    b.u8(255);
    let n = b.len() - ma_start;
    patch_u16(b, ma_size, n as i32);
    b.raw(b"EN");
    b.end_block(size_off, start);
}

/// DIV_INS_SAA1099: standard macros plus the raw pan pair and ex1.
pub fn _write_ins_saa(
    b: &mut Buf,
    name: &str,
    // Python default ins=None
    ins: Option<&ToneIns>,
) {
    let default = ToneIns::default();
    let ins = ins.unwrap_or(&default);
    let (size_off, start) = b.begin_block(b"INS2");
    b.u16(FUR_VERSION as i64);
    b.u16(INS_SAA as i64);
    b.raw(b"NA");
    let na_size = b.len();
    b.u16(0);
    let na_start = b.len();
    b.strz(name);
    let n = b.len() - na_start;
    patch_u16(b, na_size, n as i32);
    _write_tone_macros(b, ins, false, false, false, 0);
    b.raw(b"EN");
    b.end_block(size_off, start);
}

/// DIV_INS_GB: hardware envelope plus an optional wavetable (voice 3).
pub fn _write_ins_gb(b: &mut Buf, name: &str, ins: &ToneIns) {
    let (size_off, start) = b.begin_block(b"INS2");
    b.u16(FUR_VERSION as i64);
    b.u16(INS_GB as i64);
    b.raw(b"NA");
    let na_size = b.len();
    b.u16(0);
    let na_start = b.len();
    b.strz(name);
    let n = b.len() - na_start;
    patch_u16(b, na_size, n as i32);
    _write_tone_macros(b, ins, ins.gb_wave, false, true, 0);
    b.raw(b"GB");
    let gb_size = b.len();
    b.u16(0);
    let gb_start = b.len();
    b.u8((((ins.gb_env_len & 7) << 5)
        | ((ins.gb_env_dir & 1) << 4)
        | (ins.gb_env_vol & 15)) as i64);
    // `soundLen` is NRx1's encoding; `soundLenInfinite` (bit 3 of the flags
    // byte) is what keeps the hardware length counter off. Without the flag
    // Furnace sets NRx4 bit 6 and every note is cut once its length runs out.
    let sound_len_inf = ins.gb_sound_len_inf;
    let sound_len = if sound_len_inf {
        255
    } else {
        ins.gb_sound_len & 0xFF
    };
    b.u8(sound_len as i64);
    let soft = ins.gb_soft_env;
    b.u8(((if sound_len_inf { 8 } else { 0 }) | (if soft { 1 } else { 0 })) as i64);
    // Furnace applies the DIV_INS_GB envelope fields at the note-on, so a
    // hardware sequence holding the same values would only write NRx2 twice.
    // The pattern leaves the volume column empty for these notes, so nothing
    // overrides the start level and the sequence is not needed.
    b.u8(0); // no hardware envelope sequence
    let n = b.len() - gb_start;
    patch_u16(b, gb_size, n as i32);
    if ins.gb_wave {
        b.raw(b"WS");
        let ws_size = b.len();
        b.u16(0);
        let ws_start = b.len();
        b.u32(ins.wave.max(0) as i64); // wave1
        b.u32(ins.wave.max(0) as i64); // wave2
        b.u8(1); // rateDivider
        b.u8(0); // effect: none
        b.u8(1); // enabled
        b.u8(0); // global: restart on every note
        b.u8(0); // speed
        b.u8(0); // param1
        b.u8(0); // param2
        b.u8(0); // param3
        b.u8(0); // param4
        let n = b.len() - ws_start;
        patch_u16(b, ws_size, n as i32);
    }
    b.raw(b"EN");
    b.end_block(size_off, start);
}

/// DIV_INS_VBOY: wavetable instrument plus the FD modulation table.
pub fn _write_ins_vsu(
    b: &mut Buf,
    name: &str,
    ins: &ToneIns,
    mod_table: &[u8],
    // Python default wave_index=0
    wave_index: i32,
) {
    let (size_off, start) = b.begin_block(b"INS2");
    b.u16(FUR_VERSION as i64);
    b.u16(INS_VBOY as i64);
    b.raw(b"NA");
    let na_size = b.len();
    b.u16(0);
    let na_start = b.len();
    b.strz(name);
    let n = b.len() - na_start;
    patch_u16(b, na_size, n as i32);
    b.raw(b"FD");
    let fd_size = b.len();
    b.u16(0);
    let fd_start = b.len();
    b.u32(0); // modSpeed
    b.u32(0); // modDepth
    b.u8(1); // initModTableWithFirstWave: load on channel 4 note-on
    let mut table = [0u8; 32];
    let n_copy = mod_table.len().min(32);
    table[..n_copy].copy_from_slice(&mod_table[..n_copy]);
    b.raw(&table);
    let n = b.len() - fd_start;
    patch_u16(b, fd_size, n as i32);
    b.raw(b"WS");
    let ws_size = b.len();
    b.u16(0);
    let ws_start = b.len();
    b.u32(wave_index.max(0) as i64); // wave1
    b.u32(wave_index.max(0) as i64); // wave2
    b.u8(1); // rateDivider
    b.u8(0); // effect: none
    b.u8(1); // enabled
    b.u8(0); // global
    b.u8(0); // speed
    b.u8(0); // param1
    b.u8(0); // param2
    b.u8(0); // param3
    b.u8(0); // param4
    let n = b.len() - ws_start;
    patch_u16(b, ws_size, n as i32);
    _write_tone_macros(b, ins, false, false, true, 0);
    b.raw(b"EN");
    b.end_block(size_off, start);
}

/// DIV_INS_NES: a plain tone instrument (duty/volume ride the pattern).
pub fn _write_ins_nes(
    b: &mut Buf,
    name: &str,
    // Python default ins=None
    ins: Option<&ToneIns>,
) {
    let default = ToneIns::default();
    let ins = ins.unwrap_or(&default);
    let (size_off, start) = b.begin_block(b"INS2");
    b.u16(FUR_VERSION as i64);
    b.u16(INS_NES as i64);
    b.raw(b"NA");
    let na_size = b.len();
    b.u16(0);
    let na_start = b.len();
    b.strz(name);
    let n = b.len() - na_start;
    patch_u16(b, na_size, n as i32);
    _write_tone_macros(b, ins, false, false, true, 0);
    b.raw(b"EN");
    b.end_block(size_off, start);
}

/// DIV_INS_FDS: 64x6-bit wave plus the FD modulation table.
pub fn _write_ins_fds(
    b: &mut Buf,
    name: &str,
    ins: &ToneIns,
    mod_table: &[u8],
    // Python default wave_index=0
    wave_index: i32,
) {
    let (size_off, start) = b.begin_block(b"INS2");
    b.u16(FUR_VERSION as i64);
    b.u16(INS_FDS as i64);
    b.raw(b"NA");
    let na_size = b.len();
    b.u16(0);
    let na_start = b.len();
    b.strz(name);
    let n = b.len() - na_start;
    patch_u16(b, na_size, n as i32);
    b.raw(b"FD");
    let fd_size = b.len();
    b.u16(0);
    let fd_start = b.len();
    b.u32(ins.fds_mod_speed.max(0) as i64);
    b.u32(ins.fds_mod_depth.max(0) as i64);
    b.u8(0);
    let mut table = [0u8; 32];
    let n_copy = mod_table.len().min(32);
    table[..n_copy].copy_from_slice(&mod_table[..n_copy]);
    b.raw(&table);
    let n = b.len() - fd_start;
    patch_u16(b, fd_size, n as i32);
    if wave_index >= 0 {
        b.raw(b"WS");
        let ws_size = b.len();
        b.u16(0);
        let ws_start = b.len();
        b.u32(wave_index as i64);
        b.u32(wave_index as i64);
        b.u8(1); // rateDivider
        b.u8(0); // effect: none
        b.u8(1); // enabled
        b.u8(0); // global
        b.u8(0); // speed
        b.u8(0); // param1
        b.u8(0); // param2
        b.u8(0); // param3
        b.u8(0); // param4
        let n = b.len() - ws_start;
        patch_u16(b, ws_size, n as i32);
    }
    let mut macro_ins = ins.clone();
    if !ins.waves.is_empty() {
        let offset = wave_index - ins.wave.max(0);
        macro_ins.waves = ins.waves.iter().map(|&v| offset + v).collect();
    }
    _write_tone_macros(b, &macro_ins, true, false, true, 0);
    b.raw(b"EN");
    b.end_block(size_off, start);
}

/// DIV_INS_SWAN: 32x4-bit wavetable with volume/pitch/pan macros.
pub fn _write_ins_swan(
    b: &mut Buf,
    name: &str,
    ins: &ToneIns,
    // Python default wave_index=0
    wave_index: i32,
) {
    let (size_off, start) = b.begin_block(b"INS2");
    b.u16(FUR_VERSION as i64);
    b.u16(INS_SWAN as i64);
    b.raw(b"NA");
    let na_size = b.len();
    b.u16(0);
    let na_start = b.len();
    b.strz(name);
    let n = b.len() - na_start;
    patch_u16(b, na_size, n as i32);
    b.raw(b"WS");
    let ws_size = b.len();
    b.u16(0);
    let ws_start = b.len();
    b.u32(wave_index as i64);
    b.u32(wave_index as i64);
    b.u8(1); // rateDivider
    b.u8(0); // effect: none
    b.u8(1); // enabled
    b.u8(0); // global
    b.u8(0); // speed
    b.u8(0); // param1
    b.u8(0); // param2
    b.u8(0); // param3
    b.u8(0); // param4
    let n = b.len() - ws_start;
    patch_u16(b, ws_size, n as i32);
    let mut macro_ins = ins.clone();
    if !ins.waves.is_empty() {
        let offset = wave_index - ins.wave.max(0);
        macro_ins.waves = ins.waves.iter().map(|&v| offset + v).collect();
    }
    _write_tone_macros(b, &macro_ins, true, false, true, 0);
    b.raw(b"EN");
    b.end_block(size_off, start);
}

/// QSound 4-bit ADPCM body. ``samples`` is a nibble count.
pub fn _write_sample_qsound_adpcm(b: &mut Buf, name: &str, data: &[u8], rate: i32) {
    let count = data.len() * 2;
    let (size_off, start) = b.begin_block(b"SMP2");
    b.strz(name);
    b.u32(count as i64);
    b.u32(rate as i64);
    b.u32(rate as i64);
    b.u8(SMP_DEPTH_QSOUND_ADPCM as i64);
    b.u8(0);
    b.u8(0);
    b.u8(0);
    b.s32(-1);
    b.s32(-1);
    // Bank 1 is the QSound ADPCM sample bank (`renderOn[1]`).
    b.u32(0);
    b.u32(0xFFFF_FFFFi64);
    b.u32(0);
    b.u32(0);
    b.raw(data);
    b.end_block(size_off, start);
}

/// NES 1-bit DPCM body.
pub fn _write_sample_dpcm(
    b: &mut Buf,
    name: &str,
    data: &[u8],
    c4_rate: i32,
    // Python default loop=False
    r#loop: bool,
    // Python default None (module-global `_DEFAULT_RENDER_MASKS`)
    render_masks: Option<&[u32]>,
) {
    let render_masks = render_masks.unwrap_or(&DEFAULT_RENDER_MASKS);
    let bits = data.len() * 8;
    let length_dpcm = 1 + ((data.len() + 14) & !15);
    let mut body = data.to_vec();
    if body.len() < length_dpcm {
        body.resize(length_dpcm, 0xAA);
    }
    let (size_off, start) = b.begin_block(b"SMP2");
    b.strz(name);
    b.u32(bits as i64);
    b.u32(c4_rate as i64);
    b.u32(c4_rate as i64);
    b.u8(SMP_DEPTH_DPCM as i64);
    b.u8(0);
    b.u8(0);
    b.u8(0);
    if r#loop {
        b.s32(0);
        b.s32(bits as i64);
    } else {
        b.s32(-1);
        b.s32(-1);
    }
    for &mask in render_masks {
        b.u32(mask as i64);
    }
    b.raw(&body);
    b.end_block(size_off, start);
}

/// DivWavetable::putWaveData: the waveform an SCC/PCE instrument plays.
pub fn _write_wave(
    b: &mut Buf,
    name: &str,
    data: &[u8],
    // Python default maximum=255
    maximum: i32,
) -> usize {
    let ptr = b.len();
    let (size_off, start) = b.begin_block(b"WAVE");
    b.strz(name);
    b.u32(data.len() as i64);
    b.u32(0); // min
    b.u32(maximum as i64); // max
    for &val in data {
        b.u32(val as i64);
    }
    b.end_block(size_off, start);
    ptr
}

/// An ADIR asset directory over the given block pointers (0 = empty).
pub fn _write_adir_list(b: &mut Buf, ptrs: &[usize]) -> usize {
    let ptr = b.len();
    let (size_off, start) = b.begin_block(b"ADIR");
    b.u32(ptrs.len() as i64);
    for &p in ptrs {
        b.u32(p as i64);
    }
    b.end_block(size_off, start);
    ptr
}

pub fn _pcm_label(smp: &PcmSample, index: i32) -> String {
    format!("smp_{:02}_{:05x}", index, smp.start)
}

pub fn _write_sample(
    b: &mut Buf,
    name: &str,
    pcm7: &[u8],
    c4_rate: i32,
    // Python default volume_scale=1.0
    volume_scale: f64,
    // Python default None (module-global `_DEFAULT_RENDER_MASKS`)
    render_masks: Option<&[u32]>,
) {
    let render_masks = render_masks.unwrap_or(&DEFAULT_RENDER_MASKS);
    let data = pcm7_to_s8(&scale_pcm7(pcm7, volume_scale));
    let (size_off, start) = b.begin_block(b"SMP2");
    b.strz(name);
    b.u32(data.len() as i64);
    b.u32(c4_rate as i64);
    b.u32(c4_rate as i64);
    b.u8(SMP_DEPTH_8BIT as i64);
    b.u8(0);
    b.u8(0);
    b.u8(0);
    b.s32(-1);
    b.s32(-1);
    // 4 banks x 32 chip bits. Zero means "do not upload to this chip".
    for &mask in render_masks {
        b.u32(mask as i64);
    }
    b.raw(&data);
    b.end_block(size_off, start);
}

/// VOX ADPCM. The stored length is a *nibble* count, never a byte count.
pub fn _write_vox(
    b: &mut Buf,
    name: &str,
    data: &[u8],
    c4_rate: i32,
    // Python default swap_nibbles=False
    swap_nibbles: bool,
    // Python default None (module-global `_DEFAULT_RENDER_MASKS`)
    render_masks: Option<&[u32]>,
) {
    let render_masks = render_masks.unwrap_or(&DEFAULT_RENDER_MASKS);
    let stored: Vec<u8> = if swap_nibbles {
        data.iter()
            .map(|&x| ((((x & 0x0F) << 4) | (x >> 4)) & 0xFF) as u8)
            .collect()
    } else {
        data.to_vec()
    };
    let count = stored.len() * 2;
    let (size_off, start) = b.begin_block(b"SMP2");
    b.strz(name);
    b.u32(count as i64);
    b.u32(c4_rate as i64);
    b.u32(c4_rate as i64);
    b.u8(SMP_DEPTH_VOX as i64);
    b.u8(0);
    b.u8(0);
    b.u8(0);
    b.s32(-1);
    b.s32(-1);
    for &mask in render_masks {
        b.u32(mask as i64);
    }
    b.raw(&stored);
    b.end_block(size_off, start);
}

/// Signed PCM with optional loop points. Depth 16 is int16 LE (mulaw).
pub fn _write_sample_s8(
    b: &mut Buf,
    name: &str,
    data: &[u8],
    c4_rate: i32,
    // Python default loop=(None, None)
    r#loop: (Option<i32>, Option<i32>),
    // Python default depth=SMP_DEPTH_8BIT
    depth: i32,
    // Python default loop_mode=0
    loop_mode: i32,
    // Python default None
    render_masks: Option<&[u32]>,
) {
    let render_masks = render_masks.unwrap_or(&DEFAULT_RENDER_MASKS);
    let width = if depth == SMP_DEPTH_16BIT { 2 } else { 1 };
    let n_frames = data.len() / width;
    let (size_off, start) = b.begin_block(b"SMP2");
    b.strz(name);
    b.u32(n_frames as i64);
    b.u32(c4_rate as i64);
    b.u32(c4_rate as i64);
    b.u8(depth as i64);
    b.u8((loop_mode & 0xFF) as i64);
    b.u8(0);
    b.u8(0);
    let (lo, hi) = r#loop;
    if let (Some(lo), Some(hi)) = (lo, hi) {
        if 0 <= lo && lo < hi && hi <= n_frames as i32 {
            b.s32(lo as i64);
            b.s32(hi as i64);
        } else {
            b.s32(-1);
            b.s32(-1);
        }
    } else {
        b.s32(-1);
        b.s32(-1);
    }
    for &mask in render_masks {
        b.u32(mask as i64);
    }
    b.raw(&data[..n_frames * width]);
    b.end_block(size_off, start);
}

/// Packed linear 4-bit PCM (two high-nibble-first frames per byte).
pub fn _write_sample_4bit(
    b: &mut Buf,
    name: &str,
    data: &[u8],
    frames: i32,
    c4_rate: i32,
    // Python default None
    render_masks: Option<&[u32]>,
) {
    let render_masks = render_masks.unwrap_or(&DEFAULT_RENDER_MASKS);
    let mut packed = vec![0u8; ((frames + 1) / 2) as usize];
    for i in 0..(frames as usize).min(data.len()) {
        if i & 1 != 0 {
            packed[i >> 1] |= data[i] & 0x0F;
        } else {
            packed[i >> 1] = (data[i] & 0x0F) << 4;
        }
    }
    let (size_off, start) = b.begin_block(b"SMP2");
    b.strz(name);
    b.u32(frames as i64);
    b.u32(c4_rate as i64);
    b.u32(c4_rate as i64);
    b.u8(SMP_DEPTH_4BIT as i64);
    b.u8(0);
    b.u8(0);
    b.u8(0);
    b.s32(-1);
    b.s32(-1);
    for &mask in render_masks {
        b.u32(mask as i64);
    }
    b.raw(&packed);
    b.end_block(size_off, start);
}

/// YMZ280B 4-bit ADPCM body.
pub fn _write_sample_ymz_adpcm(
    b: &mut Buf,
    name: &str,
    data: &[u8],
    c4_rate: i32,
    // Python default loop=(None, None)
    r#loop: (Option<i32>, Option<i32>),
    // Python default None
    render_masks: Option<&[u32]>,
) {
    let render_masks = render_masks.unwrap_or(&DEFAULT_RENDER_MASKS);
    let samples = data.len() * 2;
    let (size_off, start) = b.begin_block(b"SMP2");
    b.strz(name);
    b.u32(samples as i64);
    b.u32(c4_rate as i64);
    b.u32(c4_rate as i64);
    b.u8(SMP_DEPTH_YMZ_ADPCM as i64);
    b.u8(0);
    b.u8(0);
    b.u8(0);
    let (lo, hi) = r#loop;
    if let (Some(lo), Some(hi)) = (lo, hi) {
        b.s32(lo as i64);
        b.s32(hi as i64);
    } else {
        b.s32(-1);
        b.s32(-1);
    }
    for &mask in render_masks {
        b.u32(mask as i64);
    }
    b.raw(data);
    b.end_block(size_off, start);
}

/// K053260 KADPCM. SMP2 stores a nibble count; two nibbles per byte.
pub fn _write_sample_k(
    b: &mut Buf,
    name: &str,
    data: &[u8],
    c4_rate: i32,
    r#loop: bool,
    // Python default None (module-global `_DEFAULT_RENDER_MASKS`)
    render_masks: Option<&[u32]>,
) {
    let render_masks = render_masks.unwrap_or(&DEFAULT_RENDER_MASKS);
    let nibbles = data.len() * 2;
    let (size_off, start) = b.begin_block(b"SMP2");
    b.strz(name);
    b.u32(nibbles as i64);
    b.u32(c4_rate as i64);
    b.u32(c4_rate as i64);
    b.u8(SMP_DEPTH_ADPCM_K as i64);
    b.u8(0);
    b.u8(0);
    b.u8(0);
    b.s32(if r#loop { 0 } else { -1 });
    b.s32(if r#loop { nibbles as i64 } else { -1 });
    for &mask in render_masks {
        b.u32(mask as i64);
    }
    b.raw(data);
    b.end_block(size_off, start);
}

/// YM2610 ADPCM-A/B body.
pub fn _write_sample_adpcm(
    b: &mut Buf,
    name: &str,
    data: &[u8],
    c4_rate: i32,
    depth: i32,
    render_masks: &[u32],
    // Python default loop=False
    r#loop: bool,
) {
    let samples = data.len() * 2;
    let (size_off, start) = b.begin_block(b"SMP2");
    b.strz(name);
    b.u32(samples as i64);
    b.u32(c4_rate as i64);
    b.u32(c4_rate as i64);
    b.u8(depth as i64);
    b.u8(0);
    b.u8(0);
    b.u8(0);
    if r#loop {
        b.s32(0);
        b.s32(samples as i64);
    } else {
        b.s32(-1);
        b.s32(-1);
    }
    for &mask in render_masks {
        b.u32(mask as i64);
    }
    b.raw(data);
    b.end_block(size_off, start);
}

/// Furnace YMZ280B system flags: named clock select or custom clock.
pub fn ymz280b_flag_text(clock: u32) -> String {
    let clk: i64 = if clock != 0 { clock as i64 } else { 16_934_400 };
    let named: [(i64, i32); 5] = [
        (14_318_182, 1), // COLOR_NTSC * 4
        (14_187_580, 2), // COLOR_PAL * 16 / 5
        (16_000_000, 3),
        (16_666_667, 4), // 50 MHz / 3
        (14_000_000, 5),
    ];
    if clk == 16_934_400 {
        return String::new();
    }
    for &(value, sel) in &named {
        if clk == value {
            return format!("clockSel={}", sel);
        }
    }
    format!("customClock={}", clk)
}

/// Furnace MultiPCM flags: the player's *224/180 clock correction.
pub fn multipcm_flag_text(clock: u32) -> String {
    let raw: i64 = if clock != 0 { clock as i64 } else { 8_053_975 };
    let effective = py_int(py_round((raw as f64) * 224.0 / 180.0));
    if (effective - 10_000_000).abs() <= 2 {
        return String::new();
    }
    format!("customClock={}", effective)
}

/// Generic PCM DAC flags: 16-bit output at the sample's own rate.
pub fn pcm_dac_flag_text(
    rate: i64,
    // Python default stereo=False
    stereo: bool,
) -> String {
    format!(
        "rate={}\noutDepth=15\nstereo={}\nvolMax=255\ninterpolation=1",
        rate,
        if stereo { "true" } else { "false" }
    )
}

/// SegaPCM system flags: clock select / custom clock and ROM size.
pub fn segapcm_flag_text(clock: u32, rom_size: i64) -> String {
    let mut parts: Vec<String> = Vec::new();
    let clk: i64 = if clock != 0 { clock as i64 } else { SEGAPCM_CLOCK_X as i64 };
    if (clk - SEGAPCM_CLOCK_X as i64).abs() <= 4000 {
        parts.push("clockSel=0".to_string());
    } else if (clk - SEGAPCM_CLOCK_Y as i64).abs() <= 4000 {
        parts.push("clockSel=1".to_string());
    } else {
        parts.push(format!("customClock={}", clk));
    }
    // 0 = 2 MB (Y board), 1 = 512 KB (the rest).
    parts.push(format!("memSize={}", if rom_size > 0x80000 { 0 } else { 1 }));
    parts.join("\n")
}

/// RF5C68 system flags: chip type and clock.
pub fn rf5c68_flag_text(clock: u32, chip_type: u8) -> String {
    let mut parts = vec![format!("chipType={}", if chip_type != 0 { 1 } else { 0 })];
    let clk: i64 = if clock != 0 { clock as i64 } else { 8_000_000 };
    if clk == 8_000_000 {
        parts.push("clockSel=0".to_string());
    } else if clk == 10_000_000 {
        parts.push("clockSel=1".to_string());
    } else if clk == 12_500_000 {
        parts.push("clockSel=2".to_string());
    } else {
        parts.push(format!("customClock={}", clk));
    }
    parts.join("\n")
}

/// C140 system flags: bank type and Furnace-notation clock.
pub fn c140_flag_text(clock: u32, c140_type: u8) -> String {
    let mut parts = vec![format!("bankType={}", if c140_type == 1 { 1 } else { 0 })];
    let clk = crate::c140::c140_furnace_clock(clock);
    if clk != C140_FURNACE_DEFAULT_CLOCK {
        parts.push(format!("customClock={}", clk));
    }
    parts.join("\n")
}

/// C219 system flags: Furnace-notation custom clock only.
pub fn c219_flag_text(clock: u32) -> String {
    format!("customClock={}", crate::c219::c219_furnace_clock(clock))
}

/// Irem GA20 system flags.
pub fn ga20_flag_text(clock: u32) -> String {
    if (clock as i64 - 3_579_545).abs() <= 2 {
        String::new()
    } else {
        format!("customClock={}", clock)
    }
}

/// OKI MSM5232 system flags.
pub fn msm5232_flag_text(clock: u32) -> String {
    if (clock as i64 - 2_119_040).abs() <= 2 {
        String::new()
    } else {
        format!("customClock={}", clock)
    }
}

/// AY-8910 system flags: chip type and clock.
pub fn ay8910_flag_text(
    clock: u32,
    chip_type: u8,
    // Python default sunsoft5b=False
    sunsoft5b: bool,
) -> String {
    if sunsoft5b {
        return format!("chipType=2\ncustomClock={}", clock);
    }
    let mut parts = vec![format!(
        "chipType={}",
        if chip_type == 0x10 || chip_type == 0x11 { 1 } else { 0 }
    )];
    if crate::pitch::module_clock(clock as f64) != crate::pitch::FURNACE_CLOCK_DEFAULT {
        parts.push(format!("customClock={}", clock));
    }
    parts.join("\n")
}

/// SAA1099 system flags: one of Furnace's three clock presets or a clock.
pub fn saa1099_flag_text(clock: u32) -> String {
    let clk = if clock != 0 {
        clock as f64
    } else {
        crate::saa1099::SAA_DEFAULT_CLOCK as f64
    };
    let presets = [
        crate::saa1099::SAA_CLOCK_8M,
        crate::saa1099::SAA_CLOCK_NTSC,
        crate::saa1099::SAA_CLOCK_PAL,
    ];
    for (sel, preset) in presets.iter().enumerate() {
        if (clk - preset).abs() <= 2.0 {
            return format!("clockSel={}", sel);
        }
    }
    format!("customClock={}", py_int(py_round(clk)))
}

/// SCC custom clock, or "" for Furnace's COLOR_NTSC/2 default.
pub fn k051649_flag_text(clock: u32) -> String {
    if crate::pitch::module_clock(clock as f64) != crate::pitch::FURNACE_CLOCK_DEFAULT {
        return format!("customClock={}", clock);
    }
    String::new()
}

/// WSG custom clock, or "" for Furnace's COLOR_NTSC default.
pub fn k005289_flag_text(clock: u32) -> String {
    let clk: i64 = if clock != 0 { clock as i64 } else { 3_579_545 };
    if (clk - 3_579_545).abs() <= 2 {
        return String::new();
    }
    format!("customClock={}", clk)
}

/// POKEY system flags: NTSC default, PAL preset, or a custom clock.
pub fn pokey_flag_text(clock: u32) -> String {
    let clk = if clock != 0 {
        clock as f64
    } else {
        POKEY_DEFAULT_CLOCK
    };
    if (clk - POKEY_PAL_CLOCK).abs() <= 2.0 {
        return "clockSel=1".to_string();
    }
    if crate::pitch::module_clock(clk) == crate::pitch::FURNACE_CLOCK_DEFAULT {
        return String::new();
    }
    format!("customClock={}", py_int(py_round(clk)))
}

/// PC Engine clock, or "" for Furnace's COLOR_NTSC*2 default.
pub fn pce_flag_text(clock: u32) -> String {
    let chip_clock: i64 = 2 * (if clock != 0 { clock as i64 } else { 3_579_545 });
    let default = 2.0 * (315_000_000.0 / 88.0);
    if ((chip_clock as f64) - default).abs() <= 1.0 {
        return String::new();
    }
    format!("customClock={}", chip_clock)
}

/// K053260 clock select: 3.579545 MHz or 4 MHz, otherwise a custom clock.
pub fn k053260_flag_text(clock: u32) -> String {
    let clk: i64 = if clock != 0 { clock as i64 } else { 3_579_545 };
    if (clk - 3_579_545).abs() <= 2 {
        return "clockSel=0".to_string();
    }
    if clk == 4_000_000 {
        return "clockSel=1".to_string();
    }
    format!("customClock={}", clk)
}

/// Crystal for a C352 core that divides by 288.
pub fn c352_flag_text(
    clock: u32,
    // Python default quad=False
    quad: bool,
) -> String {
    let mut parts = vec![format!(
        "customClock={}",
        if clock != 0 { clock } else { crate::c352::C352_DEFAULT_CLOCK }
    )];
    if quad {
        parts.push("quadOutput=true".to_string());
    }
    parts.join("\n")
}

pub fn _write_flag(b: &mut Buf, text: &str) -> usize {
    let ptr = b.len();
    let (size_off, start) = b.begin_block(b"FLAG");
    b.strz(text);
    b.end_block(size_off, start);
    ptr
}

pub fn _write_empty_adir(b: &mut Buf) -> usize {
    let ptr = b.len();
    let (size_off, start) = b.begin_block(b"ADIR");
    b.u32(0);
    b.end_block(size_off, start);
    ptr
}

/// DIV_INS_QSOUND: one sample body with volume/pitch macros.
pub fn _write_ins_qsound(b: &mut Buf, name: &str, ins: &ToneIns) {
    let (size_off, start) = b.begin_block(b"INS2");
    b.u16(FUR_VERSION as i64);
    b.u16(INS_QSOUND as i64);
    b.raw(b"NA");
    let na_size = b.len();
    b.u16(0);
    let na_start = b.len();
    b.strz(name);
    let n = b.len() - na_start;
    patch_u16(b, na_size, n as i32);
    b.raw(b"SM");
    let sm_size = b.len();
    b.u16(0);
    let sm_start = b.len();
    b.u16((ins.sample.max(0) & 0xFFFF) as i64);
    b.u8(0x00); // QSound reads initSample; useSample is not required
    b.u8(0);
    let n = b.len() - sm_start;
    patch_u16(b, sm_size, n as i32);
    // MA: volume and pitch; the platform's pan/echo effects ride the pattern.
    let moving = !ins.vol.is_empty() || !ins.pitch.is_empty();
    if moving {
        b.raw(b"MA");
        let ma_size = b.len();
        b.u16(0);
        let ma_start = b.len();
        b.u16(8);
        if !ins.vol.is_empty() {
            let width = 2;
            _write_macro(
                b,
                0,
                &ins.vol,
                ins.vol_speed,
                ins.vol_loop,
                255,
                ins.vol_delay,
                width,
            );
        }
        if !ins.pitch.is_empty() {
            _write_macro(
                b,
                4,
                &ins.pitch,
                ins.pitch_speed,
                ins.pitch_loop,
                255,
                ins.pitch_delay,
                2,
            );
        }
        b.u8(255);
        let n = b.len() - ma_start;
        patch_u16(b, ma_size, n as i32);
    } else {
        _write_ma(b, None);
    }
    b.raw(b"EN");
    b.end_block(size_off, start);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fm::FMPatch;

    fn new_buf() -> Buf {
        Buf::new()
    }

    #[test]
    fn write_flag_block_magic_and_pointer() {
        let mut b = new_buf();
        b.raw(&[0xAA, 0xBB]);
        let ptr = _write_flag(&mut b, "clockSel=1");
        assert_eq!(ptr, 2);
        assert_eq!(&b.data[2..6], b"FLAG");
        let size = u32::from_le_bytes(b.data[6..10].try_into().unwrap());
        assert_eq!(size as usize, b.data.len() - 10);
        assert_eq!(&b.data[10..], b"clockSel=1\0");

        let empty = _write_empty_adir(&mut b);
        assert_eq!(empty, b.len() - 12);
        assert_eq!(&b.data[empty..empty + 4], b"ADIR");
        let size = u32::from_le_bytes(b.data[empty + 4..empty + 8].try_into().unwrap());
        assert_eq!(size, 4);
        assert_eq!(&b.data[empty + 8..empty + 12], &[0, 0, 0, 0]);
    }

    #[test]
    fn fm_feature_opm_byte_layout() {
        let patch = FMPatch::default();
        let mut b = new_buf();
        _write_fm_feature(&mut b, &patch);
        assert_eq!(&b.data[..2], b"FM");
        assert_eq!(u16::from_le_bytes(b.data[2..4].try_into().unwrap()), 36);
        assert_eq!(&b.data[4..8], &[0xF4, 0x00, 0x00, 0x20]);
        let op = [0x31u8, 0x7F, 0x1F, 0x00, 0x00, 0xF7, 0x00, 0x00];
        assert_eq!(&b.data[8..16], &op);
        assert_eq!(&b.data[16..24], &op);
        assert_eq!(&b.data[24..32], &op);
        // Algorithm 0's only carrier is operator 4 (index 3): KVS = 2.
        assert_eq!(
            &b.data[32..40],
            &[0x31, 0x7F, 0x1F, 0x00, 0x40, 0xF7, 0x00, 0x00]
        );
        assert_eq!(b.len(), 40);
    }

    #[test]
    fn sample_body_smp2_layout() {
        let mut b = new_buf();
        // PCM7 center is 64; 127 is the top. volume_scale 1.0 keeps them.
        _write_sample(&mut b, "s", &[64, 64, 127], 22050, 1.0, None);
        assert_eq!(&b.data[..4], b"SMP2");
        assert_eq!(u32::from_le_bytes(b.data[4..8].try_into().unwrap()) as usize, 45);
        assert_eq!(&b.data[8..10], b"s\0");
        assert_eq!(u32::from_le_bytes(b.data[10..14].try_into().unwrap()), 3);
        assert_eq!(u32::from_le_bytes(b.data[14..18].try_into().unwrap()), 22050);
        assert_eq!(u32::from_le_bytes(b.data[18..22].try_into().unwrap()), 22050);
        assert_eq!(b.data[22], 8); // SMP_DEPTH_8BIT
        assert_eq!(&b.data[23..26], &[0, 0, 0]);
        assert_eq!(i32::from_le_bytes(b.data[26..30].try_into().unwrap()), -1);
        assert_eq!(i32::from_le_bytes(b.data[30..34].try_into().unwrap()), -1);
        assert_eq!(
            &b.data[34..50],
            &[
                0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF,
                0xFF, 0xFF, 0xFF
            ]
        );
        assert_eq!(&b.data[50..], &[0, 0, 126]);
    }

    #[test]
    fn wave_block_stores_u32_values() {
        let mut b = new_buf();
        let ptr = _write_wave(&mut b, "w", &[0, 1, 2], 31);
        assert_eq!(ptr, 0);
        assert_eq!(&b.data[..4], b"WAVE");
        assert_eq!(u32::from_le_bytes(b.data[4..8].try_into().unwrap()) as usize, 26);
        assert_eq!(&b.data[8..10], b"w\0");
        assert_eq!(u32::from_le_bytes(b.data[10..14].try_into().unwrap()), 3);
        assert_eq!(u32::from_le_bytes(b.data[14..18].try_into().unwrap()), 0);
        assert_eq!(u32::from_le_bytes(b.data[18..22].try_into().unwrap()), 31);
        assert_eq!(
            &b.data[22..34],
            &[0, 0, 0, 0, 1, 0, 0, 0, 2, 0, 0, 0]
        );
    }

    #[test]
    fn flag_text_hand_computed() {
        // 16934400 is the default: no flag text at all.
        assert_eq!(ymz280b_flag_text(16_934_400), "");
        assert_eq!(ymz280b_flag_text(16_000_000), "clockSel=3");
        assert_eq!(ymz280b_flag_text(12_345), "customClock=12345");
        assert_eq!(k053260_flag_text(3_579_545), "clockSel=0");
        assert_eq!(k053260_flag_text(4_000_000), "clockSel=1");
        assert_eq!(k053260_flag_text(8_000_000), "customClock=8000000");
        // 1789772 compares within one hertz of Furnace's AY default clock.
        assert_eq!(ay8910_flag_text(1_789_772, 0x10, false), "chipType=1");
        assert_eq!(
            ay8910_flag_text(1_789_772, 0x10, true),
            "chipType=2\ncustomClock=1789772"
        );
        assert_eq!(
            pcm_dac_flag_text(44100, false),
            "rate=44100\noutDepth=15\nstereo=false\nvolMax=255\ninterpolation=1"
        );
        assert_eq!(
            pcm_dac_flag_text(22020, true),
            "rate=22020\noutDepth=15\nstereo=true\nvolMax=255\ninterpolation=1"
        );
    }
}
