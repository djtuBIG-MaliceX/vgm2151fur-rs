//! YM2203 (OPN) support: pitch mapping and the three-channel FM state.
//!
//! The YM2203's FM half is the three-channel OPN core: the same register layout
//! as the YM2612 without the DAC and with registers 0x70 (sustain rate) and 0x90
//! (SSG-EG) instead of the OPM's DT2.  Furnace runs the YM2203 platform in
//! linear-pitch mode; a pattern note becomes `note << 7` engine units, E5 adds
//! 1/128-semitone units, and the platform converts that to block/fnum.  The
//! helpers here invert that mapping, so a converted note writes back the source's
//! frequency register.

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex, MutexGuard};

use crate::fm::{FMPatch, OPN_CARRIERS, OPN_OP_TO_MASK, OpRegs};
use crate::parity::{py_floor_div, py_int, py_round};

pub const YM2203_DEFAULT_CLOCK: u32 = 3_579_545;
pub const YM2203_SSG_DIVIDER: i32 = 16;

// DivPlatformOPN's fmFreqBase (144 * 32768, rounded by the OPN's nominal
// 4.72 MHz rate).
pub const OPN_FREQ_BASE: f64 = 4_720_270.0;

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
struct FnumKey {
    note: i32,
    clock: u32,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
struct NoteKey {
    block: i32,
    fnum: i32,
    clock: u32,
}

static _FNUM_CACHE: LazyLock<Mutex<HashMap<FnumKey, (i32, i32)>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static _NOTE_CACHE: LazyLock<Mutex<HashMap<NoteKey, (i32, i32)>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn lock<T>(cache: &Mutex<T>) -> MutexGuard<'_, T> {
    cache.lock().expect("YM2203 note cache poisoned")
}

pub const _MAX_FNUM: i32 = 2047;

/// Furnace's (block, fnum) for a pattern note on this clock.
///
/// A literal port of the `NOTE_FNUM_BLOCK(note, 11, 0)` macro: the note
/// picks the block, the OPN frequency base turns it into an 11-bit fnum and
/// the boundary cases move the block.
pub fn opn_note_fnum(note: i32, clock: u32) -> (i32, i32) {
    let note = note.clamp(0, 179);
    let clock = if clock == 0 {
        YM2203_DEFAULT_CLOCK
    } else {
        clock
    };
    let key = FnumKey { note, clock };
    {
        let cache = lock(&_FNUM_CACHE);
        if let Some(hit) = cache.get(&key) {
            return *hit;
        }
    }
    let base = 440.0 * 2.0_f64.powf((note - 60 + 3) as f64 / 12.0);
    // The module keeps Furnace's default linear pitch mode, where the note
    // is converted with round() rather than the legacy f-num path's
    // truncation.
    let mut bf = py_int(py_round(base * (OPN_FREQ_BASE / clock as f64)));
    let mut block = py_floor_div((note - 60) as i64, 12) as i32;
    block = block.clamp(0, 7);
    bf >>= block;
    if bf < 0 {
        bf = 0;
    }
    let boundary_bottom = py_int(440.0 * 2.0_f64.powf(0.25) * (OPN_FREQ_BASE / clock as f64));
    let boundary_top = py_int(2.0 * 440.0 * 2.0_f64.powf(0.25) * (OPN_FREQ_BASE / clock as f64));
    while bf > 0 && bf < boundary_bottom && block > 0 {
        bf <<= 1;
        block -= 1;
    }
    if bf > boundary_top {
        while block < 7 && bf > boundary_top {
            bf >>= 1;
            block += 1;
        }
        if bf > _MAX_FNUM as i64 {
            bf = _MAX_FNUM as i64;
        }
    }
    let out = (block, bf as i32);
    lock(&_FNUM_CACHE).insert(key, out);
    out
}

