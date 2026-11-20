//! YM2612 (OPN2) support: pitch mapping and the six-channel FM state.
//!
//! The YM2612 is the six-channel OPN2 core: the same operator register layout
//! as the YM2203's FM half (0x70 is the sustain rate, 0x90 is SSG-EG, there is
//! no DT2), plus a second register bank for channels 4-6, a stereo/LFO
//! sensitivity register and one DAC channel.
//!
//! Furnace runs its YM2612 platform in linear-pitch mode. It uses
//! `DivPlatformGenesis`'s `fmFreqBase` of 9437184, twice the OPN value, so the
//! same pattern note number is one octave above a YM2203's. The helpers here
//! invert the platform's own note conversion, not a generic OPN formula, so
//! the exported f-num/block is the source's.

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex, MutexGuard};

use crate::fm::{FMPatch, OpRegs, OPN_CARRIERS, OPN_OP_TO_MASK};
use crate::parity::{py_int, py_round};

pub use crate::vgm::YM2612_DEFAULT_CLOCK;

// `DivPlatformGenesis`'s fmFreqBase. `CONVERT_FNUM_BLOCK` in Furnace turns a
// linear-pitch note into `round(fbase * (divider / clock))` and shifts the
// note's block out of it.
pub const YM2612_FREQ_BASE: f64 = 9437184.0;

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
struct FnumKey {
    note: i32,
    clock: u32,
    e5: i32,
    freq_base: u64,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
struct NoteKey {
    block: i32,
    fnum: i32,
    clock: u32,
    freq_base: u64,
}

static _FNUM_CACHE: LazyLock<Mutex<HashMap<FnumKey, (i32, i32)>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static _NOTE_CACHE: LazyLock<Mutex<HashMap<NoteKey, (i32, i32)>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn lock<T>(cache: &Mutex<T>) -> MutexGuard<'_, T> {
    cache.lock().expect("YM2612 note cache poisoned")
}

pub const _MAX_FNUM: i32 = 2047;

// Furnace's ext columns write their frequency to operator (1,2,0,3)[c]:
// column 0 -> 0xA9/0xAD, column 1 -> 0xAA/0xAE, column 2 -> 0xA8/0xAC,
// column 3 -> the normal channel f-num. The state stores the three
// supplementary pairs by (register & 3), so 0 = 0xA8/0xAC, 1 = 0xA9/0xAD,
// 2 = 0xAA/0xAE, 3 = normal.
pub const EXT_FREQ_OP: [usize; 4] = [1, 2, 0, 3];

