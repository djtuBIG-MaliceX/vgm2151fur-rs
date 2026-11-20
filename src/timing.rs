//! Measure the musical row grid from key-on timing, or return None.
//!
//! Drivers schedule notes on a 16th note whose length is not a whole number of
//! 60 Hz frames. The measured unit is subdivided into rows and ticks, and EDxx
//! places every event within half a tick. The estimator is conservative, and
//! the caller falls back to the legacy 60 Hz heuristic when no lattice fits.

use std::collections::{BTreeMap, HashSet};

use indexmap::IndexMap;

use crate::opl::{chan_map_for, melodic_count, DRUM_BITS, KIND_OPL3};
use crate::parity::{format_f, py_floor_div_f, py_int, py_round};
use crate::vgm::{ChipWrite, VgmFile, VGM_RATE};
use crate::ym2413::KIT_TRIGGERS;

/// Target length of one tick: fine enough for frame-quantized note starts,
/// coarse enough that ED values stay small.
pub const TARGET_TICK_SAMPLES: f64 = 90.0;

/// Shortest row the grid will subdivide the measured 16th into. Shorter rows
/// drive the row rate (and the tracker's BPM) up; longer rows trade
/// slide-rate granularity for readability. See [`estimate_grid`]'s `min_row`.
pub const MIN_ROW_SAMPLES: f64 = 280.0;

#[derive(Clone, Debug, PartialEq)]
pub struct Grid {
    /// Samples per row (unit / subdiv).
    pub row: f64,
    /// Ticks per second (Furnace song hz).
    pub hz: f64,
    /// Ticks per row (Furnace song speed).
    pub speed: i32,
    pub confidence: f64,
    pub onsets: i64,
    pub source: String,
    /// Samples per 16th note (the measured unit).
    pub unit: f64,
    /// Rows per 16th note.
    pub subdiv: i32,
    /// The note value before the order-budget floor. A long rip with no single
    /// tempo would otherwise report song_length/1920 as its 16th. SCC/AY rows
    /// are measured from this pulse; `unit` still carries the floor for FM.
    pub pulse: f64,
}

impl Default for Grid {
    fn default() -> Self {
        Grid {
            row: 0.0,
            hz: 0.0,
            speed: 0,
            confidence: 0.0,
            onsets: 0,
            source: String::new(),
            unit: 0.0,
            subdiv: 1,
            pulse: 0.0,
        }
    }
}

impl Grid {
    /// Samples per tick.
    pub fn tick(&self) -> f64 {
        self.row / (self.speed.max(1) as f64)
    }

    /// Quarter-note tempo implied by the measured 16th note.
    pub fn bpm(&self) -> f64 {
        if self.unit != 0.0 {
            (VGM_RATE as f64 / self.unit) * 15.0
        } else {
            0.0
        }
    }

    pub fn row_note(&self) -> String {
        let name = match self.subdiv {
            1 => Some("16th"),
            2 => Some("32nd"),
            4 => Some("64th"),
            8 => Some("128th"),
            _ => None,
        };
        match name {
            Some(n) => n.to_string(),
            None => format!("1/{} of a 16th", self.subdiv),
        }
    }
}

/// Rising key edges of an OPL-family register stream.
///
/// A channel is keyed by bit 5 of its own `0xBn` register, or - in rhythm
/// mode - by one latch bit of `0xBD`, so both have to be tracked against the
/// previous byte. The channel a register belongs to comes from Furnace's own
/// table, because the YMF262 writes two banks through one register space and
/// pairs its channels for four-operator voices.
#[allow(dead_code)]
pub struct _OplKeys {
    kind: &'static str,
    opl3: bool,
    drums: bool,
    melodic: i32,
    chan_map: &'static [u16],
    by_reg: BTreeMap<(u32, i32), Vec<i32>>,
    keyed: BTreeMap<(u32, i32), bool>,
    trig: u32,
}

