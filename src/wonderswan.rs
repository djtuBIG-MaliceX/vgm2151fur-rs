//! Bandai WonderSwan sound: four wavetable voices plus one PCM voice.
//!
//! VGM command `0xBC` writes the sound ports `0x80-0x94` and `0xC6` writes
//! the 16 KB internal RAM (the wave tables and, for DMA drivers, PCM data).
//! The chip's wave RAM is only 16 KB; addresses wrap at `0x3FFF` and many
//! rips use a mirrored `0x8000+` address.
//!
//! Literal port of `vgm2151fur/wonderswan.py` from vgm2151fur 0.11.0.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use crate::parity::{py_int, py_round};
use crate::vgm::{VgmFile, VGM_RATE};

pub use crate::vgm::WSWAN_DEFAULT_CLOCK;

pub const WS_PITCH_DIVIDER: i32 = 32;
pub const WS_MAX_PERIOD: i32 = 0x800;
pub const WS_WAVE_LEN: usize = 32;
pub const WS_VOICES: usize = 4;
// A musical pitch jump this large is a new note, not a bend. Comparing
// period registers directly is wrong: one unit near period 32 is a semitone,
// while near period 2000 it is a few cents.
pub const WS_NOTE_JUMP_ST: f64 = 0.9;
pub const WS_MIN_NOTE: i64 = 48;
// A level jump this large while the gate stays open is a retriggered drum or
// a new note. Opposite-side volume flips are separate hits even when the peak
// level does not rise.
pub const WS_RETRIGGER_RISE: i32 = 4;
// Memory writes closer than this are one wave upload burst.
pub const WS_WAVE_BURST: i64 = 256;
// A volume write this close to a note's period-driven start is that note's
// initial level, not a mid-note step.
pub const WS_SETTLE: i64 = 8;
// PCM write gaps larger than this split one stream into separate sample runs.
pub const WS_PCM_GAP: i64 = 2_000;
pub const WS_PCM_MIN_RATE: i32 = 2_000;
pub const WS_PCM_MAX_RATE: i32 = 48_000;