/// Source (block, fnum) -> (pattern note, E5 fine-tune byte).
///
/// The note search uses the same forward mapping Furnace uses, so the pair
/// (note, E5) reproduces the source's frequency to the E5 resolution of
/// 1/128 semitone.
pub fn opn_note_for(block: i32, fnum: i32, clock: u32) -> (i32, i32) {
    let clock = if clock == 0 {
        YM2203_DEFAULT_CLOCK
    } else {
        clock
    };
    let block = block.clamp(0, 7);
    let fnum = fnum.clamp(1, _MAX_FNUM);
    let key = NoteKey { block, fnum, clock };
    {
        let cache = lock(&_NOTE_CACHE);
        if let Some(hit) = cache.get(&key) {
            return *hit;
        }
    }
    let mut best: Option<(f64, i32, i32, i32)> = None;
    for note in 0..180 {
        let (b, f) = opn_note_fnum(note, clock);
        if f <= 0 {
            continue;
        }
        // fnum the source would have at this note's block
        let target = fnum as f64 * 2.0_f64.powf((block - b) as f64);
        if target <= 0.0 {
            continue;
        }
        // The module keeps Furnace's default (linear) pitch mode, where E5
        // is 1/128 semitone and the OPN platform feeds it through the same
        // exponential mapping as a pattern note.  Convert the residual ratio
        // to those units instead of adding raw f-num steps.
        let off = py_int(py_round(1536.0 * (target / f as f64).log2())).clamp(-128, 127) as i32;
        let actual = f as f64 * 2.0_f64.powf(off as f64 / 1536.0);
        let err = (actual / target).log2().abs();
        let cand = (err, off.abs(), note, off);
        if best.is_none_or(|b| cand < b) {
            best = Some(cand);
        }
    }
    let (_err, _a, note, off) = best.expect("opn_note_for found no note");
    let out = (note, 0x80 + off);
    lock(&_NOTE_CACHE).insert(key, out);
    out
}

/// Pitch in 1/64 semitones; only differences are meaningful.
pub fn opn_pitch_units(block: i32, fnum: i32) -> f64 {
    let fnum = if fnum <= 0 { 1 } else { fnum };
    768.0 * ((fnum as f64).log2() + block as f64)
}

/// Register image of one YM2203's FM half.
#[derive(Clone, Debug, PartialEq)]
pub struct YM2203State {
    pub fb_alg: [i32; 3],
    pub fnum: [i32; 3],
    pub block: [i32; 3],
    pub ops: [[OpRegs; 4]; 3],
    pub kon: [bool; 3],
    pub op_mask: [i32; 3],
    // Channel 3 can switch to per-operator frequencies (3-slot mode,
    // 0x27 bit 6/7 plus 0xA8-0xAE).  Furnace's six-voice system has one
    // note per channel, so the converter uses one of the three extra
    // operators as the channel pitch and reports the approximation.
    pub special: bool,
    pub special_lo: [i32; 3],
    pub special_hi: [i32; 3],
    pub special_set: [bool; 3],
}

impl YM2203State {
    pub fn new() -> Self {
        // A YM2203 powers up with all FM registers at zero.  This differs from
        // the OPM patch defaults (TL 0x7F, RR 0xF7) and matters when a driver
        // writes only part of a patch: hardware AR=0 is silent, while a stale
        // OPM default would make the operator audible.
        let blank = OpRegs {
            dt_mul: 0,
            tl: 0,
            ks_ar: 0,
            am_d1r: 0,
            dt2_d2r: 0,
            d1l_rr: 0,
            sr: 0,
            ssg_eg: 0,
            ..OpRegs::default()
        };
        YM2203State {
            fb_alg: [0; 3],
            fnum: [0; 3],
            block: [0; 3],
            ops: core::array::from_fn(|_| core::array::from_fn(|_| blank.clone())),
            kon: [false; 3],
            op_mask: [0; 3],
            special: false,
            special_lo: [0; 3],
            special_hi: [0; 3],
            special_set: [false; 3],
        }
    }

    /// (channel, operator) for the 0x30-0x9E operator pages.
    fn _op_index(reg: i32) -> Option<(usize, usize)> {
        if !(0x30..=0x9E).contains(&reg) {
            return None;
        }
        let ch = reg & 3;
        if ch >= 3 {
            return None;
        }
        Some((ch as usize, ((reg >> 2) & 3) as usize))
    }

    pub fn write(&mut self, reg: i32, val: i32) {
        if let Some((ch, idx)) = Self::_op_index(reg) {
            let mut o = self.ops[ch][idx].clone();
            let page = reg & 0xF0;
            match page {
                0x30 => o.dt_mul = val as u8,
                0x40 => o.tl = val as u8,
                0x50 => o.ks_ar = val as u8,
                0x60 => o.am_d1r = val as u8,
                // On OPN 0x70 is the sustain rate; the E5 field in the
                // instrument carries the raw 5-bit register.
                0x70 => o.sr = (val & 0x1F) as u8,
                0x80 => o.d1l_rr = val as u8,
                0x90 => o.ssg_eg = (val & 0x0F) as u8,
                _ => {}
            }
            self.ops[ch][idx] = o;
            return;
        }
        if reg == 0x27 {
            // Bit 6/7 select CSM / 3-slot mode.  Only the per-operator
            // frequency part changes the pitch model here.
            self.special = (val & 0xC0) != 0;
            return;
        }
        if (0xA0..=0xA6).contains(&reg) {
            let ch = (reg & 3) as usize;
            if ch < 3 {
                if reg < 0xA4 {
                    self.fnum[ch] = (self.fnum[ch] & 0x700) | (val & 0xFF);
                } else {
                    self.fnum[ch] = (self.fnum[ch] & 0xFF) | ((val & 0x07) << 8);
                    self.block[ch] = (val >> 3) & 0x07;
                }
            }
            return;
        }
        if (0xA8..=0xAE).contains(&reg) {
            let idx = (reg & 3) as usize;
            if idx < 3 {
                if reg <= 0xAA {
                    self.special_lo[idx] = val & 0xFF;
                } else {
                    self.special_hi[idx] = val;
                }
                self.special_set[idx] = true;
            }
            return;
        }
        if (0xB0..=0xB2).contains(&reg) {
            self.fb_alg[(reg - 0xB0) as usize] = val;
        }
    }