impl _OplKeys {
    pub fn new(kind: &'static str, writes: &[ChipWrite]) -> Self {
        let opl3 = kind == KIND_OPL3;
        let drums = writes.iter().any(|w| {
            w.chip == kind && (w.reg & 0xFF) == 0xBD && (w.val & 0x20) != 0
        });
        let melodic = melodic_count(kind, drums);
        let chan_map = chan_map_for(kind, drums);
        let mut by_reg: BTreeMap<(u32, i32), Vec<i32>> = BTreeMap::new();
        for (fch, &cm) in chan_map.iter().enumerate() {
            let port = if opl3 { ((cm >> 8) & 1) as u32 } else { 0 };
            by_reg
                .entry((port, (cm & 0xFF) as i32))
                .or_default()
                .push(fch as i32);
        }
        _OplKeys {
            kind,
            opl3,
            drums,
            melodic,
            chan_map,
            by_reg,
            keyed: BTreeMap::new(),
            trig: 0,
        }
    }

    pub fn feed(&mut self, w: &ChipWrite) -> Vec<i32> {
        let port = if self.opl3 {
            (w.reg >> 8) & 1
        } else {
            0
        };
        let r = w.reg & 0xFF;
        let mut hit: Vec<i32> = Vec::new();
        if (0xB0..=0xB8).contains(&r) {
            let on = (w.val & 0x20) != 0;
            // The table is keyed by channel offset; the register names it.
            if let Some(chans) = self.by_reg.get(&(port, (r - 0xB0) as i32)) {
                for &fch in chans {
                    let was = *self.keyed.get(&(port, fch)).unwrap_or(&false);
                    self.keyed.insert((port, fch), on);
                    if on && !was && !(self.drums && fch >= self.melodic) {
                        hit.push(fch);
                    }
                }
            }
        } else if r == 0xBD && !self.opl3 {
            let rhythm = (w.val & 0x20) != 0;
            let trig = if rhythm { w.val & 0x1F } else { 0 };
            for (i, &(bit, _name)) in DRUM_BITS.iter().enumerate() {
                if (trig & bit as u32) != 0 && (self.trig & bit as u32) == 0 {
                    hit.push(self.melodic + i as i32);
                }
            }
            self.trig = trig;
        }
        hit
    }
}

/// Rising key edges of one YM2413.
///
/// Bit 4 of `$2x` keys a melodic voice. In rhythm mode the last three
/// registers belong to the chip's own kit and `$0e`'s five low bits latch
/// it, so the kit's onsets come from there instead. The column numbering is
/// Furnace's: six melodic voices then BD, SD, TM, TP and HH.
pub struct _OpllKeys {
    keyed: BTreeMap<i32, bool>,
    rhythm: bool,
    trig: u32,
}

impl Default for _OpllKeys {
    fn default() -> Self {
        _OpllKeys {
            keyed: BTreeMap::new(),
            rhythm: false,
            trig: 0,
        }
    }
}

impl _OpllKeys {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn feed(&mut self, w: &ChipWrite) -> Vec<i32> {
        let r = w.reg & 0xFF;
        let mut hits: Vec<i32> = Vec::new();
        if (0x20..=0x28).contains(&r) {
            let hw = (r - 0x20) as i32;
            let on = (w.val & 0x10) != 0;
            let was = *self.keyed.get(&hw).unwrap_or(&false);
            self.keyed.insert(hw, on);
            if on && !was && !(self.rhythm && hw >= 6) {
                hits.push(hw);
            }
        } else if r == 0x0E {
            self.rhythm = (w.val & 0x20) != 0;
            let trig = if self.rhythm { w.val & 0x1F } else { 0 };
            for (i, &(bit, _name)) in KIT_TRIGGERS.iter().enumerate() {
                if (trig & bit) != 0 && (self.trig & bit) == 0 {
                    hits.push(6 + i as i32);
                }
            }
            self.trig = trig;
        }
        hits
    }
}