#[derive(Clone, Debug, Default, PartialEq)]
pub struct WsHit {
    pub sample_time: i64,
    pub chip_id: i32,
    pub voice: i32,
    pub period: i32,
    pub wave: Vec<u8>,
    pub vol_l: i32,
    pub vol_r: i32,
    pub noise: i32,
    pub sweep_time: i32,
    pub sweep_amount: i32,
    pub vol_pts: Vec<(i64, i32, i32)>,
    pub period_pts: Vec<(i64, i32)>,
    pub wave_pts: Vec<(i64, Vec<u8>)>,
    pub noise_pts: Vec<(i64, i32)>,
    pub sweep_pts: Vec<(i64, i32, i32)>,
    pub off_sample: i64,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct WsPcmHit {
    pub sample_time: i64,
    pub chip_id: i32,
    pub volume: i32,
    pub pan_l: i32,
    pub pan_r: i32,
    pub vol_pts: Vec<(i64, i32, i32)>,
    pub off_sample: i64,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct WsPcmSample {
    pub chip_id: i32,
    pub data: Vec<u8>,
    pub rate: i32,
    pub hits: Vec<WsPcmHit>,
}

fn _wave_from_ram(ram: &[u8], base: i32, voice: usize) -> Vec<u8> {
    let start = (base + voice as i32 * 16) & 0x3FFF;
    let mut out = Vec::with_capacity(WS_WAVE_LEN);
    for i in 0..16 {
        let b = ram[((start + i) & 0x3FFF) as usize];
        // The core reads the low nibble first, then the high nibble.
        out.push(b & 0x0F);
        out.push((b >> 4) & 0x0F);
    }
    out
}

fn _signed(value: i32) -> i32 {
    (value & 0xFF) - 0x80
}

fn _pcm_from_points(points: &[(i64, i32)]) -> Option<(Vec<u8>, i32, i64)> {
    // Zero-order-hold reconstruction at the stream's nominal write rate.
    if points.len() < 2 {
        return None;
    }
    // Counter(gaps).most_common(1): first-inserted gap wins a tie.
    let mut gaps: Vec<(i64, i64)> = Vec::new();
    for pair in points.windows(2) {
        let d = pair[1].0 - pair[0].0;
        if 0 < d && d <= WS_PCM_GAP {
            match gaps.iter_mut().find(|e| e.0 == d) {
                Some(e) => e.1 += 1,
                None => gaps.push((d, 1)),
            }
        }
    }
    if gaps.is_empty() {
        return None;
    }
    let mut best = gaps[0];
    for &g in &gaps[1..] {
        if g.1 > best.1 {
            best = g;
        }
    }
    let gap = best.0;
    let rate = WS_PCM_MIN_RATE.max(
        WS_PCM_MAX_RATE.min(py_int(py_round(VGM_RATE as f64 / gap as f64)) as i32),
    );
    let start = points[0].0;
    let end = points[points.len() - 1].0;
    let frames =
        1.max(py_int(py_round((end - start) as f64 / VGM_RATE as f64 * rate as f64)) + 1);
    let frames = frames as usize;
    let mut out = vec![0u8; frames];
    for (i, &(when, value)) in points.iter().enumerate() {
        let first = 0.max(py_int(py_round(
            (when - start) as f64 / VGM_RATE as f64 * rate as f64,
        )));
        let last = if i + 1 < points.len() {
            py_int(py_round(
                (points[i + 1].0 - start) as f64 / VGM_RATE as f64 * rate as f64,
            ))
        } else {
            frames as i64
        };
        let last = (first + 1).max((frames as i64).min(last));
        let v = _signed(value);
        for j in first..last {
            out[j as usize] = (v & 0xFF) as u8;
        }
    }
    Some((out, rate, start))
}

fn _pcm_volume(points: &[(i64, i32)], when: i64, default_raw: i32) -> (i32, i32, i32) {
    let mut raw = default_raw;
    for &(t, v) in points {
        if t <= when {
            raw = v;
        }
    }
    // The WS PCM volume register packs left in bits 2-3 and right in
    // bits 0-1 (core: PCMVolumeRight = ((value<<2)&0xc)*2).
    let left = (raw >> 2) & 3;
    let right = raw & 3;
    let level = left.max(right);
    let vol = if level >= 3 {
        15
    } else if level == 2 {
        10
    } else {
        0
    };
    (vol, left * 5, right * 5)
}

fn period_of(raw: i32) -> i32 {
    2048 - (raw & 0x7FF)
}

#[derive(Clone, Debug, Default)]
struct WsChannel {
    period_raw: i32,
    vol_l: i32,
    vol_r: i32,
    active: Option<usize>,
    last_hit: Option<usize>,
    #[allow(dead_code)]
    last_note_t: i64,
    #[allow(dead_code)]
    last_period_t: i64,
    #[allow(dead_code)]
    last_vol_t: i64,
}

impl WsChannel {
    fn new() -> Self {
        Self {
            period_raw: 0xFFFF,
            vol_l: 0,
            vol_r: 0,
            active: None,
            last_hit: None,
            last_note_t: -1,
            last_period_t: -100000,
            last_vol_t: -100000,
        }
    }
}

#[derive(Clone, Debug)]
struct WsState {
    ram: Vec<u8>,
    base: i32,
    snd: i32,
    sweep_time: i32,
    sweep_amount: i32,
    noise: i32,
    pcm_vol: i32,
    pcm_on: bool,
    pcm_points: Vec<(i64, i32)>,
    #[allow(dead_code)]
    pcm_start: i64,
    #[allow(dead_code)]
    pcm_end: i64,
    pcm_vol_pts: Vec<(i64, i32)>,
    #[allow(dead_code)]
    wave_t: i64,
    dirty: [bool; WS_VOICES],
    ch: [WsChannel; WS_VOICES],
}

impl WsState {
    fn new() -> Self {
        Self {
            ram: vec![0u8; 0x4000],
            base: 0,
            snd: 0,
            sweep_time: 0,
            sweep_amount: 0,
            noise: 0,
            pcm_vol: 0,
            pcm_on: false,
            pcm_points: Vec::new(),
            pcm_start: 0,
            pcm_end: 0,
            pcm_vol_pts: Vec::new(),
            wave_t: -1,
            dirty: [false; WS_VOICES],
            ch: std::array::from_fn(|_| WsChannel::new()),
        }
    }
}

/// The nested closures of `collect_wonderswan` as methods over the mutable
/// collector state: Rust needs the context struct where Python closes over
/// locals.
struct WsCollector {
    state: BTreeMap<i32, WsState>,
    hits: Vec<WsHit>,
    samples: Vec<WsPcmSample>,
    seen: HashMap<(i32, Vec<u8>), usize>,
    warnings: Vec<String>,
}

impl WsCollector {
    fn new(chips: &[i32]) -> Self {
        let mut state = BTreeMap::new();
        for &cid in chips {
            state.insert(cid, WsState::new());
        }
        Self {
            state,
            hits: Vec::new(),
            samples: Vec::new(),
            seen: HashMap::new(),
            warnings: Vec::new(),
        }
    }

    fn body_for(&self, cid: i32, voice: usize) -> Vec<u8> {
        let st = &self.state[&cid];
        _wave_from_ram(&st.ram, st.base, voice)
    }

    fn close(&mut self, cid: i32, voice: usize, when: i64, off: bool) {
        let Some(hi) = self.state[&cid].ch[voice].active else {
            return;
        };
        if off && when > self.hits[hi].sample_time {
            self.hits[hi].off_sample = when;
        }
        self.state.get_mut(&cid).unwrap().ch[voice].active = None;
    }

    fn open_hit(&mut self, cid: i32, voice: usize, when: i64) -> Option<usize> {
        let (snd, vol_l, vol_r, raw, noise, sweep_time, sweep_amount) = {
            let st = &self.state[&cid];
            let ch = &st.ch[voice];
            (
                st.snd,
                ch.vol_l,
                ch.vol_r,
                ch.period_raw,
                st.noise,
                st.sweep_time,
                st.sweep_amount,
            )
        };
        if (snd & (1 << voice)) == 0 {
            return None;
        }
        if vol_l == 0 && vol_r == 0 {
            return None;
        }
        if raw == 0xFFFF {
            return None;
        }
        let noise_val = if voice == 3 && (snd & 0x80) != 0 {
            (noise & 7) + 1
        } else {
            0
        };
        let prev = self.state[&cid].ch[voice].last_hit;
        if let Some(pi) = prev {
            if self.hits[pi].sample_time == when {
                // Two gate/register paths can identify the same attack (for
                // example the noise-enable bit and the noise-reset write).
                // Keep one hit with the final state instead of two notes on
                // one row.
                let wave = self.body_for(cid, voice);
                let hit = &mut self.hits[pi];
                hit.period = period_of(raw);
                hit.wave = wave;
                hit.vol_l = vol_l;
                hit.vol_r = vol_r;
                hit.noise = noise_val;
                hit.sweep_time = sweep_time;
                hit.sweep_amount = sweep_amount;
                hit.off_sample = 0;
                self.state.get_mut(&cid).unwrap().ch[voice].active = Some(pi);
                return Some(pi);
            }
        }
        let wave = self.body_for(cid, voice);
        self.hits.push(WsHit {
            sample_time: when,
            chip_id: cid,
            voice: voice as i32,
            period: period_of(raw),
            wave,
            vol_l,
            vol_r,
            noise: noise_val,
            sweep_time,
            sweep_amount,
            ..Default::default()
        });
        let idx = self.hits.len() - 1;
        let ch = &mut self.state.get_mut(&cid).unwrap().ch[voice];
        ch.active = Some(idx);
        ch.last_hit = Some(idx);
        ch.last_note_t = when;
        Some(idx)
    }

    fn refresh(&mut self, cid: i32, voice: usize, when: i64) {
        let Some(hi) = self.state[&cid].ch[voice].active else {
            self.open_hit(cid, voice, when);
            return;
        };
        let (vol_l, vol_r, raw) = {
            let ch = &self.state[&cid].ch[voice];
            (ch.vol_l, ch.vol_r, ch.period_raw)
        };
        if vol_l == 0 && vol_r == 0 {
            self.close(cid, voice, when, true);
            return;
        }
        let last_l = self.hits[hi]
            .vol_pts
            .last()
            .map(|p| p.1)
            .unwrap_or(self.hits[hi].vol_l);
        let last_r = self.hits[hi]
            .vol_pts
            .last()
            .map(|p| p.2)
            .unwrap_or(self.hits[hi].vol_r);
        if (vol_l, vol_r) != (last_l, last_r) {
            let newmax = vol_l.max(vol_r);
            let oldmax = last_l.max(last_r);
            let hard_flip = (vol_l > 0 && vol_r == 0 && last_l == 0 && last_r > 0)
                || (vol_r > 0 && vol_l == 0 && last_r == 0 && last_l > 0);
            if when - self.hits[hi].sample_time <= WS_SETTLE && self.hits[hi].vol_pts.is_empty() {
                // The period write opened the note a sample or two before the
                // driver wrote its companion volume byte.
                self.hits[hi].vol_l = vol_l;
                self.hits[hi].vol_r = vol_r;
            } else if ((newmax - oldmax >= WS_RETRIGGER_RISE && !hard_flip) || hard_flip)
                && when - self.hits[hi].sample_time >= WS_MIN_NOTE
            {
                // A gate jump or an opposite-side volume flip is a new hit,
                // not a volume step in a long instrument macro.
                self.close(cid, voice, when, false);
                self.open_hit(cid, voice, when);
                return;
            } else {
                self.hits[hi].vol_pts.push((when, vol_l, vol_r));
            }
        }
        if raw != 0xFFFF {
            let per = period_of(raw);
            let last_per = self.hits[hi]
                .period_pts
                .last()
                .map(|p| p.1)
                .unwrap_or(self.hits[hi].period);
            if when - self.hits[hi].sample_time <= WS_SETTLE && self.hits[hi].period_pts.is_empty() {
                self.hits[hi].period = per;
            } else if (12.0 * ((per.max(1) as f64) / (last_per.max(1) as f64)).log2()).abs()
                >= WS_NOTE_JUMP_ST
            {
                self.close(cid, voice, when, true);
                self.open_hit(cid, voice, when);
                return;
            } else if per != last_per {
                self.hits[hi].period_pts.push((when, per));
            }
        }
        let (noise, snd, sweep_time, sweep_amount) = {
            let st = &self.state[&cid];
            (st.noise, st.snd, st.sweep_time, st.sweep_amount)
        };
        if voice == 3 {
            let noise_val = if (snd & 0x80) != 0 {
                (noise & 7) + 1
            } else {
                0
            };
            let last_noise = self.hits[hi]
                .noise_pts
                .last()
                .map(|p| p.1)
                .unwrap_or(self.hits[hi].noise);
            if noise_val != last_noise {
                self.hits[hi].noise_pts.push((when, noise_val));
            }
        }
        if voice == 2 {
            let last_sweep = match self.hits[hi].sweep_pts.last() {
                Some(p) => (p.1, p.2),
                None => (self.hits[hi].sweep_time, self.hits[hi].sweep_amount),
            };
            if (sweep_time, sweep_amount) != last_sweep {
                self.hits[hi]
                    .sweep_pts
                    .push((when, sweep_time, sweep_amount));
            }
        }
    }

    fn flush_waves(&mut self, cid: i32, when: i64) {
        for voice in 0..WS_VOICES {
            if !self.state[&cid].dirty[voice] {
                continue;
            }
            self.state.get_mut(&cid).unwrap().dirty[voice] = false;
            let Some(hi) = self.state[&cid].ch[voice].active else {
                continue;
            };
            let body = self.body_for(cid, voice);
            let last_wave = match self.hits[hi].wave_pts.last() {
                Some(p) => p.1.clone(),
                None => self.hits[hi].wave.clone(),
            };
            if body != last_wave {
                self.hits[hi].wave_pts.push((when, body));
            }
        }
    }

    fn finish_pcm(&mut self, cid: i32, when: i64) {
        let (points, pcm_vol_pts, pcm_vol) = {
            let st = self.state.get_mut(&cid).unwrap();
            let points = std::mem::take(&mut st.pcm_points);
            (points, st.pcm_vol_pts.clone(), st.pcm_vol)
        };
        if points.is_empty() {
            return;
        }
        let Some((data, rate, start)) = _pcm_from_points(&points) else {
            self.warnings.push(
                "WonderSwan PCM stream has no stable write rate; it was dropped".to_string(),
            );
            return;
        };
        if !data.iter().any(|&v| v != 0x80) {
            return;
        }
        let (vol, pl, pr) = _pcm_volume(&pcm_vol_pts, start, pcm_vol);
        let key = (rate, data.clone());
        let smp_idx = match self.seen.get(&key) {
            Some(&i) => i,
            None => {
                self.samples.push(WsPcmSample {
                    chip_id: cid,
                    data: data.clone(),
                    rate,
                    hits: Vec::new(),
                });
                let i = self.samples.len() - 1;
                self.seen.insert(key, i);
                i
            }
        };
        let tail = start + py_int(py_round(data.len() as f64 / rate as f64 * VGM_RATE as f64));
        let off_sample = if when != 0 {
            when.min(tail)
        } else {
            tail
        };
        let mut hit = WsPcmHit {
            sample_time: start,
            chip_id: cid,
            volume: vol,
            pan_l: pl,
            pan_r: pr,
            vol_pts: Vec::new(),
            off_sample,
        };
        for &(vt, raw) in &pcm_vol_pts {
            if start < vt && vt < hit.off_sample {
                let (v, l, r) = _pcm_volume(&[(vt, raw)], vt, pcm_vol);
                hit.vol_pts.push((vt, v, (l << 4) | r));
            }
        }
        self.samples[smp_idx].hits.push(hit);
    }
}

pub fn collect_wonderswan(vgm: &VgmFile) -> (Vec<WsHit>, Vec<WsPcmSample>, Vec<String>) {
    let mut warnings: Vec<String> = Vec::new();
    let chips: Vec<i32> = vgm
        .writes
        .iter()
        .filter(|w| w.chip == "wonderswan")
        .map(|w| w.chip_id as i32)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    if chips.is_empty() {
        return (Vec::new(), Vec::new(), warnings);
    }
    let total = vgm.total_samples;
    let mut c = WsCollector::new(&chips);

    for w in &vgm.writes {
        if w.chip != "wonderswan" {
            continue;
        }
        let cid = w.chip_id as i32;
        let t = w.sample;
        let reg = w.reg;
        let val = (w.val & 0xFF) as i32;

        if reg >= 0x10000 {
            let addr = ((reg - 0x10000) & 0x3FFF) as usize;
            let st = c.state.get_mut(&cid).unwrap();
            st.ram[addr] = val as u8;
            st.wave_t = t;
            let base = (st.base & 0x3FFF) as i64;
            for voice in 0..WS_VOICES {
                let start = (base + voice as i64 * 16) & 0x3FFF;
                if ((addr as i64 - start) & 0x3FFF) < 16 {
                    st.dirty[voice] = true;
                }
            }
            continue;
        }

        c.flush_waves(cid, t);
        if reg == 0x8F {
            c.state.get_mut(&cid).unwrap().base = ((val << 6) & 0xFFFF) as i32;
            for voice in 0..WS_VOICES {
                let Some(hi) = c.state[&cid].ch[voice].active else {
                    continue;
                };
                let body = c.body_for(cid, voice);
                let last_wave = match c.hits[hi].wave_pts.last() {
                    Some(p) => p.1.clone(),
                    None => c.hits[hi].wave.clone(),
                };
                if body != last_wave {
                    c.hits[hi].wave_pts.push((t, body));
                }
            }
            continue;
        }
        if reg == 0x90 {
            let old = c.state[&cid].snd;
            c.state.get_mut(&cid).unwrap().snd = val;
            for voice in 0..WS_VOICES {
                let was = (old & (1 << voice)) != 0;
                let now = (val & (1 << voice)) != 0;
                if now && !was {
                    c.open_hit(cid, voice, t);
                } else if was && !now {
                    c.close(cid, voice, t, true);
                }
            }
            let pcm_was = (old & 0x20) != 0;
            let pcm_now = (val & 0x20) != 0;
            if pcm_now && !pcm_was {
                let st = c.state.get_mut(&cid).unwrap();
                st.pcm_on = true;
                st.pcm_points = Vec::new();
                st.pcm_start = t;
                st.pcm_vol_pts = Vec::new();
            } else if pcm_was && !pcm_now {
                {
                    let st = c.state.get_mut(&cid).unwrap();
                    st.pcm_on = false;
                    st.pcm_end = t;
                }
                c.finish_pcm(cid, t);
            }
            if (val & 0x80) != 0 && (old & 0x80) == 0 {
                if let Some(hi) = c.state[&cid].ch[3].active {
                    if t - c.hits[hi].sample_time >= WS_MIN_NOTE {
                        // Turning the noise generator on is a new percussion hit.
                        c.close(cid, 3, t, false);
                        c.open_hit(cid, 3, t);
                    }
                }
            }
            c.refresh(cid, 3, t);
            c.refresh(cid, 2, t);
            continue;
        }
        if reg == 0x94 {
            let st = c.state.get_mut(&cid).unwrap();
            st.pcm_vol = val;
            st.pcm_vol_pts.push((t, val));
            continue;
        }
        if reg == 0x92 || reg == 0x93 {
            continue;
        }
        if reg == 0x89 && c.state[&cid].pcm_on {
            c.state.get_mut(&cid).unwrap().pcm_points.push((t, val));
            continue;
        }
        if (0x80..=0x87).contains(&reg) {
            // 0x80/0x81 voice 0, 0x82/0x83 voice 1, 0x84/0x85 voice 2,
            // 0x86/0x87 voice 3. Even is the low byte, odd the high byte.
            let voice = ((reg - 0x80) / 2) as usize;
            {
                let ch = &mut c.state.get_mut(&cid).unwrap().ch[voice];
                if (reg - 0x80) % 2 == 0 {
                    ch.period_raw = (ch.period_raw & 0xFF00) | val;
                } else {
                    ch.period_raw = (ch.period_raw & 0x00FF) | (val << 8);
                }
            }
            c.refresh(cid, voice, t);
            continue;
        }
        if (0x88..=0x8B).contains(&reg) {
            let voice = (reg - 0x88) as usize;
            {
                let ch = &mut c.state.get_mut(&cid).unwrap().ch[voice];
                ch.vol_l = (val >> 4) & 0x0F;
                ch.vol_r = val & 0x0F;
            }
            c.refresh(cid, voice, t);
            continue;
        }
        if reg == 0x8C {
            c.state.get_mut(&cid).unwrap().sweep_amount = val;
            c.refresh(cid, 2, t);
            continue;
        }
        if reg == 0x8D {
            c.state.get_mut(&cid).unwrap().sweep_time = val;
            c.refresh(cid, 2, t);
            continue;
        }
        if reg == 0x8E {
            c.state.get_mut(&cid).unwrap().noise = val;
            let hit = c.state[&cid].ch[3].active;
            if (val & 0x08) != 0 {
                if let Some(hi) = hit {
                    if t - c.hits[hi].sample_time >= WS_MIN_NOTE {
                        // The noise reset bit is written once per percussion hit.
                        c.close(cid, 3, t, false);
                        c.open_hit(cid, 3, t);
                        continue;
                    }
                }
            }
            c.refresh(cid, 3, t);
            continue;
        }
        if reg == 0x89 && c.state[&cid].pcm_on {
            c.state.get_mut(&cid).unwrap().pcm_points.push((t, val));
            continue;
        }
    }

    for &cid in &chips {
        for voice in 0..WS_VOICES {
            c.close(cid, voice, total, false);
        }
        if c.state[&cid].pcm_on {
            c.finish_pcm(cid, total);
        }
    }

    let mut hits = c.hits;
    hits.retain(|h| h.period > 0);
    hits.sort_by(|a, b| {
        (a.sample_time, a.chip_id, a.voice).cmp(&(b.sample_time, b.chip_id, b.voice))
    });
    let mut samples = c.samples;
    samples.sort_by(|a, b| {
        (a.chip_id, a.rate, &a.data).cmp(&(b.chip_id, b.rate, &b.data))
    });
    warnings = c.warnings;
    (hits, samples, warnings)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vgm::ChipWrite;

    fn write(sample: i64, reg: u32, val: u32) -> ChipWrite {
        ChipWrite::new(sample, "wonderswan", 0, reg, val)
    }

    fn vgm(writes: Vec<ChipWrite>, total: i64) -> VgmFile {
        let mut v = VgmFile::default();
        v.total_samples = total;
        v.writes = writes;
        v
    }

    fn wave_song() -> VgmFile {
        let mut writes = Vec::new();
        for i in 0..16u32 {
            writes.push(write(0, 0x10000 + i, (i << 4) | i));
        }
        writes.extend([
            write(10, 0x8F, 0),
            write(20, 0x80, 0x00),
            write(21, 0x81, 0x04),
            write(30, 0x88, 0xC0),
            write(40, 0x90, 0x01),
            write(200, 0x88, 0x00),
        ]);
        vgm(writes, 300)
    }

    #[test]
    fn semitone_pitch_jump_splits_note() {
        let writes = vec![
            write(0, 0x90, 0x01),
            write(0, 0x88, 0x88),
            write(1, 0x80, 0xE0), // period 32
            write(2, 0x81, 0x07),
            write(500, 0x80, 0xDE), // period 34 (~1 semitone up)
            write(501, 0x81, 0x07),
        ];
        let (hits, _pcm, _) = collect_wonderswan(&vgm(writes, 900));
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].period, 32);
        assert_eq!(hits[1].period, 34);
    }