    pub fn frequency(&self, ch: usize) -> (i32, i32) {
        if ch == 2 && self.special {
            for idx in 0..3 {
                if self.special_set[idx] {
                    let hi = self.special_hi[idx];
                    return ((hi >> 3) & 0x07, self.special_lo[idx] | ((hi & 0x07) << 8));
                }
            }
        }
        (self.block[ch], self.fnum[ch])
    }

    /// (block, fnum) for one operator of a 3-slot channel-3 voice.
    ///
    /// Falls back to the normal channel frequency until the driver has
    /// written that operator's per-operator registers.
    pub fn frequency_op(&self, ch: usize, op: i32) -> (i32, i32) {
        if ch == 2 && self.special && (0..3).contains(&op) && self.special_set[op as usize] {
            let hi = self.special_hi[op as usize];
            return (
                (hi >> 3) & 0x07,
                self.special_lo[op as usize] | ((hi & 0x07) << 8),
            );
        }
        (self.block[ch], self.fnum[ch])
    }

    /// Patch image; operators outside `mask` (default 0xF) get TL 0x7F.
    pub fn snapshot(&self, ch: usize, mask: i32) -> FMPatch {
        let fb_alg = self.fb_alg[ch];
        let mut ops = [(); 4].map(|_| OpRegs::default());
        for (i, src) in self.ops[ch].iter().enumerate() {
            let mut op = src.clone();
            if mask & (1 << OPN_OP_TO_MASK[i]) == 0 {
                op.tl = 0x7F;
            }
            ops[i] = op;
        }
        FMPatch {
            alg: fb_alg & 0x07,
            fb: (fb_alg >> 3) & 0x07,
            ams: 0,
            pms: 0,
            ops,
            opn: true,
            ..FMPatch::default()
        }
    }

    pub fn carrier_tl(&self, ch: usize) -> i32 {
        let alg = self.fb_alg[ch] & 0x07;
        let carriers: &[i32] = OPN_CARRIERS.get(alg as usize).copied().unwrap_or(&[3]);
        carriers
            .iter()
            .map(|i| self.ops[ch][*i as usize].tl as i32)
            .min()
            .expect("OPN carrier set is never empty")
    }
}

impl Default for YM2203State {
    fn default() -> Self {
        Self::new()
    }
}

