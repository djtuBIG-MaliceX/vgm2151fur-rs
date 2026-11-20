//! Namco C352 ROM → signed samples → Furnace.
//!
//! Register model follows libvgm c352.c. 32 voices; each voice is eight
//! uint16 registers at address `voice * 8 + reg`:
//!
//! ```text
//! 0 vol_f   1 vol_r   2 freq   3 flags
//! 4 bank    5 start   6 end    7 loop
//! ```
//!
//! Key on and key off are latched in the flag word and executed by a write to
//! 0x202. A second strobe without a new key-on bit does not restart the
//! voice. The output rate is clock/288 and the phase accumulator steps `freq`
//! per sample with a 16-bit fraction, so a voice plays
//! `clock * freq / (288 * 65536)` ROM bytes per second.
//!
//! The end address is exclusive on a one-shot and inclusive on a loop. Mulaw
//! uses the C352 table (byte 0x01 expands to 32) and is stored as linear
//! int16. Mid-note loop clears schedule a natural note-off when the voice
//! reaches wave_end; the stop is dropped by a retrigger, key-off, or loop
//! re-arm.

use std::collections::BTreeSet;

use indexmap::IndexMap;

use crate::parity::{py_int, py_round};
use crate::vgm::{assemble_rom, VgmFile, VGM_RATE};

pub const C352_VOICES: usize = 32;
pub const C352_RATE_DIVISOR: i64 = 288 * 65536;
pub const C352_DEFAULT_CLOCK: u32 = 24_192_000;
/// One 60 Hz VGM frame: these drivers strobe first and write the note's own
/// pitch right after, inside the same frame. A first write in that window is
/// the note pitch, not a slide.
pub const C352_REFIT_SAMPLES: i64 = 735;
pub const C352_MAX_FRAMES: i64 = 1 << 20;

pub const FLG_BUSY: i32 = 0x8000;
pub const FLG_KEYON: i32 = 0x4000;
pub const FLG_KEYOFF: i32 = 0x2000;
pub const FLG_LOOPHIST: i32 = 0x0800;
pub const FLG_LDIR: i32 = 0x0040;
pub const FLG_LINK: i32 = 0x0020;
pub const FLG_NOISE: i32 = 0x0010;
pub const FLG_MULAW: i32 = 0x0008;
pub const FLG_LOOP: i32 = 0x0002;
pub const FLG_REVERSE: i32 = 0x0001;

pub const REG_VOL_F: usize = 0;
pub const REG_VOL_R: usize = 1;
pub const REG_FREQ: usize = 2;
pub const REG_FLAGS: usize = 3;
pub const REG_BANK: usize = 4;
pub const REG_START: usize = 5;
pub const REG_END: usize = 6;
pub const REG_LOOP: usize = 7;
pub const ADDR_STROBE: i32 = 0x202;

/// ROM bytes per second for a frequency register.
pub fn c352_rate(clock: u32, freq: i32) -> f64 {
    let clock = if clock == 0 {
        C352_DEFAULT_CLOCK
    } else {
        clock
    };
    (clock as f64) * ((freq & 0xFFFF) as f64) / (C352_RATE_DIVISOR as f64)
}

/// libvgm `device_start_c352`. Byte 0x01 expands to 32, not C140's 256.
const fn _build_mulaw() -> [i32; 256] {
    let mut pos = [0i32; 128];
    let mut step = 0i32;
    let mut i = 0usize;
    while i < 128 {
        pos[i] = step << 5;
        if i < 16 {
            step += 1;
        } else if i < 24 {
            step += 2;
        } else if i < 48 {
            step += 4;
        } else if i < 100 {
            step += 8;
        } else {
            step += 16;
        }
        i += 1;
    }
    let mut out = [0i32; 256];
    let mut i = 0usize;
    while i < 256 {
        if i < 128 {
            out[i] = pos[i];
        } else {
            let mut v = (!pos[i - 128]) & !0x1F & 0xFFFF;
            if (v & 0x8000) != 0 {
                v -= 0x10000;
            }
            out[i] = v;
        }
        i += 1;
    }
    out
}

pub const C352_MULAW: [i32; 256] = _build_mulaw();

fn _mulaw_i16(byte: u8) -> i32 {
    C352_MULAW[(byte & 0xFF) as usize]
}

/// Keep the last write of each cluster closer than `gap` samples.
pub fn merge_step_writes(pts: &[(i64, i32)], gap: i64) -> Vec<(i64, i32)> {
    let mut merged: Vec<(i64, i32)> = Vec::new();
    for &p in pts {
        if let Some(last) = merged.last_mut() {
            if p.0 - last.0 <= gap {
                *last = p;
                continue;
            }
        }
        merged.push(p);
    }
    merged
}

/// Frequency the hit plays at. A write just after key-on replaces a stale 0.
pub fn hit_played_freq(hit: &C352Hit) -> i32 {
    let steps = merge_step_writes(&hit.freq_pts, 16);
    if let Some(&first) = steps.first() {
        if first.1 != 0
            && (first.0 - hit.sample_time <= C352_REFIT_SAMPLES || (hit.freq & 0xFFFF) == 0)
        {
            return first.1;
        }
    }
    hit.freq & 0xFFFF
}

/// ROM frames the voice advances from key-on until `end_sample`.
pub fn hit_played_frames(hit: &C352Hit, end_sample: i64, clock: u32) -> f64 {
    if end_sample <= hit.sample_time {
        return 0.0;
    }
    let mut freq = hit.freq & 0xFFFF;
    let mut prev = hit.sample_time;
    let mut total = 0.0;
    for &(t, f) in &hit.freq_pts {
        if t <= prev {
            freq = f;
            continue;
        }
        if t >= end_sample {
            break;
        }
        total += c352_rate(clock, freq) * (t - prev) as f64 / (VGM_RATE as f64);
        freq = f;
        prev = t;
    }
    total += c352_rate(clock, freq) * (end_sample - prev) as f64 / (VGM_RATE as f64);
    total
}

