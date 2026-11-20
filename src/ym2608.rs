//! YM2608 (OPNA) support: two banked FM blocks, rhythm and delta-T.
//!
//! The OPNA reaches its registers through two buses. `$56` writes bus A: the
//! SSG below `$10`, the rhythm section at `$10-$1F`, then FM block 1 from `$20`
//! (ymfm's `ym2608::write_data`, ymfm_opn.cpp:1179-1209). `$57` writes bus B:
//! ADPCM-B below `$11`, FM block 2 above it (`write_data_hi`, 1229-1259).
//! `vgm.py` flattens that into `(bus << 8) | register`, which is the address
//! space `YM2610State` already decodes. Furnace's channel offsets for the OPNA
//! are `{0x00, 0x01, 0x02, 0x100, 0x101, 0x102}` and its key-on values
//! `{0, 1, 2, 4, 5, 6}` (ym2608.h:41-47): one register on bus A keys all six
//! channels, bit 2 picking the block. Those are `CHAN_OFF_6` and `KON_6`, so
//! the FM half is a six-channel YM2610B and only the two low pages move.
//!
//! The rhythm page is the YM2610's ADPCM-A page in a different place: ymfm gives
//! the same engine `$10 & 0x0F` as its register number, so control lands on 0,
//! the global level on 1 and `$18+d` - `(pan << 6) | level` - on `8+d`, which
//! is what `_write_adpcm_a` decodes and `adpcm_a_level` folds. What differs is
//! the data: the six voices are ranges of a ROM compiled into Furnace
//! (ymfm_opn.cpp:1006-1011), so no sample travels with the module and a voice
//! is chosen by which bit of `$10` the driver sets. Bit 7 makes those bits a
//! release instead of a hit.
//!
//! ADPCM-B is the same delta-T engine one bus over, with its registers `$10`
//! lower than the YM2610's page - so `+ 0x10` hands them straight to
//! `_write_adpcm_b`. Its addresses are byte offsets divided by 32 (Furnace
//! recomputes them as `offset >> 5`, ym2608.cpp:1182-1186), its rate is
//! `chipClock/144 * delta / 65535` (ym2608.cpp:297-303) and repeat - what
//! Furnace sets for a loopable sample, `0xb0` against `0xa0` (line 1061) - is
//! bit 4 of the control byte. Bus A `$29` is the IRQ mask, where the YM2612
//! keeps its DAC data.

use std::ops::{Deref, DerefMut};

use indexmap::IndexMap;

use crate::vgm::VgmFile;
use crate::ym2610::{
    pan_byte, AdpcmBHit, AdpcmBSample, YM2610State, ADPCM_A_DIV, ADPCM_B_DIV,
};
use crate::ym2612::YM2612_FREQ_BASE;

// Furnace's own default is 8 MHz; a real FM Towns rip says 7987200 and the
// difference moves every f-num, so the clock travels with the module.
pub const YM2608_DEFAULT_CLOCK: u32 = 7_987_200;
// DivPlatformYM2608's ctor: fmFreqBase 9437184, fmDivBase 72 and ayDiv 32 at
// the /6 prescale (ym2608.cpp:2091-2097). The frequency base is the YM2612's to
// the unit, so the OPNA needs no note path of its own - every banked-OPN chip
// converts f-num to note through ym2612_note_for, and only the clock is per
// chip. Only the SSG divider differs from the YM2203's 16.
pub const YM2608_FREQ_BASE: f64 = YM2612_FREQ_BASE;
pub const YM2608_SSG_DIVIDER: i32 = 32;

// $102-$105 hold the byte address shifted right by five.
pub const ADPCM_B_ADDR_SHIFT: i32 = 5;

// ymfm's ADPCM-A channel order, which is also the order sysDef names the six
// rhythm columns (sysDef.cpp:1365-1370): bit i of $10 keys voice i, and
// $18+i is its level.
pub const RHYTHM_VOICES: [(&str, &str); 6] = [
    ("BD", "Kick"),
    ("SD", "Snare"),
    ("TP", "Top"),
    ("HH", "HiHat"),
    ("TM", "Tom"),
    ("RM", "Rim"),
];

