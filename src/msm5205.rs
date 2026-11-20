//! OKI MSM5205 streamed ADPCM -> Generic PCM DAC bodies.
//!
//! Port of `vgm2151fur/msm5205.py` (v0.11.0).
//!
//! The MSM5205 has no sample ROM.  A CPU writes one ADPCM nibble at a time
//! through VGM command 0x32, and the chip clocks the nibbles out at
//! `clock / prescaler`.  This collector mirrors libvgm's reset, FIFO, prescaler
//! and ADPCM state machine, renders one PCM track per chip at the 44.1 kHz VGM
//! timeline, then splits it at attacks and silences exactly like the YM2612 DAC
//! and 32X PWM paths.  Each piece becomes a 16-bit Generic PCM DAC body.

use std::collections::{HashMap, HashSet};
use std::sync::LazyLock;

use indexmap::IndexMap;

use crate::parity::py_int;
use crate::samplecluster::cluster_fragments16;
use crate::vgm::{VgmFile, VGM_RATE};

// The audio track is rendered at the VGM rate, so these segmentation windows
// scale directly from the PWM/YM2612-DAC rules.
pub const MSM5205_CHUNK_FRAMES: i64 = 11025; // 0.25 s
pub const MSM5205_ATTACK_SILENCE: i64 = 500; // ~11 ms of near-silence splits a hit
pub const MSM5205_ATTACK_MIN_FRAMES: i64 = 2756; // ~62 ms
pub const MSM5205_ATTACK_MAX_FRAMES: i64 = 44100; // ~1 s
pub const MSM5205_ATTACK_QUIET_DIV: i32 = 32;
pub const MSM5205_ATTACK_MAX_SAMPLES: usize = 480;
pub const MSM5205_ONSET_WINDOW: usize = 16;
pub const MSM5205_ONSET_QUIET: i64 = 64;
pub const MSM5205_ONSET_MIN_GAP: i64 = 128;
pub const MSM5205_ONSET_LOOKAHEAD: usize = 128;
pub const MSM5205_DEFAULT_PRESCALER: i32 = 0;
pub const MSM5205_DEFAULT_BITS: i32 = 4;

// libvgm/MAME ADPCM tables.
const INDEX_SHIFT: [i32; 8] = [-1, -1, -1, -1, 2, 4, 6, 8];
const NBL2BIT: [[i32; 4]; 16] = [
    [1, 0, 0, 0],
    [1, 0, 0, 1],
    [1, 0, 1, 0],
    [1, 0, 1, 1],
    [1, 1, 0, 0],
    [1, 1, 0, 1],
    [1, 1, 1, 0],
    [1, 1, 1, 1],
    [-1, 0, 0, 0],
    [-1, 0, 0, 1],
    [-1, 0, 1, 0],
    [-1, 0, 1, 1],
    [-1, 1, 0, 0],
    [-1, 1, 0, 1],
    [-1, 1, 1, 0],
    [-1, 1, 1, 1],
];

fn diff_table() -> Vec<i32> {
    let mut table = vec![0i32; 49 * 16];
    for step in 0..49 {
        let stepval = py_int(16.0 * (11.0f64 / 10.0).powf(step as f64)) as i32;
        for (nib, bits) in NBL2BIT.iter().enumerate() {
            table[step * 16 + nib] = bits[0]
                * (stepval * bits[1]
                    + stepval / 2 * bits[2]
                    + stepval / 4 * bits[3]
                    + stepval / 8);
        }
    }
    table
}

static DIFF: LazyLock<Vec<i32>> = LazyLock::new(diff_table);