pub fn _keyons_by_channel(
    vgm: &VgmFile,
    chips: Option<&HashSet<&'static str>>,
) -> IndexMap<i32, Vec<i64>> {
    let mut out: IndexMap<i32, Vec<i64>> = IndexMap::new();
    let mut opl_trackers: BTreeMap<&'static str, _OplKeys> = BTreeMap::new();
    let mut opll_trackers: BTreeMap<u8, _OpllKeys> = BTreeMap::new();
    for w in &vgm.writes {
        if let Some(chips) = chips {
            if !chips.contains(w.chip) {
                continue;
            }
        }
        if w.chip == "ym2151" && w.reg == 0x08 && (w.val & 0x78) != 0 {
            out.entry((w.val & 7) as i32).or_default().push(w.sample);
        } else if w.chip == "ym2203" && (w.reg & 0xFF) == 0x28 && (w.val & 0xF0) != 0 {
            // Per-operator key masks; any 0->1 start is an onset. Keep chip
            // instances apart without colliding with the YM2151's 0-7.
            let voice = w.val & 3;
            out.entry(1000 + w.chip_id as i32 * 6 + voice as i32)
                .or_default()
                .push(w.sample);
        } else if w.chip == "ym2612"
            && (w.reg & 0x100) == 0
            && (w.reg & 0xFF) == 0x28
            && (w.val & 0xF0) != 0
        {
            // Same rule for the YM2612's and the YM2608's six channels. Both
            // key from one register on bus A, and the second group is encoded
            // as 4-6 after the reserved 3.
            let raw = w.val & 0x07;
            if raw == 3 || raw == 7 {
                continue;
            }
            let ch = if raw < 3 { raw } else { raw - 1 };
            out.entry(2000 + w.chip_id as i32 * 6 + ch as i32)
                .or_default()
                .push(w.sample);
        } else if w.chip == "ym2608"
            && (w.reg & 0x100) == 0
            && (w.reg & 0xFF) == 0x28
            && (w.val & 0xF0) != 0
        {
            let raw = w.val & 0x07;
            if raw == 3 || raw == 7 {
                continue;
            }
            let ch = if raw < 3 { raw } else { raw - 1 };
            out.entry(2000 + w.chip_id as i32 * 6 + ch as i32)
                .or_default()
                .push(w.sample);
        } else if w.chip == "ym2610"
            && (w.reg & 0x100) == 0
            && (w.reg & 0xFF) == 0x28
            && (w.val & 0xF0) != 0
        {
            // YM2610 uses 2+2 FM channels (B: 3+3); the 0x28 channel field
            // follows the same reserved-slot encoding.
            let raw = w.val & 0x07;
            let ch: Option<i32> = if vgm.ym2610_b {
                match raw {
                    0 => Some(0),
                    1 => Some(1),
                    2 => Some(2),
                    4 => Some(3),
                    5 => Some(4),
                    6 => Some(5),
                    _ => None,
                }
            } else {
                match raw {
                    1 => Some(0),
                    2 => Some(1),
                    5 => Some(2),
                    6 => Some(3),
                    _ => None,
                }
            };
            let Some(ch) = ch else {
                continue;
            };
            out.entry(3000 + w.chip_id as i32 * 6 + ch)
                .or_default()
                .push(w.sample);
        } else if w.chip == "opl" || w.chip == "opl2" || w.chip == "opl3" {
            if !opl_trackers.contains_key(w.chip) {
                opl_trackers.insert(w.chip, _OplKeys::new(w.chip, &vgm.writes));
            }
            let tracker = opl_trackers.get_mut(w.chip).expect("opl tracker");
            // An OPL3 owns both banks in one system. A YM3812 pair keeps its
            // instances apart, and one chip's lattice cannot alias the other's.
            for fch in tracker.feed(w) {
                out.entry(4000 + w.chip_id as i32 * 32 + fch)
                    .or_default()
                    .push(w.sample);
            }
        } else if w.chip == "opll" {
            if !opll_trackers.contains_key(&w.chip_id) {
                opll_trackers.insert(w.chip_id, _OpllKeys::new());
            }
            let tracker = opll_trackers.get_mut(&w.chip_id).expect("opll tracker");
            for fch in tracker.feed(w) {
                out.entry(5000 + w.chip_id as i32 * 16 + fch)
                    .or_default()
                    .push(w.sample);
            }
        }
    }
    for v in out.values_mut() {
        v.sort();
    }
    out
}

fn _median(vals: &[f64]) -> f64 {
    let mut s = vals.to_vec();
    s.sort_by(|a, b| a.partial_cmp(b).expect("finite median"));
    s[s.len() / 2]
}

/// Most common delta, bucketed so jitter does not split the peak.
fn _bucketed_mode(vals: &[i64], bucket: i64) -> f64 {
    let mut counts: IndexMap<i64, i64> = IndexMap::new();
    for &v in vals {
        let key = py_int(py_round(v as f64 / bucket as f64)) * bucket;
        *counts.entry(key).or_insert(0) += 1;
    }
    let mut best_key = 0i64;
    let mut best_count = -1i64;
    for (&key, &count) in &counts {
        if count > best_count {
            best_count = count;
            best_key = key;
        }
    }
    best_key as f64
}