/// Front pair, plus the rear pair unless the header mutes it. Each side clamps
/// at 255.
fn _mix(vol_f: i32, vol_r: i32, mute_rear: bool) -> (i32, i32) {
    let mut left = (vol_f >> 8) & 0xFF;
    let mut right = vol_f & 0xFF;
    if !mute_rear {
        left = 255.min(left + ((vol_r >> 8) & 0xFF));
        right = 255.min(right + (vol_r & 0xFF));
    }
    (left, right)
}

/// The two 8-bit sides of a C352 volume register: high is left, low is right.
fn _sides(word: i32) -> (i32, i32) {
    ((word >> 8) & 0xFF, word & 0xFF)
}

/// The four registers the chip holds: front L/R then rear L/R. A muted rear
/// pair reads as silence.
fn _quad(vol_f: i32, vol_r: i32, mute_rear: bool) -> (i32, i32, i32, i32) {
    let (front_l, front_r) = _sides(vol_f);
    if mute_rear {
        return (front_l, front_r, 0, 0);
    }
    let (rear_l, rear_r) = _sides(vol_r);
    (front_l, front_r, rear_l, rear_r)
}

#[derive(Clone, Debug, PartialEq)]
pub struct C352Hit {
    pub sample_time: i64,
    pub chip_id: i32,
    pub voice: usize,
    pub bank: i32,
    pub start: i32,
    pub end: i32,
    pub loop_: i32,
    pub flags: i32,
    pub freq: i32,
    pub vol_l: i32,
    pub vol_r: i32,
    pub off_sample: i64,
    pub vol_pts: Vec<(i64, i32, i32)>,
    pub front_l: i32,
    pub front_r: i32,
    pub rear_l: i32,
    pub rear_r: i32,
    pub quad_pts: Vec<(i64, i32, i32, i32, i32)>,
    pub freq_pts: Vec<(i64, i32)>,
    /// The live (wave_start, wave_loop) the chip reads when the running
    /// address reaches wave_end, recorded when the driver rewrites them before
    /// the voice gets there. `None` means the key-on values.
    pub geo_at_cross: Option<(i32, i32)>,
}