/// Octave of the loudest audible carrier, relative to the channel f-num.
///
/// The pattern note should show the pitch that is actually heard.  Use the
/// carrier with the lowest TL (the loudest); equally loud carriers tie-break
/// to the highest power-of-two ratio, which keeps the historical MULT=2
/// label on patches whose carriers are mixed at equal volume.  Unkeyed
/// operators arrive here with TL=0x7F and are ignored.
///
/// Return 0 when the loudest carrier is not an exact power of two (MULT
/// 3/5/6/...), or when every carrier is silent: the channel note already is
/// the best label and nothing should move.
pub fn opn_carrier_octave(patch: &FMPatch) -> i32 {
    if !patch.opn {
        return 0;
    }
    let mut best_tl: i32 = 0x80;
    let mut best_octave: Option<i32> = None;
    for i in OPN_CARRIERS
        .get((patch.alg & 7) as usize)
        .copied()
        .unwrap_or(&[])
    {
        let op = &patch.ops[*i as usize];
        let tl = i32::from(op.tl & 0x7F);
        if tl >= 0x7F {
            continue;
        }
        let mult = op.dt_mul & 0x0F;
        let ratio = if mult == 0 { 0.5 } else { f64::from(mult) };
        let octave = if ratio == 0.5 || ratio == 1.0 || ratio == 2.0 || ratio == 4.0 || ratio == 8.0
        {
            Some(py_int(py_round(ratio.log2())) as i32)
        } else {
            None
        };
        if tl < best_tl {
            best_tl = tl;
            best_octave = octave;
        } else if tl == best_tl && octave.is_some() {
            if best_octave.is_none() || octave.unwrap() > best_octave.unwrap() {
                best_octave = octave;
            }
        }
    }
    best_octave.unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn almost(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-9
    }

    #[test]
    fn opn_pitch_units_matches_reference() {
        // Values observed from vgm2151fur/ym2203.py.
        assert_eq!(opn_pitch_units(4, 1024), 10752.0);
        assert_eq!(opn_pitch_units(4, 512), 9984.0);
        assert_eq!(opn_pitch_units(0, 1), 0.0);
        assert!(almost(opn_pitch_units(7, 2047), 13823.458857233978));
        assert!(almost(opn_pitch_units(2, 0), 1536.0));
        assert!(almost(opn_pitch_units(3, 100), 7406.481553746988));
    }

    #[test]
    fn opn_note_fnum_matches_reference() {
        let clock = YM2203_DEFAULT_CLOCK;
        for (note, expect) in [
            (0, (0, 22)),
            (36, (0, 173)),
            (48, (0, 345)),
            (60, (0, 690)),
            (69, (0, 1160)),
            (72, (1, 690)),
            (84, (2, 690)),
            (96, (3, 690)),
            (120, (5, 690)),
            (179, (7, 2047)),
        ] {
            assert_eq!(opn_note_fnum(note, clock), expect, "note {note}");
        }
        // A zero clock means the default clock.
        assert_eq!(opn_note_fnum(60, 0), opn_note_fnum(60, clock));
    }

    #[test]
    fn opn_note_for_matches_reference() {
        let clock = YM2203_DEFAULT_CLOCK;
        for (block, fnum, expect) in [
            (0, 500, (55, 54)),
            (1, 1024, (78, 234)),
            (2, 1500, (97, 185)),
            (3, 2047, (114, 236)),
            (7, 1, (31, 111)),
            (4, 256, (90, 237)),
        ] {
            assert_eq!(
                opn_note_for(block, fnum, clock),
                expect,
                "block {block} fnum {fnum}"
            );
        }
        // The E5 byte is 0x80 plus the residual; 1/128 semitone resolution.
        let (note, e5) = opn_note_for(4, 1024, clock);
        assert_eq!((note, e5), (114, 237));
        assert!((0..=255).contains(&e5));
    }

    #[test]
    fn writes_build_frequency_patch_and_mask() {
        let mut s = YM2203State::new();
        s.write(0x30, 0x71);
        s.write(0x40, 0x20);
        s.write(0x50, 0x1F);
        s.write(0x60, 0x0A);
        s.write(0x70, 0x13);
        s.write(0x80, 0x08);
        s.write(0x90, 0x0C);
        s.write(0xA4, 0x1A);
        s.write(0xA0, 0xCD);
        s.write(0xB0, 0x0E);

        assert_eq!(s.frequency(0), (3, 717));
        assert_eq!(s.carrier_tl(0), 0);
        let p = s.snapshot(0, 0xF);
        assert_eq!((p.alg, p.fb, p.opn), (6, 1, true));
        assert_eq!(p.ops[0].tl, 0x20);
        assert_eq!(p.ops[0].dt_mul, 0x71);
        assert_eq!(p.ops[0].sr, 0x13);
        assert_eq!(p.ops[0].ssg_eg, 0x0C);

        let masked = s.snapshot(0, 0x5);
        assert_eq!(
            masked.ops.iter().map(|o| o.tl).collect::<Vec<_>>(),
            vec![0x20, 0x00, 0x7F, 0x7F]
        );
        // Blank operators are TL 0 with MULT 0, so the power-of-two carrier
        // ratio is 0.5: the octave is -1, not the default 0.
        assert_eq!(opn_carrier_octave(&p), -1);
    }

    #[test]
    fn channel_three_special_mode_uses_per_operator_frequencies() {
        let mut s = YM2203State::new();
        s.write(0x27, 0x40);
        assert!(s.special);
        s.write(0xA8, 0x11);
        s.write(0xAC, 0x22);
        assert_eq!(s.frequency(2), (4, 0x211));
        assert_eq!(s.frequency_op(2, 0), (4, 0x211));
        // Unwritten groups fall back to the normal channel frequency.
        assert_eq!(s.frequency_op(2, 1), (0, 0));
        // Channel 3 special mode does not move the other channels.
        assert_eq!(s.frequency(0), (0, 0));

        s.write(0x27, 0x00);
        assert!(!s.special);
        assert_eq!(s.frequency(2), (0, 0));
    }
}