// A driver may key a drum while its level byte still holds the previous (often
// muted) value and write the real level a little later. A level write this soon
// after the key is the hit's own level, not a step on the sounding voice.
pub const RHYTHM_LEVEL_FOLD: i64 = 512;

// Where each voice lives in the rhythm ROM. The engine reads one nibble per
// chipClock/432 tick, so the range alone says how long a hit can still be
// heard - without any sample data in the module.
pub const RHYTHM_ROM: [(i64, i64); 6] = [
    (0x0000, 0x01BF), // bass drum
    (0x01C0, 0x043F), // snare drum
    (0x0440, 0x1B7F), // top cymbal
    (0x1B80, 0x1CFF), // high hat
    (0x1D00, 0x1F7F), // tom tom
    (0x1F80, 0x1FFF), // rim shot
];

// Channels per system: six FM (nine when channel 3 is split), three SSG, six
// rhythm and one ADPCM-B. sysDef gives 16 and 19 (sysDef.cpp:1352,1379).
pub const YM2608_CHANNELS: usize = 16;
pub const YM2608_EXT_CHANNELS: usize = 19;

/// Register image of one YM2608.
///
/// The FM decode, the key-on map and channel-3 special mode are inherited
/// from the six-channel YM2610B unchanged. Only the two low pages move, and
/// they replace what the YM2610 keeps there: its delta-T page on bus A
/// becomes the rhythm page, and its ADPCM-A page on bus B becomes ADPCM-B.
/// Bus B `$10` is the IRQ flag control and `$11-$2D` are unimplemented,
/// so both stop there rather than reach the inherited decoders.
#[derive(Clone, Debug, PartialEq)]
pub struct YM2608State {
    /// The inherited YM2610B register image (`b = true`).
    pub base: YM2610State,
    /// The `$10` key byte as last written, so a release reads as one.
    pub rhythm_key: i32,
    // The banked-OPN collector asks whether the DAC has taken a channel over.
    // The OPNA has no DAC: bus A $2A/$29 is its IRQ mask, and channel 6 stays
    // an FM voice.
    pub dac_enabled: bool,
}

impl YM2608State {
    pub fn new() -> Self {
        YM2608State {
            base: YM2610State::new(true),
            rhythm_key: 0,
            dac_enabled: false,
        }
    }

    pub fn write(&mut self, reg: i32, val: i32) {
        let bank = if reg & 0x100 != 0 { 1 } else { 0 };
        let r = reg & 0xFF;
        if bank == 0 {
            if (0x10..=0x1F).contains(&r) {
                self.base._write_adpcm_a(r & 0x0F, val);
                if r == 0x10 {
                    self.rhythm_key = val;
                }
                return;
            }
        } else if r <= 0x2D {
            if r < 0x10 {
                self.base._write_adpcm_b(r + 0x10, val);
            }
            return;
        }
        self.base.write(reg, val);
    }

    /// The stereo byte of `$B4` as the banked-OPN collector names it.
    ///
    /// The YM2610 module calls this `pan_byte_ch`; the shared collector was
    /// written against the YM2612, so the OPNA answers to both names.
    pub fn pan_byte(&self, ch: usize) -> i32 {
        self.base.pan_byte_ch(ch)
    }

    /// The 0-31 column value that reproduces `$18+voice`.
    ///
    /// Only the per-voice field is returned. `$11` is the kit's global
    /// level, and the platform exposes it as an `1Fxx` effect; folding it
    /// into every hit clamps quiet kits to column 0 and hides the level the
    /// driver actually wrote.
    pub fn rhythm_level(&self, voice: usize) -> i32 {
        self.base.a_vol[voice] & 0x1F
    }

    pub fn rhythm_pan(&self, voice: usize) -> i32 {
        pan_byte(self.base.a_pan[voice])
    }
}

impl Deref for YM2608State {
    type Target = YM2610State;

    fn deref(&self) -> &YM2610State {
        &self.base
    }
}

impl DerefMut for YM2608State {
    fn deref_mut(&mut self) -> &mut YM2610State {
        &mut self.base
    }
}