/// Furnace's (block, fnum) for a pattern note and E5 byte.
///
/// A literal port of `NOTE_FNUM_BLOCK(note, 11, 0)` through `calcFreq` with
/// the platform's frequency base: the E5 value joins the note in `nbase`
/// before the exponential, which is also why negative fine tune can move the
/// block down. The YM2610 shares the formula with a different base.
pub fn ym2612_note_fnum(
    note: i32,
    clock: u32,
    e5: i32,
    freq_base: f64,
) -> (i32, i32) {
    let note = note.clamp(0, 179);
    let clock = if clock == 0 {
        YM2612_DEFAULT_CLOCK
    } else {
        clock
    };
    let key = FnumKey {
        note,
        clock,
        e5,
        freq_base: freq_base.to_bits(),
    };
    {
        let cache = lock(&_FNUM_CACHE);
        if let Some(hit) = cache.get(&key) {
            return *hit;
        }
    }
    let nbase = note * 128 + (e5.clamp(0, 255) - 0x80);
    let fbase = 440.0 * 2.0_f64.powf((nbase + 384 - 7680) as f64 / 1536.0);
    // The module keeps Furnace's default linear pitch mode, where the note is
    // converted with round() rather than the legacy f-num path's truncation.
    let mut bf = py_int(py_round(fbase * (freq_base / clock as f64)));
    let nnote = nbase >> 7;
    let mut block = crate::parity::py_floor_div((nnote - 60) as i64, 12) as i32;
    block = block.clamp(0, 7);
    bf >>= block;
    if bf < 0 {
        bf = 0;
    }
    let mut boundary_bottom = py_int(440.0 * 2.0_f64.powf(0.25) * (freq_base / clock as f64));
    let mut boundary_top =
        py_int(2.0 * 440.0 * 2.0_f64.powf(0.25) * (freq_base / clock as f64));
    // CONVERT_FNUM_BLOCK scales both boundaries down until the top one fits
    // the 11-bit f-num; the block shift then chooses the same octave.
    while boundary_top > _MAX_FNUM as i64 {
        boundary_top >>= 1;
        boundary_bottom >>= 1;
    }
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
/// The note search uses the platform's forward mapping, so the pair
/// (note, E5) reproduces the source's frequency to the E5 resolution of
/// 1/128 semitone (one semitone of range). The YM2610 passes its own
/// frequency base.
pub fn ym2612_note_for(block: i32, fnum: i32, clock: u32, freq_base: f64) -> (i32, i32) {
    let clock = if clock == 0 {
        YM2612_DEFAULT_CLOCK
    } else {
        clock
    };
    let block = block.clamp(0, 7);
    let fnum = fnum.clamp(1, _MAX_FNUM);
    let key = NoteKey {
        block,
        fnum,
        clock,
        freq_base: freq_base.to_bits(),
    };
    {
        let cache = lock(&_NOTE_CACHE);
        if let Some(hit) = cache.get(&key) {
            return *hit;
        }
    }
    let mut best: Option<(f64, i32, i32, i32)> = None;
    let want = ym2612_pitch_units(block, fnum);
    for note in 0..180 {
        let (b, f) = ym2612_note_fnum(note, clock, 0x80, freq_base);
        if f <= 0 {
            continue;
        }
        // f-num the source would have at this note's block
        let target = fnum as f64 * 2.0_f64.powf((block - b) as f64);
        if target <= 0.0 {
            continue;
        }
        // Linear pitch mode: E5 is 1/128 semitone and goes through the same
        // exponential mapping as the note.
        let mut off = py_int(py_round(1536.0 * (target / f as f64).log2())) as i32;
        off = off.clamp(-128, 127);
        // Score the pair through the platform's own formula, not the
        // un-rounded ratio, so the exported byte is the predicted one.
        let (ab, af) = ym2612_note_fnum(note, clock, 0x80 + off, freq_base);
        let err = (ym2612_pitch_units(ab, af) - want).abs();
        let cand = (err, off.abs(), note, off);
        if best.is_none() || cand < best.unwrap() {
            best = Some(cand);
        }
    }
    let (_err, _a, note, off) = best.expect("YM2612 note search is non-empty");
    let out = (note, 0x80 + off);
    lock(&_NOTE_CACHE).insert(key, out);
    out
}

/// Pitch in 1/64 semitones; only differences are meaningful.
pub fn ym2612_pitch_units(block: i32, fnum: i32) -> f64 {
    let fnum = if fnum <= 0 { 1 } else { fnum };
    768.0 * ((fnum as f64).log2() + block as f64)
}

fn _blank_op() -> OpRegs {
    // A YM2612 powers up with every register at zero. A driver that writes
    // only part of a patch must not inherit an audible OPM default.
    OpRegs {
        dt_mul: 0,
        tl: 0,
        ks_ar: 0,
        am_d1r: 0,
        dt2_d2r: 0,
        d1l_rr: 0,
        sr: 0,
        ssg_eg: 0,
        ..OpRegs::default()
    }
}

/// Register image of one YM2612.
#[derive(Clone, Debug, PartialEq)]
pub struct YM2612State {
    pub fnum: [i32; 6],
    pub block: [i32; 6],
    pub fb_alg: [i32; 6],
    // 0xB4: L/R output, AMS (bits 4-5), PMS (bits 0-2). ymfm resets the
    // OPN2 to 0xC0 (both outputs on).
    pub pan_pms_ams: [i32; 6],
    pub ops: [[OpRegs; 4]; 6],
    pub kon: [bool; 6],
    pub op_mask: [i32; 6],
    pub mode: i32, // 0x27 bits 6-7: 0 normal, 1 special, 2 CSM
    pub lfo: i32,  // 0x22
    pub dac_enabled: bool,
    pub dac_data: i32,
    // The frequency registers are latched pairs: the high byte is held until
    // a low write commits it (ymfm's opn register model).
    pub _hi: [Option<i32>; 6],
    // Channel 3's supplementary frequencies (0xA8/AC, 0xA9/AD, 0xAA/AE).
    pub special: [i32; 3],
    pub special_block: [i32; 3],
    pub special_set: [bool; 3],
    pub _special_hi: [Option<i32>; 3],
}

impl YM2612State {
    pub fn new() -> Self {
        YM2612State {
            fnum: [0; 6],
            block: [0; 6],
            fb_alg: [0; 6],
            pan_pms_ams: [0xC0; 6],
            ops: core::array::from_fn(|_| core::array::from_fn(|_| _blank_op())),
            kon: [false; 6],
            op_mask: [0; 6],
            mode: 0,
            lfo: 0,
            dac_enabled: false,
            dac_data: 0,
            _hi: [None; 6],
            special: [0; 3],
            special_block: [0; 3],
            special_set: [false; 3],
            _special_hi: [None; 3],
        }
    }

    /// True in channel-3 special mode and in CSM mode (ymfm's rule).
    pub fn multi_freq(&self) -> bool {
        self.mode == 1 || self.mode == 2
    }

    fn _write_op(&mut self, ch: usize, idx: usize, page: i32, val: i32) {
        let mut o = self.ops[ch][idx].clone();
        match page {
            0x30 => o.dt_mul = val as u8,
            0x40 => o.tl = (val & 0x7F) as u8,
            0x50 => o.ks_ar = val as u8,
            0x60 => o.am_d1r = val as u8,
            // OPN 0x70 is the 5-bit sustain rate.
            0x70 => o.sr = (val & 0x1F) as u8,
            0x80 => o.d1l_rr = val as u8,
            0x90 => o.ssg_eg = (val & 0x0F) as u8,
            _ => {}
        }
        self.ops[ch][idx] = o;
    }

    pub fn write(&mut self, reg: i32, val: i32) {
        let bank = if reg & 0x100 != 0 { 0x100 } else { 0 };
        let r = reg & 0xFF;
        if bank == 0 {
            if r == 0x22 {
                self.lfo = val;
                return;
            }
            if r == 0x27 {
                self.mode = (val >> 6) & 0x03;
                return;
            }
            if r == 0x2A {
                self.dac_data = val;
                return;
            }
            if r == 0x2B {
                self.dac_enabled = (val & 0x80) != 0;
                return;
            }
        }
        if (0x30..=0x9E).contains(&r) {
            let ch = ((r & 0x03) + if bank != 0 { 3 } else { 0 }) as usize;
            if (r & 0x03) == 3 {
                return;
            }
            self._write_op(ch, ((r >> 2) & 0x03) as usize, r & 0xF0, val);
            return;
        }
        if (0xA0..=0xA6).contains(&r) {
            let ch = ((r & 0x03) + if bank != 0 { 3 } else { 0 }) as usize;
            if (r & 0x03) == 3 {
                return;
            }
            if r < 0xA4 {
                let hi = self._hi[ch];
                if let Some(hi) = hi {
                    self.fnum[ch] = val | ((hi & 0x07) << 8);
                    self.block[ch] = (hi >> 3) & 0x07;
                    self._hi[ch] = None;
                }
            } else {
                self._hi[ch] = Some(val);
            }
            return;
        }
        if bank == 0 && (0xA8..=0xAE).contains(&r) {
            let idx = (r & 0x03) as usize;
            if idx == 3 {
                return;
            }
            if r < 0xAC {
                let hi = self._special_hi[idx];
                if let Some(hi) = hi {
                    self.special[idx] = val | ((hi & 0x07) << 8);
                    self.special_block[idx] = (hi >> 3) & 0x07;
                    self.special_set[idx] = true;
                    self._special_hi[idx] = None;
                }
            } else {
                self._special_hi[idx] = Some(val);
            }
            return;
        }
        if (0xB0..=0xB2).contains(&r) {
            let ch = ((r - 0xB0) + if bank != 0 { 3 } else { 0 }) as usize;
            self.fb_alg[ch] = val;
            return;
        }
        if (0xB4..=0xB6).contains(&r) {
            let ch = ((r - 0xB4) + if bank != 0 { 3 } else { 0 }) as usize;
            self.pan_pms_ams[ch] = val;
            return;
        }
    }

    /// The channel f-num/block; in special mode channel 3's first pair.
    pub fn frequency(&self, ch: usize) -> (i32, i32) {
        if ch == 2 && self.multi_freq() {
            for idx in 0..3 {
                if self.special_set[idx] {
                    return (self.special_block[idx], self.special[idx]);
                }
            }
        }
        (self.block[ch], self.fnum[ch])
    }

    /// (block, fnum) for frequency group `op` of a special channel-3.
    ///
    /// Group 0 is 0xA8/0xAC, 1 is 0xA9/0xAD, 2 is 0xAA/0xAE and 3 is the
    /// normal channel f-num. Falls back to the normal frequency until the
    /// driver has written that pair.
    pub fn frequency_op(&self, ch: usize, op: i32) -> (i32, i32) {
        if ch == 2
            && self.multi_freq()
            && (0..3).contains(&op)
            && self.special_set[op as usize]
        {
            return (
                self.special_block[op as usize],
                self.special[op as usize],
            );
        }
        (self.block[ch], self.fnum[ch])
    }

    pub fn snapshot(&self, ch: usize, mask: i32) -> FMPatch {
        let fb_alg = self.fb_alg[ch];
        let b4 = self.pan_pms_ams[ch];
        let ops: [OpRegs; 4] = core::array::from_fn(|i| {
            let mut op = self.ops[ch][i].clone();
            if mask & (1 << OPN_OP_TO_MASK[i]) == 0 {
                op.tl = 0x7F;
            }
            op
        });
        FMPatch {
            alg: fb_alg & 0x07,
            fb: (fb_alg >> 3) & 0x07,
            ams: (b4 >> 4) & 0x03,
            pms: b4 & 0x07,
            ops,
            opn: true,
            ..FMPatch::default()
        }
    }

    pub fn pan_byte(&self, ch: usize) -> i32 {
        let b4 = self.pan_pms_ams[ch];
        let left = b4 & 0x80 != 0;
        let right = b4 & 0x40 != 0;
        if left && right {
            return 0xFF;
        }
        if left {
            return 0xF0;
        }
        if right {
            return 0x0F;
        }
        0x00
    }

    pub fn carrier_tl(&self, ch: usize) -> i32 {
        let alg = self.fb_alg[ch] & 0x07;
        let carriers: &[i32] = OPN_CARRIERS
            .get(alg as usize)
            .copied()
            .unwrap_or(&[3]);
        carriers
            .iter()
            .map(|i| self.ops[ch][*i as usize].tl as i32)
            .min()
            .unwrap()
    }
}

impl Default for YM2612State {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blank_state_matches_the_register_image() {
        let st = YM2612State::new();
        assert_eq!(st.fnum, [0; 6]);
        assert_eq!(st.block, [0; 6]);
        assert_eq!(st.pan_pms_ams, [0xC0; 6]);
        assert_eq!(st.mode, 0);
        assert!(!st.multi_freq());
        for ch in 0..6 {
            for op in &st.ops[ch] {
                assert_eq!(op.dt_mul, 0);
                assert_eq!(op.tl, 0);
                assert_eq!(op.sr, 0);
                assert_eq!(op.ssg_eg, 0);
                // Dataclass defaults for fields _blank_op does not pass.
                assert_eq!(op.mult, 1);
                assert_eq!(op.dr, 15);
            }
            assert_eq!(st.pan_byte(ch), 0xFF);
        }
    }

    #[test]
    fn frequency_latches_bank_and_channel() {
        let mut st = YM2612State::new();
        st.write(0xA4, 0x25);
        st.write(0xA0, 0xAA);
        assert_eq!(st.frequency(0), (4, 0x5AA));
        // Second bank maps to channels 4-6.
        st.write(0x1A5, 0x11);
        st.write(0x1A1, 0x22);
        assert_eq!(st.frequency(4), (2, 0x122));
        // Operator page 0x30 with low bits 3 is the invalid slot: ignored.
        st.write(0x33, 0xFF);
        assert_eq!(st.frequency(3), (0, 0));
    }

    #[test]
    fn operator_pages_and_masks() {
        let mut st = YM2612State::new();
        st.write(0x30, 0x3B);
        st.write(0x40, 0xFF);
        st.write(0x50, 0x1F);
        st.write(0x60, 0x12);
        st.write(0x70, 0xFF);
        st.write(0x80, 0xF7);
        st.write(0x90, 0xFF);
        let op = &st.ops[0][0];
        assert_eq!(op.dt_mul, 0x3B);
        assert_eq!(op.tl, 0x7F);
        assert_eq!(op.ks_ar, 0x1F);
        assert_eq!(op.am_d1r, 0x12);
        assert_eq!(op.sr, 0x1F);
        assert_eq!(op.d1l_rr, 0xF7);
        assert_eq!(op.ssg_eg, 0x0F);
        // 0x34 is register slot 1 (S3) of the same channel.
        st.write(0x34, 0x0A);
        assert_eq!(st.ops[0][1].dt_mul, 0x0A);
        assert_eq!(st.ops[0][0].dt_mul, 0x3B);
        // 0x22 / 0x27 / 0x2A / 0x2B are channel-independent.
        st.write(0x22, 0x08);
        st.write(0x27, 0x40);
        st.write(0x2A, 0x80);
        st.write(0x2B, 0x80);
        assert_eq!(st.lfo, 0x08);
        assert_eq!(st.mode, 1);
        assert!(st.multi_freq());
        assert_eq!(st.dac_data, 0x80);
        assert!(st.dac_enabled);
    }

    #[test]
    fn special_channel3_pairs_drive_frequency_groups() {
        let mut st = YM2612State::new();
        st.write(0x27, 0x80); // CSM also counts as multi_freq
        assert!(st.multi_freq());
        // High bytes are latched at 0xAC-0xAE, low bytes commit at 0xA8-0xAA.
        st.write(0xAC, 0x02);
        st.write(0xA8, 0x40);
        st.write(0xAD, 0x13);
        st.write(0xA9, 0x41);
        assert_eq!(st.frequency(2), (0, 0x240));
        assert_eq!(st.frequency_op(2, 0), (0, 0x240));
        assert_eq!(st.frequency_op(2, 1), (2, 0x341));
        // Group 2 was never written: the normal channel f-num is the fallback.
        assert_eq!(st.frequency_op(2, 2), (0, 0));
        // Only channel 3 has supplementary pairs.
        assert_eq!(st.frequency_op(1, 0), (0, 0));
    }

    #[test]
    fn note_mapping_matches_the_reference_formula() {
        for (note, want) in [
            (0, (0, 20)),
            (48, (0, 322)),
            (60, (0, 644)),
            (69, (0, 1083)),
            (96, (3, 643)),
            (120, (5, 643)),
            (179, (7, 2047)),
        ] {
            assert_eq!(
                ym2612_note_fnum(note, YM2612_DEFAULT_CLOCK, 0x80, YM2612_FREQ_BASE),
                want
            );
        }
        assert_eq!(
            ym2612_note_fnum(60, YM2612_DEFAULT_CLOCK, 0x90, YM2612_FREQ_BASE),
            (0, 648)
        );
        assert_eq!(
            ym2612_note_for(4, 0x2A0, YM2612_DEFAULT_CLOCK, YM2612_FREQ_BASE),
            (109, 95)
        );
        assert_eq!(
            ym2612_note_for(7, 2047, YM2612_DEFAULT_CLOCK, YM2612_FREQ_BASE),
            (165, 128)
        );
        assert_eq!(
            ym2612_note_for(2, 0x123, 8_000_000, YM2612_FREQ_BASE),
            (71, 126)
        );
        assert!(
            (ym2612_pitch_units(4, 0x2A0) - 10285.299_780_694_088).abs() < 1e-9
        );
    }

    #[test]
    fn snapshot_pan_and_carrier_tl() {
        let mut st = YM2612State::new();
        st.write(0xB0, (3 << 3) | 4); // fb = 3, alg = 4 -> carriers 2,3
        st.write(0xB4, 0x9F); // left on, right off, ams 1, pms 7
        st.write(0x48, 0x30); // op2 TL = 0x30
        st.write(0x4C, 0x10); // op3 TL = 0x10
        st.write(0x40, 0x05); // op1 TL, not a carrier for alg 4
        assert_eq!(st.carrier_tl(0), 0x10);
        assert_eq!(st.pan_byte(0), 0xF0);
        let p = st.snapshot(0, 0xF);
        assert_eq!((p.alg, p.fb, p.ams, p.pms), (4, 3, 1, 7));
        assert!(p.opn);
        assert_eq!(p.ops[2].tl, 0x30);
        assert_eq!(p.ops[3].tl, 0x10);
        // Masking silences every operator whose mask bit is clear.
        let masked = st.snapshot(0, 1 << OPN_OP_TO_MASK[2]);
        assert_eq!(masked.ops[2].tl, 0x30);
        assert_eq!(masked.ops[3].tl, 0x7F);
        st.write(0xB4, 0x40);
        assert_eq!(st.pan_byte(0), 0x0F);
        st.write(0xB4, 0x00);
        assert_eq!(st.pan_byte(0), 0x00);
    }
}
