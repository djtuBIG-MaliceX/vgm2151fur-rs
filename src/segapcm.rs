//! SegaPCM ROM → 8-bit signed samples → Furnace.
//!
//! Register model (libvgm segapcm.c, the core VGM rips are logged against):
//!
//! ```text
//! voice = (offset >> 3) & 0xF, reg = (offset & 7) | (offset & 0x80)
//! ```
//!
//! The 0x80 half carries the playback address (0x84/0x85) and control (0x86);
//! the low half carries volume L/R (0x02/0x03), loop address (0x04/0x05), end
//! page (0x06) and the sample delta / pitch (0x07).
//!
//! The start/loop registers are a 16-bit byte address inside a 64K window.
//! Playback stops at end_byte = ((end + 1) & 0xFF) << 8 in that window. The
//! ROM byte is
//!
//! ```text
//! ((ctrl & bankmask) << bankshift) | reg_addr
//! ```
//!
//! where bankshift is VGM header 0x3C, bankmask is header 0x3E (0 means 0x70)
//! masked with (0x1fffff >> shift). The stored body is those unsigned bytes
//! XOR 0x80 (silence 0x80 becomes 0). The playback rate is
//!
//! ```text
//! bytes/second = clock * freq / 32768      (16 voices -> clock / 128 ticks)
//! ```
//!
//! Control bits: 0 = disable (active when clear), 1 = loop disable (one-shot),
//! bits selected by bankmask = ROM bank.
//!
//! 0x84/0x85 are the live cursor, not a latched start address. The chip adds
//! the delta every tick (MAME sega_315_5218 voice_t::tick, libvgm SEGAPCM_update)
//! and a CPU write replaces those bits of the cursor (voice_addr_w). Rewriting
//! the original start after the cursor has moved seeks back to it, which is a
//! new note. A volume or delta rewrite that leaves the cursor on the same byte
//! is the same note.
//!
//! One Z80 dump writes the address, then the end page and delta, then control.
//! Across the logged sets that dump spans at most 3 VGM samples. Writes
//! within 4 samples are one note, snapshotted after the dump.

use indexmap::IndexMap;

use crate::parity::{py_int, py_round};
use crate::vgm::{assemble_rom, VgmFile, VGM_RATE};

pub const SEGAPCM_VOICES: usize = 16;
pub const SEGAPCM_SILENCE: u8 = 0x80; // unsigned centre of an idle SegaPCM body
pub const SEGAPCM_RATE_DIV: f64 = 32768.0;
pub const SEGAPCM_BANK_MASK7: i32 = 0x70; // libvgm default when header 0x3E is 0
// 16 voices, each updated at clock/128 (MAME CLOCK_DIVIDER = voices*8).
const _TICK_DENOM: i64 = 128 * VGM_RATE as i64;
const _ADDR_MASK: i32 = 0xFFFFFF;
// Width of one register dump. Drivers write a dump within ~3 samples;
// restarts 13 samples apart stay apart.
const _REGISTER_BURST: i64 = 4;

/// libvgm device_start_segapcm: mask & (0x1fffff >> shift), default 0x70.
pub fn segapcm_bankmask(shift: i32, intf_mask: i32) -> i32 {
    let mut mask = intf_mask & 0xFF;
    if mask == 0 {
        mask = SEGAPCM_BANK_MASK7;
    }
    let shift = shift & 0xFF;
    let window = if shift < 21 { 0x1FFFFF >> shift } else { 0 };
    mask & window
}

/// Hardware playback rate in bytes/second for an 8-bit delta register.
pub fn segapcm_rate(clock: u32, freq: i32) -> f64 {
    let clock = if clock == 0 { 4_000_000 } else { clock };
    (clock as f64) * ((freq & 0xFF) as f64) / SEGAPCM_RATE_DIV
}

/// MAME voice_t power-on values (segapcm.h). Control bit 0 set = stopped.
#[allow(non_camel_case_types)]
#[derive(Clone, Debug, PartialEq)]
pub struct _Voice {
    pub addr: i32,
    pub r#loop: i32,
    pub end: i32,
    pub freq: i32,
    pub lvol: i32,
    pub rvol: i32,
    pub ctrl: i32,
}

impl Default for _Voice {
    fn default() -> Self {
        _Voice {
            addr: 0xFFFF00,
            r#loop: 0xFFFF,
            end: 0xFF,
            freq: 0xFF,
            lvol: 0xFF,
            rvol: 0xFF,
            ctrl: 0xFF,
        }
    }
}