/// One rhythm voice sounding between two writes of the key byte.
#[derive(Clone, Debug, PartialEq)]
pub struct RhythmHit {
    pub sample_time: i64,
    pub chip_id: u8,
    pub voice: usize,
    pub vol: i32,
    pub pan: i32,
    pub end_time: i64,
    /// Level and pan moves while the voice sounds, past its gate window.
    pub vol_pts: Vec<(i64, i32)>,
    pub pan_pts: Vec<(i64, i32)>,
}

impl RhythmHit {
    pub fn new(sample_time: i64, chip_id: u8, voice: usize, vol: i32, pan: i32) -> Self {
        RhythmHit {
            sample_time,
            chip_id,
            voice,
            vol,
            pan,
            end_time: 0,
            vol_pts: Vec::new(),
            pan_pts: Vec::new(),
        }
    }
}

/// True while the voice's fixed ROM body can still be heard at `t`.
///
/// A level write on a voice that has already run out of sample is the driver
/// tidying its registers. Keeping it would hang a volume cell on a row where
/// the chip is silent, and change nothing anyone can hear.
fn _voice_playing(clock: u32, hit: &RhythmHit, t: i64) -> bool {
    let (lo, hi) = RHYTHM_ROM[hit.voice];
    let seconds = (hi - lo + 1) as f64 * 2.0 / (clock as f64 / ADPCM_A_DIV as f64);
    (t - hit.sample_time) as f64 / 44100.0 <= seconds
}

/// Turn the bus-A rhythm page into one hit per voice key.
///
/// A driver gates a voice by rewriting `$10`; the same byte with bit 7 set
/// releases it. A key on a voice that is already sounding is a retrigger, so
/// the old hit ends where the new one starts. A level written inside the
/// gate window belongs to the note about to sound and moves its column, a
/// later one is a step on the voice already playing.
pub fn collect_ym2608_rhythm(vgm: &VgmFile) -> (Vec<RhythmHit>, Vec<String>) {
    let warnings: Vec<String> = Vec::new();
    let clock = if vgm.ym2608_clock != 0 {
        vgm.ym2608_clock
    } else {
        YM2608_DEFAULT_CLOCK
    };
    let mut states: IndexMap<u8, YM2608State> = IndexMap::new();
    let mut active: IndexMap<(u8, usize), usize> = IndexMap::new();
    let mut hits: Vec<RhythmHit> = Vec::new();

    for w in &vgm.writes {
        if w.chip != "ym2608" {
            continue;
        }
        let reg = w.reg as i32;
        let val = w.val as i32;
        let r = reg & 0xFF;
        if reg & 0x100 != 0 || !(0x10..=0x1F).contains(&r) {
            continue;
        }
        let st = states.entry(w.chip_id).or_insert_with(YM2608State::new);
        if r == 0x12 {
            continue; // the trigger sample register; it drives no voice here
        }
        st.write(reg, val);
        if r == 0x10 {
            let bits = val & 0x3F;
            let release = val & 0x80 != 0;
            for voice in 0..RHYTHM_VOICES.len() {
                if bits & (1 << voice) == 0 {
                    continue;
                }
                let old = active.shift_remove(&(w.chip_id, voice));
                if let Some(idx) = old {
                    hits[idx].end_time = w.sample;
                }
                if release {
                    continue;
                }
                let hit = RhythmHit::new(
                    w.sample,
                    w.chip_id,
                    voice,
                    st.rhythm_level(voice),
                    st.rhythm_pan(voice),
                );
                hits.push(hit);
                active.insert((w.chip_id, voice), hits.len() - 1);
            }
            continue;
        }
        if r == 0x11 {
            // The global level moves every voice that is sounding.
            for (&(cid, voice), &idx) in active.iter() {
                if cid != w.chip_id {
                    continue;
                }
                let hit = &mut hits[idx];
                let vol = st.rhythm_level(voice);
                let dt = w.sample - hit.sample_time;
                if (0..=RHYTHM_LEVEL_FOLD).contains(&dt) {
                    hit.vol = vol;
                } else if _voice_playing(clock, hit, w.sample) {
                    let last = hit.vol_pts.last().map_or(hit.vol, |pt| pt.1);
                    if vol != last {
                        hit.vol_pts.push((w.sample, vol));
                    }
                }
            }
            continue;
        }
        let voice = r - 0x18;
        if !(0..RHYTHM_VOICES.len() as i32).contains(&voice) {
            continue;
        }
        let voice = voice as usize;
        let Some(&idx) = active.get(&(w.chip_id, voice)) else {
            continue;
        };
        let hit = &mut hits[idx];
        let vol = st.rhythm_level(voice);
        let pan = st.rhythm_pan(voice);
        let dt = w.sample - hit.sample_time;
        if (0..=RHYTHM_LEVEL_FOLD).contains(&dt) {
            hit.vol = vol;
            hit.pan = pan;
        } else if _voice_playing(clock, hit, w.sample) {
            let last = hit.vol_pts.last().map_or(hit.vol, |pt| pt.1);
            if vol != last {
                hit.vol_pts.push((w.sample, vol));
            }
            let last = hit.pan_pts.last().map_or(hit.pan, |pt| pt.1);
            if pan != last {
                hit.pan_pts.push((w.sample, pan));
            }
        }
    }

    let end = vgm.total_samples;
    for &idx in active.values() {
        if hits[idx].end_time == 0 {
            hits[idx].end_time = end;
        }
    }
    (hits, warnings)
}