/// Master-clock divider for the S1/S2 pair.
pub fn msm5205_prescaler(value: i32) -> i32 {
    let value = value & 3;
    if value & 1 != 0 {
        return if value & 2 != 0 { 1 } else { 64 };
    }
    if value & 2 != 0 { 48 } else { 96 }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Msm5205Hit {
    pub time: i64,
}

impl Default for Msm5205Hit {
    fn default() -> Self {
        Msm5205Hit { time: 0 }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Msm5205Sample {
    pub chip_id: i32,
    /// Signed 16-bit little-endian at 44100 Hz.
    pub data: Vec<u8>,
    pub rate: i32,
    pub hits: Vec<Msm5205Hit>,
    pub chunk: i32,
    pub first_chunk: bool,
}

impl Default for Msm5205Sample {
    fn default() -> Self {
        Msm5205Sample {
            chip_id: 0,
            data: Vec::new(),
            rate: VGM_RATE as i32,
            hits: Vec::new(),
            chunk: 0,
            first_chunk: true,
        }
    }
}

impl Msm5205Sample {
    pub fn frames(&self) -> i64 {
        self.data.len() as i64 / 2
    }

    pub fn name(&self) -> String {
        format!("MSM5205 {} {:03}", self.chip_id + 1, self.chunk)
    }
}

struct ChipState {
    clock: u32,
    prescaler: i32,
    bitwidth: i32,
    reset: i32,
    signal: i32,
    step: i32,
    vclk: i32,
    data: [u8; 8],
    pos: i32,
    accum: f64,
}

impl ChipState {
    fn new(clock: u32, flags: u32) -> Self {
        ChipState {
            clock,
            prescaler: (flags & 0x03) as i32,
            bitwidth: if flags & 0x04 != 0 { 4 } else { 3 },
            reset: 0,
            signal: -2,
            step: 0,
            vclk: 0,
            data: [0; 8],
            pos: 0,
            accum: 0.0,
        }
    }

    fn rate(&self) -> f64 {
        (self.clock as f64 / msm5205_prescaler(self.prescaler) as f64).max(1.0)
    }

    fn write(&mut self, reg: u32, value: u32) {
        let value = (value & 0xFF) as i32;
        match reg {
            0 => {
                let old = self.reset;
                self.reset = value;
                if old ^ value != 0 {
                    self.signal = 0;
                    self.step = 0;
                }
                if self.reset != 0 {
                    self.pos = 0;
                }
            }
            1 => {
                let write_pos = (self.pos >> 4) & 0x07;
                let read_pos = self.pos & 0x07;
                if ((write_pos + 1) & 0x07) != read_pos {
                    self.data[write_pos as usize] = value as u8;
                    self.pos = ((write_pos + 1) << 4) | read_pos;
                }
            }
            2 => {
                let old = self.vclk;
                self.vclk = value;
                if msm5205_prescaler(self.prescaler) == 1
                    && ((old ^ value) & 1) != 0
                    && self.vclk != 0
                {
                    self.clock_one();
                }
            }
            4 => {
                self.prescaler = value & 0x03;
            }
            5 => {
                self.bitwidth = if value != 0 { 4 } else { 3 };
            }
            _ => {}
        }
    }

    fn clock_one(&mut self) {
        if self.reset != 0 {
            return;
        }
        let read_pos = self.pos & 0x0F;
        let write_pos = (self.pos >> 4) & 0x07;
        if read_pos == write_pos || msm5205_prescaler(self.prescaler) == 1 {
            return;
        }
        let mut data = self.data[read_pos as usize] as i32;
        if self.bitwidth == 3 {
            data <<= 1;
        }
        let nib = data & 0x0F;
        let sample = DIFF[self.step as usize * 16 + nib as usize];
        self.signal = ((sample << 8) + (self.signal * 245)) >> 8;
        self.signal = self.signal.clamp(-2048, 2047);
        self.step = (self.step + INDEX_SHIFT[(nib & 7) as usize]).clamp(0, 48);
        self.pos = (write_pos << 4) | ((read_pos + 1) & 0x07);
    }

    /// Advance the chip clock and return the new output level.
    fn tick(&mut self, samples: i32) -> i32 {
        if self.reset != 0 {
            return 0;
        }
        let step = self.rate() / VGM_RATE as f64;
        self.accum += step * samples as f64;
        while self.accum >= 1.0 {
            self.clock_one();
            self.accum -= 1.0;
        }
        // Master mode repeats the last converted level between clocks.
        self.signal
    }

    fn level(&self) -> i32 {
        if self.reset != 0 {
            return 0;
        }
        (self.signal << 4).clamp(-32768, 32767)
    }
}

fn write_i16(out: &mut Vec<u8>, value: i32) {
    out.extend_from_slice(&(value as i16).to_le_bytes());
}

fn render_chip(vgm: &VgmFile, cid: i32, clock: u32, flags: u32, total: i64) -> Vec<u8> {
    let mut state = ChipState::new(clock, flags);
    let mut out: Vec<u8> = Vec::new();
    let writes: Vec<(i64, u32, u32)> = vgm
        .writes
        .iter()
        .filter(|w| w.chip == "msm5205" && w.chip_id as i32 == cid)
        .map(|w| (w.sample, w.reg, w.val))
        .collect();
    let mut wi = 0usize;
    for t in 0..total.max(1) {
        while wi < writes.len() && writes[wi].0 <= t {
            let (_, reg, val) = writes[wi];
            state.write(reg, val);
            wi += 1;
        }
        state.tick(1);
        write_i16(&mut out, state.level());
    }
    // Writes after the header's stream end still matter to the first sample.
    while wi < writes.len() {
        let (_, reg, val) = writes[wi];
        state.write(reg, val);
        wi += 1;
        write_i16(&mut out, state.level());
    }
    out
}

fn words(data: &[u8]) -> Vec<i16> {
    data.chunks_exact(2)
        .map(|c| i16::from_le_bytes([c[0], c[1]]))
        .collect()
}

fn pcm_stats(data: &[u8]) -> (Vec<i16>, i32, i32, i32) {
    let vals = words(data);
    if vals.is_empty() {
        return (vals, 0, 64, 0);
    }
    let peak = vals.iter().map(|&v| (v as i32).abs()).max().unwrap();
    // Counter(vals).most_common(1)[0]: highest count, earliest first
    // occurrence on ties.
    let mut order: Vec<i16> = Vec::new();
    let mut counts: HashMap<i16, usize> = HashMap::new();
    for &v in &vals {
        match counts.get_mut(&v) {
            Some(count) => *count += 1,
            None => {
                counts.insert(v, 1);
                order.push(v);
            }
        }
    }
    let mut idle = 0i32;
    let mut idle_count = 0usize;
    for v in order {
        let count = counts[&v];
        if count > idle_count {
            idle = v as i32;
            idle_count = count;
        }
    }
    if idle_count * 100 < vals.len() || idle.abs() > (peak / 4).max(64) {
        idle = 0;
    }
    let width = (peak / MSM5205_ATTACK_QUIET_DIV).max(64);
    (vals, idle, width, peak)
}

fn audible_pcm(data: &[u8], idle: i32, width: i32) -> bool {
    words(data).iter().any(|&v| (v as i32 - idle).abs() > width)
}

fn onset_starts(vals: &[i16], idle: i32, width: i32, peak: i32) -> Vec<i64> {
    if vals.is_empty() {
        return Vec::new();
    }
    let amp: Vec<i32> = vals.iter().map(|&v| (v as i32 - idle).abs()).collect();
    let mut env: Vec<i32> = Vec::with_capacity(amp.len());
    let mut window: Vec<i32> = Vec::new();
    for &value in &amp {
        window.push(value);
        if window.len() > MSM5205_ONSET_WINDOW {
            window.remove(0);
        }
        env.push(*window.iter().max().unwrap());
    }
    let low = width;
    let high = (width * 4).max(peak / 8);
    let mut starts: Vec<i64> = Vec::new();

    let mut add = |pos: i64| {
        if starts.is_empty() || pos - starts[starts.len() - 1] >= MSM5205_ONSET_MIN_GAP {
            starts.push(pos);
        }
    };

    if env[0] >= high {
        add(0);
    }
    let mut low_start: Option<i64> = None;
    for (i, &value) in env.iter().enumerate() {
        let i = i as i64;
        if value <= low {
            if low_start.is_none() {
                low_start = Some(i);
            }
            continue;
        }
        if let Some(start) = low_start {
            if i - start >= MSM5205_ONSET_QUIET {
                let end = (i as usize + MSM5205_ONSET_LOOKAHEAD).min(env.len());
                let look = &env[i as usize..end];
                if !look.is_empty() && *look.iter().max().unwrap() >= high {
                    add(i);
                }
            }
        }
        low_start = None;
    }
    for i in 1..env.len() {
        if env[i] >= high && env[i - 1] <= low {
            add(i as i64);
        }
    }
    starts.sort();
    starts
}

fn attack_bodies(data: &[u8], vals: &[i16], idle: i32, width: i32) -> Vec<(i64, Vec<u8>)> {
    if vals.is_empty() {
        return Vec::new();
    }
    let peak = vals.iter().map(|&v| (v as i32 - idle).abs()).max().unwrap();
    let mut starts = onset_starts(vals, idle, width, peak);
    if starts.len() < 2 {
        starts = Vec::new();
    }

    let quiet = |value: i16| (value as i32 - idle).abs() <= width;

    let mut bounds: Vec<(i64, i64)> = Vec::new();
    if !starts.is_empty() {
        for (i, &a) in starts.iter().enumerate() {
            let mut b = if i + 1 < starts.len() {
                starts[i + 1]
            } else {
                vals.len() as i64
            };
            while b > a && quiet(vals[(b - 1) as usize]) {
                b -= 1;
            }
            if b > a {
                bounds.push((a, b));
            }
        }
    } else {
        let mut start = 0i64;
        let mut quiet_start: Option<i64> = None;
        for (i, &value) in vals.iter().enumerate() {
            let i = i as i64;
            if quiet(value) {
                if quiet_start.is_none() {
                    quiet_start = Some(i);
                }
                continue;
            }
            if let Some(qs) = quiet_start {
                if i - qs >= MSM5205_ATTACK_SILENCE {
                    let mut a = start;
                    let mut b = qs;
                    while a < b && quiet(vals[a as usize]) {
                        a += 1;
                    }
                    while b > a && quiet(vals[(b - 1) as usize]) {
                        b -= 1;
                    }
                    if b - a >= MSM5205_ATTACK_MIN_FRAMES {
                        bounds.push((a, b));
                    }
                    start = i;
                }
            }
            quiet_start = None;
        }
        let mut a = start;
        let mut b = vals.len() as i64;
        while a < b && quiet(vals[a as usize]) {
            a += 1;
        }
        while b > a && quiet(vals[(b - 1) as usize]) {
            b -= 1;
        }
        if b - a >= MSM5205_ATTACK_MIN_FRAMES {
            bounds.push((a, b));
        }
    }
    let mut merged: Vec<(i64, i64)> = Vec::new();
    let mut i = 0usize;
    while i < bounds.len() {
        let (a, b) = bounds[i];
        if b - a < MSM5205_ATTACK_MIN_FRAMES && i + 1 < bounds.len() {
            bounds[i + 1] = (a, bounds[i + 1].1);
            i += 1;
            continue;
        }
        merged.push((a, b));
        i += 1;
    }
    if let Some(&(la, lb)) = merged.last() {
        if lb - la < MSM5205_ATTACK_MIN_FRAMES {
            if merged.len() > 1 {
                let last = merged.pop().unwrap();
                let prev = merged.len() - 1;
                merged[prev] = (merged[prev].0, last.1);
            } else {
                merged.pop();
            }
        }
    }
    let mut out: Vec<(i64, Vec<u8>)> = Vec::new();
    for (mut a, b) in merged {
        let body = data[(a * 2) as usize..(b * 2) as usize].to_vec();
        if !audible_pcm(&body, idle, width) {
            continue;
        }
        while b - a > MSM5205_ATTACK_MAX_FRAMES {
            let mut cut = a + MSM5205_ATTACK_MAX_FRAMES;
            if b - cut < MSM5205_ATTACK_MIN_FRAMES {
                cut = a + (b - a) / 2;
            }
            out.push((a, data[(a * 2) as usize..(cut * 2) as usize].to_vec()));
            a = cut;
        }
        out.push((a, data[(a * 2) as usize..(b * 2) as usize].to_vec()));
    }
    out
}

/// Render and split every MSM5205 instance into Generic PCM DAC bodies.
pub fn collect_msm5205(vgm: &VgmFile, lossy: bool) -> (Vec<Msm5205Sample>, Vec<String>) {
    let mut warnings: Vec<String> = Vec::new();
    let ids: Vec<i32> = {
        let set: std::collections::BTreeSet<i32> = vgm
            .writes
            .iter()
            .filter(|w| w.chip == "msm5205")
            .map(|w| w.chip_id as i32)
            .collect();
        set.into_iter().collect()
    };
    if ids.is_empty() {
        return (Vec::new(), warnings);
    }
    let mut total = vgm.total_samples;
    if total <= 0 {
        total = vgm
            .writes
            .iter()
            .filter(|w| w.chip == "msm5205")
            .map(|w| w.sample)
            .max()
            .unwrap_or(0)
            + 1;
    }
    let clock = if vgm.msm5205_clock != 0 {
        vgm.msm5205_clock
    } else {
        384_000
    };
    let flags = vgm.msm5205_flags as u32;
    let mut tracks: IndexMap<i32, Vec<u8>> = IndexMap::new();
    for &cid in &ids {
        tracks.insert(cid, render_chip(vgm, cid, clock, flags, total));
    }

    let occurrences = |use_attack: bool| -> Vec<(i32, i64, Vec<u8>)> {
        let mut out: Vec<(i32, i64, Vec<u8>)> = Vec::new();
        for (&cid, data) in &tracks {
            let frames = data.len() as i64 / 2;
            if frames <= 0 {
                continue;
            }
            let (vals, idle, width, _peak) = pcm_stats(data);
            if use_attack {
                for (start, body) in attack_bodies(data, &vals, idle, width) {
                    out.push((cid, start, body));
                }
            } else {
                let mut start = 0i64;
                while start < frames {
                    let end = frames.min(start + MSM5205_CHUNK_FRAMES);
                    let body = data[(start * 2) as usize..(end * 2) as usize].to_vec();
                    if audible_pcm(&body, idle, width) {
                        out.push((cid, start, body));
                    }
                    start += MSM5205_CHUNK_FRAMES;
                }
            }
        }
        out
    };

    let attack_occ = occurrences(true);
    let chunk_occ = occurrences(false);
    let attack_unique: HashSet<(i32, Vec<u8>)> = attack_occ
        .iter()
        .map(|(cid, _s, body)| (*cid, body.clone()))
        .collect();
    let use_attack =
        attack_occ.len() > 1 && attack_unique.len() <= MSM5205_ATTACK_MAX_SAMPLES;
    let chosen = if use_attack { attack_occ } else { chunk_occ };

    let mut audible: Vec<Msm5205Sample> = Vec::new();
    let mut seen: HashMap<(i32, Vec<u8>), usize> = HashMap::new();
    for (cid, start, body) in chosen {
        let key = (cid, body.clone());
        if let Some(&index) = seen.get(&key) {
            audible[index].hits.push(Msm5205Hit { time: start });
            continue;
        }
        let chunk = Msm5205Sample {
            chip_id: cid,
            data: body,
            hits: vec![Msm5205Hit { time: start }],
            chunk: audible.len() as i32,
            first_chunk: start == 0,
            ..Msm5205Sample::default()
        };
        seen.insert(key, audible.len());
        audible.push(chunk);
    }
    if audible.is_empty() {
        warnings.push("MSM5205 streams decoded to silence".to_string());
    }
    if lossy && audible.len() > 32 {
        let before = audible.len();
        let bodies: Vec<Vec<u8>> = audible.iter().map(|s| s.data.clone()).collect();
        let rates: Vec<i32> = audible.iter().map(|s| s.rate).collect();
        let mut uses: IndexMap<i32, i64> = IndexMap::new();
        for (i, s) in audible.iter().enumerate() {
            uses.insert(i as i32, s.hits.len() as i64);
        }
        let (keep, remap) = cluster_fragments16(
            &bodies,
            &rates,
            &uses,
            crate::samplecluster::DEFAULT_TARGET,
            crate::samplecluster::DEFAULT_HOT,
        );
        // Only the moved hits are relocated; a kept sample's own hits stay put.
        let mut extra_hits: Vec<Vec<Msm5205Hit>> = vec![Vec::new(); keep.len()];
        let mut slots: Vec<Option<Msm5205Sample>> =
            audible.into_iter().map(Some).collect();
        for old in 0..slots.len() {
            let new = remap[old];
            if new < 0 || keep[new as usize] == old as i32 {
                continue;
            }
            let hits = std::mem::take(&mut slots[old].as_mut().unwrap().hits);
            extra_hits[new as usize].extend(hits);
        }
        let mut compact: Vec<Msm5205Sample> = Vec::with_capacity(keep.len());
        for (new, &old) in keep.iter().enumerate() {
            let mut sample = slots[old as usize].take().unwrap();
            sample.hits.extend(extra_hits[new].drain(..));
            compact.push(sample);
        }
        if compact.len() < before {
            warnings.push(format!(
                "MSM5205: lossy sample clustering {} -> {} bodies",
                before,
                compact.len()
            ));
        }
        audible = compact;
    }
    let mut seen_warnings: HashSet<String> = HashSet::new();
    warnings.retain(|w| seen_warnings.insert(w.clone()));
    (audible, warnings)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vgm::ChipWrite;

    #[test]
    fn prescaler_matches_s1_s2_pair() {
        assert_eq!(msm5205_prescaler(0), 96);
        assert_eq!(msm5205_prescaler(1), 64);
        assert_eq!(msm5205_prescaler(2), 48);
        assert_eq!(msm5205_prescaler(3), 1);
        // Only the low two bits are consulted.
        assert_eq!(msm5205_prescaler(0x07), 1);
        assert_eq!(msm5205_prescaler(-2), 48);
    }

    #[test]
    fn diff_table_matches_mame() {
        assert_eq!(&DIFF[..32], &[
            2, 6, 10, 14, 18, 22, 26, 30, -2, -6, -10, -14, -18, -22, -26, -30, 2, 6, 10, 14,
            19, 23, 27, 31, -2, -6, -10, -14, -19, -23, -27, -31,
        ]);
        assert_eq!(&DIFF[16 * 48..16 * 49], &[
            194, 582, 970, 1358, 1746, 2134, 2522, 2910, -194, -582, -970, -1358, -1746,
            -2134, -2522, -2910,
        ]);
        assert_eq!(DIFF.len(), 784);
    }

    #[test]
    fn chip_state_decodes_nibble_stream() {
        let mut state = ChipState::new(384_000, 0);
        state.write(0, 0);
        state.write(1, 0xFF);
        let mut levels = Vec::new();
        for _ in 0..60 {
            state.tick(1);
            levels.push(state.level());
        }
        let expected: Vec<i32> = (0..60)
            .map(|i| if i < 11 { -32 } else { -448 })
            .collect();
        assert_eq!(levels, expected);
        assert_eq!(state.pos, 17);
        assert_eq!(state.signal, -28);
        assert_eq!(state.step, 6);
        assert_eq!(state.rate(), 4000.0);
    }

    #[test]
    fn pcm_stats_detects_idle_and_silence() {
        let mut data = Vec::new();
        for _ in 0..100 {
            write_i16(&mut data, 1000);
        }
        for i in 0..20 {
            write_i16(&mut data, 1000 + if i % 2 == 0 { 500 } else { -500 });
        }
        let (vals, idle, width, peak) = pcm_stats(&data);
        assert_eq!(vals.len(), 120);
        assert_eq!(idle, 0); // the mode covers < 100% and sits above peak/4
        assert_eq!(peak, 1500);
        assert_eq!(width, 64);
        assert!(audible_pcm(&data, idle, width));

        let (vals, idle, width, peak) = pcm_stats(&[]);
        assert!(vals.is_empty());
        assert_eq!((idle, width, peak), (0, 64, 0));

        let mut quiet = Vec::new();
        for _ in 0..8 {
            write_i16(&mut quiet, 7);
        }
        let (_, idle, width, _) = pcm_stats(&quiet);
        assert_eq!(idle, 7);
        assert!(!audible_pcm(&quiet, idle, width));
    }

    #[test]
    fn collect_splits_into_chunks_and_names_them() {
        let mut vgm = VgmFile::default();
        vgm.total_samples = 22_050;
        vgm.msm5205_clock = 384_000;
        vgm.msm5205_flags = 0;
        vgm.writes
            .push(ChipWrite::new(0, "msm5205", 0, 0, 0));
        vgm.writes
            .push(ChipWrite::new(0, "msm5205", 0, 1, 0xFF));
        let (samples, warnings) = collect_msm5205(&vgm, false);
        assert!(warnings.is_empty());
        assert_eq!(samples.len(), 2);
        assert_eq!(samples[0].chunk, 0);
        assert_eq!(samples[0].name(), "MSM5205 1 000");
        assert!(samples[0].first_chunk);
        assert_eq!(samples[0].frames(), MSM5205_CHUNK_FRAMES);
        assert_eq!(samples[0].hits, vec![Msm5205Hit { time: 0 }]);
        assert_eq!(samples[1].chunk, 1);
        assert!(!samples[1].first_chunk);
        assert_eq!(samples[1].frames(), 22_050 - MSM5205_CHUNK_FRAMES);
        assert_eq!(samples[1].hits, vec![Msm5205Hit { time: MSM5205_CHUNK_FRAMES }]);
        assert_eq!(samples[0].rate, VGM_RATE as i32);
    }

    #[test]
    fn attack_bodies_splits_bursts_on_silence() {
        let mut vals: Vec<i16> = Vec::new();
        vals.extend(std::iter::repeat_n(0i16, 1000));
        vals.extend(std::iter::repeat_n(5000i16, 4000));
        vals.extend(std::iter::repeat_n(0i16, 1000));
        vals.extend(std::iter::repeat_n(-5000i16, 4000));
        vals.extend(std::iter::repeat_n(0i16, 1000));
        let mut data = Vec::new();
        for v in &vals {
            data.extend_from_slice(&v.to_le_bytes());
        }
        let (parsed, idle, width, peak) = pcm_stats(&data);
        assert_eq!((idle, width, peak), (0, 156, 5000));
        assert_eq!(onset_starts(&parsed, idle, width, peak), vec![1000, 6000]);
        let bodies = attack_bodies(&data, &parsed, idle, width);
        assert_eq!(bodies.len(), 2);
        assert_eq!((bodies[0].0, bodies[0].1.len() / 2), (1000, 4000));
        assert_eq!((bodies[1].0, bodies[1].1.len() / 2), (6000, 4000));
        assert_eq!(&bodies[1].1[..2], &(-5000i16).to_le_bytes());
    }

    #[test]
    fn collect_without_writes_returns_nothing() {
        let vgm = VgmFile::default();
        let (samples, warnings) = collect_msm5205(&vgm, true);
        assert!(samples.is_empty());
        assert!(warnings.is_empty());
    }
}