/// Least-squares refinement of the note unit around a candidate.
///
/// Only intervals at least ~0.6x the guess take part. Ornament notes on a
/// finer grid must not drag the fit away from the candidate's lattice.
fn _refine_unit(deltas: &[i64], guess: f64) -> f64 {
    let mut usable: Vec<i64> = deltas
        .iter()
        .copied()
        .filter(|&d| d as f64 >= 0.6 * guess)
        .collect();
    if usable.len() < 8 {
        usable = deltas.to_vec();
    }
    // Repeated intervals dominate the sum. Folding them into weights keeps
    // the 600-point search affordable on dense percussive tracks.
    let mut weight_map: IndexMap<i64, i64> = IndexMap::new();
    for d in usable {
        *weight_map.entry(d).or_insert(0) += 1;
    }
    let weights: Vec<(i64, i64)> = weight_map.into_iter().collect();

    let cost = |t: f64| -> f64 {
        let mut total = 0.0;
        for &(d, n) in &weights {
            let mut k = py_int(py_round(d as f64 / t));
            if k < 1 {
                k = 1;
            }
            let r = d as f64 - k as f64 * t;
            total += n as f64 * r * r / k as f64; // weight long deltas a little less
        }
        total
    };

    let lo = guess * 0.9;
    let hi = guess * 1.1;
    let mut best_t = guess;
    let mut best_c = cost(guess);
    for i in 0..=400 {
        let t = lo + (hi - lo) * i as f64 / 400.0;
        let c = cost(t);
        if c < best_c {
            best_c = c;
            best_t = t;
        }
    }
    let lo2 = best_t * 0.995;
    let hi2 = best_t * 1.005;
    for i in 0..=200 {
        let t = lo2 + (hi2 - lo2) * i as f64 / 200.0;
        let c = cost(t);
        if c < best_c {
            best_c = c;
            best_t = t;
        }
    }
    best_t
}

/// Fraction of intervals that are near-integer multiples of the unit.
fn _lattice_coverage(deltas: &[i64], t: f64) -> f64 {
    let mut good = 0usize;
    for &d in deltas {
        let k = py_int(py_round(d as f64 / t)).max(1);
        if (d as f64 - k as f64 * t).abs() <= 0.06 * k as f64 * t {
            good += 1;
        }
    }
    if deltas.is_empty() {
        0.0
    } else {
        good as f64 / deltas.len() as f64
    }
}

/// Furnace hz/speed for a row of `row` samples at ~`tick_samples` per tick.
///
/// EDxx must stay below the row's tick count (strict delay policy), and the
/// tick rate has to stay a sane "song hz" for the tracker UI.
pub fn hz_speed_for_row(row: f64, tick_samples: f64) -> (f64, i32) {
    let mut speed = py_int(py_round(row / tick_samples)).clamp(1, 255) as i32;
    while speed > 1 && row / (speed as f64) < 40.0 {
        speed /= 2;
    }
    (VGM_RATE as f64 * speed as f64 / row, speed)
}

/// Map a source sample to (row, ED delay in ticks).
///
/// `floor=false` rounds to the nearest tick. `floor=true` uses the preceding
/// tick. A caller can prepend the held byte and keep the exact source time.
pub fn place_tick(
    sample: i64,
    t0: i64,
    row_samples: f64,
    speed: i32,
    tick_samples: f64,
    stats: Option<&mut IndexMap<String, i64>>,
) -> (i32, i32) {
    place_tick_floor(sample, t0, row_samples, speed, tick_samples, false, stats)
}

/// [`place_tick`] with the Python keyword-only `floor` switch. `floor=true`
/// uses the preceding tick instead of the nearest one.
pub fn place_tick_floor(
    sample: i64,
    t0: i64,
    row_samples: f64,
    speed: i32,
    tick_samples: f64,
    floor: bool,
    stats: Option<&mut IndexMap<String, i64>>,
) -> (i32, i32) {
    if speed <= 1 {
        let row = ((sample - t0) as f64 / row_samples + if floor { 0.0 } else { 0.5 })
            .floor()
            .max(0.0) as i32;
        return (row, 0);
    }
    let row = py_int(py_floor_div_f((sample - t0) as f64, row_samples)).max(0);
    let target = t0 as f64 + row as f64 * row_samples;
    let offset = (sample as f64 - target) / tick_samples;
    let mut d = if floor {
        offset.floor() as i64
    } else {
        py_int(py_round(offset))
    };
    let mut row = row;
    if d >= speed as i64 {
        row += 1;
        d = 0;
        if let Some(stats) = stats {
            *stats.entry("ed_rolled".to_string()).or_insert(0) += 1;
        }
    }
    (row as i32, d.max(0) as i32)
}