// libvgm's ym2608 delta-T memory: address shift 5 for ROM and 8-bit DRAM,
// 2 for 1-bit DRAM (ymdeltat.c, dram_rightshift).
pub const DELTAT_PORT_SHIFT: i32 = 5;
pub const _DELTAT_DRAM_SHIFT: [i32; 4] = [3, 0, 0, 0];

/// Rebuild the ADPCM-B memory from the port-B data-register writes.
///
/// A VGM can upload a delta-T sample instead of shipping a ROM block: the
/// driver puts the unit in external-memory write mode (`$100 = $60`), sets
/// the start/stop address and writes bytes through `$108`.  VGMPlay does
/// exactly what the chip does (`YM_DELTAT_ADPCM_Write` in
/// `ymdeltat.c`), so the sample only exists after replaying that stream.
/// The 0x81 block in such a log declares the memory size and carries no data,
/// which otherwise leaves a zero-filled ROM and silent/wrong samples.
pub fn replay_deltat_memory(vgm: &VgmFile, base: &[u8]) -> Vec<u8> {
    replay_deltat_memory_chip(vgm, base, 0)
}

/// The chip-id form of [`replay_deltat_memory`] for a dual OPNA.
pub fn replay_deltat_memory_chip(vgm: &VgmFile, base: &[u8], chip_id: u8) -> Vec<u8> {
    let size = base.len() as i64;
    let mut mem = base.to_vec();
    let mut regs = [0i32; 16];
    let mut portstate = 0i32;
    let mut control2 = 0i32;
    let mut shift = DELTAT_PORT_SHIFT;
    let mut memread = 0i32;
    let mut now_addr = 0i64;
    let mut start = 0i64;
    let mut end = 0i64;
    let mask = if size != 0 && (size & (size - 1)) == 0 {
        size - 1
    } else {
        0
    };
    for w in &vgm.writes {
        if w.chip != "ym2608" || w.chip_id != chip_id {
            continue;
        }
        if w.reg & 0x100 == 0 {
            continue;
        }
        let r = w.reg & 0xFF;
        if r > 0x0F {
            continue;
        }
        let v = w.val as i32 & 0xFF;
        regs[r as usize] = v;
        if r == 0x00 {
            portstate = v & (0x80 | 0x40 | 0x20 | 0x10 | 0x01);
            if portstate & 0x20 != 0 {
                now_addr = start << 1;
                memread = 2;
                if mem.is_empty() {
                    portstate = 0;
                }
            } else {
                now_addr = 0;
            }
        } else if r == 0x01 {
            if (v & 3) != (control2 & 3) {
                let new_shift = DELTAT_PORT_SHIFT - _DELTAT_DRAM_SHIFT[(v & 3) as usize];
                if new_shift != shift {
                    shift = new_shift;
                    start = (((regs[0x03] << 8) | regs[0x02]) as i64) << shift;
                    end = (((regs[0x05] << 8) | regs[0x04]) as i64) << shift;
                    end |= (1i64 << shift) - 1;
                }
            }
            control2 = v;
        } else if r == 0x02 || r == 0x03 {
            start = (((regs[0x03] << 8) | regs[0x02]) as i64) << shift;
        } else if r == 0x04 || r == 0x05 {
            end = (((regs[0x05] << 8) | regs[0x04]) as i64) << shift;
            end |= (1i64 << shift) - 1;
        } else if r == 0x08 {
            if (portstate & 0xE0) != 0x60 || mem.is_empty() {
                continue;
            }
            if memread != 0 {
                now_addr = start << 1;
                memread = 0;
            }
            if now_addr != (end << 1) {
                let mut index = now_addr >> 1;
                index = if mask != 0 { index & mask } else { index % size };
                mem[index as usize] = v as u8;
                now_addr += 2;
            }
        }
    }
    mem
}