impl Default for C352Hit {
    fn default() -> Self {
        Self {
            sample_time: 0,
            chip_id: 0,
            voice: 0,
            bank: 0,
            start: 0,
            end: 0,
            loop_: 0,
            flags: 0,
            freq: 0,
            vol_l: 0,
            vol_r: 0,
            off_sample: 0,
            vol_pts: Vec::new(),
            front_l: 0,
            front_r: 0,
            rear_l: 0,
            rear_r: 0,
            quad_pts: Vec::new(),
            freq_pts: Vec::new(),
            geo_at_cross: None,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct C352Off {
    pub sample_time: i64,
    pub chip_id: i32,
    pub voice: usize,
}

#[derive(Clone, Debug, PartialEq)]
pub struct C352Sample {
    pub start: i32,
    pub data: Vec<u8>,
    pub loop_start: Option<i64>,
    pub loop_end: Option<i64>,
    pub hits: Vec<C352Hit>,
    pub depth: i32,
    /// Furnace SMP2: 0 forward, 2 ping-pong.
    pub loop_mode: i32,
}

impl Default for C352Sample {
    fn default() -> Self {
        Self {
            start: 0,
            data: Vec::new(),
            loop_start: None,
            loop_end: None,
            hits: Vec::new(),
            depth: 8,
            loop_mode: 0,
        }
    }
}

impl C352Sample {
    pub fn length(&self) -> usize {
        if self.depth == 16 {
            self.data.len() / 2
        } else {
            self.data.len()
        }
    }

    pub fn mode_freq(&self) -> i32 {
        if self.hits.is_empty() {
            return 0;
        }
        let mut freqs: IndexMap<i32, i64> = IndexMap::new();
        for h in &self.hits {
            let f = hit_played_freq(h);
            if f != 0 {
                *freqs.entry(f).or_insert(0) += 1;
            }
        }
        let mut best: Option<(i32, i64)> = None;
        for (&f, &n) in freqs.iter() {
            match best {
                Some((_, bn)) if n <= bn => {}
                _ => best = Some((f, n)),
            }
        }
        best.map(|(f, _)| f).unwrap_or(0)
    }

    pub fn c4_rate(&self, clock: u32) -> i64 {
        let played = c352_rate(clock, self.mode_freq());
        if played <= 0.0 {
            return 1000;
        }
        1000.max(py_int(py_round(played)))
    }
}

/// The wave ROM reads through pow2_mask(size), libvgm's window mask.
fn _rom_mask(vgm: &VgmFile, chip_id: i32) -> i64 {
    let mut size: i64 = 0;
    for s in &vgm.roms {
        if s.chip as i32 == chip_id && s.kind == "c352" && !s.data.is_empty() {
            size = size.max(s.rom_size as i64);
        }
    }
    if size <= 0 {
        return 0;
    }
    let v = (size - 1) as u64;
    let bits = 64 - v.leading_zeros();
    ((1u64 << bits) - 1) as i64
}

fn _covered(vgm: &VgmFile, chip_id: i32, addr: i64) -> bool {
    let addr = addr & _rom_mask(vgm, chip_id);
    for s in &vgm.roms {
        if s.chip as i32 == chip_id && s.kind == "c352" && !s.data.is_empty() {
            let start = s.start as i64;
            if start <= addr && addr < start + s.data.len() as i64 {
                return true;
            }
        }
    }
    false
}

fn _read(rom: &[u8], vgm: &VgmFile, chip_id: i32, off: i64, n: i64) -> Option<Vec<u8>> {
    if n <= 0 {
        return Some(Vec::new());
    }
    if off < 0 {
        return None;
    }
    let mask = _rom_mask(vgm, chip_id);
    let start = off & mask;
    if !_covered(vgm, chip_id, start) {
        return None;
    }
    if start + n <= rom.len() as i64 && start + n <= mask + 1 {
        return Some(rom[start as usize..(start + n) as usize].to_vec());
    }
    // the span wraps the window: the core's counter keeps rising and every
    // read folds, so the bytes continue from the start of the ROM image
    let mut out = Vec::with_capacity(n as usize);
    for i in 0..n {
        let a = (start + i) & mask;
        if a >= rom.len() as i64 {
            return None;
        }
        out.push(rom[a as usize]);
    }
    Some(out)
}

/// Frame count after dropping trailing silence. Never cuts the loop.
fn _trim_frames(body: &[u8], width: usize, loop_off: Option<i64>) -> i64 {
    let mut frames = (body.len() / width) as i64;
    let mut floor = 2i64;
    if let Some(loop_off) = loop_off {
        floor = floor.max(loop_off + 64);
    }
    let silence = vec![0u8; width];
    while frames > floor
        && body[((frames - 1) as usize * width)..(frames as usize * width)] == silence[..]
    {
        frames -= 1;
    }
    frames
}

struct VoiceState {
    reg: [[i32; 8]; C352_VOICES],
    keyed: [bool; C352_VOICES],
    active: [Option<usize>; C352_VOICES],
    vol: [(i32, i32); C352_VOICES],
    quad: [(i32, i32, i32, i32); C352_VOICES],
    freq: [i32; C352_VOICES],
    /// Frames played since key-on, integrated.
    played: [f64; C352_VOICES],
    /// Sample time the integration is up to date to.
    anchor: [i64; C352_VOICES],
    /// Pending natural end after a mid-note loop clear.
    stop: [Option<i64>; C352_VOICES],
    /// Immediate stop after a mid-note BUSY clear.
    hard: [Option<i64>; C352_VOICES],
}

fn _blank_state() -> VoiceState {
    VoiceState {
        reg: [[0; 8]; C352_VOICES],
        keyed: [false; C352_VOICES],
        active: [None; C352_VOICES],
        vol: [(0, 0); C352_VOICES],
        quad: [(0, 0, 0, 0); C352_VOICES],
        freq: [0; C352_VOICES],
        played: [0.0; C352_VOICES],
        anchor: [0; C352_VOICES],
        stop: [None; C352_VOICES],
        hard: [None; C352_VOICES],
    }
}

struct Plan {
    rom_off: i64,
    chunks: Vec<(i64, i64)>,
    n: i64,
    loop_off: Option<i64>,
    loop_mode: i32,
    mulaw: bool,
    reversed: bool,
    link_at: i64,
    start: i64,
}

fn bump(acc: &mut IndexMap<&'static str, i64>, key: &'static str) {
    *acc.entry(key).or_insert(0) += 1;
}

/// Integrate frames played up to time `t` (VGM samples).
fn advance(st: &mut VoiceState, clock: u32, voice: usize, t: i64) {
    let dt = t - st.anchor[voice];
    if dt <= 0 {
        return;
    }
    let freq = st.freq[voice];
    if freq != 0 {
        st.played[voice] += c352_rate(clock, freq) * dt as f64 / (VGM_RATE as f64);
    }
    st.anchor[voice] = t;
}

/// Time the voice reaches wave_end with the loop off, from its position.
fn natural_end(st: &VoiceState, clock: u32, voice: usize, t: i64) -> Option<i64> {
    let start = st.reg[voice][REG_START] & 0xFFFF;
    let end = st.reg[voice][REG_END] & 0xFFFF;
    let loop_ = st.reg[voice][REG_LOOP] & 0xFFFF;
    let first = end - start;
    if first <= 0 {
        return None;
    }
    let played = st.played[voice];
    let pos = if played < first as f64 {
        start as f64 + played
    } else {
        let wrap = end - loop_;
        if wrap <= 0 {
            return None;
        }
        loop_ as f64 + (played - first as f64) % wrap as f64
    };
    let remaining = end as f64 - pos;
    let freq = st.freq[voice];
    if remaining <= 0.0 || freq == 0 {
        return Some(t);
    }
    let rate = c352_rate(clock, freq);
    if rate <= 0.0 {
        return None;
    }
    Some(t + py_int(py_round(remaining * (VGM_RATE as f64) / rate)))
}

/// Emit a pending stop that has passed; keep a future one unless told to
/// cancel.
fn flush_stop(
    st: &mut VoiceState,
    hits: &mut [C352Hit],
    offs: &mut Vec<C352Off>,
    cid: i32,
    voice: usize,
    t: i64,
    cancel: bool,
) {
    let Some(stop) = st.stop[voice] else {
        return;
    };
    if stop <= t {
        offs.push(C352Off {
            sample_time: stop,
            chip_id: cid,
            voice,
        });
        if let Some(idx) = st.active[voice] {
            hits[idx].off_sample = stop;
        }
        st.active[voice] = None; // the voice finished on its own
        st.stop[voice] = None;
    } else if cancel {
        st.stop[voice] = None;
    }
}

/// Emit an immediate stop that has passed; a key event drops it silently.
fn flush_hard(
    st: &mut VoiceState,
    hits: &mut [C352Hit],
    offs: &mut Vec<C352Off>,
    cid: i32,
    voice: usize,
    t: i64,
    cancel: bool,
) {
    let Some(hard) = st.hard[voice] else {
        return;
    };
    if cancel {
        st.hard[voice] = None;
    } else if hard <= t {
        offs.push(C352Off {
            sample_time: hard,
            chip_id: cid,
            voice,
        });
        if let Some(idx) = st.active[voice] {
            hits[idx].off_sample = hard;
        }
        st.active[voice] = None;
        st.hard[voice] = None;
    }
}

/// Describe the bytes a hit plays. `None` means the hit produces no sample.
fn _plan(
    hit: &C352Hit,
    rom: &[u8],
    vgm: &VgmFile,
    acc: &mut IndexMap<&'static str, i64>,
) -> Option<Plan> {
    let flags = hit.flags;
    let mulaw = (flags & FLG_MULAW) != 0;
    let looping = (flags & FLG_LOOP) != 0;
    let reverse = (flags & FLG_REVERSE) != 0;
    let link = (flags & FLG_LINK) != 0;
    let bank = (hit.bank & 0xFFFF) as i64;
    let start = (hit.start & 0xFFFF) as i64;
    let end = (hit.end & 0xFFFF) as i64;
    let mut loop_ = (hit.loop_ & 0xFFFF) as i64;
    let clock = if vgm.c352_clock == 0 {
        C352_DEFAULT_CLOCK
    } else {
        vgm.c352_clock as u32
    };
    let stop = if hit.off_sample != 0 {
        hit.off_sample
    } else {
        let total = vgm.total_samples as i64;
        if total != 0 {
            total
        } else {
            0
        }
    };
    let swing = if reverse {
        (start - end) & 0xFFFF
    } else {
        (end - start) & 0xFFFF
    };
    // a rewrite only matters if the voice reaches the crossing it feeds; a
    // driver that preloads the next note's start/loop on a voice it stops
    // first must not move this note's loop
    let crossed = stop == 0 || hit_played_frames(hit, stop, clock) >= swing as f64;
    let start_l;
    if let Some((geo_start, geo_loop)) = hit.geo_at_cross {
        if crossed {
            // the jump uses the live wave_loop and wave_start (the LINK
            // target's high half); end and bank keep their key-on values. The
            // walk still starts at the key-on address.
            start_l = (geo_start & 0xFFFF) as i64;
            loop_ = (geo_loop & 0xFFFF) as i64;
        } else {
            start_l = start;
        }
    } else {
        start_l = start;
    }
    let base = (bank << 16) | start;

    if reverse && looping {
        let lo = loop_.min(end);
        let hi = loop_.max(end);
        // a voice that starts below the bounce region plays through the
        // pre-attack first; keep those bytes and put the loop at the region.
        let body_start = if start < lo { start } else { lo };
        if start != body_start {
            bump(acc, "ping");
        }
        let rom_off = (bank << 16) | body_start;
        let mut n = (hi - body_start) + 1;
        if n > C352_MAX_FRAMES {
            bump(acc, "cut");
            n = C352_MAX_FRAMES;
        }
        let mut loop_off = lo - body_start;
        if loop_off >= n {
            loop_off = 0;
        }
        return Some(Plan {
            rom_off,
            chunks: vec![(rom_off, n)],
            n,
            loop_off: Some(loop_off),
            loop_mode: 2,
            mulaw,
            reversed: false,
            link_at: 0,
            start: rom_off,
        });
    }

    if reverse {
        if start == end {
            return None;
        }
        if start < end && bank == 0 {
            // the walk would leave the ROM window (the reference reads
            // nothing there), so the voice is silent
            bump(acc, "reverse_wrap");
            return None;
        }
        // a reverse voice walks down from start and stops at wave_end, so the
        // body is the descending run [end + 1, start] (wrapping into the
        // previous bank when start < end).
        let n = (start - end) & 0xFFFF;
        let hi = base;
        let lo = hi - (n - 1);
        if lo < 0 {
            return None;
        }
        return Some(Plan {
            rom_off: lo,
            chunks: vec![(lo, n)],
            n,
            loop_off: None,
            loop_mode: 0,
            mulaw,
            reversed: true,
            link_at: 0,
            start: base,
        });
    }

    let span = (end - start) & 0xFFFF;
    if !looping {
        if span == 0 {
            return None;
        }
        let mut n = span;
        if n > C352_MAX_FRAMES {
            bump(acc, "cut");
            n = C352_MAX_FRAMES;
        }
        return Some(Plan {
            rom_off: base,
            chunks: vec![(base, n)],
            n,
            loop_off: None,
            loop_mode: 0,
            mulaw,
            reversed: false,
            link_at: 0,
            start: base,
        });
    }

    let mut n = span + 1;
    let mut loop_off = (loop_ - start) & 0xFFFF;
    if n > C352_MAX_FRAMES {
        bump(acc, "cut");
        n = C352_MAX_FRAMES;
        if loop_off >= n {
            loop_off = 0;
        }
    }

    // A note that is stopped before the running address reaches wave_end
    // never loops, so the loop region does not have to be carried (the module
    // would only hold bytes its playback never reaches). off_sample 0 means
    // the note was never stopped and runs to the end.
    let reaches_loop = stop == 0 || hit_played_frames(hit, stop, clock) >= n as f64;

    if link {
        let intro_n = n;
        let target = (start_l << 16) | loop_;
        let end_addr = base + intro_n - 1;
        let arrival_bank = (end_addr >> 16) & 0xFFFF;
        let normal = (arrival_bank << 16) | loop_;
        if target != normal && !reaches_loop {
            return Some(Plan {
                rom_off: base,
                chunks: vec![(base, intro_n)],
                n: intro_n,
                loop_off: None,
                loop_mode: 0,
                mulaw,
                reversed: false,
                link_at: 0,
                start: base,
            });
        }
        if target != normal {
            let mut loop_n = ((end - loop_) & 0xFFFF) + 1;
            if intro_n + loop_n > C352_MAX_FRAMES {
                bump(acc, "cut");
                loop_n = 0.max(C352_MAX_FRAMES - intro_n);
            }
            let loop_body = _read(rom, vgm, hit.chip_id, target, loop_n);
            if loop_body.is_none() || loop_body.as_ref().unwrap().len() < loop_n as usize {
                bump(acc, "link_out");
                // The chip jumps into ROM this rip does not dump; that region
                // reads as silence, so only the intro is heard - once. It must
                // not loop, or an ambient that goes quiet on the source keeps
                // droning in the module.
                return Some(Plan {
                    rom_off: base,
                    chunks: vec![(base, intro_n)],
                    n: intro_n,
                    loop_off: None,
                    loop_mode: 0,
                    mulaw,
                    reversed: false,
                    link_at: 0,
                    start: base,
                });
            } else {
                return Some(Plan {
                    rom_off: base,
                    chunks: vec![(base, intro_n), (target, loop_n)],
                    n: intro_n + loop_n,
                    loop_off: Some(intro_n),
                    loop_mode: 0,
                    mulaw,
                    reversed: false,
                    link_at: target,
                    start: base,
                });
            }
        }
    }

    if loop_off >= n {
        // The loop register lies outside the sampled span: a loop register
        // one byte past wave_end. The chip jumps at wave_end to the arrival
        // bank's copy of `loop`, plays there until the low 16 bits meet
        // wave_end again (a full bank for the +1 case) and repeats. Without
        // the region the sample plays its intro once and stops.
        if !reaches_loop {
            return Some(Plan {
                rom_off: base,
                chunks: vec![(base, n)],
                n,
                loop_off: None,
                loop_mode: 0,
                mulaw,
                reversed: false,
                link_at: 0,
                start: base,
            });
        }
        let loop_addr = ((base + n - 1) & 0xFF0000) | loop_;
        let mut loop_n = ((end - loop_) & 0xFFFF) + 1;
        if n + loop_n > C352_MAX_FRAMES {
            bump(acc, "cut");
            loop_n = 0.max(C352_MAX_FRAMES - n);
        }
        let tail = if loop_n != 0 {
            _read(rom, vgm, hit.chip_id, loop_addr, loop_n)
        } else {
            None
        };
        if tail.is_none() || tail.as_ref().unwrap().len() < loop_n as usize {
            bump(acc, "link_out");
            return Some(Plan {
                rom_off: base,
                chunks: vec![(base, n)],
                n,
                loop_off: None,
                loop_mode: 0,
                mulaw,
                reversed: false,
                link_at: 0,
                start: base,
            });
        }
        return Some(Plan {
            rom_off: base,
            chunks: vec![(base, n), (loop_addr, loop_n)],
            n: n + loop_n,
            loop_off: Some(n),
            loop_mode: 0,
            mulaw,
            reversed: false,
            link_at: loop_addr,
            start: base,
        });
    }

    Some(Plan {
        rom_off: base,
        chunks: vec![(base, n)],
        n,
        loop_off: Some(loop_off),
        loop_mode: 0,
        mulaw,
        reversed: false,
        link_at: 0,
        start: base,
    })
}

fn _decode(raw: &[u8], mulaw: bool, reversed_play: bool) -> (Vec<u8>, i32) {
    let raw: Vec<u8> = if reversed_play {
        raw.iter().rev().copied().collect()
    } else {
        raw.to_vec()
    };
    if !mulaw {
        return (raw, 8);
    }
    let mut out = Vec::with_capacity(raw.len() * 2);
    for &b in &raw {
        out.extend_from_slice(&(_mulaw_i16(b) as i16).to_le_bytes());
    }
    (out, 16)
}

fn _flush(acc: &IndexMap<&'static str, i64>, warnings: &mut Vec<String>) {
    if acc.get("undumped").copied().unwrap_or(0) != 0 {
        let v = acc["undumped"];
        warnings.push(format!(
            "{v} C352 triggers point at ROM that the VGM does not dump; those notes are silent"
        ));
    }
    if acc.get("noise").copied().unwrap_or(0) != 0 {
        let v = acc["noise"];
        warnings.push(format!("{v} C352 noise voices are not converted"));
    }
    if acc.get("reverse_wrap").copied().unwrap_or(0) != 0 {
        let v = acc["reverse_wrap"];
        warnings.push(format!(
            "{v} C352 reverse voices wrap past the ROM window; those notes are silent"
        ));
    }
    if acc.get("link_out").copied().unwrap_or(0) != 0 {
        let v = acc["link_out"];
        warnings.push(format!(
            "{v} C352 link targets fall outside the dumped ROM; those notes play the intro once (the chip reads silence past it)"
        ));
    }
    if acc.get("ping").copied().unwrap_or(0) != 0 {
        let v = acc["ping"];
        warnings.push(format!(
            "{v} C352 ping-pong voices start inside the loop; the attack before the bounce is omitted"
        ));
    }
    if acc.get("cut").copied().unwrap_or(0) != 0 {
        let v = acc["cut"];
        warnings.push(format!("{v} C352 samples were cut at {C352_MAX_FRAMES} frames"));
    }
}

pub fn collect_c352(
    vgm: &VgmFile,
    quad: bool,
) -> (Vec<u8>, Vec<C352Sample>, Vec<C352Off>, Vec<String>) {
    let ids: BTreeSet<i32> = vgm
        .writes
        .iter()
        .filter(|w| w.chip == "c352")
        .map(|w| w.chip_id as i32)
        .collect();
    let mut roms: IndexMap<i32, Vec<u8>> = IndexMap::new();
    for cid in ids {
        roms.insert(cid, assemble_rom(vgm, "c352", cid as u8, 0, false, true));
    }
    let mut warnings: Vec<String> = Vec::new();
    let mut offs: Vec<C352Off> = Vec::new();
    if roms.is_empty() {
        return (Vec::new(), Vec::new(), offs, warnings);
    }

    let mute_rear = vgm.c352_mute_rear;
    let mut chips: IndexMap<i32, VoiceState> = IndexMap::new();
    let mut hits: Vec<C352Hit> = Vec::new();
    let mut acc: IndexMap<&'static str, i64> = IndexMap::new();

    // Drivers key a voice with LOOP set and clear the loop flag while it
    // runs, so the chip finishes the sample at wave_end and stops. The flags
    // snapshot at key-on keeps the note looping forever in the module, so a
    // stop is scheduled for the time the voice reaches wave_end. It is
    // cancelled by a retrigger, a key-off, a loop re-set, or a direction
    // change.
    let clock = if vgm.c352_clock == 0 {
        C352_DEFAULT_CLOCK
    } else {
        vgm.c352_clock as u32
    };

    for w in &vgm.writes {
        if w.chip != "c352" {
            continue;
        }
        let sample = w.sample as i64;
        let cid = w.chip_id as i32;
        let st = chips.entry(cid).or_insert_with(_blank_state);
        let addr = w.reg as i32 & 0x7FFF;
        if addr == ADDR_STROBE {
            for voice in 0..C352_VOICES {
                let flags = st.reg[voice][REG_FLAGS];
                if (flags & FLG_KEYON) != 0 {
                    // a retrigger supersedes a future natural end and a BUSY
                    // clear
                    flush_stop(&mut *st, &mut hits, &mut offs, cid, voice, sample, true);
                    flush_hard(&mut *st, &mut hits, &mut offs, cid, voice, sample, true);
                    let active = st.active[voice];
                    st.played[voice] = 0.0;
                    st.anchor[voice] = sample;
                    if (flags & FLG_NOISE) != 0 {
                        if let Some(idx) = active {
                            hits[idx].off_sample = sample;
                            offs.push(C352Off {
                                sample_time: sample,
                                chip_id: cid,
                                voice,
                            });
                            st.active[voice] = None;
                        }
                        st.keyed[voice] = false;
                        bump(&mut acc, "noise");
                    } else {
                        if let Some(idx) = active {
                            hits[idx].off_sample = sample;
                        }
                        let pair = _mix(
                            st.reg[voice][REG_VOL_F],
                            st.reg[voice][REG_VOL_R],
                            mute_rear,
                        );
                        let quad_vol = _quad(
                            st.reg[voice][REG_VOL_F],
                            st.reg[voice][REG_VOL_R],
                            mute_rear,
                        );
                        let freq = st.reg[voice][REG_FREQ] & 0xFFFF;
                        let hit = C352Hit {
                            sample_time: sample,
                            chip_id: cid,
                            voice,
                            bank: st.reg[voice][REG_BANK],
                            start: st.reg[voice][REG_START],
                            end: st.reg[voice][REG_END],
                            loop_: st.reg[voice][REG_LOOP],
                            flags,
                            freq,
                            vol_l: pair.0,
                            vol_r: pair.1,
                            off_sample: 0,
                            vol_pts: Vec::new(),
                            front_l: quad_vol.0,
                            front_r: quad_vol.1,
                            rear_l: quad_vol.2,
                            rear_r: quad_vol.3,
                            quad_pts: Vec::new(),
                            freq_pts: Vec::new(),
                            geo_at_cross: None,
                        };
                        let idx = hits.len();
                        hits.push(hit);
                        st.active[voice] = Some(idx);
                        st.keyed[voice] = true;
                        st.vol[voice] = pair;
                        st.quad[voice] = quad_vol;
                        st.freq[voice] = freq;
                    }
                    st.reg[voice][REG_FLAGS] =
                        (flags | FLG_BUSY) & !(FLG_KEYON | FLG_LOOPHIST);
                } else if (flags & FLG_KEYOFF) != 0 {
                    // the write already silenced the voice; the strobe only
                    // adds a second stop when none has fired
                    flush_hard(&mut *st, &mut hits, &mut offs, cid, voice, sample, false);
                    flush_stop(&mut *st, &mut hits, &mut offs, cid, voice, sample, true);
                    if st.keyed[voice] && st.active[voice].is_some() {
                        offs.push(C352Off {
                            sample_time: sample,
                            chip_id: cid,
                            voice,
                        });
                        let idx = st.active[voice].unwrap();
                        hits[idx].off_sample = sample;
                        st.active[voice] = None;
                    }
                    st.keyed[voice] = false;
                    st.reg[voice][REG_FLAGS] = flags & !(FLG_BUSY | FLG_KEYOFF);
                } else {
                    // a natural end or BUSY clear that has passed fires
                    flush_stop(&mut *st, &mut hits, &mut offs, cid, voice, sample, false);
                    flush_hard(&mut *st, &mut hits, &mut offs, cid, voice, sample, false);
                }
            }
            continue;
        }
        if addr >= 0x100 {
            continue;
        }
        let voice = (addr >> 3) as usize;
        let r = (addr & 7) as usize;
        let val = w.val as i32 & 0xFFFF;
        let prev = st.reg[voice][r];
        st.reg[voice][r] = val;
        if !st.keyed[voice] {
            continue;
        }
        let Some(idx) = st.active[voice] else {
            continue;
        };
        advance(&mut *st, clock, voice, sample);
        if r == REG_VOL_F || r == REG_VOL_R {
            let pair = _mix(
                st.reg[voice][REG_VOL_F],
                st.reg[voice][REG_VOL_R],
                mute_rear,
            );
            if pair != st.vol[voice] {
                hits[idx].vol_pts.push((sample, pair.0, pair.1));
                st.vol[voice] = pair;
            }
            if quad {
                let four = _quad(
                    st.reg[voice][REG_VOL_F],
                    st.reg[voice][REG_VOL_R],
                    mute_rear,
                );
                if four != st.quad[voice] {
                    hits[idx].quad_pts.push((sample, four.0, four.1, four.2, four.3));
                    st.quad[voice] = four;
                }
            }
        } else if r == REG_FREQ {
            let f = val & 0xFFFF;
            if f != st.freq[voice] {
                hits[idx].freq_pts.push((sample, f));
                st.freq[voice] = f;
            }
            if st.stop[voice].is_some() {
                let ns = natural_end(&*st, clock, voice, sample);
                st.stop[voice] = ns;
            }
        } else if r == REG_FLAGS {
            if ((prev ^ val) & FLG_REVERSE) != 0 {
                // direction change: the run-to-end model no longer applies
                st.stop[voice] = None;
            }
            if (prev & FLG_BUSY) != 0 && (val & FLG_BUSY) == 0 {
                // the write clears BUSY: the chip stops the voice at once
                st.stop[voice] = None;
                st.hard[voice] = Some(sample);
            } else {
                let old_loop = prev & FLG_LOOP;
                let new_loop = val & FLG_LOOP;
                if old_loop != 0 && new_loop == 0 && (val & FLG_REVERSE) == 0 {
                    // the driver lets the voice finish at wave_end: schedule
                    // the stop
                    let ns = natural_end(&*st, clock, voice, sample);
                    st.stop[voice] = ns;
                } else if old_loop == 0 && new_loop != 0 {
                    // loops again: a stop that already passed still counts
                    st.stop[voice] = None;
                }
            }
        } else if r == REG_START || r == REG_LOOP {
            // The chip jumps with the live registers when the running address
            // reaches wave_end, and these drivers rewrite start/loop a few ms
            // after the strobe, pointing the LINK jump at the byte after the
            // intro (the sample's real continuation). The body must use the
            // rewrite; a write after the crossing is for the next pass. end
            // and bank are also rewritten mid-note, but as stop/shorten
            // tricks, so the key-on values stay in charge.
            if st.played[voice] < (((hits[idx].end - hits[idx].start) & 0xFFFF) as f64) {
                hits[idx].geo_at_cross =
                    Some((st.reg[voice][REG_START], st.reg[voice][REG_LOOP]));
            }
        } else if r == REG_END || r == REG_LOOP {
            if st.stop[voice].is_some() {
                let ns = natural_end(&*st, clock, voice, sample);
                st.stop[voice] = ns;
            }
        }
    }

    for (&cid, st) in chips.iter_mut() {
        for voice in 0..C352_VOICES {
            if let Some(stop) = st.stop[voice] {
                offs.push(C352Off {
                    sample_time: stop,
                    chip_id: cid,
                    voice,
                });
                if let Some(idx) = st.active[voice] {
                    hits[idx].off_sample = stop;
                }
                st.stop[voice] = None;
            }
            if let Some(hard) = st.hard[voice] {
                offs.push(C352Off {
                    sample_time: hard,
                    chip_id: cid,
                    voice,
                });
                if let Some(idx) = st.active[voice] {
                    hits[idx].off_sample = hard;
                }
                st.hard[voice] = None;
            }
        }
    }

    if hits.is_empty() {
        _flush(&acc, &mut warnings);
        return (Vec::new(), Vec::new(), offs, warnings);
    }
    if !roms.values().any(|r| !r.is_empty()) {
        warnings.push(
            "no C352 ROM block (0x92); PCM notes will be silent until samples are added".into(),
        );
        _flush(&acc, &mut warnings);
        return (Vec::new(), Vec::new(), offs, warnings);
    }

    let mut by_key: IndexMap<(i32, i64, i64, Option<i64>, i32, bool, bool, i64), C352Sample> =
        IndexMap::new();
    for h in hits.iter() {
        let rom: &[u8] = match roms.get(&h.chip_id) {
            Some(r) if !r.is_empty() => r.as_slice(),
            _ => {
                bump(&mut acc, "undumped");
                continue;
            }
        };
        let Some(mut plan) = _plan(h, rom, vgm, &mut acc) else {
            continue;
        };
        // fold the ROM-window alias so an 0x80xxxxxx hit and its plain address
        // share one body (and one name) instead of duplicating the sample
        let plan_mask = _rom_mask(vgm, h.chip_id);
        plan.rom_off &= plan_mask;
        plan.start &= plan_mask;
        if plan.link_at != 0 {
            plan.link_at &= plan_mask;
        }
        let mut parts: Vec<Vec<u8>> = Vec::new();
        let mut failed = false;
        for &(off, n) in &plan.chunks {
            match _read(rom, vgm, h.chip_id, off, n) {
                None => {
                    failed = true;
                    break;
                }
                Some(chunk) => parts.push(chunk),
            }
        }
        if failed {
            bump(&mut acc, "undumped");
            continue;
        }
        let mut raw: Vec<u8> = Vec::new();
        for part in &parts {
            raw.extend_from_slice(part);
        }
        if raw.is_empty() {
            continue;
        }
        // A clipped intro still loops at the bytes that were actually read.
        let mut loop_off: Option<i64>;
        if plan.link_at != 0 && !parts.is_empty() {
            let lo = parts[0].len() as i64;
            loop_off = if lo <= 0 || lo >= raw.len() as i64 {
                None
            } else {
                Some(lo)
            };
        } else {
            loop_off = plan.loop_off;
            if let Some(lo) = loop_off {
                if lo >= raw.len() as i64 {
                    loop_off = None;
                }
            }
        }
        let key = (
            h.chip_id,
            plan.rom_off,
            raw.len() as i64,
            loop_off,
            plan.loop_mode,
            plan.mulaw,
            plan.reversed,
            plan.link_at,
        );
        if !by_key.contains_key(&key) {
            let (data, depth) = _decode(&raw, plan.mulaw, plan.reversed);
            let width = if depth == 16 { 2 } else { 1 };
            let frames = if plan.loop_mode == 2 {
                (data.len() / width) as i64
            } else {
                _trim_frames(&data, width, loop_off)
            };
            if let Some(lo) = loop_off {
                if lo >= frames {
                    loop_off = None;
                }
            }
            let smp = C352Sample {
                start: plan.start as i32,
                data: data[..frames as usize * width].to_vec(),
                loop_start: loop_off,
                loop_end: if loop_off.is_some() {
                    Some(frames)
                } else {
                    None
                },
                hits: Vec::new(),
                depth,
                loop_mode: if loop_off.is_some() {
                    plan.loop_mode
                } else {
                    0
                },
            };
            by_key.insert(key, smp);
        }
        by_key.get_mut(&key).unwrap().hits.push(h.clone());
    }

    _flush(&acc, &mut warnings);
    let mut samples: Vec<C352Sample> = by_key.into_values().collect();
    samples.sort_by(|a, b| {
        (
            a.start,
            a.loop_start.unwrap_or(-1),
            a.loop_mode,
        )
            .cmp(&(b.start, b.loop_start.unwrap_or(-1), b.loop_mode))
    });
    let rom0 = roms
        .get(&0)
        .cloned()
        .unwrap_or_else(|| roms.values().next().cloned().unwrap_or_default());
    (rom0, samples, offs, warnings)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vgm::{ChipWrite, RomSlice};

    #[test]
    fn rate_is_clock_times_freq_over_divisor() {
        assert_eq!(c352_rate(24_192_000, 0x8000), 42000.0);
        assert_eq!(c352_rate(0, 0x8000), 42000.0);
        assert_eq!(c352_rate(4_000_000, 0x84), 4_000_000.0 * 132.0 / 18_874_368.0);
        assert_eq!(c352_rate(24_192_000, 0), 0.0);
    }

    #[test]
    fn merge_keeps_last_write_of_a_cluster() {
        let pts = [(0i64, 1i32), (10, 2), (40, 3), (45, 4)];
        assert_eq!(merge_step_writes(&pts, 16), vec![(10, 2), (45, 4)]);
        assert_eq!(merge_step_writes(&[], 16), Vec::<(i64, i32)>::new());
    }

    #[test]
    fn volume_folding_and_mulaw_table() {
        assert_eq!(_mix(0x8040, 0x2010, false), (160, 80));
        assert_eq!(_mix(0x8040, 0x2010, true), (128, 64));
        assert_eq!(_quad(0x8040, 0x2010, false), (128, 64, 32, 16));
        assert_eq!(_quad(0x8040, 0x2010, true), (128, 64, 0, 0));
        assert_eq!(_sides(0x8040), (128, 64));
        // C352 table: byte 0x01 expands to 32, not C140's 256.
        assert_eq!(_mulaw_i16(0), 0);
        assert_eq!(_mulaw_i16(1), 32);
        assert_eq!(_mulaw_i16(0x80), -32);
        assert_eq!(C352_MULAW.len(), 256);
    }

    #[test]
    fn empty_vgm_collects_nothing() {
        let vgm = VgmFile::default();
        let (rom, samples, offs, warnings) = collect_c352(&vgm, false);
        assert!(rom.is_empty());
        assert!(samples.is_empty());
        assert!(offs.is_empty());
        assert!(warnings.is_empty());
    }

    #[test]
    fn keyon_decode_reads_start_to_end() {
        let mut vgm = VgmFile::default();
        vgm.c352_clock = 24_192_000;
        vgm.roms.push(RomSlice {
            chip: 0,
            rom_size: 8,
            start: 0,
            data: vec![1, 2, 3, 4, 5, 6, 7, 8],
            kind: "c352".into(),
            time: 0,
        });
        let write = |sample, reg, val| ChipWrite {
            sample,
            chip: "c352".into(),
            chip_id: 0,
            reg,
            val,
        };
        vgm.writes = vec![
            write(0, REG_START as u32, 0),
            write(0, REG_END as u32, 4),
            write(0, REG_FREQ as u32, 0x4000),
            write(1, REG_FLAGS as u32, FLG_KEYON as u32),
            write(2, ADDR_STROBE as u32, 0),
        ];
        let (rom, samples, offs, warnings) = collect_c352(&vgm, false);
        assert_eq!(rom.len(), 8);
        assert_eq!(samples.len(), 1);
        assert!(offs.is_empty());
        assert!(warnings.is_empty());
        let smp = &samples[0];
        assert_eq!(smp.start, 0);
        assert_eq!(smp.data, vec![1, 2, 3, 4]);
        assert_eq!(smp.depth, 8);
        assert_eq!(smp.loop_start, None);
        assert_eq!(smp.loop_mode, 0);
        assert_eq!(smp.mode_freq(), 0x4000);
        assert_eq!(smp.c4_rate(24_192_000), 21000);
    }
}