/// Options-free [`place_tick`] wrapper: nearest tick, default tick length,
/// no stats.
pub fn place_tick_simple(sample: i64, t0: i64, row_samples: f64, speed: i32) -> (i32, i32) {
    place_tick(
        sample,
        t0,
        row_samples,
        speed,
        TARGET_TICK_SAMPLES,
        None,
    )
}

/// Lengthen every row by `factor` (rows/s and so BPM drop by `factor`).
///
/// `subdiv` (rows per measured 16th) is cosmetic - it only feeds Song.bpm - so
/// a non-integer result (3x) is fine. Returns the new (hz, speed, subdiv).
pub fn condense_row(
    hz: f64,
    speed: i32,
    subdiv: f64,
    factor: f64,
    tick_samples: f64,
) -> (f64, i32, f64) {
    if factor <= 1.0 {
        return (hz, speed, subdiv);
    }
    let row = (VGM_RATE as f64 / hz) * speed as f64 * factor;
    let (nhz, nspeed) = hz_speed_for_row(row, tick_samples);
    (nhz, nspeed, subdiv / factor)
}

/// Return the measured grid, a subdivided 16th note, or None.
///
/// Candidate units are scored by how many intervals they explain as integer
/// multiples, and the coarsest near-best unit wins. `onsets` overrides the
/// note times, keyed by channel. `chips` restricts the measured register
/// stream to the family a mixed log actually converts.
#[allow(clippy::too_many_arguments)]
pub fn estimate_grid(
    vgm: &VgmFile,
    min_confidence: f64,
    tick_samples: f64,
    min_row: f64,
    onsets: Option<&IndexMap<i32, Vec<i64>>>,
    label: &str,
    chips: Option<&HashSet<&'static str>>,
) -> Option<Grid> {
    let owned;
    let by_ch: &IndexMap<i32, Vec<i64>> = match onsets {
        Some(o) => o,
        None => {
            owned = _keyons_by_channel(vgm, chips);
            &owned
        }
    };
    let mut deltas: Vec<i64> = Vec::new();
    for ons in by_ch.values() {
        for pair in ons.windows(2) {
            let d = pair[1] - pair[0];
            if d > 0 && d <= 20000 {
                deltas.push(d);
            }
        }
    }
    if deltas.len() < 24 {
        return None;
    }

    let mut candidates: Vec<f64> = Vec::new();
    let add_candidate = |candidates: &mut Vec<f64>, v: f64| {
        if !candidates.iter().any(|&c| c == v) {
            candidates.push(v);
        }
    };
    for ons in by_ch.values() {
        let ds: Vec<i64> = ons
            .windows(2)
            .map(|p| p[1] - p[0])
            .filter(|&d| d > 0 && d <= 20000)
            .collect();
        if ds.len() < 8 {
            continue;
        }
        add_candidate(&mut candidates, _bucketed_mode(&ds, 4));
        let floats: Vec<f64> = ds.iter().map(|&d| d as f64).collect();
        add_candidate(&mut candidates, _median(&floats));
    }
    add_candidate(&mut candidates, _bucketed_mode(&deltas, 4));
    candidates.retain(|&c| c > 0.0);

    let mut scored: Vec<(f64, f64)> = Vec::new(); // (unit, coverage)
    let mut fine_scored: Vec<(f64, f64)> = Vec::new();
    let mut folded_scored: Vec<(f64, f64)> = Vec::new();
    let mut shortest = *deltas.iter().min().expect("nonempty deltas");
    for &c in &candidates {
        let t = _refine_unit(&deltas, c);
        if (800.0..=8000.0).contains(&t) {
            scored.push((t, _lattice_coverage(&deltas, t)));
            continue;
        }
        if t < 800.0 {
            // A few drivers (notably fast FDS engines) tick far faster than a
            // musical 16th. Accept such a unit only when the stream is large
            // and almost every interval sits on it, so ordinary ornamental
            // notes and stray writes cannot become the song grid.
            if t >= 64.0 && deltas.len() >= 64 && shortest as f64 <= 1.5 * t {
                let cov = _lattice_coverage(&deltas, t);
                if cov >= 0.9 {
                    fine_scored.push((t, cov));
                }
            }
            continue;
        }
        // Out of band, too coarse. A slow track's *fastest* note value can
        // still be longer than a 16th note: some drivers run 9.5k-19k samples
        // per melodic event. Fold by halves back into the band so the rows
        // can subdivide it (a 15 rows/s fallback cannot carry the driver's
        // per-frame pitch writes). Only used when no in-band candidate
        // explains the stream, and only when the folded unit really fits the
        // intervals, or noise would become a grid.
        let mut folded = t;
        while folded > 8000.0 {
            folded /= 2.0;
        }
        let mut recovered: Vec<(f64, f64)> = Vec::new();
        if folded >= 800.0 {
            let cov = _lattice_coverage(&deltas, folded);
            if cov >= 0.8 {
                recovered.push((folded, cov));
            }
        }
        if recovered.is_empty() {
            // Half-folding fails when the mode/median sits on a *multiple* of
            // the row unit - a melody that moves in 8ths, quarters or whole
            // bars most of the time - so the unit is c/2, c/3, ... rather than
            // c/2^n. Divide the raw candidate back down and demand a tight
            // fit *and* that the unit is not far finer than the shortest gap
            // seen: a divided noise unit fits the loose 0.8 bar (the
            // estimator's pseudorandom fixture did) but describes a grid no
            // note ever lands on.
            shortest = *deltas.iter().min().expect("nonempty deltas");
            for div in 2..=8 {
                let sub = c / div as f64;
                if sub < 800.0 {
                    continue;
                }
                let ts = _refine_unit(&deltas, sub);
                if !(800.0..=8000.0).contains(&ts) {
                    continue;
                }
                if shortest as f64 > 2.0 * ts {
                    continue;
                }
                let cov = _lattice_coverage(&deltas, ts);
                if cov >= 0.9 {
                    recovered.push((ts, cov));
                }
            }
        }
        if !recovered.is_empty() {
            // max(recovered) over (unit, coverage) tuples: first on ties.
            let mut best = recovered[0];
            for &item in &recovered[1..] {
                if item.0 > best.0 || (item.0 == best.0 && item.1 > best.1) {
                    best = item;
                }
            }
            folded_scored.push(best);
        }
    }
    if scored.is_empty() {
        scored = fine_scored;
    }
    if scored.is_empty() {
        scored = folded_scored;
    }
    if scored.is_empty() {
        return None;
    }
    let mut low_note = "";
    let ok: Vec<(f64, f64)> = scored
        .iter()
        .copied()
        .filter(|&(_t, cov)| cov >= 0.8)
        .collect();
    let (unit, best_conf, pulse);
    if ok.is_empty() {
        // Tracks that change tempo between sections have no single unit that
        // explains the whole song. Use the smallest real candidate so the row
        // grid stays fine enough to place the fastest section without
        // collisions; EDxx still puts every event at its true write time.
        let mut smalls: Vec<f64> = candidates.iter().copied().filter(|&c| c >= 800.0).collect();
        smalls.sort_by(|a, b| a.partial_cmp(b).expect("finite candidate"));
        if smalls.is_empty() {
            return None;
        }
        // `pulse` stays the fastest real note value. The floor below only
        // lengthens the FM row so a long rip still fits in about 240 orders
        // of 64-row patterns. Coverage is scored on the floored unit, which
        // is why a medley can report 0% here.
        pulse = smalls[0];
        let mut u = pulse;
        if vgm.total_samples != 0 {
            u = u.max(vgm.total_samples as f64 / 1920.0); // <= ~240 orders
        }
        let conf = _lattice_coverage(&deltas, u);
        unit = u;
        best_conf = conf;
        low_note = ", sections change unit";
    } else {
        // Prefer the coarsest unit among those with near-best coverage.
        let best_cov = ok.iter().map(|&(_t, cov)| cov).fold(f64::NEG_INFINITY, f64::max);
        let near: Vec<(f64, f64)> = ok
            .iter()
            .copied()
            .filter(|&(_t, cov)| cov >= best_cov - 0.15)
            .collect();
        let mut best = near[0];
        for &item in &near[1..] {
            if item.0 > best.0 || (item.0 == best.0 && item.1 > best.1) {
                best = item;
            }
        }
        unit = best.0;
        best_conf = best.1;
        pulse = unit;
    }
    let mut subdiv = 1i32;
    for n in [8, 4, 2] {
        if unit / n as f64 >= min_row {
            subdiv = n;
            break;
        }
    }
    let row = unit / subdiv as f64;
    let (hz, speed) = hz_speed_for_row(row, tick_samples);

    let mut note = format!("{} ({} intervals)", label, deltas.len());
    if best_conf < min_confidence {
        note += ", low confidence";
    }
    note += low_note;
    Some(Grid {
        row,
        hz,
        speed,
        confidence: best_conf,
        onsets: by_ch.values().map(|v| v.len() as i64).sum(),
        source: note,
        unit,
        subdiv,
        pulse,
    })
}