/// Pad a delta-T body out to the 256-byte length Furnace will play.
///
/// `DivSample` sizes a depth-6 sample as `(((count+1)/2)+255)&~0xff`
/// (sample.cpp:593) and the platform plays `offset` to `offset + lengthB -
/// 1`, so a body whose declared length is not a multiple of 256 makes the chip
/// read whatever sample was packed after it. Tailing on the idle nibble costs
/// at most 255 bytes of silence and keeps that read inside our own sample.
pub fn deltat_store_body(data: &[u8]) -> Vec<u8> {
    let mut out = data.to_vec();
    if out.len() & 0xFF != 0 {
        out.extend(std::iter::repeat_n(0x80u8, 0x100 - (out.len() & 0xFF)));
    }
    out
}

/// One delta-T range at the chip's own 32-byte address granularity.
///
/// `$102-$105` name blocks of 32, so this is the byte span the source actually
/// played - no wider. Rounding down to a 256-byte block instead, which is what
/// an ADPCM-A ROM bank wants, would put bytes from *before* the sample in front
/// of it and delay its attack by up to 255 bytes. The tail is
/// [`deltat_store_body`] business, on the other end. A range running past the
/// end of the data block is padded with the nibble that holds the accumulator
/// still.
pub fn _deltat_slice(rom: &[u8], start: i32, end: i32) -> Vec<u8> {
    if rom.is_empty() {
        return Vec::new();
    }
    let size = 1i64 << ADPCM_B_ADDR_SHIFT;
    let start = (start as i64) & !(size - 1);
    let mut end = ((end as i64) & !(size - 1)) | (size - 1);
    if end < start {
        end = start + size - 1;
    }
    let stop = std::cmp::min(rom.len() as i64, end + 1);
    let mut data = if start >= stop {
        Vec::new()
    } else {
        rom[start as usize..stop as usize].to_vec()
    };
    if (data.len() as i64) < end - start + 1 {
        data.extend(std::iter::repeat_n(
            0x80u8,
            (end - start + 1 - data.len() as i64) as usize,
        ));
    }
    data
}

