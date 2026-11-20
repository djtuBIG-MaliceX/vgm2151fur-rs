//! YM2610 / YM2610B (OPNB) support.
//!
//! The chip is an OPN core with 4 FM channels (6 on the YM2610B), the AY SSG,
//! and two sample engines: ADPCM-A (six fixed-rate channels) and ADPCM-B (one
//! variable-rate channel). Furnace's YM2610 platforms run in linear pitch mode
//! with a 9440540 frequency base. The FM note inverse is the YM2612 formula
//! with that base. The sample engines use Furnace's ADPCM-A/B sample formats
//! and sample-map instruments.

use indexmap::IndexMap;

use crate::fm::{FMPatch, OPN_CARRIERS, OPN_OP_TO_MASK, OpRegs};
use crate::vgm::{ChipWrite, VgmFile};
use crate::ym2612::{ym2612_note_fnum, ym2612_note_for};

pub const YM2610_DEFAULT_CLOCK: u32 = 8_000_000;
/// DivPlatformYM2610Base passes 9440540.0 to DivPlatformOPN.
pub const YM2610_FREQ_BASE: f64 = 9440540.0;
/// Furnace's YM2610 SSG divider is 32 (the YM2203 uses 16).
pub const YM2610_SSG_DIVIDER: i32 = 32;
/// ADPCM-A is fixed at clock/432; ADPCM-B is clock/144 * (freq / 65535).
pub const ADPCM_A_DIV: i32 = 432;
pub const ADPCM_B_DIV: i32 = 144;
pub const ADPCM_MAP_SLOTS: i32 = 120;

/// Channel register offsets per port. The 4-channel part places FM1-2 on port
/// 0 offsets 1/2 and FM3-4 on port 1 offsets 1/2. The B part is 3+3.
pub const CHAN_OFF_4: [(usize, usize); 4] = [(0, 1), (0, 2), (1, 1), (1, 2)];
pub const CHAN_OFF_6: [(usize, usize); 6] = [(0, 0), (0, 1), (0, 2), (1, 0), (1, 1), (1, 2)];
/// 0x28 channel field -> channel index.
pub const KON_4: [(i32, i32); 4] = [(1, 0), (2, 1), (5, 2), (6, 3)];
pub const KON_6: [(i32, i32); 6] = [(0, 0), (1, 1), (2, 2), (4, 3), (5, 4), (6, 5)];
pub const MAX_FNUM: i32 = 2047;

pub fn ym2610_note_fnum(note: i32, clock: u32, e5: i32) -> (i32, i32) {
    ym2612_note_fnum(note, clock, e5, YM2610_FREQ_BASE)
}

pub fn ym2610_note_for(block: i32, fnum: i32, clock: u32) -> (i32, i32) {
    ym2612_note_for(block, fnum, clock, YM2610_FREQ_BASE)
}

/// 2-bit L/R field -> Furnace 08xy pan value.
pub fn pan_byte(bits: i32) -> i32 {
    const PAN: [i32; 4] = [0x00, 0x0F, 0xF0, 0xFF];
    PAN[(bits & 3) as usize]
}