    #[test]
    fn wave_note_and_cut() {
        let (hits, pcm, _) = collect_wonderswan(&wave_song());
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].period, 2048 - 0x400);
        assert_eq!((hits[0].vol_l, hits[0].vol_r), (12, 0));
        assert_eq!(hits[0].off_sample, 200);
        assert!(pcm.is_empty());
        // Wave image: low nibble then high nibble of each RAM byte.
        assert_eq!(hits[0].wave.len(), WS_WAVE_LEN);
        assert_eq!(&hits[0].wave[..4], &[0, 0, 1, 1]);
        assert_eq!(&hits[0].wave[30..], &[15, 15]);
    }

    #[test]
    fn pcm_pan_decodes_low_right_bits() {
        let mut writes = vec![write(0, 0x90, 0x22)];
        writes.push(write(1, 0x94, 0x0F));
        for i in 0..20 {
            writes.push(write(2 + i * 4, 0x89, 128 + i as u32));
        }
        writes.push(write(200, 0x90, 0x00));
        let (_hits, pcm, _) = collect_wonderswan(&vgm(writes, 300));
        assert_eq!(pcm.len(), 1);
        assert_eq!(pcm[0].hits[0].pan_l, 15);
        assert_eq!(pcm[0].hits[0].pan_r, 15);
        assert_eq!(pcm[0].hits[0].volume, 15);
    }

    #[test]
    fn pcm_stream() {
        let mut writes = vec![write(0, 0x90, 0x22)];
        for i in 0..40 {
            writes.push(write(1 + i * 4, 0x89, 128 + (i % 5) as u32));
        }
        writes.push(write(200, 0x90, 0x00));
        let (hits, pcm, _) = collect_wonderswan(&vgm(writes, 300));
        assert!(hits.is_empty());
        assert_eq!(pcm.len(), 1);
        assert!(pcm[0].data.len() > 10);
        assert!((pcm[0].rate as f64 - 44100.0 / 4.0).abs() < 2.0);
        assert_eq!(pcm[0].hits[0].off_sample, 161);
    }

    #[test]
    fn ram_upload_mid_note_emits_wave_point() {
        let mut writes = Vec::new();
        for i in 0..16u32 {
            writes.push(write(0, 0x10000 + i, (i << 4) | i));
        }
        writes.extend([
            write(10, 0x8F, 0),
            write(20, 0x80, 0x00),
            write(21, 0x81, 0x04),
            write(30, 0x88, 0xC0),
            write(40, 0x90, 0x01),
            write(100, 0x10000, 0xAB),
            write(110, 0x88, 0xC0),
            write(200, 0x88, 0x00),
        ]);
        let (hits, _, _) = collect_wonderswan(&vgm(writes, 300));
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].wave_pts.len(), 1);
        assert_eq!(hits[0].wave_pts[0].0, 110);
        assert_eq!(&hits[0].wave_pts[0].1[..2], &[0x0B, 0x0A]);
    }
}