/// Walk bus B's ADPCM-B page and group its plays into samples.
///
/// A play is a write of `$100` with bit 7 set; bit 4 asks for a repeat and
/// bit 0 resets the engine. Clearing bit 7 stops it too - ymfm's
/// `adpcm_b_channel::clock` returns to idle without it, which is how a
/// driver releases a sample it does not want to hear again.
pub fn collect_ym2608_delta_t(vgm: &VgmFile, rom: &[u8]) -> (Vec<AdpcmBSample>, Vec<String>) {
    let mut warnings: Vec<String> = Vec::new();
    let clock = if vgm.ym2608_clock != 0 {
        vgm.ym2608_clock
    } else {
        YM2608_DEFAULT_CLOCK
    };
    let mut states: IndexMap<u8, YM2608State> = IndexMap::new();
    let mut active: IndexMap<u8, usize> = IndexMap::new();
    let mut hits: Vec<AdpcmBHit> = Vec::new();

    // Move the hit's play position to `t`. False once it has ended.
    fn advance(hit: &mut AdpcmBHit, t: i64, clock: u32) -> bool {
        if hit.ended {
            return false;
        }
        let dt = t - hit.play_t;
        if dt > 0 && !hit.r#loop {
            let rate_hz =
                clock as f64 / ADPCM_B_DIV as f64 * (hit.play_rate & 0xFFFF) as f64 / 65535.0;
            hit.play_pos += dt as f64 * rate_hz / 44100.0;
        }
        hit.play_t = t;
        if !hit.r#loop && hit.play_pos >= (hit.end - hit.start + 1) as f64 {
            hit.ended = true;
            return false;
        }
        true
    }

    fn follows(
        vgm: &VgmFile,
        wi: usize,
        cid: u8,
        reg_no: i32,
        t: i64,
        span: usize,
    ) -> bool {
        // True when a write of this bus-B register follows within `span`.
        //
        // The low rate byte alone is half a value: treating it as a change
        // would step the sample to whatever stale high byte was left in the
        // register. A second control write inside the gate window is a
        // retrigger, so the moves it carries belong to the next note, not
        // this one.
        let limit = if reg_no == 0x09 { 8 } else { 64 };
        let end = std::cmp::min(wi + span, vgm.writes.len());
        for nw in &vgm.writes[wi + 1..end] {
            if nw.sample - t > limit {
                break;
            }
            if nw.chip == "ym2608"
                && nw.chip_id == cid
                && (nw.reg & 0x1FF) == (0x100 | reg_no as u32)
            {
                return true;
            }
        }
        false
    }

    for (wi, w) in vgm.writes.iter().enumerate() {
        if w.chip != "ym2608" {
            continue;
        }
        let reg = w.reg as i32;
        let val = w.val as i32;
        let r = reg & 0xFF;
        if reg & 0x100 == 0 || r > 0x0F {
            continue;
        }
        let st = states.entry(w.chip_id).or_insert_with(YM2608State::new);
        st.write(reg, val);
        let hit = active.get(&w.chip_id).copied();
        if let Some(idx) = hit {
            let hit = &mut hits[idx];
            let dt = w.sample - hit.sample_time;
            if (0..=64).contains(&dt) {
                // The driver commonly writes the pan, and sometimes the
                // level or rate, a few samples after the start bit. That is
                // this note's own setting, not a step inside it.
                if r == 0x01 {
                    hit.pan = pan_byte(st.b_pan);
                } else if r == 0x09 || r == 0x0A {
                    hit.rate = st.b_freq;
                    hit.play_rate = hit.rate;
                } else if r == 0x0B {
                    hit.vol = st.b_vol;
                }
            } else if dt > 64
                && !follows(vgm, wi, w.chip_id, 0x00, w.sample, 25)
                && advance(hit, w.sample, clock)
            {
                if r == 0x01 {
                    let pan = pan_byte(st.b_pan);
                    let last = hit.pan_pts.last().map_or(hit.pan, |pt| pt.1);
                    if pan != last {
                        hit.pan_pts.push((w.sample, pan));
                    }
                } else if r == 0x0B {
                    let vol = st.b_vol;
                    let last = hit.vol_pts.last().map_or(hit.vol, |pt| pt.1);
                    if vol != last {
                        hit.vol_pts.push((w.sample, vol));
                    }
                } else if r == 0x0A
                    || (r == 0x09 && !follows(vgm, wi, w.chip_id, 0x0A, w.sample, 9))
                {
                    let rate = st.b_freq;
                    let last = hit.rate_pts.last().map_or(hit.rate, |pt| pt.1);
                    if rate != last {
                        hit.rate_pts.push((w.sample, rate));
                        hit.play_rate = rate;
                    }
                }
            }
        }
        if r != 0x00 {
            continue;
        }
        let start = val & 0x80 != 0;
        let reset = val & 0x01 != 0;
        let old = active.shift_remove(&w.chip_id);
        if !start || reset {
            if let Some(idx) = old {
                hits[idx].end_time = w.sample;
            }
            continue;
        }
        if let Some(idx) = old {
            hits[idx].end_time = w.sample;
        }
        let lo = st.b_start << ADPCM_B_ADDR_SHIFT;
        let mut hi = (st.b_end << ADPCM_B_ADDR_SHIFT)
            | ((1i32 << ADPCM_B_ADDR_SHIFT) - 1);
        if hi < lo {
            hi = lo + ((1i32 << ADPCM_B_ADDR_SHIFT) - 1);
        }
        let mut hit = AdpcmBHit::new(
            w.sample,
            w.chip_id,
            lo,
            hi,
            st.b_freq,
            st.b_vol,
            pan_byte(st.b_pan),
            val & 0x10 != 0,
        );
        hit.play_t = w.sample;
        hit.play_rate = st.b_freq;
        hits.push(hit);
        active.insert(w.chip_id, hits.len() - 1);
    }

    let end = vgm.total_samples;
    for &idx in active.values() {
        if hits[idx].end_time == 0 {
            hits[idx].end_time = end;
        }
    }

    // A pan of 0 silences the voice, but Furnace's panning command reads 0 as
    // center. A mute that never returns ends the note where it was written, so
    // the sample cannot play on past where the source fell silent.
    for hit in hits.iter_mut() {
        for idx in 0..hit.pan_pts.len() {
            let (t, pan) = hit.pan_pts[idx];
            if pan != 0 || hit.pan_pts[idx + 1..].iter().any(|&(_, p)| p != 0) {
                continue;
            }
            hit.end_time = t;
            hit.pan_pts.truncate(idx);
            hit.vol_pts.retain(|&(pt, _)| pt < t);
            hit.rate_pts.retain(|&(pt, _)| pt < t);
            break;
        }
    }

    let mut samples: IndexMap<(u8, i32, i32, i32), AdpcmBSample> = IndexMap::new();
    for hit in hits {
        let key = (hit.chip_id, hit.start, hit.end, hit.rate);
        let smp = samples.entry(key).or_insert_with(|| {
            let data = _deltat_slice(rom, hit.start, hit.end);
            AdpcmBSample::new(hit.chip_id, hit.start, hit.end, hit.rate, data)
        });
        smp.hits.push(hit);
    }

    if !samples.is_empty() && rom.is_empty() {
        warnings.push(
            "YM2608 ADPCM-B key-ons but no delta-T data block (0x81); the ADPCM channel is silent"
                .to_string(),
        );
    }
    (samples.into_values().collect(), warnings)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vgm::ChipWrite;

    fn vgm(writes: Vec<ChipWrite>, total_samples: i64) -> VgmFile {
        VgmFile {
            total_samples,
            writes,
            ym2608_clock: YM2608_DEFAULT_CLOCK,
            ..VgmFile::default()
        }
    }

    #[test]
    fn state_decodes_both_low_pages_and_ignores_the_irq_pair() {
        let mut st = YM2608State::new();
        st.write(0x11, 49);
        st.write(0x18, (3 << 6) | 26);
        assert_eq!(st.a_master, 49);
        assert_eq!(st.a_vol[0], 26);
        assert_eq!(st.a_pan[0], 3);
        for (reg, val) in [
            (0x101, 0xC0),
            (0x102, 0x60),
            (0x103, 0x01),
            (0x104, 0xBF),
            (0x105, 0x01),
            (0x109, 0x64),
            (0x10A, 0x62),
            (0x10B, 139),
        ] {
            st.write(reg, val);
        }
        assert_eq!(st.b_pan, 3);
        assert_eq!(st.b_start, 0x160);
        assert_eq!(st.b_end, 0x1BF);
        assert_eq!(st.b_freq, 0x6264);
        assert_eq!(st.b_vol, 139);
        // Port 1 $10 and port 0 $29/$2A are the IRQ pair, not a decoder.
        let before = (st.b_control, st.b_pan, st.b_freq, st.a_master, st.dac_enabled);
        st.write(0x110, 0xFF);
        st.write(0x29, 0xFF);
        st.write(0x2A, 0x80);
        assert_eq!(
            (st.b_control, st.b_pan, st.b_freq, st.a_master, st.dac_enabled),
            before
        );
    }

    #[test]
    fn rhythm_collector_folds_the_gate_window_only() {
        let writes = vec![
            ChipWrite::new(90, "ym2608", 0, 0x18 + 1, (3 << 6) | 0),
            ChipWrite::new(100, "ym2608", 0, 0x10, 0x02),
            ChipWrite::new(410, "ym2608", 0, 0x18 + 1, (3 << 6) | 24),
            ChipWrite::new(800, "ym2608", 0, 0x18 + 1, (3 << 6) | 8),
        ];
        let (hits, warn) = collect_ym2608_rhythm(&vgm(writes, 8000));
        assert!(warn.is_empty());
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].vol, 24);
        assert_eq!(hits[0].vol_pts, vec![(800, 8)]);
        assert_eq!(hits[0].pan, 0xFF);
    }

    #[test]
    fn delta_t_collector_slices_the_range_and_folds_late_steps() {
        let rom: Vec<u8> = (0..0x1000u32).map(|i| ((i * 7 + 3) & 0xFF) as u8).collect();
        let writes = vec![
            // start 0x200, end 0xFFF, rate 0x4000, pan both, then start+loop.
            ChipWrite::new(100, "ym2608", 0, 0x102, 0x10),
            ChipWrite::new(100, "ym2608", 0, 0x103, 0x00),
            ChipWrite::new(100, "ym2608", 0, 0x104, 0x7F),
            ChipWrite::new(100, "ym2608", 0, 0x105, 0x00),
            ChipWrite::new(100, "ym2608", 0, 0x109, 0x00),
            ChipWrite::new(100, "ym2608", 0, 0x10A, 0x40),
            ChipWrite::new(100, "ym2608", 0, 0x10B, 200),
            ChipWrite::new(100, "ym2608", 0, 0x101, 0xC0),
            ChipWrite::new(101, "ym2608", 0, 0x100, 0x90),
            ChipWrite::new(600, "ym2608", 0, 0x10B, 90),
            // The rate's low byte alone is half a value; the high byte at 701
            // commits it and is the step the module keeps.
            ChipWrite::new(700, "ym2608", 0, 0x109, 0x00),
            ChipWrite::new(701, "ym2608", 0, 0x10A, 0x80),
        ];
        let (smp, warn) = collect_ym2608_delta_t(&vgm(writes.clone(), 8000), &rom);
        assert!(warn.is_empty());
        assert_eq!(smp.len(), 1);
        let h = &smp[0].hits[0];
        assert_eq!((h.start, h.end), (0x200, 0xFFF));
        assert_eq!(h.rate, 0x4000);
        assert!(h.r#loop);
        assert_eq!(h.pan, 0xFF);
        assert_eq!(smp[0].data.as_slice(), &rom[0x200..0x1000]);
        assert!(h.vol_pts.contains(&(600, 90)));
        assert!(h.rate_pts.contains(&(701, 0x8000)));
        // The same stream with no 0x81 block leaves the channel silent.
        let (_, warn) = collect_ym2608_delta_t(&vgm(writes, 8000), b"");
        assert!(warn.iter().any(|w| w.contains("0x81")), "{warn:?}");
    }

    #[test]
    fn deltat_slice_keeps_the_chip_address_unit() {
        let rom: Vec<u8> = (0..0x600u32).map(|i| ((i * 5 + 1) & 0xFF) as u8).collect();
        let body = _deltat_slice(&rom, 0x260, 0x3FF);
        assert_eq!(&body[..8], &rom[0x260..0x268]);
        assert_eq!(body.len() % 32, 0);
        let padded = deltat_store_body(&vec![1u8; 832]);
        assert_eq!(padded.len(), 1024);
        assert_eq!(&padded[832..], &[0x80u8; 192][..]);
    }

    #[test]
    fn delta_t_register_upload_rebuilds_the_memory() {
        let data: Vec<u8> = (0..31u32).map(|i| ((i * 11 + 5) & 0xFF) as u8).collect();
        let mut writes = vec![
            ChipWrite::new(0, "ym2608", 0, 0x100, 0x01),
            ChipWrite::new(0, "ym2608", 0, 0x101, 0x02),
            ChipWrite::new(0, "ym2608", 0, 0x102, 0x04),
            ChipWrite::new(0, "ym2608", 0, 0x103, 0x00),
            ChipWrite::new(0, "ym2608", 0, 0x104, 0x04),
            ChipWrite::new(0, "ym2608", 0, 0x105, 0x00),
            ChipWrite::new(0, "ym2608", 0, 0x100, 0x60),
        ];
        for b in &data {
            writes.push(ChipWrite::new(0, "ym2608", 0, 0x108, *b as u32));
        }
        let mem = replay_deltat_memory(&vgm(writes, 0), &vec![0u8; 0x200]);
        assert_eq!(&mem[0x80..0x80 + data.len()], &data[..]);
    }
}