fn _blank_op() -> OpRegs {
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

/// Register image of one YM2610's FM and ADPCM-B halves.
#[derive(Clone, Debug, PartialEq)]
pub struct YM2610State {
    pub b: bool,
    pub channels: usize,
    pub chan_off: Vec<(usize, usize)>,
    pub kon_map: Vec<(i32, i32)>,
    pub fnum: Vec<i32>,
    pub block: Vec<i32>,
    pub fb_alg: Vec<i32>,
    pub pan_pms_ams: Vec<i32>,
    pub ops: Vec<[OpRegs; 4]>,
    pub kon: Vec<bool>,
    pub op_mask: Vec<i32>,
    pub mode: i32,
    pub lfo: i32,
    pub _hi: Vec<Option<i32>>,
    /// Channel-2 per-operator frequencies (0xA8-0xAE on port 0).
    pub special: [i32; 3],
    pub special_block: [i32; 3],
    pub special_set: [bool; 3],
    pub _special_hi: [Option<i32>; 3],
    // ADPCM-A registers (port 1).  Start/end are address/256. The reset
    // values are the chip's and Furnace's, not zeros: ymfm loads `0xDF` into
    // each level byte - "max instrument volume; some neogeo homebrews rely on
    // this" (ymfm_adpcm.cpp:48-51) - and DivPlatformYM2610/YM2608 write the
    // global level to full at init. A zero default here would fold an
    // unwritten voice to silence and drop its notes as muted.
    pub a_control: i32,
    pub a_master: i32,
    pub a_vol: [i32; 6],
    pub a_pan: [i32; 6],
    pub a_start: [i32; 6],
    pub a_end: [i32; 6],
    // ADPCM-B registers (port 0).  Start/end are address/256. Same argument:
    // the platform's reset writes $0B to 0xFF, and a channel pans both until
    // told otherwise.
    pub b_control: i32,
    pub b_pan: i32,
    pub b_start: i32,
    pub b_end: i32,
    pub b_freq: i32,
    pub b_vol: i32,
    pub b_flags: i32,
}

impl Default for YM2610State {
    fn default() -> Self {
        Self::new(false)
    }
}

impl YM2610State {
    pub fn new(b: bool) -> Self {
        let channels: usize = if b { 6 } else { 4 };
        let chan_off: Vec<(usize, usize)> = if b {
            CHAN_OFF_6.to_vec()
        } else {
            CHAN_OFF_4.to_vec()
        };
        let kon_map: Vec<(i32, i32)> = if b { KON_6.to_vec() } else { KON_4.to_vec() };
        YM2610State {
            b,
            channels,
            chan_off,
            kon_map,
            fnum: vec![0; channels],
            block: vec![0; channels],
            fb_alg: vec![0; channels],
            pan_pms_ams: vec![0xC0; channels],
            ops: (0..channels)
                .map(|_| core::array::from_fn(|_| _blank_op()))
                .collect(),
            kon: vec![false; channels],
            op_mask: vec![0; channels],
            mode: 0,
            lfo: 0,
            _hi: vec![None; channels],
            special: [0; 3],
            special_block: [0; 3],
            special_set: [false; 3],
            _special_hi: [None; 3],
            a_control: 0,
            a_master: 0x3F,
            a_vol: [0x1F; 6],
            a_pan: [3; 6],
            a_start: [0; 6],
            a_end: [0; 6],
            b_control: 0,
            b_pan: 3,
            b_start: 0,
            b_end: 0,
            b_freq: 0,
            b_vol: 0xFF,
            b_flags: 0,
        }
    }

    pub fn multi_freq(&self) -> bool {
        self.mode == 1 || self.mode == 2
    }

    pub fn _channel(&self, bank: i32, offset: i32) -> Option<usize> {
        for (ch, &(b, o)) in self.chan_off.iter().enumerate() {
            if b == bank as usize && o == offset as usize {
                return Some(ch);
            }
        }
        None
    }

    /// Channel selected by an FM register, or None for global registers.
    pub fn channel_of(&self, reg: i32) -> Option<usize> {
        let bank = if reg & 0x100 != 0 { 1 } else { 0 };
        let r = reg & 0xFF;
        if (0x30..=0x9E).contains(&r) || (0xA0..=0xA6).contains(&r) || (0xB0..=0xB6).contains(&r) {
            return self._channel(bank, r & 3);
        }
        None
    }

    pub fn _write_op(&mut self, ch: usize, idx: usize, page: i32, val: i32) {
        let mut o = self.ops[ch][idx].clone();
        match page {
            0x30 => o.dt_mul = val as u8,
            0x40 => o.tl = (val & 0x7F) as u8,
            0x50 => o.ks_ar = val as u8,
            0x60 => o.am_d1r = val as u8,
            0x70 => o.sr = (val & 0x1F) as u8,
            0x80 => o.d1l_rr = val as u8,
            0x90 => o.ssg_eg = (val & 0x0F) as u8,
            _ => {}
        }
        self.ops[ch][idx] = o;
    }

    pub fn write(&mut self, reg: i32, val: i32) {
        let bank = if reg & 0x100 != 0 { 1 } else { 0 };
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
            if r == 0x28 {
                if let Some(ch) = self.kon_chan(val & 7) {
                    self.op_mask[ch] = (val >> 4) & 0x0F;
                }
                return;
            }
            if (0x10..=0x1F).contains(&r) {
                self._write_adpcm_b(r, val);
                return;
            }
        } else if r <= 0x2D {
            self._write_adpcm_a(r, val);
            return;
        }
        if (0x30..=0x9E).contains(&r) {
            if let Some(ch) = self._channel(bank, r & 3) {
                self._write_op(ch, ((r >> 2) & 3) as usize, r & 0xF0, val);
            }
            return;
        }
        if (0xA0..=0xA6).contains(&r) {
            let ch = match self._channel(bank, r & 3) {
                Some(ch) => ch,
                None => return,
            };
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
            let idx = (r & 3) as usize;
            if idx < 3 {
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
            }
            return;
        }
        if (0xB0..=0xB2).contains(&r) {
            if let Some(ch) = self._channel(bank, r & 3) {
                self.fb_alg[ch] = val;
            }
            return;
        }
        if (0xB4..=0xB6).contains(&r) {
            if let Some(ch) = self._channel(bank, r & 3) {
                self.pan_pms_ams[ch] = val;
            }
            return;
        }
    }

    fn kon_chan(&self, field: i32) -> Option<usize> {
        for &(f, ch) in &self.kon_map {
            if f == field {
                return Some(ch as usize);
            }
        }
        None
    }

    pub fn _write_adpcm_a(&mut self, r: i32, val: i32) {
        if r == 0x00 {
            self.a_control = val;
        } else if r == 0x01 {
            self.a_master = val & 0x3F;
        } else if (0x08..=0x0D).contains(&r) {
            let ch = (r - 0x08) as usize;
            self.a_vol[ch] = val & 0x1F;
            self.a_pan[ch] = (val >> 6) & 0x03;
        } else if (0x10..=0x15).contains(&r) {
            let ch = (r - 0x10) as usize;
            self.a_start[ch] = (self.a_start[ch] & 0xFF00) | val;
        } else if (0x18..=0x1D).contains(&r) {
            let ch = (r - 0x18) as usize;
            self.a_start[ch] = (self.a_start[ch] & 0x00FF) | (val << 8);
        } else if (0x20..=0x25).contains(&r) {
            let ch = (r - 0x20) as usize;
            self.a_end[ch] = (self.a_end[ch] & 0xFF00) | val;
        } else if (0x28..=0x2D).contains(&r) {
            let ch = (r - 0x28) as usize;
            self.a_end[ch] = (self.a_end[ch] & 0x00FF) | (val << 8);
        }
    }

    pub fn _write_adpcm_b(&mut self, r: i32, val: i32) {
        if r == 0x10 {
            self.b_control = val;
        } else if r == 0x11 {
            self.b_pan = (val >> 6) & 0x03;
        } else if r == 0x12 {
            self.b_start = (self.b_start & 0xFF00) | val;
        } else if r == 0x13 {
            self.b_start = (self.b_start & 0x00FF) | (val << 8);
        } else if r == 0x14 {
            self.b_end = (self.b_end & 0xFF00) | val;
        } else if r == 0x15 {
            self.b_end = (self.b_end & 0x00FF) | (val << 8);
        } else if r == 0x19 {
            self.b_freq = (self.b_freq & 0xFF00) | val;
        } else if r == 0x1A {
            self.b_freq = (self.b_freq & 0x00FF) | (val << 8);
        } else if r == 0x1B {
            self.b_vol = val;
        } else if r == 0x1C {
            self.b_flags = val;
        }
    }

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

    pub fn frequency_op(&self, ch: usize, op: i32) -> (i32, i32) {
        if ch == 2 && self.multi_freq() && (0..3).contains(&op) && self.special_set[op as usize] {
            return (self.special_block[op as usize], self.special[op as usize]);
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

    pub fn pan_byte_ch(&self, ch: usize) -> i32 {
        let b4 = self.pan_pms_ams[ch];
        pan_byte(((if b4 & 0x80 != 0 { 1 } else { 0 }) << 1) | (if b4 & 0x40 != 0 { 1 } else { 0 }))
    }

    pub fn carrier_tl(&self, ch: usize) -> i32 {
        let alg = self.fb_alg[ch] & 0x07;
        let fallback = [3i32];
        let carriers: &[i32] = OPN_CARRIERS.get(alg as usize).copied().unwrap_or(&fallback);
        carriers
            .iter()
            .map(|&i| self.ops[ch][i as usize].tl as i32)
            .min()
            .expect("OPN carrier set is non-empty")
    }

    /// Effective 0-31 column level with the master attenuation folded in.
    ///
    /// The chip adds the channel's 5-bit attenuation to the 6-bit total-level
    /// attenuation (0-63) before applying its volume curve. The module column
    /// has only 31 steps, so the combined attenuation is clamped into that
    /// range.
    pub fn adpcm_a_level(&self, ch: usize) -> i32 {
        let atten = (0x1F - (self.a_vol[ch] & 0x1F)) + (0x3F - (self.a_master & 0x3F));
        std::cmp::max(0, 0x1F - std::cmp::min(0x1F, atten))
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct AdpcmAHit {
    pub sample_time: i64,
    pub chip_id: u8,
    pub channel: i32,
    pub start: i32,
    pub end: i32,
    pub vol: i32,
    pub pan: i32,
    pub end_time: i64,
    /// Late 0x101/0x108-0x10D writes while the sample plays.
    pub vol_pts: Vec<(i64, i32)>,
    pub pan_pts: Vec<(i64, i32)>,
}

impl Default for AdpcmAHit {
    fn default() -> Self {
        AdpcmAHit {
            sample_time: 0,
            chip_id: 0,
            channel: 0,
            start: 0,
            end: 0,
            vol: 0,
            pan: 0,
            end_time: 0,
            vol_pts: Vec::new(),
            pan_pts: Vec::new(),
        }
    }
}

impl AdpcmAHit {
    pub fn new(
        sample_time: i64,
        chip_id: u8,
        channel: i32,
        start: i32,
        end: i32,
        vol: i32,
        pan: i32,
    ) -> Self {
        AdpcmAHit {
            sample_time,
            chip_id,
            channel,
            start,
            end,
            vol,
            pan,
            ..AdpcmAHit::default()
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct AdpcmASample {
    pub chip_id: u8,
    pub start: i32,
    pub end: i32,
    pub data: Vec<u8>,
    pub hits: Vec<AdpcmAHit>,
}

impl Default for AdpcmASample {
    fn default() -> Self {
        AdpcmASample {
            chip_id: 0,
            start: 0,
            end: 0,
            data: Vec::new(),
            hits: Vec::new(),
        }
    }
}

impl AdpcmASample {
    pub fn new(chip_id: u8, start: i32, end: i32, data: Vec<u8>) -> Self {
        AdpcmASample {
            chip_id,
            start,
            end,
            data,
            hits: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct AdpcmBHit {
    pub sample_time: i64,
    pub chip_id: u8,
    pub start: i32,
    pub end: i32,
    pub rate: i32,
    pub vol: i32,
    pub pan: i32,
    pub r#loop: bool,
    pub end_time: i64,
    /// Late 0x11/0x19/0x1A/0x1B writes while the sample plays.
    pub rate_pts: Vec<(i64, i32)>,
    pub vol_pts: Vec<(i64, i32)>,
    pub pan_pts: Vec<(i64, i32)>,
    pub play_pos: f64,
    pub play_t: i64,
    pub play_rate: i32,
    pub ended: bool,
}

impl Default for AdpcmBHit {
    fn default() -> Self {
        AdpcmBHit {
            sample_time: 0,
            chip_id: 0,
            start: 0,
            end: 0,
            rate: 0,
            vol: 0,
            pan: 0,
            r#loop: false,
            end_time: 0,
            rate_pts: Vec::new(),
            vol_pts: Vec::new(),
            pan_pts: Vec::new(),
            play_pos: 0.0,
            play_t: 0,
            play_rate: 0,
            ended: false,
        }
    }
}

impl AdpcmBHit {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        sample_time: i64,
        chip_id: u8,
        start: i32,
        end: i32,
        rate: i32,
        vol: i32,
        pan: i32,
        r#loop: bool,
    ) -> Self {
        AdpcmBHit {
            sample_time,
            chip_id,
            start,
            end,
            rate,
            vol,
            pan,
            r#loop,
            ..AdpcmBHit::default()
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct AdpcmBSample {
    pub chip_id: u8,
    pub start: i32,
    pub end: i32,
    pub rate: i32,
    pub data: Vec<u8>,
    pub hits: Vec<AdpcmBHit>,
}

impl Default for AdpcmBSample {
    fn default() -> Self {
        AdpcmBSample {
            chip_id: 0,
            start: 0,
            end: 0,
            rate: 0,
            data: Vec::new(),
            hits: Vec::new(),
        }
    }
}

impl AdpcmBSample {
    pub fn new(chip_id: u8, start: i32, end: i32, rate: i32, data: Vec<u8>) -> Self {
        AdpcmBSample {
            chip_id,
            start,
            end,
            rate,
            data,
            hits: Vec::new(),
        }
    }
}

/// One 256-byte-aligned ADPCM range, padded to the format's block size.
pub fn _rom_slice(rom: &[u8], start: i32, end: i32) -> Vec<u8> {
    if rom.is_empty() {
        return Vec::new();
    }
    let start = start & !0xFF;
    let mut end = (end & !0xFF) | 0xFF;
    if end < start {
        end = start + 0xFF;
    }
    let (start, end) = (start as usize, end as usize);
    let stop = std::cmp::min(rom.len(), end + 1);
    let mut data = if start < stop {
        rom[start..stop].to_vec()
    } else {
        Vec::new()
    };
    if data.len() < 256 {
        data.extend(std::iter::repeat_n(0x80u8, 256 - data.len()));
    }
    if data.len() & 0xFF != 0 {
        data.extend(std::iter::repeat_n(0x80u8, 0x100 - (data.len() & 0xFF)));
    }
    data
}

fn _b_advance(hit: &mut AdpcmBHit, t: i64, clock: u32) -> bool {
    // Move the B hit's ROM position to `t`. False once a one-shot ended.
    if hit.ended {
        return false;
    }
    let dt = t - hit.play_t;
    if dt > 0 && !hit.r#loop {
        let rate_hz = clock as f64 / 144.0 * (hit.play_rate & 0xFFFF) as f64 / 65535.0;
        hit.play_pos += dt as f64 * rate_hz / 44100.0;
    }
    hit.play_t = t;
    if !hit.r#loop && hit.play_pos >= ((hit.end - hit.start + 1) * 2) as f64 {
        hit.ended = true;
        return false;
    }
    true
}

fn _rate_hi_follows(writes: &[ChipWrite], wi: usize, chip_id: u8, t: i64) -> bool {
    let hi = std::cmp::min(wi + 9, writes.len());
    for nw in &writes[wi + 1..hi] {
        if nw.sample - t > 8 {
            break;
        }
        if nw.chip == "ym2610"
            && nw.chip_id == chip_id
            && nw.reg & 0x100 == 0
            && (nw.reg & 0xFF) == 0x1A
        {
            return true;
        }
    }
    false
}

fn _gate_follows(writes: &[ChipWrite], wi: usize, chip_id: u8, t: i64) -> bool {
    let hi = std::cmp::min(wi + 17, writes.len());
    for nw in &writes[wi + 1..hi] {
        if nw.sample - t > 64 {
            break;
        }
        if nw.chip == "ym2610"
            && nw.chip_id == chip_id
            && nw.reg & 0x100 == 0
            && (nw.reg & 0xFF) == 0x10
        {
            return true;
        }
    }
    false
}

fn _a_playing(hit: &AdpcmAHit, t: i64, clock: u32) -> bool {
    let rate_hz = clock as f64 / 432.0;
    ((t - hit.sample_time) as f64 * rate_hz / 44100.0) < ((hit.end - hit.start + 1) * 2) as f64
}

fn _a_gate_follows(writes: &[ChipWrite], wi: usize, chip_id: u8, ch: usize, t: i64) -> bool {
    let hi = std::cmp::min(wi + 17, writes.len());
    for nw in &writes[wi + 1..hi] {
        if nw.sample - t > 64 {
            break;
        }
        if nw.chip == "ym2610"
            && nw.chip_id == chip_id
            && (nw.reg & 0x1FF) == 0x100
            && (nw.val & (1 << ch)) != 0
        {
            return true;
        }
    }
    false
}

/// Walk the YM2610 register stream and build ADPCM-A/B sample lists.
///
/// A uses the 0x82 ROM.  B uses the 0x83 ROM when that block carries data
/// at the selected range, else 0x82.  Rips differ here: some put both banks
/// in 0x82, others put the ADPCM-B bank in 0x83.
pub fn collect_ym2610_adpcm(
    vgm: &VgmFile,
    rom: &[u8],
    rom_b: Option<&[u8]>,
) -> (Vec<AdpcmASample>, Vec<AdpcmBSample>, Vec<String>) {
    let rom_b = rom_b.unwrap_or(rom);
    let mut warnings: Vec<String> = Vec::new();
    let mut a_st = YM2610State::new(vgm.ym2610_b);
    let mut b_st = YM2610State::new(vgm.ym2610_b);
    // The active maps hold indices into the hit vectors: Python's active
    // dict and the collected lists share the same hit objects, so a stop
    // write updates the end time the sample list later sees.
    let mut a_active: IndexMap<(u8, usize), usize> = IndexMap::new();
    let mut b_active: IndexMap<u8, usize> = IndexMap::new();
    let mut a_hits: Vec<AdpcmAHit> = Vec::new();
    let mut b_hits: Vec<AdpcmBHit> = Vec::new();
    let writes = &vgm.writes;
    let clock = if vgm.ym2610_clock != 0 {
        vgm.ym2610_clock
    } else {
        8_000_000
    };

    for (wi, w) in writes.iter().enumerate() {
        if w.chip != "ym2610" {
            continue;
        }
        let reg = w.reg as i32;
        let val = w.val as i32;
        let bank = if reg & 0x100 != 0 { 1 } else { 0 };
        let r = reg & 0xFF;
        if bank != 0 {
            a_st.write(reg, val);
            // Cyber-bat style drivers write the real channel level a few
            // samples after the key-on (0xC0 at the gate, 0xC8 shortly after).
            // Update the active hit for that short window so the note carries
            // the audible level rather than the pre-gate mute.
            if (0x08..=0x0D).contains(&r) {
                let ch = (r - 0x08) as usize;
                let idx = a_active.get(&(w.chip_id, ch)).copied();
                if let Some(idx) = idx {
                    let hit = &mut a_hits[idx];
                    let dt = w.sample - hit.sample_time;
                    let vol = a_st.adpcm_a_level(ch);
                    let pan = pan_byte(a_st.a_pan[ch]);
                    if (0..=64).contains(&dt) {
                        hit.vol = vol;
                        hit.pan = pan;
                    } else if dt > 64
                        && !_a_gate_follows(writes, wi, w.chip_id, ch, w.sample)
                        && _a_playing(hit, w.sample, clock)
                    {
                        let last = hit.vol_pts.last().map_or(hit.vol, |&(_, v)| v);
                        if vol != last {
                            hit.vol_pts.push((w.sample, vol));
                        }
                        let last = hit.pan_pts.last().map_or(hit.pan, |&(_, p)| p);
                        if pan != last {
                            hit.pan_pts.push((w.sample, pan));
                        }
                    }
                }
            } else if r == 0x01 {
                for (&(cid, ch), &idx) in a_active.iter() {
                    if cid != w.chip_id {
                        continue;
                    }
                    let hit = &mut a_hits[idx];
                    let vol = a_st.adpcm_a_level(ch);
                    let dt = w.sample - hit.sample_time;
                    if (0..=64).contains(&dt) {
                        hit.vol = vol;
                    } else if dt > 64 && _a_playing(hit, w.sample, clock) {
                        let last = hit.vol_pts.last().map_or(hit.vol, |&(_, v)| v);
                        if vol != last {
                            hit.vol_pts.push((w.sample, vol));
                        }
                    }
                }
            }
            if r != 0x00 {
                continue;
            }
            let start_bits = val & 0x3F;
            let stop = val & 0x80 != 0;
            if stop {
                for ch in 0..6 {
                    if start_bits & (1 << ch) != 0 {
                        if let Some(idx) = a_active.shift_remove(&(w.chip_id, ch)) {
                            a_hits[idx].end_time = w.sample;
                        }
                    }
                }
                continue;
            }
            // A non-stop 0x100 write retriggers every selected channel, even
            // one that is already sounding.  Furnace's export writes combined
            // masks (0x3, 0x6) on one row. A 0->1 test would lose them.
            for ch in 0..6 {
                if start_bits & (1 << ch) == 0 {
                    continue;
                }
                if let Some(idx) = a_active.shift_remove(&(w.chip_id, ch)) {
                    a_hits[idx].end_time = w.sample;
                }
                let start = a_st.a_start[ch] << 8;
                let end = (a_st.a_end[ch] << 8) | 0xFF;
                let hit = AdpcmAHit::new(
                    w.sample,
                    w.chip_id,
                    ch as i32,
                    start,
                    end,
                    a_st.adpcm_a_level(ch),
                    pan_byte(a_st.a_pan[ch]),
                );
                let idx = a_hits.len();
                a_hits.push(hit);
                a_active.insert((w.chip_id, ch), idx);
            }
            continue;
        }

        b_st.write(reg, val);
        if let Some(&idx) = b_active.get(&w.chip_id) {
            let hit = &mut b_hits[idx];
            let dt = w.sample - hit.sample_time;
            if (0..=64).contains(&dt) {
                // The driver commonly writes the pan (and sometimes the
                // level or rate) a few samples after the start bit.
                if r == 0x11 {
                    hit.pan = pan_byte(b_st.b_pan);
                } else if r == 0x19 || r == 0x1A {
                    hit.rate = b_st.b_freq;
                    hit.play_rate = hit.rate;
                } else if r == 0x1B {
                    hit.vol = b_st.b_vol;
                }
            } else if dt > 64
                && !_gate_follows(writes, wi, w.chip_id, w.sample)
                && _b_advance(hit, w.sample, clock)
            {
                if r == 0x11 {
                    let pan = pan_byte(b_st.b_pan);
                    let last = hit.pan_pts.last().map_or(hit.pan, |&(_, p)| p);
                    if pan != last {
                        hit.pan_pts.push((w.sample, pan));
                    }
                } else if r == 0x1B {
                    let vol = b_st.b_vol;
                    let last = hit.vol_pts.last().map_or(hit.vol, |&(_, v)| v);
                    if vol != last {
                        hit.vol_pts.push((w.sample, vol));
                    }
                } else if r == 0x1A
                    || (r == 0x19 && !_rate_hi_follows(writes, wi, w.chip_id, w.sample))
                {
                    let rate = b_st.b_freq;
                    let last = hit.rate_pts.last().map_or(hit.rate, |&(_, v)| v);
                    if rate != last {
                        hit.rate_pts.push((w.sample, rate));
                        hit.play_rate = rate;
                    }
                }
            }
        }
        if r != 0x10 {
            continue;
        }
        let start = val & 0x80 != 0;
        // Bit 0 resets Start/Repeat; bit 6 is Repeat.
        let reset = val & 0x01 != 0;
        if !start || reset {
            if let Some(idx) = b_active.shift_remove(&w.chip_id) {
                b_hits[idx].end_time = w.sample;
            }
            continue;
        }
        if let Some(idx) = b_active.shift_remove(&w.chip_id) {
            b_hits[idx].end_time = w.sample;
        }
        let mut hit = AdpcmBHit::new(
            w.sample,
            w.chip_id,
            b_st.b_start << 8,
            (b_st.b_end << 8) | 0xFF,
            b_st.b_freq,
            b_st.b_vol,
            pan_byte(b_st.b_pan),
            val & 0x40 != 0,
        );
        hit.play_t = w.sample;
        hit.play_rate = b_st.b_freq;
        let idx = b_hits.len();
        b_hits.push(hit);
        b_active.insert(w.chip_id, idx);
    }

    let end = vgm.total_samples;
    for &idx in a_active.values() {
        a_hits[idx].end_time = end;
    }
    for &idx in b_active.values() {
        b_hits[idx].end_time = end;
    }

    // A pan of 0 mutes the channel, but Furnace's panning command turns 0
    // into center. When the pan never returns, stop the note at the mute.
    // The sample cannot then play on to the end of its ROM body.
    for hit in a_hits.iter_mut() {
        if let Some(t) = _muted_pan_end(&mut hit.pan_pts, &mut hit.vol_pts, None) {
            hit.end_time = t;
        }
    }
    for hit in b_hits.iter_mut() {
        if let Some(t) = _muted_pan_end(&mut hit.pan_pts, &mut hit.vol_pts, Some(&mut hit.rate_pts))
        {
            hit.end_time = t;
        }
    }

    let mut a_samples: IndexMap<(u8, i32, i32), AdpcmASample> = IndexMap::new();
    if !rom.is_empty() {
        for hit in &a_hits {
            let key = (hit.chip_id, hit.start, hit.end);
            if !a_samples.contains_key(&key) {
                let data = _rom_slice(rom, hit.start, hit.end);
                a_samples.insert(
                    key,
                    AdpcmASample::new(hit.chip_id, hit.start, hit.end, data),
                );
            }
            a_samples.get_mut(&key).unwrap().hits.push(hit.clone());
        }
    }

    let mut b_samples: IndexMap<(u8, i32, i32, i32), AdpcmBSample> = IndexMap::new();
    for hit in &b_hits {
        let key = (hit.chip_id, hit.start, hit.end, hit.rate);
        if !b_samples.contains_key(&key) {
            let mut data = _rom_slice(rom_b, hit.start, hit.end);
            if data.iter().all(|&b| b == 0) && !std::ptr::eq(rom_b, rom) {
                data = _rom_slice(rom, hit.start, hit.end);
            }
            b_samples.insert(
                key,
                AdpcmBSample::new(hit.chip_id, hit.start, hit.end, hit.rate, data),
            );
        }
        b_samples.get_mut(&key).unwrap().hits.push(hit.clone());
    }

    if !a_hits.is_empty() && rom.is_empty() {
        warnings.push(
            "YM2610 ADPCM-A key-ons but no ADPCM ROM; sample channels are silent".to_string(),
        );
    }
    if !b_hits.is_empty() && rom.is_empty() && rom_b.is_empty() {
        warnings.push(
            "YM2610 ADPCM-B key-ons but no ADPCM ROM; sample channels are silent".to_string(),
        );
    }
    (
        a_samples.into_values().collect(),
        b_samples.into_values().collect(),
        warnings,
    )
}

/// Apply the pan-0 mute rule to one hit's point lists; returns the mute time.
fn _muted_pan_end(
    pan_pts: &mut Vec<(i64, i32)>,
    vol_pts: &mut Vec<(i64, i32)>,
    rate_pts: Option<&mut Vec<(i64, i32)>>,
) -> Option<i64> {
    for idx in 0..pan_pts.len() {
        let (t, pan) = pan_pts[idx];
        if pan != 0 || pan_pts[idx + 1..].iter().any(|&(_, p)| p != 0) {
            continue;
        }
        vol_pts.retain(|&(vt, _)| vt < t);
        if let Some(rate_pts) = rate_pts {
            rate_pts.retain(|&(rt, _)| rt < t);
        }
        pan_pts.truncate(idx);
        return Some(t);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chip_write(sample: i64, reg: u32, val: u32) -> ChipWrite {
        ChipWrite::new(sample, "ym2610", 0, reg, val)
    }

    #[test]
    fn pan_bits_map_to_furnace_bytes() {
        assert_eq!(pan_byte(0), 0x00);
        assert_eq!(pan_byte(1), 0x0F);
        assert_eq!(pan_byte(2), 0xF0);
        assert_eq!(pan_byte(3), 0xFF);
        assert_eq!(pan_byte(0x7F), 0xFF);
    }

    #[test]
    fn four_channel_part_uses_offset_pairs() {
        let mut st = YM2610State::new(false);
        assert_eq!(st.channels, 4);
        // Port 0 offset 1 is channel 0; the high byte latches first.
        st.write(0xA5, 0x12);
        st.write(0xA1, 0xCD);
        assert_eq!(st.frequency(0), (2, 0x2CD));
        // Port 1 offset 1 is channel 2.
        st.write(0x1A5, 0x0A);
        st.write(0x1A1, 0x34);
        assert_eq!(st.frequency(2), (1, 0x234));
        // Offset 0 exists only on the six-channel part.
        assert_eq!(st.channel_of(0x1A4), None);
        assert_eq!(st.channel_of(0x1A5), Some(2));
    }

    #[test]
    fn six_channel_kon_and_operator_mask() {
        let mut st = YM2610State::new(true);
        assert_eq!(st.channels, 6);
        // 0x28 field 5 -> channel 4 on the B part, mask 0xC.
        st.write(0x28, 0xC5);
        assert_eq!(st.op_mask[4], 0xC);
        assert_eq!(st.op_mask[0], 0);
        // Port 1 offset 0 is channel 3.
        st.write(0x1A4, 0x08);
        st.write(0x1A0, 0x11);
        assert_eq!(st.frequency(3), (1, 0x11));
    }

    #[test]
    fn adpcm_a_gate_collects_one_padded_page() {
        let mut vgm = VgmFile {
            ym2610_clock: 8_000_000,
            total_samples: 44100,
            ..VgmFile::default()
        };
        vgm.writes = vec![
            chip_write(0, 0x101, 0x3F), // master full
            chip_write(0, 0x108, 0xDF), // ch0 pan both, vol 31
            chip_write(0, 0x110, 0x01),
            chip_write(0, 0x118, 0x00), // start page 1
            chip_write(0, 0x120, 0x01),
            chip_write(0, 0x128, 0x00), // end page 1
            chip_write(100, 0x100, 0x01),
            chip_write(4000, 0x100, 0x81), // stop ch0
        ];
        let rom = vec![0x5Au8; 512];
        let (a, b, warnings) = collect_ym2610_adpcm(&vgm, &rom, None);
        assert!(b.is_empty());
        assert!(warnings.is_empty());
        assert_eq!(a.len(), 1);
        assert_eq!((a[0].start, a[0].end), (0x100, 0x1FF));
        assert_eq!(a[0].data.len(), 256);
        assert_eq!(a[0].hits.len(), 1);
        let hit = &a[0].hits[0];
        assert_eq!(hit.channel, 0);
        assert_eq!((hit.vol, hit.pan), (31, 0xFF));
        assert_eq!((hit.sample_time, hit.end_time), (100, 4000));
    }

    #[test]
    fn adpcm_b_start_and_no_rom_warning() {
        let mut vgm = VgmFile {
            ym2610_clock: 8_000_000,
            total_samples: 44100,
            ..VgmFile::default()
        };
        vgm.writes = vec![
            chip_write(0, 0x11, 0xC0), // pan both
            chip_write(0, 0x12, 0x01),
            chip_write(0, 0x13, 0x00), // start page 1
            chip_write(0, 0x14, 0x00),
            chip_write(0, 0x15, 0x00), // end 0x00
            chip_write(0, 0x19, 0x00),
            chip_write(0, 0x1A, 0x10), // rate 0x1000
            chip_write(0, 0x1B, 0x80), // vol 128
            chip_write(100, 0x10, 0xC0),
        ];
        let rom = vec![0x33u8; 512];
        let (a, b, warnings) = collect_ym2610_adpcm(&vgm, &rom, None);
        assert!(a.is_empty());
        assert!(warnings.is_empty());
        assert_eq!(b.len(), 1);
        let hit = &b[0].hits[0];
        assert_eq!((hit.start, hit.end), (0x100, 0xFF));
        assert_eq!((hit.rate, hit.vol, hit.pan), (0x1000, 0x80, 0xFF));
        assert!(hit.r#loop);
        assert_eq!((hit.play_t, hit.play_rate), (100, 0x1000));
        assert_eq!(hit.end_time, 44100);
        assert_eq!(b[0].data.len(), 256);

        let (a2, b2, warnings) = collect_ym2610_adpcm(&vgm, &[], None);
        assert!(a2.is_empty());
        assert_eq!(b2.len(), 1);
        assert_eq!(
            warnings,
            vec!["YM2610 ADPCM-B key-ons but no ADPCM ROM; sample channels are silent".to_string()]
        );
    }
}