#[allow(non_camel_case_types)]
#[derive(Clone, Debug, PartialEq)]
struct _Burst {
    start: i64,
    retrigger: bool,
    when: Option<i64>,
    window: Option<i32>,
    // Mid-note volume (L, R) and frequency writes seen before this burst
    // commits.  A retrigger dump discards them: its values are the new note's
    // snapshot, not a step away from the previous note.
    pending_vol: Vec<(i64, i32, i32)>,
    pending_freq: Vec<(i64, i32)>,
}

fn _ticks_until_stop(addr: i32, freq: i32, stop_page: i32) -> i64 {
    // Playback ticks until addr lands in stop_page. 0 if it is already there.
    //
    // freq is 1..255: one tick cannot jump over a 64K page.
    if ((addr >> 16) & 0xFF) == stop_page {
        return 0;
    }
    let target = (stop_page << 16) as i64;
    let cur = (addr & _ADDR_MASK) as i64;
    let gap = if cur < target {
        target - cur
    } else {
        (_ADDR_MASK as i64 + 1 - cur) + target
    };
    (gap + freq as i64 - 1) / freq as i64
}

fn _loop_cycle(r#loop: i32, freq: i32, stop_page: i32) -> i64 {
    // Ticks from the address just after a loop relocation back to itself.
    let addr = ((((r#loop & 0xFFFF) as i64) << 8) + freq as i64) & _ADDR_MASK as i64;
    if (((addr as i32) >> 16) & 0xFF) == stop_page {
        return 1;
    }
    _ticks_until_stop(addr as i32, freq, stop_page) + 1
}

/// Run MAME voice_t::tick n_ticks times.
///
/// A disabled voice clears the fractional byte and holds. An enabled voice
/// plays until page (end+1), then either sets control bit 0 (one-shot) or
/// reloads the loop address and keeps going. The stop is tested before the
/// delta add, so the end page itself is not played.
#[allow(non_snake_case)]
pub fn _advance_voice(voice: &mut _Voice, mut n_ticks: i64) {
    if n_ticks <= 0 {
        return;
    }
    if voice.ctrl & 1 != 0 {
        voice.addr &= 0xFFFF00;
        return;
    }
    let freq = (voice.freq & 0xFF) as i64;
    let stop = (voice.end + 1) & 0xFF;
    if freq == 0 {
        if n_ticks != 0 && ((voice.addr >> 16) & 0xFF) == stop {
            if voice.ctrl & 2 != 0 {
                voice.ctrl |= 1;
                if n_ticks > 1 {
                    voice.addr &= 0xFFFF00;
                }
            } else {
                voice.addr = (voice.r#loop & 0xFFFF) << 8;
            }
        }
        return;
    }
    while n_ticks > 0 {
        if ((voice.addr >> 16) & 0xFF) == stop {
            if voice.ctrl & 2 != 0 {
                voice.ctrl |= 1;
                n_ticks -= 1;
                if n_ticks > 0 {
                    voice.addr &= 0xFFFF00;
                }
                return;
            }
            voice.addr =
                ((((voice.r#loop & 0xFFFF) as i64) << 8) + freq) as i32 & _ADDR_MASK;
            n_ticks -= 1;
            if n_ticks == 0 {
                return;
            }
            let cycle = _loop_cycle(voice.r#loop, freq as i32, stop);
            if cycle <= 1 {
                return;
            }
            n_ticks %= cycle;
            continue;
        }
        let n = _ticks_until_stop(voice.addr, freq as i32, stop);
        if n <= 0 {
            return;
        }
        let step = if n < n_ticks { n } else { n_ticks };
        voice.addr =
            ((voice.addr as i64 + step * freq) & _ADDR_MASK as i64) as i32;
        n_ticks -= step;
    }
}

/// Apply one CPU write. Return (seek, off_to_on, bank_changed, window).
fn _apply_write(voice: &mut _Voice, off: i32, val: i32, bankmask: i32) -> (bool, bool, bool, i32) {
    let r = off & 7;
    let val = val & 0xFF;
    let mut window = (voice.addr >> 8) & 0xFFFF;
    if off & 0x80 != 0 {
        if r == 4 || r == 5 {
            let shift = if r == 4 { 8 } else { 16 };
            let old = window;
            voice.addr = (voice.addr & !(0xFF << shift)) | (val << shift);
            voice.addr &= _ADDR_MASK;
            window = (voice.addr >> 8) & 0xFFFF;
            return (window != old, false, false, window);
        }
        if r == 6 {
            let old = voice.ctrl;
            voice.ctrl = val;
            let off_to_on = (old & 1) != 0 && (val & 1) == 0;
            let bank_changed = ((old ^ val) & bankmask) != 0 && (val & 1) == 0;
            return (false, off_to_on, bank_changed, (voice.addr >> 8) & 0xFFFF);
        }
        return (false, false, false, window);
    }
    if r == 2 {
        voice.lvol = val & 0x7F;
    } else if r == 3 {
        voice.rvol = val & 0x7F;
    } else if r == 4 {
        voice.r#loop = (voice.r#loop & 0xFF00) | val;
    } else if r == 5 {
        voice.r#loop = (voice.r#loop & 0x00FF) | (val << 8);
    } else if r == 6 {
        voice.end = val;
    } else if r == 7 {
        voice.freq = val;
    }
    (false, false, false, window)
}

#[derive(Clone, Debug, PartialEq)]
pub struct SegapcmHit {
    pub sample_time: i64,
    pub voice: i32,
    pub start: i32,
    pub end: i32,
    pub r#loop: i32,
    pub loop_on: bool,
    pub freq: i32,
    pub vol_l: i32,
    pub vol_r: i32,
    pub ctrl: i32,
    pub chip_id: i32,
    // Mid-note register writes while the voice stays keyed: the (L, R) volume
    // pair (the balance is part of the write) and the 8-bit frequency.
    pub vol_pts: Vec<(i64, i32, i32)>,
    pub freq_pts: Vec<(i64, i32)>,
}

/// The frequency a hit actually plays at (0 when the driver never set one).
///
/// The register dump carries the pitch, so the key-on snapshot is usually
/// right.  A voice keyed with a zero frequency takes the first later write,
/// which is the note's own pitch rather than a bend.
pub fn hit_played_freq(hit: &SegapcmHit) -> i32 {
    if hit.freq & 0xFF != 0 {
        return hit.freq & 0xFF;
    }
    for (_t, f) in &hit.freq_pts {
        if f & 0xFF != 0 {
            return f & 0xFF;
        }
    }
    0
}

#[derive(Clone, Debug, PartialEq)]
pub struct SegapcmSample {
    pub start: i32,
    pub data: Vec<u8>, // signed 8-bit
    pub loop_start: Option<i32>,
    pub loop_end: Option<i32>,
    pub hits: Vec<SegapcmHit>,
    pub chip_id: i32,
}

impl SegapcmSample {
    pub fn length(&self) -> usize {
        self.data.len()
    }

    /// Most common nonzero key-on frequency (0 when unknown).
    ///
    /// A zero register means the driver had not set the pitch yet (or muted
    /// the voice), which must not pin the sample's reference rate to 0.
    pub fn mode_freq(&self) -> i32 {
        if self.hits.is_empty() {
            return 0;
        }
        // Counter.most_common(1): highest count, first-seen on ties.
        let mut freqs: IndexMap<i32, i64> = IndexMap::new();
        for h in &self.hits {
            let f = hit_played_freq(h);
            if f != 0 {
                *freqs.entry(f).or_insert(0) += 1;
            }
        }
        if freqs.is_empty() {
            return 0;
        }
        let mut best_f = 0;
        let mut best_n = i64::MIN;
        for (f, n) in freqs {
            if n > best_n {
                best_n = n;
                best_f = f;
            }
        }
        best_f
    }

    pub fn c4_rate(&self, clock: u32) -> i32 {
        std::cmp::max(1000, py_int(py_round(segapcm_rate(clock, self.mode_freq()))) as i32)
    }
}

/// Drop trailing idle 0x80 bytes; never cut into the loop body.
fn _trim_end(rom: &[u8], start: usize, end: usize, loop_off: Option<i32>) -> usize {
    let mut floor = 2usize;
    if let Some(lo) = loop_off {
        floor = std::cmp::max(floor, lo as usize + 64);
    }
    let mut end = end;
    while end > start + floor && rom[end - 1] == SEGAPCM_SILENCE {
        end -= 1;
    }
    end
}

struct Collector {
    voices: IndexMap<i32, Vec<_Voice>>,
    open_burst: IndexMap<i32, Vec<Option<_Burst>>>,
    hits: Vec<SegapcmHit>,
    // Index into `hits` for the note currently sounding on each voice.
    active: IndexMap<i32, Vec<Option<usize>>>,
    vol_last: IndexMap<i32, Vec<(i32, i32)>>,
    freq_last: IndexMap<i32, Vec<i32>>,
    bankmask: i32,
    clock: i64,
    tick_acc: i64,
    last_t: i64,
}

impl Collector {
    fn advance_to(&mut self, t: i64) {
        let dt = t - self.last_t;
        if dt <= 0 {
            return;
        }
        self.tick_acc += dt * self.clock;
        let n = self.tick_acc / _TICK_DENOM;
        self.tick_acc -= n * _TICK_DENOM;
        self.last_t = t;
        if n != 0 {
            for chip_voices in self.voices.values_mut() {
                for voice in chip_voices.iter_mut() {
                    _advance_voice(voice, n);
                }
            }
        }
    }

    fn commit(&mut self, cid: i32, i: usize) {
        let burst = match self.open_burst.get_mut(&cid).unwrap()[i].take() {
            Some(b) => b,
            None => return,
        };
        let voice_end = self.voices[&cid][i].end;
        let voice_loop = self.voices[&cid][i].r#loop;
        let voice_freq = self.voices[&cid][i].freq;
        let voice_ctrl = self.voices[&cid][i].ctrl;
        let voice_lvol = self.voices[&cid][i].lvol;
        let voice_rvol = self.voices[&cid][i].rvol;
        if burst.retrigger && burst.window.is_some() {
            if voice_ctrl & 1 != 0 {
                self.active.get_mut(&cid).unwrap()[i] = None;
                return;
            }
            let hit = SegapcmHit {
                sample_time: match burst.when {
                    Some(w) => w,
                    None => burst.start,
                },
                voice: i as i32,
                start: burst.window.unwrap() & 0xFFFF,
                end: voice_end & 0xFF,
                r#loop: voice_loop & 0xFFFF,
                loop_on: voice_ctrl & 2 == 0,
                freq: voice_freq & 0xFF,
                vol_l: voice_lvol & 0x7F,
                vol_r: voice_rvol & 0x7F,
                ctrl: voice_ctrl & 0xFF,
                chip_id: cid,
                vol_pts: Vec::new(),
                freq_pts: Vec::new(),
            };
            self.hits.push(hit);
            let idx = self.hits.len() - 1;
            let h = &self.hits[idx];
            let (vol_l, vol_r, freq) = (h.vol_l, h.vol_r, h.freq);
            self.active.get_mut(&cid).unwrap()[i] = Some(idx);
            self.vol_last.get_mut(&cid).unwrap()[i] = (vol_l, vol_r);
            self.freq_last.get_mut(&cid).unwrap()[i] = freq;
            return;
        }
        // A burst without a retrigger is a mid-note register update.  Flush
        // the volume and frequency steps it carried to the sounding note.
        // The steps were captured while it was still playing, so the last
        // one before a disable (usually a fade to zero) still counts.
        let active_idx = self.active[&cid][i];
        if let Some(idx) = active_idx {
            let mut vlast = self.vol_last[&cid][i];
            for &(t, left, right) in &burst.pending_vol {
                if (left, right) != vlast {
                    self.hits[idx].vol_pts.push((t, left, right));
                    vlast = (left, right);
                }
            }
            self.vol_last.get_mut(&cid).unwrap()[i] = vlast;
            let mut flast = self.freq_last[&cid][i];
            for &(t, freq) in &burst.pending_freq {
                if freq != flast {
                    self.hits[idx].freq_pts.push((t, freq));
                    flast = freq;
                }
            }
            self.freq_last.get_mut(&cid).unwrap()[i] = flast;
        }
        if voice_ctrl & 1 != 0 {
            self.active.get_mut(&cid).unwrap()[i] = None;
        }
    }

    fn close_expired(&mut self, t: i64) {
        let cids: Vec<i32> = self.open_burst.keys().copied().collect();
        for cid in cids {
            for i in 0..SEGAPCM_VOICES {
                let expired = match &self.open_burst[&cid][i] {
                    Some(burst) => t - burst.start > _REGISTER_BURST,
                    None => false,
                };
                if expired {
                    self.commit(cid, i);
                }
            }
        }
    }
}

pub fn collect_segapcm(vgm: &VgmFile) -> (Vec<u8>, Vec<SegapcmSample>, Vec<String>) {
    let mut id_set: std::collections::BTreeSet<i32> = std::collections::BTreeSet::new();
    for w in &vgm.writes {
        if w.chip == "segapcm" {
            id_set.insert(w.chip_id as i32);
        }
    }
    let ids: Vec<i32> = id_set.iter().copied().collect();
    let rom_ids: Vec<i32> = if ids.is_empty() { vec![0] } else { ids.clone() };
    let mut roms: IndexMap<i32, Vec<u8>> = IndexMap::new();
    for cid in rom_ids {
        roms.insert(cid, assemble_rom(vgm, "segapcm", cid as u8, 0, false, true));
    }
    let rom = roms.get(&0).cloned().unwrap_or_default();
    let mut warnings: Vec<String> = Vec::new();
    if ids.is_empty() {
        return (rom, Vec::new(), warnings);
    }

    let mut voices: IndexMap<i32, Vec<_Voice>> = IndexMap::new();
    let mut open_burst: IndexMap<i32, Vec<Option<_Burst>>> = IndexMap::new();
    let mut active: IndexMap<i32, Vec<Option<usize>>> = IndexMap::new();
    let mut vol_last: IndexMap<i32, Vec<(i32, i32)>> = IndexMap::new();
    let mut freq_last: IndexMap<i32, Vec<i32>> = IndexMap::new();
    for &cid in &ids {
        voices.insert(cid, (0..SEGAPCM_VOICES).map(|_| _Voice::default()).collect());
        open_burst.insert(cid, (0..SEGAPCM_VOICES).map(|_| None).collect());
        active.insert(cid, (0..SEGAPCM_VOICES).map(|_| None).collect());
        vol_last.insert(cid, vec![(0, 0); SEGAPCM_VOICES]);
        freq_last.insert(cid, vec![0; SEGAPCM_VOICES]);
    }
    let bankmask = segapcm_bankmask(
        vgm.segapcm_bankshift as i32,
        vgm.segapcm_bankmask as i32,
    );
    let clock = if vgm.segapcm_clock == 0 {
        4_000_000
    } else {
        vgm.segapcm_clock as i64
    };
    let mut st = Collector {
        voices,
        open_burst,
        hits: Vec::new(),
        active,
        vol_last,
        freq_last,
        bankmask,
        clock,
        tick_acc: 0,
        last_t: 0,
    };

    for w in &vgm.writes {
        if w.chip != "segapcm" {
            continue;
        }
        let raw_cid = w.chip_id as i32;
        let cid = if st.voices.contains_key(&raw_cid) {
            raw_cid
        } else {
            0
        };
        let off = (w.reg as i32) & 0x7FF;
        let vi = ((off >> 3) & 0xF) as usize;
        st.close_expired(w.sample);
        st.advance_to(w.sample);
        let bankmask = st.bankmask;
        let (seek, off_to_on, bank_changed, window) = {
            let voice = &mut st.voices.get_mut(&cid).unwrap()[vi];
            _apply_write(voice, off, w.val as i32, bankmask)
        };
        if st.open_burst.get(&cid).unwrap()[vi].is_none() {
            st.open_burst.get_mut(&cid).unwrap()[vi] = Some(_Burst {
                start: w.sample,
                retrigger: false,
                when: None,
                window: None,
                pending_vol: Vec::new(),
                pending_freq: Vec::new(),
            });
        }
        {
            let burst = st.open_burst.get_mut(&cid).unwrap()[vi].as_mut().unwrap();
            if seek || off_to_on || bank_changed {
                burst.retrigger = true;
                if burst.when.is_none() {
                    burst.when = Some(w.sample);
                }
                if seek || burst.window.is_none() {
                    burst.window = Some(window);
                }
            }
        }
        // Collect volume/pitch changes made while the voice keeps playing.
        // The burst decides later whether they are steps or the next note's
        // key-on values.
        let vctrl = st.voices[&cid][vi].ctrl;
        if vctrl & 1 == 0 && off & 0x80 == 0 {
            let r = off & 7;
            if r == 2 || r == 3 {
                let pair = (
                    st.voices[&cid][vi].lvol & 0x7F,
                    st.voices[&cid][vi].rvol & 0x7F,
                );
                if pair != st.vol_last[&cid][vi] {
                    let burst = st.open_burst.get_mut(&cid).unwrap()[vi]
                        .as_mut()
                        .unwrap();
                    let same_sample =
                        burst.pending_vol.last().map(|l| l.0 == w.sample) == Some(true);
                    if same_sample {
                        let last = burst.pending_vol.last_mut().unwrap();
                        *last = (w.sample, pair.0, pair.1);
                    } else {
                        burst.pending_vol.push((w.sample, pair.0, pair.1));
                    }
                }
            } else if r == 7 {
                let freq = st.voices[&cid][vi].freq & 0xFF;
                if freq != st.freq_last[&cid][vi] {
                    let burst = st.open_burst.get_mut(&cid).unwrap()[vi]
                        .as_mut()
                        .unwrap();
                    let same_sample =
                        burst.pending_freq.last().map(|l| l.0 == w.sample) == Some(true);
                    if same_sample {
                        let last = burst.pending_freq.last_mut().unwrap();
                        *last = (w.sample, freq);
                    } else {
                        burst.pending_freq.push((w.sample, freq));
                    }
                }
            }
        }
    }
    for &cid in &ids {
        for i in 0..SEGAPCM_VOICES {
            st.commit(cid, i);
        }
    }

    let hits = std::mem::take(&mut st.hits);
    if !roms.values().any(|r| !r.is_empty()) {
        if !hits.is_empty() {
            warnings.push(
                "no SegaPCM ROM block (0x80); PCM notes will be silent until samples are added"
                    .to_string(),
            );
        }
        return (rom, Vec::new(), warnings);
    }

    let mut by_key: IndexMap<(i32, i64, i64, Option<i32>), SegapcmSample> = IndexMap::new();
    let shift = (vgm.segapcm_bankshift as i32) & 0xFF;
    for h in hits {
        // End page is inside the 64K window; the bank is only the ROM offset.
        let mut win_end = (((h.end + 1) & 0xFF) << 8) as i64;
        if win_end <= h.start as i64 {
            win_end = h.start as i64 + 2;
        }
        // The window mask is zero for shift >= 21, so the bank base is zero
        // there; guard the Rust shift width the same way.
        let bank_base: i64 = if shift < 21 {
            ((h.ctrl & bankmask) as i64) << shift
        } else {
            0
        };
        let abs_start = bank_base | h.start as i64;
        let abs_end = if win_end > 0xFFFF {
            bank_base + win_end
        } else {
            bank_base | win_end
        };
        let chip_rom: &[u8] = roms.get(&h.chip_id).map(|r| r.as_slice()).unwrap_or(&[]);
        if abs_start >= chip_rom.len() as i64 || abs_end <= abs_start {
            continue;
        }
        let abs_end = std::cmp::min(abs_end, chip_rom.len() as i64);
        let mut loop_off: Option<i32> = None;
        if h.loop_on && h.start as i64 <= h.r#loop as i64
            && (h.r#loop as i64) < std::cmp::min(win_end, 0x10000)
        {
            loop_off = Some(h.r#loop - h.start);
        }
        let key = (h.chip_id, abs_start, abs_end, loop_off);
        if !by_key.contains_key(&key) {
            let mut loop_off = loop_off;
            let end_trim = _trim_end(
                chip_rom,
                abs_start as usize,
                abs_end as usize,
                loop_off,
            );
            if let Some(lo) = loop_off {
                if lo as i64 >= end_trim as i64 - abs_start {
                    loop_off = None;
                }
            }
            let raw = &chip_rom[abs_start as usize..end_trim];
            let body: Vec<u8> = raw.iter().map(|b| b ^ 0x80).collect();
            let loop_end = if loop_off.is_some() {
                Some((end_trim as i64 - abs_start) as i32)
            } else {
                None
            };
            let smp = SegapcmSample {
                start: abs_start as i32,
                data: body,
                loop_start: loop_off,
                loop_end,
                hits: Vec::new(),
                chip_id: h.chip_id,
            };
            by_key.insert(key, smp);
        }
        by_key.get_mut(&key).unwrap().hits.push(h);
    }
    let mut samples: Vec<SegapcmSample> = by_key.into_values().collect();
    // Python sorts on `s.loop_start or -1`, so a zero loop start sorts as -1.
    samples.sort_by_key(|s| {
        (
            s.chip_id,
            s.start,
            match s.loop_start {
                Some(v) if v != 0 => v,
                _ => -1,
            },
        )
    });
    (rom, samples, warnings)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bankmask_defaults_and_window() {
        assert_eq!(segapcm_bankmask(0, 0), 0x70);
        assert_eq!(segapcm_bankmask(12, 0), 0x70);
        assert_eq!(segapcm_bankmask(12, 0x70), 0x70);
        assert_eq!(segapcm_bankmask(12, 0xC0), 0xC0);
        assert_eq!(segapcm_bankmask(21, 0x70), 0);
        assert_eq!(segapcm_bankmask(200, 0x70), 0);
    }

    #[test]
    fn rate_uses_clock_default_and_delta_low_byte() {
        assert_eq!(segapcm_rate(0, 0x84), 4_000_000.0 * 132.0 / 32768.0);
        assert_eq!(segapcm_rate(4_000_000, 0x84), 16113.28125);
        assert_eq!(segapcm_rate(4_000_000, 0x184), segapcm_rate(4_000_000, 0x84));
    }

    #[test]
    fn apply_write_reports_seek_and_key_on() {
        let mut v = _Voice::default();
        // Default cursor page 0xFFFF: writing address bits 8..15 to 0 seeks.
        let (seek, off_to_on, bank, window) = _apply_write(&mut v, 0x84, 0x00, 0x70);
        assert!(seek);
        assert!(!off_to_on && !bank);
        assert_eq!(window, 0xFF00);
        assert_eq!(v.addr, 0xFF0000);
        // Control 0xFF -> 0x00 is key-on and bank change.
        let (seek, off_to_on, bank, _window) = _apply_write(&mut v, 0x86, 0x00, 0x70);
        assert!(!seek && off_to_on && bank);
        assert_eq!(v.ctrl, 0x00);
        // Register writes mask the volume to 7 bits.
        _apply_write(&mut v, 0x02, 0xFF, 0x70);
        assert_eq!(v.lvol, 0x7F);
    }

    #[test]
    fn advance_reloads_loop_after_stop_page() {
        let mut v = _Voice {
            addr: 0x040000,
            r#loop: 0x0400,
            end: 0x04,
            freq: 0x10,
            ..Default::default()
        };
        v.ctrl = 0x00;
        _advance_voice(&mut v, 4096);
        assert_eq!(v.addr, 0x050000);
        _advance_voice(&mut v, 1);
        assert_eq!(v.addr, 0x040010);
        // One-shot voice sets the stop bit and holds at the page.
        let mut one = _Voice {
            addr: 0x040000,
            r#loop: 0x0400,
            end: 0x04,
            freq: 0xFF,
            ..Default::default()
        };
        one.ctrl = 0x02;
        _advance_voice(&mut one, 1000);
        assert_eq!(one.ctrl, 0x03);
        assert_eq!(one.addr, 0x050000);
    }

    fn hit(freq: i32) -> SegapcmHit {
        SegapcmHit {
            sample_time: 0,
            voice: 0,
            start: 0,
            end: 0x04,
            r#loop: 0,
            loop_on: true,
            freq,
            vol_l: 0x40,
            vol_r: 0x40,
            ctrl: 0,
            chip_id: 0,
            vol_pts: Vec::new(),
            freq_pts: Vec::new(),
        }
    }

    #[test]
    fn mode_freq_ignores_zero_and_rates_at_mode() {
        let smp = SegapcmSample {
            start: 0x400,
            data: vec![0; 256],
            loop_start: Some(0),
            loop_end: Some(256),
            hits: vec![hit(0), hit(0x84), hit(0x40), hit(0x84)],
            chip_id: 0,
        };
        assert_eq!(smp.length(), 256);
        assert_eq!(smp.mode_freq(), 0x84);
        assert_eq!(smp.c4_rate(4_000_000), 16113);
        assert_eq!(
            SegapcmSample {
                hits: Vec::new(),
                ..smp.clone()
            }
            .mode_freq(),
            0
        );
        assert_eq!(hit_played_freq(&hit(0)), 0);
        let mut late = hit(0);
        late.freq_pts.push((10, 0));
        late.freq_pts.push((20, 0xB4));
        assert_eq!(hit_played_freq(&late), 0xB4);
    }
}