/// Estimated-grid wrapper with the Python keyword defaults.
pub fn estimate_grid_default(vgm: &VgmFile) -> Option<Grid> {
    estimate_grid(
        vgm,
        0.75,
        TARGET_TICK_SAMPLES,
        MIN_ROW_SAMPLES,
        None,
        "key-on lattice",
        None,
    )
}

// Writer uses row_of(last) + 2 and caps at 256 orders of 64 rows.
pub const _ORDER_ROWS: i64 = 256 * 64 - 4;

/// Shrink the row until each playable same-voice restart has its own row.
///
/// `gaps` are distances between restarts on one voice. A gap below the
/// finest row that fits in 256 orders cannot have its own row. Returns the
/// input grid unchanged when every gap already fits.
pub fn refine_pcm_grid(
    hz: f64,
    speed: i32,
    subdiv: i32,
    extent: i64,
    gaps: &[i64],
    tick_samples: f64,
) -> (f64, i32, i32, String) {
    if extent <= 0 || speed <= 0 || hz <= 0.0 || gaps.is_empty() {
        return (hz, speed, subdiv, String::new());
    }
    let row = (VGM_RATE as f64 / hz) * speed as f64;
    let finest = 1.0f64.max(extent as f64 / _ORDER_ROWS as f64);
    let honor: Vec<i64> = gaps
        .iter()
        .copied()
        .filter(|&g| g as f64 >= finest)
        .collect();
    if honor.is_empty() {
        return (hz, speed, subdiv, String::new());
    }
    let need = *honor.iter().min().expect("nonempty honor") as f64;
    if need >= row - 0.5 {
        return (hz, speed, subdiv, String::new());
    }
    let target = finest.max(need - 1.0);
    let mut new_speed = py_int(py_round(target / tick_samples)).clamp(1, 255) as i32;
    while new_speed > 1 && target / (new_speed as f64) < 40.0 {
        new_speed /= 2;
    }
    let new_hz = VGM_RATE as f64 * new_speed as f64 / target;
    let new_sub = if row > 0.0 {
        (py_int(py_round(subdiv as f64 * row / target)).max(1)) as i32
    } else {
        subdiv
    };
    let note = format!(
        "PCM restarts down to {} samples, row {} samples so each has its own row",
        format_f(need, 0),
        format_f(target, 0)
    );
    (new_hz, new_speed, new_sub, note)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(chip: &'static str, reg: u32, val: u32, sample: i64) -> ChipWrite {
        ChipWrite::new(sample, chip, 0, reg, val)
    }

    #[test]
    fn grid_properties() {
        let g = Grid {
            row: 900.0,
            hz: 490.0,
            speed: 10,
            confidence: 1.0,
            onsets: 12,
            source: "test".into(),
            unit: 1800.0,
            subdiv: 2,
            pulse: 1800.0,
        };
        assert!((g.tick() - 90.0).abs() < 1e-9);
        assert!((g.bpm() - (44100.0 / 1800.0) * 15.0).abs() < 1e-9);
        assert_eq!(g.row_note(), "32nd");
        let d = Grid::default();
        assert_eq!(d.tick(), 0.0);
        assert_eq!(d.bpm(), 0.0);
        assert_eq!(d.row_note(), "16th");
        let odd = Grid {
            subdiv: 3,
            ..Grid::default()
        };
        assert_eq!(odd.row_note(), "1/3 of a 16th");
    }

    #[test]
    fn opl_keys_rising_edges() {
        let writes = vec![
            write("opl2", 0xB0, 0x20, 10),
            write("opl2", 0xB0, 0x20, 20),
            write("opl2", 0xB0, 0x00, 30),
            write("opl2", 0xB0, 0x20, 40),
            write("opl2", 0xB1, 0x20, 50),
        ];
        let mut keys = _OplKeys::new("opl2", &writes);
        assert_eq!(keys.feed(&writes[0]), vec![0]);
        assert!(keys.feed(&writes[1]).is_empty());
        assert!(keys.feed(&writes[2]).is_empty());
        assert_eq!(keys.feed(&writes[3]), vec![0]);
        assert_eq!(keys.feed(&writes[4]), vec![1]);
    }

    #[test]
    fn opll_keys_and_rhythm() {
        let mut keys = _OpllKeys::new();
        assert_eq!(keys.feed(&write("opll", 0x20, 0x10, 0)), vec![0]);
        assert!(keys.feed(&write("opll", 0x20, 0x10, 1)).is_empty());
        assert!(keys.feed(&write("opll", 0x20, 0x00, 2)).is_empty());
        assert_eq!(keys.feed(&write("opll", 0x20, 0x10, 3)), vec![0]);
        // Rhythm latch keys the kit on channels 6.. in KIT_TRIGGERS order.
        assert_eq!(keys.feed(&write("opll", 0x0E, 0x20 | 0x10, 4)), vec![6]);
        assert_eq!(keys.feed(&write("opll", 0x0E, 0x20u32 | 0x10, 5)), Vec::<i32>::new());
    }

    #[test]
    fn place_tick_rounding_floor_and_rollover() {
        assert_eq!(place_tick_simple(1000, 0, 1000.0, 4), (1, 0));
        assert_eq!(place_tick_simple(1030, 0, 1000.0, 4), (1, 0));
        assert_eq!(place_tick_simple(1045, 0, 1000.0, 4), (1, 0));
        assert_eq!(place_tick_simple(1050, 0, 1000.0, 4), (1, 1));
        assert_eq!(
            place_tick_floor(1900, 0, 1000.0, 2, 500.0, true, None),
            (1, 1)
        );
        let mut stats: IndexMap<String, i64> = IndexMap::new();
        assert_eq!(
            place_tick(1900, 0, 1000.0, 2, 500.0, Some(&mut stats)),
            (2, 0)
        );
        assert_eq!(stats.get("ed_rolled"), Some(&1));
    }

    #[test]
    fn speed_condense_and_pcm_grid() {
        assert_eq!(hz_speed_for_row(900.0, 90.0), (490.0, 10));
        assert_eq!(hz_speed_for_row(30.0, 90.0), (1470.0, 1));
        assert_eq!(condense_row(490.0, 10, 2.0, 1.0, 90.0), (490.0, 10, 2.0));
        let (nhz, nspeed, nsub) = condense_row(490.0, 10, 2.0, 2.0, 90.0);
        assert_eq!(nhz, 490.0);
        assert_eq!(nspeed, 20);
        assert_eq!(nsub, 1.0);

        // A gap below the row forces a finer grid.
        let (hz, speed, sub, note) = refine_pcm_grid(490.0, 10, 2, 334656, &[100], 90.0);
        assert!(note.starts_with("PCM restarts down to 100 samples"));
        assert!((hz - 44100.0 / 99.0).abs() < 1e-9);
        assert_eq!(speed, 1);
        assert_eq!(sub, 18);
        // Everything already fits: unchanged, no note.
        assert_eq!(
            refine_pcm_grid(490.0, 10, 2, 334656, &[2000], 90.0),
            (490.0, 10, 2, String::new())
        );
        // 10/4 and 12/4 round half to even: buckets 8 and 12 tie at two, and
        // the first key inserted wins the tie.
        assert_eq!(_bucketed_mode(&[10, 10, 12, 12], 4), 8.0);
        assert_eq!(_bucketed_mode(&[10, 10, 11, 12, 12], 4), 12.0);
    }
}

/// Memoised grid estimates across condense factors.
///
/// Python keys the dict with `(kind, tick, row_floor)`; floats are stored as
/// their bit patterns so the map can hash them, which is equivalent for the
/// positive tick/floor values the analyzer passes.
pub type GridCache = indexmap::IndexMap<(String, u64, u64), Option<Grid>>;
