//! YM2612 DAC: rate-exact PCM chunks (default), editable slices, or the old
//! timeline.
//!
//! The default emits one FM6 note per audio chunk. `slices=True` keeps the
//! per-run slice representation; `timeline=True` is the old comparison body.

use std::collections::{HashMap, HashSet};

use indexmap::IndexMap;

use crate::parity::{py_floor_div, py_int, py_round};
use crate::samplecluster::{cluster_fragments, DEFAULT_HOT, DEFAULT_TARGET};
use crate::timing::place_tick_floor;
use crate::vgm::{DacStreamPlay, VgmFile, DAC_STREAM_YM2612, VGM_RATE};

pub const DAC_GAP: i64 = 512;
pub const DAC_MIN_SAMPLES: i64 = 8;
pub const DAC_MAP_CAPACITY: i32 = 120;
/// A pause over this starts another phrase.
pub const DAC_RUN_GAP: i64 = 2048;
/// Two runs at one bank offset whose trimmed mean byte interval differ by more
/// than this are genuinely repitched and get their own sample.  4/5-wait
/// quantisation jitter at one nominal rate stays inside this window.
pub const DAC_WAIT_CLUSTER: f64 = 0.35;
pub const DAC_TIMELINE_RATE: i32 = 22050;
/// Keep the module below release Furnace's sample-count limit.
pub const DAC_RELEASE_SAMPLES: usize = 240;
/// Audio chunk length; short sources stay one chunk.
pub const DAC_CHUNK_SECONDS: f64 = 0.25;
/// Furnace clamps a sample-playback rate to this value.
pub const DAC_RATE_MAX: i32 = 192000;
pub const DAC_RATE_MIN: i32 = 1;
/// Bank samples whose bodies are the same audio at a small frame offset share
/// one module sample. Rate and length are compared loosely, but the interior of
/// the body must match almost exactly; only the leading/trailing fill may move.
pub const DAC_SIMILAR_RATE: i32 = 32;
pub const DAC_SIMILAR_SHIFT: i32 = 16;
pub const DAC_SIMILAR_EDGE: usize = 128;
pub const DAC_SIMILAR_MIN_MATCH: f64 = 0.90;
pub const DAC_SIMILAR_INTERIOR: f64 = 0.999;
pub const DAC_SIMILAR_MIN_LEN: usize = 32;
/// A unique sample that starts this soon after the previous unique sample is
/// part of the same contiguous one-shot phrase.
pub const DAC_ONESHOT_GAP: i64 = 64;
/// A final body at least this long whose longest silent stretch is over 40%
/// of it is split at that stretch.  Bank runs split earlier at any held
/// silence of DAC_RUN_GAP samples, so a kick does not carry a delayed snare.
pub const DAC_SILENCE_MIN_LEN: usize = 8192;
pub const DAC_SILENCE_FRACTION: f64 = 0.40;
/// A signed DAC body whose peak is at or below this is effectively inaudible.
/// A body where every byte is the same is a flat DC line, not a sample.  Both
/// are dropped before they take a sample-map slot: the source driver often
/// leaves a few bytes of near-zero noise between real hits.
pub const DAC_QUIET_PEAK: i32 = 2;
/// The fallback path splits the word stream at silent holes instead of cutting
/// fixed 0.25 s chunks.  A zero hold this long (in VGM samples) is a rest
/// between hits, so the next audio starts a new candidate body.  A short dip
/// inside a waveform does not.
pub const DAC_ATTACK_SILENCE: i64 = 64;
/// A candidate shorter than this many frames is a fragment, not a playable
/// drum hit.  Loud one-frame clicks still pass the amplitude test above.
pub const DAC_ATTACK_MIN_FRAMES: i64 = 2;
/// A E5-like amplitude jump after this much near-silence is also an attack,
/// even when the source never wrote an exact zero.  The thresholds are in
/// signed 8-bit units and VGM samples.
pub const DAC_ATTACK_QUIET_PEAK: i32 = 4;
pub const DAC_ATTACK_JUMP: i32 = 24;
pub const DAC_ATTACK_QUIET_SAMPLES: i64 = 32;
/// Optional aggressive (lossy) clustering target.  This is off by default;
/// `lossy_samples=True` asks for it.  See samplecluster.py for the rule.
pub const DAC_TARGET_SAMPLES: i32 = DEFAULT_TARGET;
pub const DAC_HOT_ANCHORS: i32 = DEFAULT_HOT;

#[derive(Clone, Debug, PartialEq)]
pub struct DacSample {
    pub start: i64,
    pub length: i64,
    pub data: Vec<u8>,
    pub c4_rate: i32,
    pub onset: i64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct DacPlay {
    /// One source run played on FM6.
    ///
    /// ``source`` marks a play whose sample is a bank segment longer than the
    /// run itself. A later note cuts it, so a short slice and the longer block it
    /// came from share one sample instead of storing a copy per length.
    pub onset: i64,
    pub sample: i32,
    pub pan: i32,
    pub span: i64,
    pub collapsed: bool,
    pub source: bool,
    pub fresh: bool,
    /// Value the DAC holds after this run when the driver wrote silence next.
    /// A dropped quiet run leaves ``None`` here; the body builder keeps the
    /// previous payload byte instead.
    pub hold: Option<i32>,
}

impl Default for DacPlay {
    fn default() -> Self {
        DacPlay::new(0, 0, 0, 0)
    }
}

impl DacPlay {
    pub fn new(onset: i64, sample: i32, pan: i32, span: i64) -> Self {
        DacPlay {
            onset,
            sample,
            pan,
            span,
            collapsed: false,
            source: false,
            fresh: false,
            hold: None,
        }
    }

    pub fn with_flags(
        onset: i64,
        sample: i32,
        pan: i32,
        span: i64,
        collapsed: bool,
        source: bool,
        fresh: bool,
        hold: Option<i32>,
    ) -> Self {
        DacPlay {
            onset,
            sample,
            pan,
            span,
            collapsed,
            source,
            fresh,
            hold,
        }
    }
}

/// One value in the collector's stats map.
#[derive(Clone, Debug, PartialEq)]
pub enum DacStat {
    Int(i64),
    Text(&'static str),
    Points(Vec<(i64, i32)>),
}

/// Insertion-ordered YM2612 DAC stats dict.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct DacStats(pub IndexMap<String, DacStat>);

impl DacStats {
    pub fn new() -> Self {
        DacStats(IndexMap::new())
    }

    pub fn int(&self, key: &str) -> i64 {
        match self.0.get(key) {
            Some(DacStat::Int(value)) => *value,
            _ => 0,
        }
    }

    pub fn text(&self, key: &str, default: &'static str) -> &'static str {
        match self.0.get(key) {
            Some(DacStat::Text(value)) => value,
            _ => default,
        }
    }

    pub fn points(&self, key: &str) -> &[(i64, i32)] {
        match self.0.get(key) {
            Some(DacStat::Points(value)) => value,
            _ => &[],
        }
    }

    pub fn contains(&self, key: &str) -> bool {
        self.0.contains_key(key)
    }

    pub fn remove(&mut self, key: &str) {
        self.0.shift_remove(key);
    }

    pub fn bump(&mut self, key: &str, amount: i64) {
        match self.0.entry(key.to_string()).or_insert(DacStat::Int(0)) {
            DacStat::Int(value) => *value += amount,
            _ => panic!("stat {key} is not an integer"),
        }
    }

    pub fn bump1(&mut self, key: &str) {
        self.bump(key, 1);
    }

    pub fn set_int(&mut self, key: &str, value: i64) {
        self.0.insert(key.to_string(), DacStat::Int(value));
    }

    pub fn set_text(&mut self, key: &str, value: &'static str) {
        self.0.insert(key.to_string(), DacStat::Text(value));
    }

    pub fn set_points(&mut self, key: &str, value: Vec<(i64, i32)>) {
        self.0.insert(key.to_string(), DacStat::Points(value));
    }

    pub fn merge(&mut self, other: &DacStats) {
        for (key, value) in &other.0 {
            self.0.insert(key.clone(), value.clone());
        }
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

pub fn _signed_bank(raw: &[u8]) -> Vec<u8> {
    raw.iter().map(|&x| x.wrapping_sub(0x80)).collect()
}

/// Peak absolute signed value of an 8-bit body.
pub fn _body_peak(data: &[u8]) -> i32 {
    let mut peak = 0;
    for &v in data {
        let value = if v & 0x80 != 0 { 256 - v as i32 } else { v as i32 };
        if value > peak {
            peak = value;
        }
    }
    peak
}

/// Whether a body carries audio rather than silence or a flat DC line.
pub fn _audible_body(data: &[u8]) -> bool {
    if data.is_empty() {
        return false;
    }
    let first = data[0];
    let varied = data[1..].iter().any(|&value| value != first);
    varied && _body_peak(data) > DAC_QUIET_PEAK
}

/// Genesis 0xB6 pan bits to Furnace's split 4-bit pan byte.
pub fn _pan_byte(raw: i64) -> i32 {
    let left = raw & 0x80 != 0;
    let right = raw & 0x40 != 0;
    if left && right {
        return 0xFF;
    }
    if left {
        return 0xF0;
    }
    if right {
        return 0x0F;
    }
    0
}

/// (sample, raw 0xB6 pan bits) for the first YM2612's channel 6.
///
/// Channel 6 lives on the second register port, so its pan is ``$1B6``.
/// Port-0 ``$B6`` is channel 3's pan and says nothing about the DAC.
pub fn _pan_timeline(vgm: &VgmFile) -> Vec<(i64, i64)> {
    let mut out: Vec<(i64, i64)> = Vec::new();
    for w in &vgm.writes {
        if w.chip == "ym2612"
            && w.chip_id == 0
            && (w.reg & 0x100) != 0
            && (w.reg & 0xFF) == 0xB6
        {
            out.push((w.sample, (w.val & 0xC0) as i64));
        }
    }
    out
}

/// How close a 0xB6 write must be to a DAC play to count as that hit's pan.
/// The hardware DAC is mono: the register only pans FM6, and drivers that pan
/// DAC drums write it right at the hit. A value left over from an FM6 voice
/// should not hard-pan the drums. This is decided once per log: a driver that
/// never keys FM6 has no other owner for the register, while one that does may
/// leave an FM6 pan behind.
pub const DAC_PAN_WINDOW: i64 = 128;

/// Whether channel 6 is keyed as an FM voice while the DAC is off.
///
/// A log that never touches FM6 has no other owner for the 0xB6 register, so
/// a pan write belongs to the DAC drums. When the driver also plays FM6, its
/// pan value is just channel state and must not pan the DAC.
pub fn _fm6_is_used(vgm: &VgmFile) -> bool {
    let mut enabled = false;
    for w in &vgm.writes {
        if w.chip != "ym2612" || w.chip_id != 0 || (w.reg & 0x100) != 0 {
            continue;
        }
        let r = w.reg & 0xFF;
        if r == 0x2B {
            enabled = (w.val & 0x80) != 0;
        } else if r == 0x28 && (w.val & 7) == 6 && (w.val & 0xF0) != 0 && !enabled {
            return true;
        }
    }
    false
}

/// Pan byte for a DAC play at `t`, or -1 when the DAC is not panned.
pub fn _pan_at(timeline: Option<&[(i64, i64)]>, t: i64) -> i32 {
    let timeline = match timeline {
        Some(timeline) if !timeline.is_empty() => timeline,
        _ => return -1,
    };
    let key = (t, 0xFFFFi64);
    let idx = bisect_right(timeline, key);
    let raw = if idx > 0 {
        timeline[idx - 1].1
    } else {
        0xC0
    };
    _pan_byte(raw)
}

fn bisect_right<T: Ord + Copy>(slice: &[T], key: T) -> usize {
    let mut lo = 0usize;
    let mut hi = slice.len();
    while lo < hi {
        let mid = (lo + hi) / 2;
        if key < slice[mid] {
            hi = mid;
        } else {
            lo = mid + 1;
        }
    }
    lo
}

/// (sample, enabled) for the first YM2612's 0x2B writes.
pub fn _dac_enable_timeline(vgm: &VgmFile) -> Vec<(i64, bool)> {
    let mut out = Vec::new();
    for w in &vgm.writes {
        if w.chip == "ym2612"
            && w.chip_id == 0
            && (w.reg & 0x100) == 0
            && (w.reg & 0xFF) == 0x2B
        {
            out.push((w.sample, (w.val & 0x80) != 0));
        }
    }
    out
}

pub fn _dac_enabled_at(timeline: &[(i64, bool)], t: i64) -> bool {
    if timeline.is_empty() {
        return true;
    }
    // bisect_right on (sample, True): bool compares False < True.
    let key = (t, 1u8);
    let mut lo = 0usize;
    let mut hi = timeline.len();
    while lo < hi {
        let mid = (lo + hi) / 2;
        let item = (timeline[mid].0, timeline[mid].1 as u8);
        if key < item {
            hi = mid;
        } else {
            lo = mid + 1;
        }
    }
    if lo > 0 { timeline[lo - 1].1 } else { false }
}

pub fn _clamp_rate(rate: f64, stats: &mut DacStats) -> i32 {
    let mut value = py_int(py_round(rate)) as i32;
    if value > DAC_RATE_MAX {
        value = DAC_RATE_MAX;
        stats.bump1("dac_rate_clamped");
    } else if value < DAC_RATE_MIN {
        value = DAC_RATE_MIN;
        stats.bump1("dac_rate_clamped");
    }
    value
}

/// (sample, signed byte, wait) for every audible DAC word, in order.
///
/// Old bank plays carry their wait nibble.  Direct 0x2A writes have no
/// explicit wait, so their hold is the time to the next DAC word, capped at
/// `DAC_GAP` (a longer gap is a new run).  Pointer changes are folded into
/// the bank address and must not break the stream by themselves.
///
/// Writes while 0x2B bit 7 is clear are inaudible; the cursor still advances
/// so the next enabled read lands at the source address.
pub fn _byte_events(
    vgm: &VgmFile,
    stats: &mut DacStats,
    enable_tl: &[(i64, bool)],
) -> Vec<(i64, i32, i64)> {
    let bank = &vgm.dac_data;
    let signed = _signed_bank(bank);
    let mut words: Vec<[i64; 4]> = Vec::new();
    let mut cursor: i64 = 0;
    let mut pos: i64 = 0;
    for &(t, kind, value) in &vgm.dac_events {
        if kind == 0 {
            cursor = value.max(0);
            pos = 0;
            continue;
        }
        let byte: i64;
        if kind == 1 {
            let addr = cursor + pos;
            pos += 1;
            if addr >= bank.len() as i64 {
                stats.bump1("dac_bank_overrun");
                continue;
            }
            byte = signed[addr as usize] as i64;
        } else if kind == 2 {
            byte = (value - 0x80) & 0xFF;
        } else {
            continue;
        }
        if !_dac_enabled_at(enable_tl, t) {
            stats.bump1("dac_disabled_bytes");
            if byte != 0 {
                stats.bump1("dac_disabled_audible");
            }
            continue;
        }
        let wait = if kind == 1 { value & 0x0F } else { 0 };
        words.push([t, byte, wait, kind as i64]);
    }
    for i in 0..words.len() {
        if words[i][3] != 2 {
            continue;
        }
        let nxt = if i + 1 < words.len() {
            words[i + 1][0]
        } else {
            words[i][0]
        };
        words[i][2] = (nxt - words[i][0]).clamp(0, DAC_GAP);
    }
    words
        .iter()
        .map(|w| (w[0], w[1] as i32, w[2]))
        .collect()
}

pub fn _add_sample(
    samples: &mut Vec<DacSample>,
    by_key: &mut HashMap<(Vec<u8>, i32), i32>,
    data: Vec<u8>,
    rate: i32,
    onset: i64,
) -> i32 {
    let key = (data.clone(), rate);
    if let Some(&idx) = by_key.get(&key) {
        return idx;
    }
    let idx = samples.len() as i32;
    samples.push(DacSample {
        start: -1,
        length: data.len() as i64,
        data,
        c4_rate: rate,
        onset,
    });
    by_key.insert(key, idx);
    idx
}

/// Explicit per-run slice representation for editing.
pub fn _collect_slices(vgm: &VgmFile) -> (Vec<DacSample>, Vec<DacPlay>, Vec<String>, DacStats) {
    if vgm.dac_events.is_empty()
        && vgm.dac_block_plays.is_empty()
        && vgm.dac_stream_plays.is_empty()
    {
        return (Vec::new(), Vec::new(), Vec::new(), DacStats::new());
    }

    let mut stats = DacStats::new();
    let mut warnings: Vec<String> = Vec::new();
    let pan_vec = if _fm6_is_used(vgm) {
        None
    } else {
        Some(_pan_timeline(vgm))
    };
    let pan_timeline = pan_vec.as_deref();
    let enable_tl = _dac_enable_timeline(vgm);
    let words = _byte_events(vgm, &mut stats, &enable_tl);
    let mut samples: Vec<DacSample> = Vec::new();
    let mut by_key: HashMap<(Vec<u8>, i32), i32> = HashMap::new();
    let mut plays: Vec<DacPlay> = Vec::new();

    let mut run: Vec<(i64, i32, i64)> = Vec::new();

    fn flush(
        run: &mut Vec<(i64, i32, i64)>,
        samples: &mut Vec<DacSample>,
        by_key: &mut HashMap<(Vec<u8>, i32), i32>,
        plays: &mut Vec<DacPlay>,
        stats: &mut DacStats,
        pan_timeline: Option<&[(i64, i64)]>,
    ) {
        if run.is_empty() {
            return;
        }
        let onset = run[0].0;
        let last = run[run.len() - 1].0;
        let span = last - onset;
        let mut data: Vec<u8> = run.iter().map(|v| v.1 as u8).collect();
        let count = data.len() as i64;
        stats.bump1("dac_runs");
        stats.bump("dac_source_bytes", count);
        let mut collapsed = count > 1 && data.iter().all(|&b| b == data[0]);
        let rate: i32;
        if collapsed {
            data.truncate(1);
            rate = VGM_RATE as i32;
            stats.bump("dac_collapsed_bytes", count);
        } else if count == 1 {
            rate = VGM_RATE as i32;
        } else if span > 0 {
            rate = _clamp_rate(VGM_RATE as f64 * (count - 1) as f64 / span as f64, stats);
        } else {
            data = vec![data[data.len() - 1]];
            rate = VGM_RATE as i32;
            collapsed = true;
        }
        let idx = _add_sample(samples, by_key, data, rate, onset);
        let pan = _pan_at(pan_timeline, onset);
        plays.push(DacPlay::with_flags(
            onset, idx, pan, span, collapsed, false, false, None,
        ));
        run.clear();
    }

    let mut i = 0usize;
    while i < words.len() {
        let (t, byte, wait) = words[i];
        let mut j = i + 1;
        while j < words.len() && words[j].0 == t {
            j += 1;
        }
        if j - i > 1 {
            // Same-time burst: only the final value sounds.
            if !run.is_empty() {
                flush(
                    &mut run, &mut samples, &mut by_key, &mut plays, &mut stats, pan_timeline,
                );
            }
            let kept = words[j - 1];
            let same = words[i..j].iter().all(|w| w.1 == words[i].1);
            if same {
                stats.bump("dac_collapsed_bytes", (j - i) as i64);
            } else {
                stats.bump1("dac_same_time_runs");
                stats.bump("dac_same_time_bytes", (j - i) as i64);
                stats.bump("dac_same_time_dropped", (j - i - 1) as i64);
            }
            stats.bump("dac_source_bytes", (j - i - 1) as i64);
            run = vec![kept];
            flush(
                &mut run, &mut samples, &mut by_key, &mut plays, &mut stats, pan_timeline,
            );
            i = j;
            continue;
        }
        if !run.is_empty() && t > run[run.len() - 1].0 + run[run.len() - 1].2 {
            flush(
                &mut run, &mut samples, &mut by_key, &mut plays, &mut stats, pan_timeline,
            );
        }
        run.push((t, byte, wait));
        i += 1;
    }
    flush(
        &mut run, &mut samples, &mut by_key, &mut plays, &mut stats, pan_timeline,
    );

    // Modern block plays: each 0x95 supplies one slice, cut by the next play
    // because starting a stream stops the previous one.
    for (t, block, rate, stop) in _block_play_spans(vgm) {
        if !(0 <= block && block < vgm.dac_blocks.len() as i64) {
            continue;
        }
        if !_dac_enabled_at(&enable_tl, t) {
            stats.bump1("dac_disabled_bytes");
            continue;
        }
        let mut body = _signed_bank(&vgm.dac_blocks[block as usize]);
        if body.is_empty() {
            continue;
        }
        let rate = _clamp_rate(rate as f64, &mut stats);
        if let Some(stop) = stop {
            let cut = py_int(
                (((stop - t).max(0) as f64 * rate as f64) / VGM_RATE as f64).ceil(),
            );
            let cut = if cut < 0 { 0 } else { cut as usize };
            body.truncate(body.len().min(cut));
            if body.is_empty() {
                continue;
            }
        }
        let span = py_int(py_round(body.len() as f64 * VGM_RATE as f64 / rate as f64));
        let idx = _add_sample(&mut samples, &mut by_key, body, rate, t);
        let pan = _pan_at(pan_timeline, t);
        plays.push(DacPlay::with_flags(
            t, idx, pan, span, false, false, false, None,
        ));
        stats.bump1("dac_block_plays");
    }

    // Stream starts (0x93): each one is a contiguous slice of the PCM bank.
    for play in &vgm.dac_stream_plays {
        if !_stream_is_dac(vgm, play, &mut stats) {
            continue;
        }
        let body = _stream_body(vgm, play);
        if body.is_empty() {
            continue;
        }
        if !_dac_enabled_at(&enable_tl, play.sample) {
            stats.bump1("dac_disabled_bytes");
            continue;
        }
        let rate = _clamp_rate(
            if play.rate != 0 {
                play.rate as f64
            } else {
                VGM_RATE as f64
            },
            &mut stats,
        );
        let span = py_int(py_round(body.len() as f64 * VGM_RATE as f64 / rate as f64));
        let idx = _add_sample(&mut samples, &mut by_key, body, rate, play.sample);
        let pan = _pan_at(pan_timeline, play.sample);
        plays.push(DacPlay::with_flags(
            play.sample, idx, pan, span, false, false, false, None,
        ));
        stats.bump1("dac_stream_plays");
    }

    plays.sort_by(|a, b| a.onset.cmp(&b.onset));
    stats.set_int("dac_unique_samples", samples.len() as i64);
    stats.set_int(
        "dac_sample_bytes",
        samples.iter().map(|s| s.data.len() as i64).sum(),
    );

    if stats.int("dac_disabled_bytes") != 0 && plays.is_empty() {
        warnings.push(format!(
            "{} YM2612 DAC writes while the DAC is disabled (0x2B bit 7 clear); no DAC audio",
            stats.int("dac_disabled_bytes")
        ));
    } else if stats.int("dac_disabled_audible") != 0 {
        warnings.push(format!(
            "{} non-silent YM2612 DAC writes while the DAC is disabled; ignored",
            stats.int("dac_disabled_audible")
        ));
    }
    if stats.int("dac_rate_clamped") != 0 {
        warnings.push(format!(
            "{} YM2612 DAC runs clamped to {} Hz; their bytes are preserved but the timing is longer than the source",
            stats.int("dac_rate_clamped"),
            DAC_RATE_MAX
        ));
    }
    if stats.int("dac_same_time_runs") != 0 {
        warnings.push(format!(
            "{} YM2612 DAC bursts ({} bytes) share one VGM sample; only the final value is kept because the intermediate writes cannot be timed at VGM resolution",
            stats.int("dac_same_time_runs"),
            stats.int("dac_same_time_bytes")
        ));
    }
    if stats.int("dac_bank_overrun") != 0 {
        warnings.push(format!(
            "{} YM2612 DAC reads ran past the PCM bank",
            stats.int("dac_bank_overrun")
        ));
    }
    (samples, plays, warnings, stats)
}

/// Block plays with the next play's onset as their stop time.
///
/// A YM2612 DAC stream plays one data block at a time: the next `0x95`
/// restarts it, so the current block is cut there.  Expanding every block to
/// its full length instead leaves two copies of the same (or different) PCM
/// running at once; the later word at each frame wins, which garbles the
/// waveform.  Drivers that repeat a long block before it ends rely on this
/// cut (SA2's Leaf Forest intro does exactly that).
pub fn _block_play_spans(vgm: &VgmFile) -> Vec<(i64, i64, i64, Option<i64>)> {
    let mut plays = vgm.dac_block_plays.clone();
    plays.sort_by(|a, b| a.0.cmp(&b.0));
    let mut out = Vec::new();
    for (i, &(t, block, rate)) in plays.iter().enumerate() {
        let stop = if i + 1 < plays.len() {
            Some(plays[i + 1].0)
        } else {
            None
        };
        out.push((t, block, rate, stop));
    }
    out
}

/// Canonical words with block plays expanded, same-time groups reduced.
pub fn _packed_words(
    vgm: &VgmFile,
    stats: &mut DacStats,
    enable_tl: &[(i64, bool)],
) -> Vec<(f64, i32, i64)> {
    let mut words: Vec<(f64, i32, i64)> = _byte_events(vgm, stats, enable_tl)
        .into_iter()
        .map(|(t, byte, wait)| (t as f64, byte, wait))
        .collect();
    for (t, block, rate, stop) in _block_play_spans(vgm) {
        if !(0 <= block && block < vgm.dac_blocks.len() as i64) {
            continue;
        }
        if !_dac_enabled_at(enable_tl, t) {
            stats.bump1("dac_disabled_bytes");
            continue;
        }
        let body = _signed_bank(&vgm.dac_blocks[block as usize]);
        if body.is_empty() {
            continue;
        }
        let play_rate = if rate > 0 { rate } else { VGM_RATE };
        let step = VGM_RATE as f64 / play_rate as f64;
        let mut count = body.len();
        if let Some(stop) = stop {
            let cut = py_int(
                (((stop - t).max(0) as f64 * play_rate as f64) / VGM_RATE as f64).ceil(),
            );
            count = count.min(if cut < 0 { 0 } else { cut as usize });
        }
        for (i, &byte) in body[..count].iter().enumerate() {
            words.push((t as f64 + i as f64 * step, byte as i32, 0));
        }
        stats.bump1("dac_block_plays");
        if count < body.len() {
            stats.bump1("dac_block_plays_cut");
        }
    }
    if words.is_empty() {
        return Vec::new();
    }
    words.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
    let mut reduced: Vec<(f64, i32, i64)> = Vec::new();
    let mut i = 0usize;
    while i < words.len() {
        let t = words[i].0;
        let mut j = i + 1;
        while j < words.len() && words[j].0 == t {
            j += 1;
        }
        let group = &words[i..j];
        let kept = group[group.len() - 1];
        if group.len() > 1 {
            let same = group.iter().all(|w| w.1 == group[0].1);
            if same {
                stats.bump("dac_collapsed_bytes", group.len() as i64);
            } else {
                stats.bump1("dac_same_time_runs");
                stats.bump("dac_same_time_bytes", group.len() as i64);
                stats.bump("dac_same_time_dropped", (group.len() - 1) as i64);
            }
        }
        reduced.push(kept);
        i = j;
    }
    reduced
}

/// Drop samples that are silence, flat DC, or below the audible peak.
pub fn _prune_quiet_plays(
    samples: Vec<DacSample>,
    plays: Vec<DacPlay>,
    stats: &mut DacStats,
) -> (Vec<DacSample>, Vec<DacPlay>) {
    let keep: Vec<usize> = samples
        .iter()
        .enumerate()
        .filter(|(_, smp)| _audible_body(&smp.data))
        .map(|(i, _)| i)
        .collect();
    if keep.len() == samples.len() {
        return (samples, plays);
    }
    let mut remap: HashMap<usize, i32> = HashMap::new();
    let mut compact: Vec<DacSample> = Vec::new();
    for &i in &keep {
        remap.insert(i, compact.len() as i32);
        compact.push(samples[i].clone());
    }
    stats.set_int("dac_quiet_samples_removed", (samples.len() - keep.len()) as i64);
    let plays = plays
        .into_iter()
        .filter(|p| remap.contains_key(&(p.sample as usize)))
        .map(|p| {
            DacPlay::with_flags(
                p.onset,
                remap[&(p.sample as usize)],
                p.pan,
                p.span,
                p.collapsed,
                p.source,
                p.fresh,
                p.hold,
            )
        })
        .collect();
    (compact, plays)
}

/// Split a word stream at rests and sharp attacks.
///
/// A silent hold of ``silence`` VGM samples is a rest between hits.  A jump
/// out of a longer near-silent stretch is an attack too, so a driver that
/// sends a DC level instead of an exact zero still starts a new body.  A
/// short dip inside a waveform does not split it.
pub fn _attack_segments(
    words: &[(f64, i32, i64)],
    silence: i64,
) -> Vec<Vec<(f64, i32, i64)>> {
    let mut segments: Vec<Vec<(f64, i32, i64)>> = Vec::new();
    let mut current: Vec<(f64, i32, i64)> = Vec::new();
    let mut zero_start: Option<f64> = None;
    let mut quiet_start: Option<f64> = None;
    for &word in words {
        let (t, byte, _wait) = word;
        let amplitude = if byte < 128 { byte } else { byte - 256 }.abs();
        let mut split = false;
        if !current.is_empty() && t - current[current.len() - 1].0 >= silence as f64 {
            split = true;
        }
        if !current.is_empty() && byte != 0 {
            if let Some(zero_start) = zero_start {
                split = split || t - zero_start >= silence as f64;
            }
        }
        if !split && !current.is_empty() && amplitude > DAC_ATTACK_JUMP {
            if let Some(quiet_start) = quiet_start {
                split = t - quiet_start >= DAC_ATTACK_QUIET_SAMPLES as f64;
            }
        }
        if split {
            segments.push(std::mem::take(&mut current));
        }
        if byte == 0 {
            if zero_start.is_none() {
                zero_start = Some(t);
            }
        } else {
            zero_start = None;
        }
        if amplitude <= DAC_ATTACK_QUIET_PEAK {
            if quiet_start.is_none() {
                quiet_start = Some(t);
            }
        } else {
            quiet_start = None;
        }
        current.push(word);
    }
    if !current.is_empty() {
        segments.push(current);
    }
    segments
}

/// A pan change inside a candidate starts a new playable body.
pub fn _split_pan_segments(
    segments: Vec<Vec<(f64, i32, i64)>>,
    pan_timeline: Option<&[(i64, i64)]>,
) -> Vec<Vec<(f64, i32, i64)>> {
    let mut out: Vec<Vec<(f64, i32, i64)>> = Vec::new();
    for segment in segments {
        if segment.is_empty() {
            continue;
        }
        let mut current: Vec<(f64, i32, i64)> = Vec::new();
        let mut last_pan = _pan_at(pan_timeline, py_int(segment[0].0));
        for &word in &segment {
            let pan = _pan_at(pan_timeline, py_int(word.0));
            if !current.is_empty() && pan != last_pan {
                out.push(std::mem::take(&mut current));
            }
            current.push(word);
            last_pan = pan;
        }
        if !current.is_empty() {
            out.push(current);
        }
    }
    out
}

/// Store a short body once when a longer same-rate body starts with it.
pub fn _share_prefix_samples(
    samples: Vec<DacSample>,
    plays: Vec<DacPlay>,
    stats: &mut DacStats,
) -> (Vec<DacSample>, Vec<DacPlay>) {
    if samples.len() < 2 {
        return (samples, plays);
    }
    let mut by_head: IndexMap<(i32, u8), Vec<usize>> = IndexMap::new();
    for (i, smp) in samples.iter().enumerate() {
        if smp.data.is_empty() {
            continue;
        }
        by_head
            .entry((smp.c4_rate, smp.data[0]))
            .or_default()
            .push(i);
    }
    let mut remap: Vec<usize> = (0..samples.len()).collect();
    let mut reused = 0i64;
    for indexes in by_head.values() {
        if indexes.len() < 2 {
            continue;
        }
        let mut indexes = indexes.clone();
        indexes.sort_by(|&a, &b| samples[b].data.len().cmp(&samples[a].data.len()));
        for (pos, &i) in indexes.iter().enumerate() {
            for &j in &indexes[..pos] {
                if !samples[j].data.starts_with(&samples[i].data) {
                    continue;
                }
                remap[i] = j;
                reused += 1;
                break;
            }
        }
    }
    if reused == 0 {
        return (samples, plays);
    }
    // A prefix can itself have been merged into a still longer body.
    for i in 0..remap.len() {
        while remap[i] != remap[remap[i]] {
            remap[i] = remap[remap[i]];
        }
    }
    let new_plays: Vec<DacPlay> = plays
        .iter()
        .map(|p| {
            DacPlay::with_flags(
                p.onset,
                remap[p.sample as usize] as i32,
                p.pan,
                p.span,
                p.collapsed,
                p.source,
                p.fresh,
                p.hold,
            )
        })
        .collect();
    let used: HashSet<i32> = new_plays.iter().map(|p| p.sample).collect();
    let mut compact: Vec<DacSample> = Vec::new();
    let mut compact_map: HashMap<usize, i32> = HashMap::new();
    for (i, smp) in samples.iter().enumerate() {
        if !used.contains(&(i as i32)) {
            continue;
        }
        compact_map.insert(i, compact.len() as i32);
        compact.push(smp.clone());
    }
    stats.set_int("dac_attack_prefix_reused", reused);
    let rebuilt = new_plays
        .into_iter()
        .map(|p| {
            DacPlay::with_flags(
                p.onset,
                compact_map[&(p.sample as usize)],
                p.pan,
                p.span,
                p.collapsed,
                p.source,
                p.fresh,
                p.hold,
            )
        })
        .collect();
    (compact, rebuilt)
}

/// Rate-exact bodies split at attacks, with exact duplicates shared.
pub fn _attack_sample_set(
    words: &[(f64, i32, i64)],
    pan_timeline: Option<&[(i64, i64)]>,
    silence: i64,
) -> (Vec<DacSample>, Vec<DacPlay>, DacStats) {
    let mut local = DacStats::new();
    let segments = _split_pan_segments(_attack_segments(words, silence), pan_timeline);
    let mut samples: Vec<DacSample> = Vec::new();
    let mut by_key: HashMap<(Vec<u8>, i32), i32> = HashMap::new();
    let mut plays: Vec<DacPlay> = Vec::new();
    for segment in segments {
        let mut start_i = 0usize;
        let mut end_i = segment.len();
        while start_i < end_i && segment[start_i].1 == 0 {
            start_i += 1;
        }
        while end_i > start_i && segment[end_i - 1].1 == 0 {
            end_i -= 1;
        }
        let segment = &segment[start_i..end_i];
        if segment.is_empty() {
            local.bump1("dac_attack_silent_segments");
            continue;
        }
        let data_bytes: Vec<u8> = segment.iter().map(|w| w.1 as u8).collect();
        if !_audible_body(&data_bytes) {
            local.bump1("dac_attack_quiet_segments");
            continue;
        }
        let times: Vec<f64> = segment.iter().map(|w| w.0).collect();
        let mut deltas: Vec<f64> = Vec::new();
        for pair in times.windows(2) {
            if pair[1] > pair[0] {
                deltas.push(pair[1] - pair[0]);
            }
        }
        let min_wait = deltas.iter().copied().fold(None, |acc: Option<f64>, v| {
            Some(match acc {
                None => v,
                Some(a) => a.min(v),
            })
        });
        let min_wait = min_wait.unwrap_or(1.0);
        let rate = DAC_RATE_MIN.max(DAC_RATE_MAX.min(py_int(py_round(
            VGM_RATE as f64 / min_wait,
        )) as i32));
        let body = _frames_from_times(segment, rate, Some(&mut local));
        if (body.len() as i64) < DAC_ATTACK_MIN_FRAMES {
            local.bump1("dac_attack_short_segments");
            continue;
        }
        let start = py_int(py_round(times[0]));
        let idx = _add_sample(&mut samples, &mut by_key, body, rate, start);
        let span = py_int(py_round(times[times.len() - 1] - times[0]));
        let pan = _pan_at(pan_timeline, start);
        plays.push(DacPlay::with_flags(
            start, idx, pan, span, false, true, false, None,
        ));
        local.bump1("dac_attack_segments");
    }
    let (samples, plays) = _share_prefix_samples(samples, plays, &mut local);
    local.set_int("dac_attack_samples", samples.len() as i64);
    local.set_int(
        "dac_attack_bytes",
        samples.iter().map(|s| s.data.len() as i64).sum(),
    );
    (samples, plays, local)
}

/// Join the least-significant tail bodies into the final sample.
///
/// This is the last resort, the same rule the old chunk fallback used: the
/// first ``budget - 1`` distinct bodies stay exact and the rest play in order
/// out of one final body.  The initial attack split already keeps the common
/// hits separate; this only runs when the candidate count itself overruns.
pub fn _merge_attack_tail(
    samples: Vec<DacSample>,
    plays: Vec<DacPlay>,
    budget: usize,
    stats: &mut DacStats,
) -> (Vec<DacSample>, Vec<DacPlay>) {
    if budget < 2 || samples.len() <= budget {
        return (samples, plays);
    }
    let mut order: Vec<i32> = Vec::new();
    let mut seen: HashSet<i32> = HashSet::new();
    let mut sorted_plays: Vec<&DacPlay> = plays.iter().collect();
    sorted_plays.sort_by(|a, b| a.onset.cmp(&b.onset));
    for p in &sorted_plays {
        if !seen.contains(&p.sample) {
            seen.insert(p.sample);
            order.push(p.sample);
        }
    }
    if order.len() <= budget {
        return (samples, plays);
    }
    let keep: HashSet<i32> = order[..budget - 1].iter().copied().collect();
    let tail: Vec<i32> = order
        .iter()
        .copied()
        .filter(|i| !keep.contains(i))
        .collect();
    let mut tail_plays: Vec<&DacPlay> = plays
        .iter()
        .filter(|p| !keep.contains(&p.sample))
        .collect();
    if tail_plays.is_empty() {
        return (samples, plays);
    }
    let mut rates: HashSet<i32> = HashSet::new();
    for &i in &tail {
        rates.insert(samples[i as usize].c4_rate);
    }
    if rates.len() != 1 {
        // Bodies at different rates cannot share one stored rate without
        // resampling; let the caller trim instead of detuning the tail.
        return (samples, plays);
    }
    let rate = *rates.iter().next().unwrap();
    tail_plays.sort_by(|a, b| a.onset.cmp(&b.onset));
    let first = tail_plays[0];
    let start = first.onset;
    let mut body: Vec<u8> = Vec::new();
    let mut ordered_tail: Vec<&DacPlay> = tail_plays.clone();
    ordered_tail.sort_by(|a, b| (a.onset, a.sample).cmp(&(b.onset, b.sample)));
    for p in &ordered_tail {
        let target = py_int(py_round(
            (p.onset - start) as f64 * rate as f64 / VGM_RATE as f64,
        ));
        if target > body.len() as i64 {
            // The attack splitter put a rest here; hold silence between the
            // joined tail hits instead of repeating the previous byte.
            body.extend(std::iter::repeat(0u8).take((target - body.len() as i64) as usize));
        } else if target < body.len() as i64 {
            body.truncate(target.max(0) as usize);
        }
        body.extend_from_slice(&samples[p.sample as usize].data);
    }
    let end = tail_plays
        .iter()
        .map(|p| p.onset + p.span)
        .max()
        .unwrap_or(0);

    let mut remap: HashMap<i32, i32> = HashMap::new();
    let mut compact: Vec<DacSample> = Vec::new();
    for &i in &order {
        if !keep.contains(&i) {
            continue;
        }
        remap.insert(i, compact.len() as i32);
        compact.push(samples[i as usize].clone());
    }
    let tail_idx = compact.len() as i32;
    compact.push(DacSample {
        start: -1,
        length: body.len() as i64,
        data: body,
        c4_rate: rate,
        onset: start,
    });
    let mut new_plays: Vec<DacPlay> = plays
        .iter()
        .filter(|p| keep.contains(&p.sample))
        .map(|p| {
            DacPlay::with_flags(
                p.onset,
                remap[&p.sample],
                p.pan,
                p.span,
                p.collapsed,
                p.source,
                p.fresh,
                p.hold,
            )
        })
        .collect();
    new_plays.push(DacPlay::with_flags(
        start,
        tail_idx,
        first.pan,
        end - start,
        false,
        true,
        first.fresh,
        first.hold,
    ));
    stats.set_int("dac_release_merged_tail", tail.len() as i64);
    stats.set_int("dac_attack_tail_plays", tail_plays.len() as i64);
    (compact, new_plays)
}

/// Attack-split fallback.  Widen the rest threshold a little, then fit.
pub fn _attack_chunks(
    words: &[(f64, i32, i64)],
    pan_timeline: Option<&[(i64, i64)]>,
    release_budget: usize,
) -> (Vec<DacSample>, Vec<DacPlay>, DacStats) {
    let mut silence = DAC_ATTACK_SILENCE;
    let mut best: Option<(Vec<DacSample>, Vec<DacPlay>, DacStats)> = None;
    while silence <= DAC_ATTACK_SILENCE.max(DAC_RUN_GAP) {
        let (samples, plays, local) = _attack_sample_set(words, pan_timeline, silence);
        if best.is_none() || samples.len() < best.as_ref().unwrap().0.len() {
            best = Some((samples, plays, local));
        }
        if best.as_ref().unwrap().0.len() <= release_budget {
            best.as_mut().unwrap().2.set_int("dac_attack_silence", silence);
            return best.unwrap();
        }
        silence *= 2;
    }
    let (mut samples, mut plays, mut local) = best.expect("at least one attack pass");
    // Python reads the module global here, which the tests patch; use the
    // budget threaded through the collector so the seam works.
    let merged = _merge_attack_tail(
        std::mem::take(&mut samples),
        std::mem::take(&mut plays),
        release_budget,
        &mut local,
    );
    samples = merged.0;
    plays = merged.1;
    local.set_int("dac_attack_silence", silence);
    (samples, plays, local)
}

/// Lay source bytes on a ``rate`` frame grid, holding the last value.
pub fn _frames_from_times(
    selected: &[(f64, i32, i64)],
    rate: i32,
    mut stats: Option<&mut DacStats>,
) -> Vec<u8> {
    let mut out: Vec<u8> = Vec::new();
    let mut previous: u8 = 0x00;
    let start = selected[0].0;
    for &(t, byte, _wait) in selected {
        let frame = py_int(py_round((t - start) * rate as f64 / VGM_RATE as f64));
        let byte = byte as u8;
        if frame < out.len() as i64 {
            out[frame as usize] = byte;
            if let Some(stats) = stats.as_deref_mut() {
                stats.bump1("dac_frame_collisions");
            }
            previous = byte;
            continue;
        }
        if frame > out.len() as i64 {
            out.extend(std::iter::repeat(previous).take((frame - out.len() as i64) as usize));
        }
        out.push(byte);
        previous = byte;
    }
    out
}

/// One bank read grouped by the `0xE0` pointer, with its source offset.
#[derive(Clone, Debug, Default)]
pub struct SourceRun {
    pub t: i64,
    pub off: i64,
    pub bytes: Vec<u8>,
    pub times: Vec<i64>,
    pub fresh: bool,
    pub cluster: Option<i32>,
    pub pitch: i32,
}

/// Bank reads grouped by the `0xE0` pointer, with their source offsets.
///
/// Each pointer change starts a run. A run keeps reading forward from that
/// offset until the driver moves the pointer, so a shorter play of the same
/// data is a prefix of the longer run. A `$2B` state change ends the run as
/// well, because the driver resumes at the next bank address afterwards.
/// Returns `None` when the log writes the DAC directly (`0x2A`), which has no
/// bank offset to reuse.
pub fn _source_runs(
    vgm: &VgmFile,
    stats: &mut DacStats,
    enable_tl: &[(i64, bool)],
) -> Option<Vec<SourceRun>> {
    let bank = &vgm.dac_data;
    let signed = _signed_bank(bank);
    let mut local = DacStats::new();
    let mut runs: Vec<SourceRun> = Vec::new();
    let mut cursor: i64 = 0;
    let mut pos: i64 = 0;
    let mut current: Option<SourceRun> = None;
    let mut saw_pointer = false;
    let mut fresh_next = false;
    // A 0x2B transition ends a bank run even when no bytes are written while
    // the DAC is off. The driver resumes reading at the next bank address, so
    // the post-enable bytes are a new source segment, not a held gap inside
    // the previous one.
    let mut change_times: Vec<i64> = Vec::new();
    let mut previous_state: Option<bool> = None;
    for &(change_t, change_en) in enable_tl {
        if previous_state != Some(change_en) {
            change_times.push(change_t);
            previous_state = Some(change_en);
        }
    }
    let mut change_i = 0usize;

    fn flush(current: &mut Option<SourceRun>, runs: &mut Vec<SourceRun>) {
        if let Some(run) = current.take() {
            if !run.bytes.is_empty() {
                runs.push(run);
            }
        }
    }

    for &(t, kind, value) in &vgm.dac_events {
        while change_i < change_times.len() && t >= change_times[change_i] {
            // The state at this instant belongs to the new run.
            flush(&mut current, &mut runs);
            fresh_next = true;
            change_i += 1;
        }
        if kind == 0 {
            flush(&mut current, &mut runs);
            cursor = value.max(0);
            pos = 0;
            saw_pointer = true;
            fresh_next = false;
            continue;
        }
        if kind != 1 {
            // A direct 0x2A stream has no bank offset to reuse.
            return None;
        }
        let addr = cursor + pos;
        pos += 1;
        if addr >= bank.len() as i64 {
            local.bump1("dac_bank_overrun");
            continue;
        }
        if !_dac_enabled_at(enable_tl, t) {
            // Inaudible bytes break the bank run: playback resumes at this
            // later address, which is a new source segment.
            flush(&mut current, &mut runs);
            fresh_next = true;
            local.bump1("dac_disabled_bytes");
            if signed[addr as usize] != 0 {
                local.bump1("dac_disabled_audible");
            }
            continue;
        }
        if current.is_none() {
            current = Some(SourceRun {
                t,
                off: addr,
                bytes: Vec::new(),
                times: Vec::new(),
                fresh: fresh_next,
                cluster: None,
                pitch: 0,
            });
            fresh_next = false;
        }
        let run = current.as_mut().unwrap();
        run.bytes.push(signed[addr as usize]);
        run.times.push(t);
    }
    flush(&mut current, &mut runs);

    if !saw_pointer || runs.is_empty() {
        return None;
    }
    stats.merge(&local);
    Some(runs)
}

/// Most common byte cadence of a run, in bytes per second.
///
/// A driver's regular cadence is the wait it uses most often; one short
/// interval caused by a wait-command boundary must not set the whole run's
/// pitch.  Ties go to the shorter wait, which is the nominal 4-wait in a
/// 4/5-alternating jitter pair.
pub fn _source_pitch(run: &SourceRun) -> i32 {
    let mut waits: Vec<i64> = Vec::new();
    for pair in run.times.windows(2) {
        if pair[1] > pair[0] {
            waits.push(pair[1] - pair[0]);
        }
    }
    if waits.is_empty() {
        return VGM_RATE as i32;
    }
    let mut counts: HashMap<i64, i64> = HashMap::new();
    for wait in waits {
        *counts.entry(wait).or_insert(0) += 1;
    }
    let top = *counts.values().max().unwrap();
    let wait = counts
        .iter()
        .filter(|&(_, &n)| n == top)
        .map(|(&w, _)| w)
        .min()
        .unwrap();
    DAC_RATE_MIN.max(DAC_RATE_MAX.min(py_int(py_round(
        VGM_RATE as f64 / wait as f64,
    )) as i32))
}

/// Typical byte interval of a run, ignoring held-silence outliers.
///
/// Drivers quantise one nominal cadence to a mix of four- and five-sample
/// waits.  The trimmed mean keeps that jitter near 4.5 while a genuinely
/// repitched play lands on a different cadence.
pub fn _run_wait(run: &SourceRun) -> f64 {
    let mut waits: Vec<f64> = Vec::new();
    for pair in run.times.windows(2) {
        if pair[1] > pair[0] {
            waits.push((pair[1] - pair[0]) as f64);
        }
    }
    if waits.is_empty() {
        return 1.0;
    }
    let median = median_f64(&mut waits.clone());
    let cap = (median * 2.0).max(median + 3.0);
    let kept: Vec<f64> = waits.into_iter().filter(|&w| w <= cap).collect();
    if kept.is_empty() {
        median
    } else {
        kept.iter().sum::<f64>() / kept.len() as f64
    }
}

fn median_f64(values: &mut [f64]) -> f64 {
    values.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let n = values.len();
    if n == 0 {
        return 0.0;
    }
    if n % 2 == 1 {
        values[n / 2]
    } else {
        (values[n / 2 - 1] + values[n / 2]) / 2.0
    }
}

/// Split one offset's runs into pitch clusters by trimmed mean wait.
pub fn _wait_clusters(items: &[usize], runs: &[SourceRun]) -> Vec<Vec<usize>> {
    let mut ordered: Vec<usize> = items.to_vec();
    ordered.sort_by(|&a, &b| {
        _run_wait(&runs[a])
            .partial_cmp(&_run_wait(&runs[b]))
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    let mut clusters: Vec<Vec<usize>> = Vec::new();
    let mut current: Vec<usize> = Vec::new();
    let mut previous: Option<f64> = None;
    for &ri in &ordered {
        let wait = _run_wait(&runs[ri]);
        if !current.is_empty() {
            if let Some(previous) = previous {
                if wait - previous > DAC_WAIT_CLUSTER {
                    clusters.push(std::mem::take(&mut current));
                }
            }
        }
        current.push(ri);
        previous = Some(wait);
    }
    if !current.is_empty() {
        clusters.push(current);
    }
    clusters
}

/// Split one bank run at a long held-silence gap.
///
/// A driver can read from offset 0 through a kick into the next bank
/// sample after a long pause.  Keeping that as one body delays the next
/// sample and stores the pause; split at the gap and let the post part keep
/// its own bank offset and onset.  A held silence of ``DAC_RUN_GAP`` or
/// more is a rest between source segments, not sample data.
pub fn _split_silent_run(run: &SourceRun) -> Option<Vec<SourceRun>> {
    let times = &run.times;
    let values = &run.bytes;
    if times.len() < 2 || values.len() < 2 {
        return None;
    }
    let mut best_gap: i64 = 0;
    let mut best_i: i64 = -1;
    for i in 0..times.len() - 1 {
        if values[i] != 0 {
            continue;
        }
        let gap = times[i + 1] - times[i];
        if gap > best_gap {
            best_gap = gap;
            best_i = i as i64;
        }
    }
    if best_i < 0 || best_gap < DAC_RUN_GAP {
        return None;
    }
    let best_i = best_i as usize;
    let mut pre = run.clone();
    pre.bytes = values[..=best_i].to_vec();
    pre.times = times[..=best_i].to_vec();
    pre.pitch = _source_pitch(&pre);
    let mut post = run.clone();
    post.t = times[best_i + 1];
    post.bytes = values[best_i + 1..].to_vec();
    post.times = times[best_i + 1..].to_vec();
    post.off = run.off + best_i as i64 + 1;
    post.pitch = _source_pitch(&post);
    post.fresh = false;
    let pieces: Vec<SourceRun> = vec![pre, post]
        .into_iter()
        .filter(|p| !p.bytes.is_empty())
        .collect();
    Some(pieces)
}

/// Apply `_split_silent_run` repeatedly, keeping source order.
pub fn _split_silent_runs(runs: Vec<SourceRun>, stats: &mut DacStats) -> Vec<SourceRun> {
    let mut out: Vec<SourceRun> = Vec::new();
    for run in runs {
        let mut pieces: Vec<SourceRun> = vec![run];
        let mut changed = true;
        while changed {
            changed = false;
            let mut expanded: Vec<SourceRun> = Vec::new();
            for piece in pieces {
                match _split_silent_run(&piece) {
                    None => expanded.push(piece),
                    Some(parts) => {
                        changed = true;
                        stats.bump1("dac_source_silence_splits");
                        expanded.extend(parts);
                    }
                }
            }
            pieces = expanded;
        }
        out.extend(pieces);
    }
    out
}

/// One sample per bank offset and pitch cluster, one play per run.
///
/// The longest read in a cluster is the stored body.  Every shorter read there
/// is a prefix of it: the next note or the explicit note-off cuts it, so a
/// one-frame hit and a full drum share one module sample.
///
/// Runs at one offset are grouped by their trimmed mean byte interval rather
/// than by exact cadence.  Drivers alternate four- and five-sample waits at
/// one nominal rate, so the trimmed mean keeps that jitter together.  A
/// genuinely repitched play moves the mean by more than ``DAC_WAIT_CLUSTER``
/// and gets its own sample, which lets the pattern play it at its real pitch.
///
/// Silent, flat-line and near-silent reads take no sample slot.  They are
/// rests between hits, and storing them as samples only burns the release
/// budget.  A dropped quiet run still leaves its last value on some plays so
/// a coalesced one-shot body keeps its real silence.
pub fn _collect_source(
    mut runs: Vec<SourceRun>,
    stats: &mut DacStats,
    pan_timeline: Option<&[(i64, i64)]>,
) -> (Vec<DacSample>, Vec<DacPlay>) {
    stats.set_int("dac_source_runs_unsplit", runs.len() as i64);
    runs = _split_silent_runs(runs, stats);
    stats.set_int(
        "dac_source_silent_runs",
        runs.iter().filter(|r| !r.bytes.iter().any(|&b| b != 0)).count() as i64,
    );
    let mut audible: Vec<usize> = Vec::new();
    let mut quiet_bytes = 0i64;
    for (i, run) in runs.iter().enumerate() {
        if _audible_body(&run.bytes) {
            audible.push(i);
            continue;
        }
        quiet_bytes += run.bytes.len() as i64;
    }
    stats.set_int("dac_source_quiet_runs", (runs.len() - audible.len()) as i64);
    stats.set_int("dac_source_quiet_bytes", quiet_bytes);
    if audible.is_empty() {
        return (Vec::new(), Vec::new());
    }

    // Map each audible run to the value the DAC holds after it.  When a quiet
    // run follows before the next audible one, that run wrote the hold (0 for
    // a real rest); otherwise the last audio byte stays on the line.
    let mut hold_after: Vec<Option<i32>> = vec![None; runs.len()];
    for &i in &audible {
        let value = runs[i].bytes.last().copied().unwrap_or(0) as i32;
        let mut value = value;
        if i + 1 < runs.len() {
            let nxt = &runs[i + 1];
            if !_audible_body(&nxt.bytes) && !nxt.bytes.is_empty() {
                value = nxt.bytes[nxt.bytes.len() - 1] as i32;
            }
        }
        hold_after[i] = Some(value);
    }

    let mut groups: IndexMap<i64, Vec<usize>> = IndexMap::new();
    for &ri in &audible {
        groups.entry(runs[ri].off).or_default().push(ri);
    }

    let mut samples: Vec<DacSample> = Vec::new();
    let mut sample_of: HashMap<(i64, i32), i32> = HashMap::new();
    for (off, items) in &groups {
        for (cluster_i, cluster) in _wait_clusters(items, &runs).iter().enumerate() {
            for &ri in cluster {
                runs[ri].cluster = Some(cluster_i as i32);
            }
            let mut rep = cluster[0];
            for &ri in &cluster[1..] {
                let a = (
                    runs[ri].bytes.len(),
                    runs[ri].times[runs[ri].times.len() - 1] - runs[ri].times[0],
                );
                let b = (
                    runs[rep].bytes.len(),
                    runs[rep].times[runs[rep].times.len() - 1] - runs[rep].times[0],
                );
                if a > b {
                    rep = ri;
                }
            }
            // Playback pitch follows the run's regular byte interval, not one
            // short gap inside it.  A single 3-sample wait in a 4-wait read
            // otherwise stretched the whole body to 14.7 kHz and every reuse
            // played a fourth too high; the most common interval ignores it.
            let rate = _source_pitch(&runs[rep]);
            let selected: Vec<(f64, i32, i64)> = runs[rep]
                .times
                .iter()
                .zip(runs[rep].bytes.iter())
                .map(|(&t, &b)| (t as f64, b as i32, 0))
                .collect();
            let body = _frames_from_times(&selected, rate, Some(&mut *stats));
            sample_of.insert((*off, cluster_i as i32), samples.len() as i32);
            samples.push(DacSample {
                start: *off,
                length: body.len() as i64,
                data: body,
                c4_rate: rate,
                onset: runs[rep].t,
            });
        }
    }

    let mut plays: Vec<DacPlay> = Vec::new();
    for &ri in &audible {
        let run = &runs[ri];
        let idx = sample_of[&(run.off, run.cluster.unwrap())];
        let pan = _pan_at(pan_timeline, run.t);
        plays.push(DacPlay::with_flags(
            run.t,
            idx,
            pan,
            run.times[run.times.len() - 1] - run.t,
            false,
            true,
            run.fresh,
            hold_after[ri],
        ));
    }
    plays.sort_by(|a, b| a.onset.cmp(&b.onset));

    let offsets: HashSet<i64> = audible.iter().map(|&ri| runs[ri].off).collect();
    stats.set_int("dac_source_offsets", offsets.len() as i64);
    stats.set_int("dac_source_runs", audible.len() as i64);
    stats.set_int("dac_source_groups", samples.len() as i64);
    stats.set_int(
        "dac_source_runs_reused",
        (audible.len() - samples.len()) as i64,
    );
    stats.set_int(
        "dac_source_bytes",
        audible.iter().map(|&ri| runs[ri].bytes.len() as i64).sum(),
    );
    stats.set_int(
        "dac_source_sample_bytes",
        samples.iter().map(|s| s.data.len() as i64).sum(),
    );
    (samples, plays)
}

/// Join consecutive runs that are each used exactly once.
///
/// A one-shot sequence of bank samples can be stored as one body and played
/// as-is: there is no later note that could need an earlier entry. Only
/// source plays whose sample is used once, with the same pan and rate, join.
pub fn _coalesce_singles(
    samples: Vec<DacSample>,
    plays: Vec<DacPlay>,
    stats: &mut DacStats,
) -> (Vec<DacSample>, Vec<DacPlay>) {
    if plays.len() < 2 {
        return (samples, plays);
    }
    let mut uses: HashMap<i32, i64> = HashMap::new();
    for p in &plays {
        *uses.entry(p.sample).or_insert(0) += 1;
    }
    let mut ordered: Vec<usize> = (0..plays.len()).collect();
    ordered.sort_by(|&a, &b| plays[a].onset.cmp(&plays[b].onset));
    let mut chains: Vec<Vec<usize>> = Vec::new();
    let mut current: Vec<usize> = Vec::new();

    fn flush(current: &mut Vec<usize>, chains: &mut Vec<Vec<usize>>) {
        if current.len() > 1 {
            chains.push(std::mem::take(current));
        } else {
            current.clear();
        }
    }

    for &pi in &ordered {
        let p = &plays[pi];
        let smp = &samples[p.sample as usize];
        if !p.source || uses.get(&p.sample).copied().unwrap_or(0) != 1 || p.collapsed {
            flush(&mut current, &mut chains);
            continue;
        }
        if p.fresh {
            // The DAC was disabled before this run; the gap is silence, not
            // a held value, so it cannot join the previous body.
            flush(&mut current, &mut chains);
        }
        if !current.is_empty() {
            let prev = &plays[current[current.len() - 1]];
            let psmp = &samples[prev.sample as usize];
            if p.pan != prev.pan || smp.c4_rate != psmp.c4_rate {
                flush(&mut current, &mut chains);
            }
        }
        current.push(pi);
    }
    flush(&mut current, &mut chains);
    if chains.is_empty() {
        return (samples, plays);
    }

    let mut new_samples = samples.clone();
    let chained: HashSet<usize> = chains.iter().flatten().copied().collect();
    let mut new_plays: Vec<DacPlay> = ordered
        .iter()
        .filter(|pi| !chained.contains(pi))
        .map(|&pi| plays[pi].clone())
        .collect();
    for chain in &chains {
        let first = &plays[chain[0]];
        let rate = samples[first.sample as usize].c4_rate;
        let start = first.onset;
        let last_play = &plays[chain[chain.len() - 1]];
        let end = last_play.onset + last_play.span;
        let mut body: Vec<u8> = Vec::new();
        let mut held: i32 = 0x00;
        for &pi in chain {
            let p = &plays[pi];
            let target = py_int(py_round(
                (p.onset - start) as f64 * rate as f64 / VGM_RATE as f64,
            ));
            if target > body.len() as i64 {
                let fill = held as u8;
                body.extend(std::iter::repeat(fill).take((target - body.len() as i64) as usize));
            }
            let smp = &samples[p.sample as usize];
            let frames = 1.max(py_int(py_round(
                p.span as f64 * rate as f64 / VGM_RATE as f64,
            )));
            let frames = frames.min(smp.data.len() as i64).max(0) as usize;
            let payload = &smp.data[..frames];
            if payload.is_empty() {
                continue;
            }
            if body.len() as i64 > target {
                let target = target as usize;
                let end_i = target + payload.len();
                if end_i <= body.len() {
                    body[target..end_i].copy_from_slice(payload);
                } else {
                    body.truncate(target);
                    body.extend_from_slice(payload);
                }
            } else {
                body.extend_from_slice(payload);
            }
            held = p.hold.unwrap_or(payload[payload.len() - 1] as i32);
        }
        if body.len() < 2 {
            continue;
        }
        let idx = new_samples.len() as i32;
        new_samples.push(DacSample {
            start: -1,
            length: body.len() as i64,
            data: body,
            c4_rate: rate,
            onset: start,
        });
        new_plays.push(DacPlay::with_flags(
            start,
            idx,
            first.pan,
            end - start,
            false,
            true,
            first.fresh,
            chain
                .last()
                .and_then(|&pi| plays[pi].hold),
        ));
        stats.bump1("dac_source_coalesced_chains");
        stats.bump("dac_source_coalesced_plays", chain.len() as i64);
        stats.bump("dac_source_coalesced_samples", (chain.len() - 1) as i64);
    }

    let used: HashSet<i32> = new_plays.iter().map(|p| p.sample).collect();
    let mut remap: HashMap<usize, i32> = HashMap::new();
    let mut compact: Vec<DacSample> = Vec::new();
    for (i, smp) in new_samples.iter().enumerate() {
        if used.contains(&(i as i32)) {
            remap.insert(i, compact.len() as i32);
            compact.push(smp.clone());
        }
    }
    let mut rebuilt: Vec<DacPlay> = new_plays
        .into_iter()
        .map(|p| {
            DacPlay::with_flags(
                p.onset,
                remap[&(p.sample as usize)],
                p.pan,
                p.span,
                p.collapsed,
                p.source,
                p.fresh,
                p.hold,
            )
        })
        .collect();
    rebuilt.sort_by(|a, b| a.onset.cmp(&b.onset));
    stats.set_int("dac_unique_samples", compact.len() as i64);
    (compact, rebuilt)
}

struct PhraseInfo {
    phrase: Vec<(f64, i32, i64)>,
    audio: Vec<usize>,
    first_i: usize,
    last_i: usize,
    tail_i: Option<usize>,
    span: f64,
    pans: Vec<i32>,
}

/// Turn per-byte DAC writes into release-capped rate-exact PCM chunks.
pub fn _chunk_words(
    words: &[(f64, i32, i64)],
    stats: &mut DacStats,
    pan_timeline: Option<&[(i64, i64)]>,
    release_budget: usize,
) -> (Vec<DacSample>, Vec<DacPlay>, i64, i64) {
    let mut phrases: Vec<Vec<(f64, i32, i64)>> = Vec::new();
    let mut current: Vec<(f64, i32, i64)> = Vec::new();
    for &word in words {
        if !current.is_empty() && word.0 - current[current.len() - 1].0 > DAC_RUN_GAP as f64 {
            phrases.push(std::mem::take(&mut current));
        }
        current.push(word);
    }
    if !current.is_empty() {
        phrases.push(current);
    }

    let mut phrase_info: Vec<PhraseInfo> = Vec::new();
    let mut trimmed = 0i64;
    let mut total_span = 0.0f64;
    for phrase in &phrases {
        let audio: Vec<usize> = phrase
            .iter()
            .enumerate()
            .filter(|(_, w)| w.1 != 0)
            .map(|(i, _)| i)
            .collect();
        if audio.is_empty() {
            stats.bump1("dac_silent_phrases");
            trimmed += phrase.len() as i64;
            continue;
        }
        let first_i = audio[0];
        let last_i = audio[audio.len() - 1];
        let mut tail_i: Option<usize> = None;
        for j in last_i + 1..phrase.len() {
            if phrase[j].1 == 0 {
                tail_i = Some(j);
                break;
            }
        }
        trimmed += first_i as i64
            + (phrase.len() - 1 - last_i - tail_i.is_some() as usize) as i64;
        let span = (phrase[last_i].0 - phrase[first_i].0).max(1.0);
        phrase_info.push(PhraseInfo {
            phrase: phrase.clone(),
            audio: audio.clone(),
            first_i,
            last_i,
            tail_i,
            span,
            pans: audio
                .iter()
                .map(|&i| _pan_at(pan_timeline, py_int(phrase[i].0)))
                .collect(),
        });
        total_span += span;
        stats.bump1("dac_phrases");
    }
    total_span = total_span.max(1.0);
    let target_chunks = 1.max(
        (release_budget as f64).min(
            py_int(total_span / (VGM_RATE as f64 * DAC_CHUNK_SECONDS) + 0.9999) as f64,
        ) as i64,
    );
    stats.set_int("dac_target_chunks", target_chunks);

    let mut samples: Vec<DacSample> = Vec::new();
    let mut plays: Vec<DacPlay> = Vec::new();
    let mut pan_chunks = 0i64;
    for info in &phrase_info {
        let phrase = &info.phrase;
        let audio = &info.audio;
        let pans = &info.pans;
        let budget = 1.max(py_int(py_round(
            target_chunks as f64 * info.span / total_span,
        )));
        let mut segments: Vec<(usize, usize, i32)> = Vec::new();
        let mut seg_start = 0usize;
        for k in 1..audio.len() {
            if pans[k] != pans[seg_start] {
                segments.push((seg_start, k - 1, pans[seg_start]));
                seg_start = k;
            }
        }
        segments.push((seg_start, audio.len() - 1, pans[seg_start]));
        let mut allocations: Vec<i64> = Vec::new();
        for &(s0, s1, _pan) in &segments {
            let count = 1.max(py_int(py_round(
                budget as f64 * (s1 - s0 + 1) as f64 / audio.len() as f64,
            )));
            allocations.push(count.min((s1 - s0 + 1) as i64));
        }
        for (&(seg0, seg1, seg_pan), &count) in segments.iter().zip(allocations.iter()) {
            let seg_len = (seg1 - seg0 + 1) as i64;
            let mut last_pan: Option<i32> = None;
            for k in 0..count {
                let a0 = seg0 as i64 + py_floor_div(k * seg_len, count);
                let a1 = seg0 as i64 + py_floor_div((k + 1) * seg_len, count) - 1;
                if a1 < a0 {
                    continue;
                }
                let w0 = audio[a0 as usize];
                let w1 = audio[a1 as usize];
                // A chunk may take the zero byte that immediately follows its
                // last word, but not a zero thousands of words later: that is
                // the phrase's trailing silence, and appending it stretched
                // every chunk to the whole phrase (two overlapped chunks then
                // made group_dac_plays double the rate).
                let chunk_tail = if w1 + 1 < phrase.len() && phrase[w1 + 1].1 == 0 {
                    Some(w1 + 1)
                } else {
                    None
                };
                let mut selected: Vec<(f64, i32, i64)> = phrase[w0..=w1].to_vec();
                if let Some(tail) = chunk_tail {
                    selected.push(phrase[tail]);
                }
                let start = selected[0].0;
                let mut deltas: Vec<f64> = Vec::new();
                for pair in selected.windows(2) {
                    if pair[1].0 > pair[0].0 {
                        deltas.push(pair[1].0 - pair[0].0);
                    }
                }
                let min_wait = deltas.iter().copied().fold(None, |acc: Option<f64>, v| {
                    Some(match acc {
                        None => v,
                        Some(a) => a.min(v),
                    })
                });
                let rate = DAC_RATE_MIN.max(DAC_RATE_MAX.min(py_int(py_round(
                    VGM_RATE as f64 / min_wait.unwrap_or(1.0),
                )) as i32));
                let mut out: Vec<u8> = Vec::new();
                let mut previous: u8 = 0x00;
                for &(t, byte, _wait) in &selected {
                    let frame =
                        py_int(py_round((t - start) * rate as f64 / VGM_RATE as f64));
                    let byte = byte as u8;
                    if frame < out.len() as i64 {
                        out[frame as usize] = byte;
                        stats.bump1("dac_frame_collisions");
                        previous = byte;
                        continue;
                    }
                    if frame > out.len() as i64 {
                        out.extend(
                            std::iter::repeat(previous)
                                .take((frame - out.len() as i64) as usize),
                        );
                    }
                    out.push(byte);
                    previous = byte;
                }
                let data = out;
                let idx = samples.len() as i32;
                samples.push(DacSample {
                    start: 0,
                    length: data.len() as i64,
                    data,
                    c4_rate: rate,
                    onset: py_int(py_round(start)),
                });
                let chunk_pan = _pan_at(pan_timeline, py_int(start));
                let span = py_int(py_round(selected[selected.len() - 1].0 - start));
                plays.push(DacPlay::new(py_int(py_round(start)), idx, chunk_pan, span));
                if Some(chunk_pan) != last_pan {
                    pan_chunks += 1;
                    last_pan = Some(chunk_pan);
                }
                stats.bump1("dac_chunks");
                if chunk_pan != seg_pan {
                    stats.bump1("dac_pan_mismatch");
                }
            }
        }
    }

    if samples.len() > release_budget {
        let keep = release_budget - 1;
        let mut tail: Vec<usize> = (0..plays.len())
            .filter(|&pi| plays[pi].sample >= keep as i32)
            .collect();
        tail.sort_by(|&a, &b| plays[a].onset.cmp(&plays[b].onset));
        let mut rates: HashSet<i32> = HashSet::new();
        let mut pans: HashSet<i32> = HashSet::new();
        for &pi in &tail {
            rates.insert(samples[plays[pi].sample as usize].c4_rate);
            pans.insert(plays[pi].pan);
        }
        if tail.len() > 1 && rates.len() == 1 && pans.len() == 1 {
            // The rest of the song is played in order: one final sample can
            // hold the whole remainder and play as-is. This keeps the end of
            // a long stream instead of dropping it at the budget.
            let first = plays[tail[0]].clone();
            let rate = samples[first.sample as usize].c4_rate;
            let start = first.onset;
            let mut body: Vec<u8> = Vec::new();
            let mut held: u8 = 0x00;
            for &pi in &tail {
                let p = &plays[pi];
                let target = py_int(py_round(
                    (p.onset - start) as f64 * rate as f64 / VGM_RATE as f64,
                ));
                if target > body.len() as i64 {
                    body.extend(
                        std::iter::repeat(held).take((target - body.len() as i64) as usize),
                    );
                } else if target < body.len() as i64 {
                    body.truncate(target.max(0) as usize);
                }
                let payload = &samples[p.sample as usize].data;
                body.extend_from_slice(payload);
                if let Some(&last) = payload.last() {
                    held = last;
                }
            }
            let last = plays[tail[tail.len() - 1]].clone();
            let end = last.onset + last.span;
            let mut new_samples: Vec<DacSample> = samples[..keep].to_vec();
            new_samples.push(DacSample {
                start: 0,
                length: body.len() as i64,
                data: body,
                c4_rate: rate,
                onset: start,
            });
            let mut new_plays: Vec<DacPlay> =
                plays.iter().filter(|p| p.sample < keep as i32).cloned().collect();
            new_plays.push(DacPlay::new(start, keep as i32, first.pan, end - start));
            samples = new_samples;
            plays = new_plays;
            let tail_len = tail.len() as i64;
            stats.set_int("dac_release_merged_tail", tail_len);
        } else {
            stats.set_int(
                "dac_release_trimmed",
                (samples.len() - release_budget) as i64,
            );
            samples.truncate(release_budget);
            plays.retain(|p| p.sample < release_budget as i32);
        }
    }
    (samples, plays, pan_chunks, trimmed)
}

/// Whether a 0x93 start is the first YM2612's DAC and inside the song.
///
/// Both collection modes take every PCM byte from `dac_data`, so a stream on
/// another chip, another register, or the exporter's end-of-file flush has to
/// be refused rather than played twice.
pub fn _stream_is_dac(vgm: &VgmFile, play: &DacStreamPlay, stats: &mut DacStats) -> bool {
    if play.chip_type != DAC_STREAM_YM2612 {
        stats.bump1("dac_stream_other_chip");
        return false;
    }
    if play.reg != 0x2A {
        // The DAC takes only $2A. A stream aimed at another register is an FM
        // write stream, not PCM this tool can host in a sample map.
        stats.bump1("dac_stream_other_reg");
        return false;
    }
    if play.chip_id != 0 {
        stats.bump1("dac_stream_second_chip");
        return false;
    }
    if vgm.total_samples != 0 && play.sample >= vgm.total_samples {
        // The exporter flushes a final start at the end-of-file offset; the
        // song has already stopped there.
        stats.bump1("dac_stream_tail_plays");
        return false;
    }
    true
}

/// The byte range a 0x93 start plays out of the stream's data bank.
///
/// The YM2612's bank is the concatenated type-0x00 PCM database, which the
/// loader keeps in `dac_data`.  Mode 1 counts commands (one byte each on the
/// YM2612), mode 2 milliseconds of the 0x92 rate, mode 3 the rest of the bank.
/// The 0x91 step size is in command units, so a step of two plays every other
/// byte.  The database is unsigned (0x80 centre), while a module sample is
/// signed 8-bit: convert here so the playback engine does not store a -128 DC
/// offset, which clips the waveform against one rail.
pub fn _stream_body(vgm: &VgmFile, play: &DacStreamPlay) -> Vec<u8> {
    let bank = &vgm.dac_data;
    let start = (play.offset as i64).clamp(0, bank.len() as i64);
    let end: i64 = match play.length_mode() {
        1 => start + (play.length as i64).max(0),
        2 => {
            let rate = if play.rate != 0 {
                play.rate as f64
            } else {
                VGM_RATE as f64
            };
            start + (py_int(py_round(play.length as f64 * rate / 1000.0))).max(0)
        }
        3 => bank.len() as i64,
        _ => start,
    };
    let step = if play.step > 1 { play.step as usize } else { 1 };
    let hi = end.min(bank.len() as i64).max(0) as usize;
    let lo = start as usize;
    let body: Vec<u8> = bank[lo..hi].iter().step_by(step).copied().collect();
    if play.reverse {
        let reversed: Vec<u8> = body.into_iter().rev().collect();
        _signed_bank(&reversed)
    } else {
        _signed_bank(&body)
    }
}

/// (onset, body, rate) chunks for YM2612 0x93 stream starts.
///
/// A stream is already contiguous PCM at one rate, so it never needs the
/// per-byte word expansion.  The release sample budget is spread over the
/// plays by duration, which keeps a minute-long drum stream inside the same
/// 240-sample cap the direct-write path uses.
pub fn _stream_chunks(vgm: &VgmFile, stats: &mut DacStats, release_budget: usize) -> Vec<(i64, Vec<u8>, i32)> {
    if vgm.dac_stream_plays.is_empty() {
        return Vec::new();
    }
    let mut bodies: Vec<(i64, Vec<u8>, i32)> = Vec::new();
    for play in &vgm.dac_stream_plays {
        if !_stream_is_dac(vgm, play, stats) {
            continue;
        }
        let body = _stream_body(vgm, play);
        if body.is_empty() {
            stats.bump1("dac_stream_empty");
            continue;
        }
        let rate = _clamp_rate(
            if play.rate != 0 {
                play.rate as f64
            } else {
                VGM_RATE as f64
            },
            stats,
        );
        if play.loop_ {
            stats.bump1("dac_stream_loops");
        }
        bodies.push((play.sample, body, rate));
    }
    if bodies.is_empty() {
        return Vec::new();
    }
    let spans: Vec<f64> = bodies
        .iter()
        .map(|(_, body, rate)| body.len() as f64 * VGM_RATE as f64 / *rate as f64)
        .collect();
    let total_span = spans.iter().sum::<f64>().max(1.0);
    let target = 1.max(
        (release_budget as f64).min(
            py_int(total_span / (VGM_RATE as f64 * DAC_CHUNK_SECONDS) + 0.9999) as f64,
        ) as i64,
    );
    stats.set_int("dac_stream_plays", bodies.len() as i64);
    stats.set_int("dac_stream_target_chunks", target);
    let mut out: Vec<(i64, Vec<u8>, i32)> = Vec::new();
    for ((onset, body, rate), span) in bodies.iter().zip(spans.iter()) {
        let len = body.len() as i64;
        let count = 1.max(py_int(py_round(
            target as f64 * span / total_span,
        )))
        .min(len);
        for k in 0..count {
            let i0 = py_floor_div(k * len, count);
            let i1 = py_floor_div((k + 1) * len, count);
            if i1 <= i0 {
                continue;
            }
            let start = onset + py_int(py_round(i0 as f64 * VGM_RATE as f64 / *rate as f64));
            out.push((start, body[i0 as usize..i1 as usize].to_vec(), *rate));
            stats.bump1("dac_stream_chunks");
        }
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

pub fn _pan_chunks(plays: &[DacPlay]) -> i64 {
    let mut count = 0i64;
    let mut last: Option<i32> = None;
    let mut ordered: Vec<&DacPlay> = plays.iter().collect();
    ordered.sort_by(|a, b| a.onset.cmp(&b.onset));
    for p in ordered {
        if Some(p.pan) != last {
            count += 1;
            last = Some(p.pan);
        }
    }
    count
}

/// Options for [`collect_dac`].
#[derive(Clone, Debug, Default)]
pub struct CollectDacOptions {
    pub slices: bool,
    pub timeline: bool,
    pub grid: Option<(f64, i32, i64)>,
    pub lossy_samples: bool,
    /// Python patches `DAC_RELEASE_SAMPLES` in tests; `None` keeps the real
    /// budget (240), a value overrides it for the release-cap decisions.
    pub release_samples: Option<usize>,
}

/// Build source-segment samples, or release-capped chunks as a fallback.
///
/// A log whose DAC reads a PCM bank through `0xE0` pointers keeps one sample
/// per (offset, byte cadence). Shorter reads are prefixes of the longest run
/// at that offset, so the note is cut by the next run rather than stored
/// again. Other logs (direct `0x2A` writes, block/stream plays) use the
/// time-chunk path.
pub fn _collect_packed(
    vgm: &VgmFile,
    grid: Option<(f64, i32, i64)>,
    lossy_samples: bool,
    release_budget: usize,
) -> (Vec<DacSample>, Vec<DacPlay>, Vec<String>, DacStats) {
    if vgm.dac_events.is_empty()
        && vgm.dac_block_plays.is_empty()
        && vgm.dac_stream_plays.is_empty()
    {
        return (Vec::new(), Vec::new(), Vec::new(), DacStats::new());
    }
    let mut stats = DacStats::new();
    let mut warnings: Vec<String> = Vec::new();
    let pan_vec = if _fm6_is_used(vgm) {
        None
    } else {
        Some(_pan_timeline(vgm))
    };
    let pan_timeline = pan_vec.as_deref();
    let enable_tl = _dac_enable_timeline(vgm);

    let mut source_runs: Option<Vec<SourceRun>> = None;
    if vgm.dac_block_plays.is_empty() && vgm.dac_stream_plays.is_empty() {
        source_runs = _source_runs(vgm, &mut stats, &enable_tl);
    }

    let mut samples: Vec<DacSample> = Vec::new();
    let mut plays: Vec<DacPlay> = Vec::new();
    let has_source = source_runs.as_ref().map(|r| !r.is_empty()).unwrap_or(false);
    let mut source_ok = has_source;
    if has_source {
        let runs = source_runs.take().unwrap();
        if !runs.iter().any(|run| run.bytes.iter().any(|&b| b != 0)) {
            warnings.push(
                "YM2612 DAC stream contains only silence; no DAC notes written".to_string(),
            );
            return (Vec::new(), Vec::new(), warnings, stats);
        }
        let (s, p) = _collect_source(runs, &mut stats, pan_timeline);
        let (s, p) = _coalesce_singles(s, p, &mut stats);
        let (s, p) = _prune_quiet_plays(s, p, &mut stats);
        samples = s;
        plays = p;
        if samples.is_empty() || plays.is_empty() {
            warnings.push(
                "YM2612 DAC stream contains only silence; no DAC notes written".to_string(),
            );
            return (Vec::new(), Vec::new(), warnings, stats);
        }
        let estimate = if let Some((hz, speed, t0)) = grid {
            // Count the bodies the real row grouping will keep, not an upper
            // bound: clustering, one-shot joining and silence splitting can
            // remove most of the source slots.  A preview run avoids dropping
            // a 104-sample source representation for the 240-sample chunk path.
            let preview = group_dac_plays(
                &samples,
                &plays,
                &GroupDacOptions {
                    hz,
                    speed,
                    t0,
                    total_samples: vgm.total_samples,
                    map_capacity: DAC_MAP_CAPACITY,
                    trim: false,
                    drop_quiet: true,
                    cluster_fragments: lossy_samples,
                },
            );
            preview.0.len()
        } else {
            samples.len()
        };
        if estimate > release_budget {
            warnings.push(format!(
                "{estimate} YM2612 DAC source samples would exceed the {release_budget}-sample budget; using time chunks"
            ));
            stats.set_int("dac_source_over_budget", estimate as i64);
            source_ok = false;
        }
    }

    if source_ok {
        stats.set_text("dac_mode", "source");
        stats.set_int("dac_chunks", samples.len() as i64);
        stats.set_int("dac_pan_chunks", _pan_chunks(&plays));
        stats.set_int("dac_unique_samples", samples.len() as i64);
        stats.set_int(
            "dac_sample_bytes",
            samples.iter().map(|s| s.data.len() as i64).sum(),
        );
        stats.set_int("dac_notes", plays.len() as i64);
        let default_bytes: i64 = samples.iter().map(|s| s.data.len() as i64).sum();
        let source_bytes = if stats.contains("dac_source_bytes") {
            stats.int("dac_source_bytes")
        } else {
            default_bytes
        };
        stats.set_int("dac_source_bytes", source_bytes);
        // A source sample longer than the run is cut by the next note; the
        // extra body is intentional.
        stats.set_int(
            "dac_source_cut_plays",
            plays
                .iter()
                .filter(|p| p.source && (p.sample as usize) < samples.len()
                    && p.span < samples[p.sample as usize].data.len() as i64)
                .count() as i64,
        );
        // A pan change during one long sample is channel state: the writer
        // places 08xx on the row it lands on, so the note keeps its body and
        // the pan still moves during playback.
        if let Some(pan_timeline) = pan_timeline {
            if !pan_timeline.is_empty() {
                let mut points: Vec<(i64, i32)> = Vec::new();
                let mut last_pan: Option<i32> = None;
                for &(t, raw) in pan_timeline {
                    let pan = _pan_byte(raw);
                    if Some(pan) != last_pan {
                        points.push((t, pan));
                        last_pan = Some(pan);
                    }
                }
                stats.set_points("dac_pan_points", points);
            }
        }
    } else {
        // Drop source-only counters so the chunk stats describe what ran.
        for key in [
            "dac_source_offsets",
            "dac_source_runs",
            "dac_source_groups",
            "dac_source_runs_reused",
            "dac_source_sample_bytes",
            "dac_source_coalesced_chains",
            "dac_source_coalesced_plays",
            "dac_source_coalesced_samples",
            "dac_source_runs_unsplit",
            "dac_source_silent_runs",
            "dac_source_quiet_runs",
            "dac_source_quiet_bytes",
            "dac_source_silence_splits",
        ] {
            stats.remove(key);
        }
        let words = _packed_words(vgm, &mut stats, &enable_tl);
        let chunks = _stream_chunks(vgm, &mut stats, release_budget);
        if words.is_empty() && chunks.is_empty() {
            if stats.int("dac_disabled_bytes") != 0 {
                warnings.push(format!(
                    "{} YM2612 DAC writes while the DAC is disabled (0x2B bit 7 clear); no DAC audio",
                    stats.int("dac_disabled_bytes")
                ));
            }
            return (Vec::new(), Vec::new(), warnings, stats);
        }
        let mut mode: &'static str = "chunks";
        let mut trimmed = 0i64;
        let mut pan_chunks: i64;
        if !words.is_empty() {
            // Attack detection first: a rest or a sharp onset starts a new
            // candidate body, so a repeated drum hit reuses one sample instead
            // of every fixed 0.25 s window becoming its own slice.  Uniform
            // chunks remain a fallback only when they fit better.
            let (s, p, attack_stats) = _attack_chunks(&words, pan_timeline, release_budget);
            stats.merge(&attack_stats);
            samples = s;
            plays = p;
            let attack_pan = _pan_chunks(&plays);
            pan_chunks = attack_pan;
            mode = "attacks";
            if samples.len() > release_budget {
                let (chunk_samples, chunk_plays, chunk_pan, chunk_trim) =
                    _chunk_words(&words, &mut stats, pan_timeline, release_budget);
                if chunk_samples.len() < samples.len() {
                    samples = chunk_samples;
                    plays = chunk_plays;
                    pan_chunks = chunk_pan;
                    trimmed = chunk_trim;
                    mode = "chunks";
                }
            }
        } else {
            samples = Vec::new();
            plays = Vec::new();
            pan_chunks = 0;
        }
        let chunks_bytes: i64 = chunks.iter().map(|c| c.1.len() as i64).sum();
        for (onset, body, rate) in chunks {
            let idx = samples.len() as i32;
            let span = py_int(py_round(
                body.len() as f64 * VGM_RATE as f64 / rate as f64,
            ));
            samples.push(DacSample {
                start: 0,
                length: body.len() as i64,
                data: body,
                c4_rate: rate,
                onset,
            });
            let pan = _pan_at(pan_timeline, onset);
            plays.push(DacPlay::new(onset, idx, pan, span));
        }
        let (s, p) = _prune_quiet_plays(samples, plays, &mut stats);
        samples = s;
        plays = p;
        plays.sort_by(|a, b| a.onset.cmp(&b.onset));
        if samples.len() > release_budget {
            stats.set_int(
                "dac_release_trimmed",
                (samples.len() - release_budget) as i64,
            );
            samples.truncate(release_budget);
            plays.retain(|p| p.sample < release_budget as i32);
        }
        stats.set_text("dac_mode", mode);
        stats.set_int(
            "dac_source_bytes",
            words.len() as i64 + chunks_bytes,
        );
        stats.set_int("dac_trimmed_silence_bytes", trimmed);
        stats.set_int("dac_chunks", samples.len() as i64);
        stats.set_int("dac_pan_chunks", pan_chunks);
        stats.set_int("dac_unique_samples", samples.len() as i64);
        stats.set_int(
            "dac_sample_bytes",
            samples.iter().map(|s| s.data.len() as i64).sum(),
        );
        stats.set_int("dac_notes", plays.len() as i64);
    }

    if stats.int("dac_disabled_audible") != 0 {
        warnings.push(format!(
            "{} non-silent YM2612 DAC writes while the DAC is disabled; ignored",
            stats.int("dac_disabled_audible")
        ));
    }
    if stats.int("dac_same_time_runs") != 0 {
        warnings.push(format!(
            "{} YM2612 DAC bursts ({} bytes) share one VGM sample; only the final value is kept",
            stats.int("dac_same_time_runs"),
            stats.int("dac_same_time_bytes")
        ));
    }
    if stats.int("dac_frame_collisions") != 0 {
        warnings.push(format!(
            "{} YM2612 DAC writes rounded to an already-written phrase frame; the later value at that frame is kept",
            stats.int("dac_frame_collisions")
        ));
    }
    if stats.int("dac_bank_overrun") != 0 {
        warnings.push(format!(
            "{} YM2612 DAC reads ran past the PCM bank",
            stats.int("dac_bank_overrun")
        ));
    }
    if stats.int("dac_release_merged_tail") != 0 {
        warnings.push(format!(
            "{} YM2612 DAC tail chunks were joined into the final sample at the {}-sample budget; they play in order",
            stats.int("dac_release_merged_tail"),
            release_budget
        ));
    }
    if stats.int("dac_release_trimmed") != 0 {
        warnings.push(format!(
            "{} YM2612 DAC chunks were dropped to stay inside the {}-sample budget; the end of the stream is missing",
            stats.int("dac_release_trimmed"),
            release_budget
        ));
    }
    if samples.is_empty() {
        warnings.push(
            "YM2612 DAC stream contains only silence; no DAC notes written".to_string(),
        );
    }
    (samples, plays, warnings, stats)
}

/// Return (samples, plays, warnings, stats).
///
/// Default: bank source samples reused by offset (or rate-exact chunks when
/// the source representation cannot fit). `slices=True`: the per-run
/// sample-map editor representation. `timeline=True`: the old 22.05 kHz
/// comparison body. `grid=(hz, speed, t0)` lets the collector check the row
/// grid's final sample count before choosing a mode.
pub fn collect_dac(
    vgm: &VgmFile,
    opts: &CollectDacOptions,
) -> (Vec<DacSample>, Vec<DacPlay>, Vec<String>, DacStats) {
    if opts.timeline {
        return _collect_timeline(vgm);
    }
    if opts.slices {
        return _collect_slices(vgm);
    }
    let release = opts.release_samples.unwrap_or(DAC_RELEASE_SAMPLES);
    _collect_packed(vgm, opts.grid, opts.lossy_samples, release)
}

/// Options for [`group_dac_plays`].
#[derive(Clone, Debug)]
pub struct GroupDacOptions {
    pub hz: f64,
    pub speed: i32,
    pub t0: i64,
    pub total_samples: i64,
    pub map_capacity: i32,
    pub trim: bool,
    pub drop_quiet: bool,
    pub cluster_fragments: bool,
}

impl Default for GroupDacOptions {
    fn default() -> Self {
        GroupDacOptions {
            hz: 60.0,
            speed: 1,
            t0: 0,
            total_samples: 0,
            map_capacity: DAC_MAP_CAPACITY,
            trim: true,
            drop_quiet: true,
            cluster_fragments: false,
        }
    }
}

/// Frames this play needs, which can be fewer than its sample's body.
pub fn _play_frames(samples: &[DacSample], p: &DacPlay) -> i64 {
    let data = &samples[p.sample as usize].data;
    if p.span <= 0 || !p.source {
        return data.len() as i64;
    }
    let frames = py_int(py_round(
        p.span as f64 * samples[p.sample as usize].c4_rate as f64 / VGM_RATE as f64,
    ));
    1.max((data.len() as i64).min(frames))
}

/// One playback rate for a merged row.
pub fn _group_rate(
    samples: &[DacSample],
    items: &[usize],
    plays: &[DacPlay],
    next_onset: &HashMap<usize, i64>,
) -> i32 {
    let mut weighted = 0.0f64;
    let mut weight = 0i64;
    for &pi in items {
        let p = &plays[pi];
        let n = _play_frames(samples, p);
        if n > 1 && !p.collapsed {
            weighted += samples[p.sample as usize].c4_rate as f64 * n as f64;
            weight += n;
        }
    }
    let natural = if weight != 0 {
        weighted / weight as f64
    } else {
        VGM_RATE as f64
    };
    let mut lower = 0.0f64;
    if !items.iter().any(|&pi| plays[pi].source) {
        // A source play is cut by the next note, so it must keep its own
        // pitch. The old chunks had to be stretched to fit; source samples
        // do not.
        for &pi in items {
            let p = &plays[pi];
            let n = _play_frames(samples, p);
            let nxt = next_onset.get(&pi).copied();
            if n > 1 {
                if let Some(nxt) = nxt {
                    if nxt > p.onset {
                        lower = lower.max(VGM_RATE as f64 * n as f64 / (nxt - p.onset) as f64);
                    }
                }
            }
        }
    }
    py_int(py_round(natural.max(lower))) as i32
}

/// The value the DAC is holding when the next note starts.
pub fn _held_byte(
    samples: &[DacSample],
    previous: Option<i32>,
    previous_time: i64,
    cut_time: i64,
) -> i32 {
    let previous = match previous {
        Some(previous) => previous,
        None => return 0x00,
    };
    let smp = &samples[previous as usize];
    let mut frame = py_int(py_round(
        (cut_time - previous_time) as f64 * smp.c4_rate as f64 / VGM_RATE as f64,
    ));
    frame = frame.clamp(0, smp.data.len() as i64 - 1);
    smp.data[frame as usize] as i32
}

/// Overlap of two bodies when ``b[i]`` aligns with ``a[i + shift]``.
pub fn _body_overlap(a: &[u8], b: &[u8], shift: i32) -> Option<(usize, usize, usize)> {
    if shift >= 0 {
        let n = (a.len() as i64 - shift as i64).min(b.len() as i64);
        if n <= 0 {
            return None;
        }
        return Some((n as usize, shift as usize, 0));
    }
    let n = (a.len() as i64).min(b.len() as i64 + shift as i64);
    if n <= 0 {
        return None;
    }
    Some((n as usize, 0, (-shift) as usize))
}

pub fn _sampled_mismatch(a: &[u8], b: &[u8], shift: i32, limit: usize) -> f64 {
    let (n, sa, sb) = match _body_overlap(a, b, shift) {
        Some(ov) => ov,
        None => return 1.0,
    };
    let step = 1.max(n / limit);
    let mut count = 0usize;
    let mut mism = 0usize;
    let mut i = 0usize;
    while i < n {
        count += 1;
        if a[i + sa] != b[i + sb] {
            mism += 1;
        }
        i += step;
    }
    mism as f64 / 1.max(count) as f64
}

pub fn _full_mismatch(a: &[u8], b: &[u8], shift: i32) -> (usize, Vec<usize>) {
    let (n, sa, sb) = match _body_overlap(a, b, shift) {
        Some(ov) => ov,
        None => return (0, Vec::new()),
    };
    let mism: Vec<usize> = (0..n).filter(|&i| a[i + sa] != b[i + sb]).collect();
    (n, mism)
}

/// Best body offset if two bank samples are the same audio.
///
/// The leading and trailing fill can move by a few frames when a driver
/// reaches the same bank body from a different row position.  The interior
/// of the bodies must agree almost exactly; returning the offset lets the
/// writer move that play's note so its main body still lands on time.
pub fn _similar_alignment(a: &[u8], b: &[u8]) -> Option<(i32, f64, f64)> {
    if a.len() < DAC_SIMILAR_MIN_LEN || b.len() < DAC_SIMILAR_MIN_LEN {
        return None;
    }
    let mut best_sample: Option<(f64, i32, i32)> = None;
    for sh in -DAC_SIMILAR_SHIFT..=DAC_SIMILAR_SHIFT {
        let score = _sampled_mismatch(a, b, sh, 128);
        let cand = (score, sh.abs(), sh);
        let better = match best_sample {
            None => true,
            Some((bs, ba, bx)) => {
                if cand.0 < bs {
                    true
                } else if cand.0 > bs {
                    false
                } else if cand.1 < ba {
                    true
                } else if cand.1 > ba {
                    false
                } else {
                    cand.2 < bx
                }
            }
        };
        if better {
            best_sample = Some(cand);
        }
    }
    let best_sample = best_sample.unwrap();
    if best_sample.0 > 0.5 {
        // Even the best coarse alignment is clearly a different body.
        return None;
    }
    let mut best: Option<((f64, f64, i64, i32), i32, f64, f64)> = None;
    let center = best_sample.2;
    for sh in center - 3..=center + 3 {
        if sh.abs() > DAC_SIMILAR_SHIFT {
            continue;
        }
        let (n, mism) = _full_mismatch(a, b, sh);
        if n < 8.max(a.len() / 4) {
            continue;
        }
        let edge = DAC_SIMILAR_EDGE.min(4.max(n / 8));
        let interior_n = n as i64 - 2 * edge as i64;
        let interior_mis = mism.iter().filter(|&&i| edge <= i && i < n - edge).count();
        let overall = 1.0 - mism.len() as f64 / n as f64;
        let interior = if interior_n > 0 {
            1.0 - interior_mis as f64 / interior_n as f64
        } else {
            overall
        };
        if overall < DAC_SIMILAR_MIN_MATCH {
            continue;
        }
        if interior < DAC_SIMILAR_INTERIOR {
            continue;
        }
        let cand = (
            interior,
            overall,
            -(mism.len() as i64),
            -(sh.abs()),
        );
        let better = match best {
            None => true,
            Some((bc, _, _, _)) => {
                cand.0 > bc.0
                    || (cand.0 == bc.0 && cand.1 > bc.1)
                    || (cand.0 == bc.0 && cand.1 == bc.1 && cand.2 > bc.2)
                    || (cand.0 == bc.0
                        && cand.1 == bc.1
                        && cand.2 == bc.2
                        && cand.3 > bc.3)
            }
        };
        if better {
            best = Some((cand, sh, overall, interior));
        }
    }
    best.map(|(_, sh, overall, interior)| (sh, overall, interior))
}

pub fn _similar_candidates(a: &DacSample, b: &DacSample) -> bool {
    if (a.c4_rate - b.c4_rate).abs() > DAC_SIMILAR_RATE {
        return false;
    }
    let small = a.data.len().min(b.data.len());
    if small < DAC_SIMILAR_MIN_LEN {
        return false;
    }
    let tolerance = 256.min(16.max(small / 16));
    (a.data.len() as i64 - b.data.len() as i64).unsigned_abs() as usize <= tolerance
}

/// Join unique samples that play back-to-back into one body.
///
/// A sample used exactly once cannot be needed again, so a sequence of such
/// notes on the same channel, pan and rate is stored as one body and played
/// by the first note.  The next note's onset truncates the previous body, so
/// overlapping tails stay exact.
pub fn _merge_one_shot_samples(
    samples: Vec<DacSample>,
    mut notes: Vec<(i64, i32, i32)>,
    off_items: Vec<(usize, i64)>,
    stats: &mut DacStats,
) -> (Vec<DacSample>, Vec<(i64, i32, i32)>, Vec<(usize, i64)>) {
    if notes.len() < 2 {
        return (samples, notes, off_items);
    }
    let mut uses: HashMap<i32, i64> = HashMap::new();
    for &(_t, idx, _pan) in &notes {
        *uses.entry(idx).or_insert(0) += 1;
    }
    let mut off_by_note: HashMap<usize, Vec<i64>> = HashMap::new();
    for &(ni, t) in &off_items {
        off_by_note.entry(ni).or_default().push(t);
    }

    let mut ordered: Vec<usize> = (0..notes.len()).collect();
    ordered.sort_by(|&a, &b| (notes[a].0, a).cmp(&(notes[b].0, b)));
    let mut runs: Vec<Vec<usize>> = Vec::new();
    let mut current: Vec<usize> = Vec::new();
    for ni in ordered {
        let (t, idx, pan) = notes[ni];
        if uses.get(&idx).copied().unwrap_or(0) != 1 {
            if current.len() > 1 {
                runs.push(std::mem::take(&mut current));
            } else {
                current.clear();
            }
            continue;
        }
        if !current.is_empty() {
            let prev_ni = current[current.len() - 1];
            let (pt, pidx, ppan) = notes[prev_ni];
            let prev = &samples[pidx as usize];
            let mut end = pt as f64
                + prev.data.len() as f64 * VGM_RATE as f64 / prev.c4_rate as f64;
            if let Some(cut) = off_by_note.get(&prev_ni) {
                if let Some(&min_cut) = cut.iter().min() {
                    end = end.min(min_cut as f64);
                }
            }
            if pan == ppan
                && samples[idx as usize].c4_rate == prev.c4_rate
                && t >= pt
                && t as f64 <= end + DAC_ONESHOT_GAP as f64
            {
                current.push(ni);
                continue;
            }
            if current.len() > 1 {
                runs.push(std::mem::take(&mut current));
            }
        }
        current = vec![ni];
    }
    if current.len() > 1 {
        runs.push(current);
    }
    if runs.is_empty() {
        return (samples, notes, off_items);
    }

    let mut samples = samples;
    let mut removed: HashSet<usize> = HashSet::new();
    for run in runs {
        let first_ni = run[0];
        let (start, first_idx, pan) = notes[first_ni];
        let rate = samples[first_idx as usize].c4_rate;
        let mut body = samples[first_idx as usize].data.clone();
        let mut held = body.last().copied().unwrap_or(0x00);
        for &ni in &run[1..] {
            let (t, idx, _pan) = notes[ni];
            let target = py_int(py_round(
                (t - start) as f64 * rate as f64 / VGM_RATE as f64,
            ));
            if target < body.len() as i64 {
                body.truncate(target.max(0) as usize);
            } else if target > body.len() as i64 {
                body.extend(
                    std::iter::repeat(held).take((target - body.len() as i64) as usize),
                );
            }
            let payload = &samples[idx as usize].data;
            body.extend_from_slice(payload);
            if let Some(&last) = payload.last() {
                held = last;
            }
            removed.insert(ni);
        }
        let new_idx = samples.len() as i32;
        samples.push(DacSample {
            start: -1,
            length: body.len() as i64,
            data: body,
            c4_rate: rate,
            onset: start,
        });
        notes[first_ni] = (start, new_idx, pan);
        stats.bump1("dac_oneshot_chains");
        stats.bump("dac_oneshot_notes", (run.len() - 1) as i64);
        stats.bump("dac_oneshot_samples", (run.len() - 1) as i64);
    }
    if removed.is_empty() {
        return (samples, notes, off_items);
    }

    let mut remap_note: HashMap<usize, usize> = HashMap::new();
    let mut new_notes: Vec<(i64, i32, i32)> = Vec::new();
    for (i, note) in notes.iter().enumerate() {
        if removed.contains(&i) {
            continue;
        }
        remap_note.insert(i, new_notes.len());
        new_notes.push(*note);
    }
    let new_offs = off_items
        .into_iter()
        .filter(|(ni, _)| remap_note.contains_key(ni))
        .map(|(ni, t)| (remap_note[&ni], t))
        .collect();
    (samples, new_notes, new_offs)
}

/// Share one body between bank samples that differ only by a start offset.
///
/// The representative is the longest member.  A play that used a shorter
/// body starts at its own note time, so the note moves by the body offset to
/// keep the shared main waveform on the same spot on the timeline.
pub fn _cluster_similar_samples(
    samples: Vec<DacSample>,
    notes: Vec<(i64, i32, i32)>,
    off_items: Vec<(usize, i64)>,
    stats: &mut DacStats,
) -> (Vec<DacSample>, Vec<(i64, i32, i32)>, Vec<(usize, i64)>) {
    let count = samples.len();
    if count < 2 {
        return (samples, notes, off_items);
    }
    let mut groups: Vec<Vec<usize>> = Vec::new();
    for (i, smp) in samples.iter().enumerate() {
        let mut placed = false;
        for group in groups.iter_mut() {
            let rep = group[0];
            if !_similar_candidates(&samples[rep], smp) {
                continue;
            }
            if _similar_alignment(&samples[rep].data, &smp.data).is_some() {
                group.push(i);
                placed = true;
                break;
            }
        }
        if !placed {
            groups.push(vec![i]);
        }
    }

    let mut remap: Vec<usize> = (0..count).collect();
    let mut shifts: Vec<i32> = vec![0; count];
    let mut merged_groups = 0i64;
    for group in &groups {
        if group.len() < 2 {
            continue;
        }
        let mut rep = group[0];
        for &i in &group[1..] {
            if samples[i].data.len() > samples[rep].data.len() {
                rep = i;
            }
        }
        let mut members: Vec<usize> = Vec::new();
        for &i in group {
            if i == rep {
                members.push(i);
                continue;
            }
            if !_similar_candidates(&samples[rep], &samples[i]) {
                continue;
            }
            match _similar_alignment(&samples[rep].data, &samples[i].data) {
                None => continue,
                Some(hit) => {
                    shifts[i] = hit.0;
                    members.push(i);
                }
            }
        }
        if members.len() < 2 {
            continue;
        }
        merged_groups += 1;
        for &i in &members {
            remap[i] = rep;
        }
        stats.bump("dac_similar_samples", (members.len() - 1) as i64);
    }
    if merged_groups == 0 {
        return (samples, notes, off_items);
    }
    stats.set_int("dac_similar_groups", merged_groups);

    let mut note_dt: Vec<i64> = vec![0; notes.len()];
    let mut new_notes: Vec<(i64, i32, i32)> = Vec::new();
    for (i, &(t, idx, pan)) in notes.iter().enumerate() {
        let target = remap[idx as usize];
        let sh = shifts[idx as usize];
        let mut t = t;
        if sh != 0 {
            let dt = -(sh as f64) * VGM_RATE as f64 / samples[target].c4_rate as f64;
            note_dt[i] = py_int(py_round(dt));
            t = (t + note_dt[i]).max(0);
            stats.bump1("dac_similar_shifted_notes");
        }
        new_notes.push((t, target as i32, pan));
    }
    new_notes.sort_by(|a, b| a.0.cmp(&b.0));
    let new_offs = off_items
        .into_iter()
        .map(|(ni, t)| (ni, (t + note_dt[ni]).max(0)))
        .collect();
    (samples, new_notes, new_offs)
}

/// (length, start) of the longest run of signed-silence bytes (0).
pub fn _longest_silence(data: &[u8]) -> (usize, usize) {
    let mut best_len = 0usize;
    let mut best_start = 0usize;
    let mut run = 0usize;
    let mut start = 0usize;
    for (i, &value) in data.iter().enumerate() {
        if value != 0 {
            run = 0;
            continue;
        }
        if run == 0 {
            start = i;
        }
        run += 1;
        if run > best_len {
            best_len = run;
            best_start = start;
        }
    }
    (best_len, best_start)
}

/// Remove a long silent stretch from a bank body and keep its timing.
///
/// A driver can read one bank range across a long DAC hold, so a kick and a
/// later snare end up in one body.  When over 40% of an 8,192-frame body is
/// one silent run, split before and after it.  Plays that reach the later
/// part get their own note at the original time; the silence becomes note
/// spacing instead of sample data.
pub fn _split_long_silence(
    mut samples: Vec<DacSample>,
    notes: Vec<(i64, i32, i32)>,
    off_items: Vec<(usize, i64)>,
    stats: &mut DacStats,
) -> (Vec<DacSample>, Vec<(i64, i32, i32)>, Vec<(usize, i64)>) {
    if notes.is_empty() {
        return (samples, notes, off_items);
    }
    let mut split_info: HashMap<usize, (i64, i64, i64, Option<i32>, Option<i32>)> =
        HashMap::new();
    let mut si = 0usize;
    while si < samples.len() {
        let cur = si;
        si += 1;
        let data_len = samples[cur].data.len();
        if data_len < DAC_SILENCE_MIN_LEN {
            continue;
        }
        let (run, start) = _longest_silence(&samples[cur].data);
        if run as f64 <= data_len as f64 * DAC_SILENCE_FRACTION {
            continue;
        }
        let pre_end = start as i64;
        let post_start = start as i64 + run as i64;
        if pre_end == 0 && post_start >= data_len as i64 {
            continue;
        }
        let rate = samples[cur].c4_rate;
        let onset = samples[cur].onset;
        let pre_body = if pre_end > 0 {
            Some(samples[cur].data[..pre_end as usize].to_vec())
        } else {
            None
        };
        let post_body = if post_start < data_len as i64 {
            Some(samples[cur].data[post_start as usize..].to_vec())
        } else {
            None
        };
        let mut pre_idx: Option<i32> = None;
        let mut post_idx: Option<i32> = None;
        if let Some(body) = pre_body {
            pre_idx = Some(samples.len() as i32);
            samples.push(DacSample {
                start: -1,
                length: body.len() as i64,
                data: body,
                c4_rate: rate,
                onset,
            });
        }
        if let Some(body) = post_body {
            post_idx = Some(samples.len() as i32);
            samples.push(DacSample {
                start: -1,
                length: body.len() as i64,
                data: body,
                c4_rate: rate,
                onset,
            });
        }
        split_info.insert(cur, (pre_end, post_start, run as i64, pre_idx, post_idx));
        stats.bump("dac_silence_split_frames", run as i64);
        stats.bump1("dac_silence_split_samples");
    }
    if split_info.is_empty() {
        return (samples, notes, off_items);
    }

    let mut off_by_note: HashMap<usize, Vec<i64>> = HashMap::new();
    for &(ni, t) in &off_items {
        off_by_note.entry(ni).or_default().push(t);
    }
    let mut ordered: Vec<usize> = (0..notes.len()).collect();
    ordered.sort_by(|&a, &b| (notes[a].0, a).cmp(&(notes[b].0, b)));
    let mut next_onset: HashMap<usize, Option<i64>> = HashMap::new();
    for (k, &ni) in ordered.iter().enumerate() {
        next_onset.insert(
            ni,
            if k + 1 < ordered.len() {
                Some(notes[ordered[k + 1]].0)
            } else {
                None
            },
        );
    }

    let mut new_notes: Vec<(i64, i32, i32)> = Vec::new();
    let mut new_offs: Vec<(usize, i64)> = Vec::new();
    for (ni, &(t, si, pan)) in notes.iter().enumerate() {
        let info = match split_info.get(&(si as usize)) {
            None => {
                let idx = new_notes.len();
                new_notes.push((t, si, pan));
                if let Some(offs) = off_by_note.get(&ni) {
                    for &off_t in offs {
                        new_offs.push((idx, off_t));
                    }
                }
                continue;
            }
            Some(info) => *info,
        };
        let (pre_end, post_start, _run, pre_idx, post_idx) = info;
        let smp = &samples[si as usize];
        let rate = smp.c4_rate;
        let pre_natural = t as f64 + pre_end as f64 * VGM_RATE as f64 / rate as f64;
        let post_time = t as f64 + post_start as f64 * VGM_RATE as f64 / rate as f64;
        let natural_end = t as f64 + smp.data.len() as f64 * VGM_RATE as f64 / rate as f64;
        let explicit = off_by_note
            .get(&ni)
            .and_then(|v| v.iter().min().copied());
        let mut end = natural_end;
        if let Some(explicit) = explicit {
            end = end.min(explicit as f64);
        }
        if let Some(Some(nxt)) = next_onset.get(&ni) {
            end = end.min(*nxt as f64);
        }

        let mut added = false;
        if let Some(pre_idx) = pre_idx {
            let idx = new_notes.len();
            new_notes.push((t, pre_idx, pan));
            added = true;
            if let Some(explicit) = explicit {
                if (explicit as f64) < pre_natural {
                    new_offs.push((
                        idx,
                        py_int(py_round((t as f64).max(explicit as f64))),
                    ));
                }
            }
        }
        if let Some(post_idx) = post_idx {
            if end > post_time {
                let placed = py_int(py_round(post_time));
                let idx = new_notes.len();
                new_notes.push((placed, post_idx, pan));
                added = true;
                if let Some(explicit) = explicit {
                    if (explicit as f64) < natural_end {
                        new_offs.push((
                            idx,
                            py_int(py_round((placed as f64).max(explicit as f64))),
                        ));
                    }
                }
            }
        }
        if added {
            stats.bump1("dac_silence_split_notes");
        } else {
            stats.bump1("dac_silence_dropped_notes");
        }
    }
    new_notes.sort_by(|a, b| a.0.cmp(&b.0));
    (samples, new_notes, new_offs)
}

/// Drop bodies no note references and renumber the sample map.
pub fn _compact_samples(
    samples: Vec<DacSample>,
    notes: Vec<(i64, i32, i32)>,
) -> (Vec<DacSample>, Vec<(i64, i32, i32)>) {
    let used: HashSet<i32> = notes.iter().map(|&(_t, idx, _pan)| idx).collect();
    let mut remap: HashMap<usize, i32> = HashMap::new();
    let mut compact: Vec<DacSample> = Vec::new();
    for (i, smp) in samples.iter().enumerate() {
        if !used.contains(&(i as i32)) {
            continue;
        }
        remap.insert(i, compact.len() as i32);
        compact.push(smp.clone());
    }
    let notes = notes
        .into_iter()
        .map(|(t, idx, pan)| (t, remap[&(idx as usize)], pan))
        .collect();
    (compact, notes)
}

/// Optionally reduce the sample map to ``target`` bodies.
pub fn _cluster_fragments(
    samples: Vec<DacSample>,
    notes: Vec<(i64, i32, i32)>,
    stats: &mut DacStats,
    target: i32,
) -> (Vec<DacSample>, Vec<(i64, i32, i32)>) {
    if notes.is_empty() || samples.len() as i64 <= target as i64 {
        return (samples, notes);
    }
    let mut uses_map: HashMap<i32, i64> = HashMap::new();
    for &(_t, idx, _pan) in &notes {
        *uses_map.entry(idx).or_insert(0) += 1;
    }
    let mut uses: IndexMap<i32, i64> = IndexMap::new();
    for smp_i in 0..samples.len() {
        let idx = smp_i as i32;
        uses.insert(idx, uses_map.get(&idx).copied().unwrap_or(0));
    }
    let bodies: Vec<Vec<u8>> = samples.iter().map(|smp| smp.data.clone()).collect();
    let rates: Vec<i32> = samples.iter().map(|smp| smp.c4_rate).collect();
    let (keep, remap) = cluster_fragments(
        &bodies,
        &rates,
        &uses,
        target,
        DAC_HOT_ANCHORS,
    );
    if keep.len() == samples.len() {
        return (samples, notes);
    }
    let new_notes: Vec<(i64, i32, i32)> = notes
        .iter()
        .map(|&(t, idx, pan)| {
            let mapped = remap[idx as usize];
            (t, if mapped >= 0 { mapped } else { idx }, pan)
        })
        .collect();
    stats.set_int("dac_fragment_clustered", (samples.len() - keep.len()) as i64);
    stats.set_int("dac_fragment_samples_kept", keep.len() as i64);
    let kept: Vec<DacSample> = keep.iter().map(|&i| samples[i as usize].clone()).collect();
    (kept, new_notes)
}

/// Merge same-row plays into samples, note-ons, note-offs and pan events.
///
/// A source sample can be longer than the run that triggered it. When the
/// next run is late, the note-off list cuts the sample at the run's real end
/// instead of letting the longer body play into the gap.
pub fn group_dac_plays(
    samples: &[DacSample],
    plays: &[DacPlay],
    opts: &GroupDacOptions,
) -> (
    Vec<DacSample>,
    Vec<(i64, i32, i32)>,
    Vec<(i64, i32)>,
    Vec<(i64, i32)>,
    Vec<String>,
    DacStats,
) {
    if plays.is_empty() {
        return (samples.to_vec(), Vec::new(), Vec::new(), Vec::new(), Vec::new(), DacStats::new());
    }
    let mut stats = DacStats::new();
    let row_samples = (VGM_RATE as f64 / opts.hz) * opts.speed as f64;
    let tick = VGM_RATE as f64 / opts.hz;
    let t0 = opts.t0;
    let speed = opts.speed;
    let total_samples = opts.total_samples;

    let place = |sample: i64| -> (i64, i32) {
        let (row, delay) =
            place_tick_floor(sample, t0, row_samples, speed, tick, true, None);
        (row as i64, delay)
    };
    let placed_time = |row: i64, delay: i32| -> i64 {
        py_int(py_round(
            t0 as f64 + row as f64 * row_samples + delay as f64 * tick,
        ))
    };

    let mut ordered: Vec<usize> = (0..plays.len()).collect();
    ordered.sort_by(|&a, &b| plays[a].onset.cmp(&plays[b].onset));
    let mut groups: IndexMap<i64, Vec<usize>> = IndexMap::new();
    for &pi in &ordered {
        let (row, _delay) = place(plays[pi].onset);
        groups.entry(row).or_default().push(pi);
    }
    // The lower-bound rate guard must see the next *note* (the next row), not
    // the next chunk of the same note. A phrase is split into 0.25 s chunks
    // that share a row; using a following chunk's onset made the group play at
    // twice its real rate (a 44.1 kHz chunk stretched to 88.2 kHz).
    let mut rows: Vec<i64> = groups.keys().copied().collect();
    rows.sort();
    let mut next_onset: HashMap<usize, i64> = HashMap::new();
    for (i, &row) in rows.iter().enumerate() {
        let nxt = if i + 1 < rows.len() {
            plays[groups[&rows[i + 1]][0]].onset
        } else {
            total_samples
        };
        for &pi in &groups[&row] {
            next_onset.insert(pi, nxt);
        }
    }

    let mut out: Vec<DacSample> = Vec::new();
    let mut by_key: HashMap<(Vec<u8>, i32), i32> = HashMap::new();
    let mut notes: Vec<(i64, i32, i32)> = Vec::new();
    let mut off_items: Vec<(usize, i64)> = Vec::new();
    let mut pan_events: Vec<(i64, i32)> = Vec::new();
    let mut previous_sample: Option<i32> = None;
    let mut previous_time: i64 = 0;

    fn is_silent(samples: &[DacSample], p: &DacPlay) -> bool {
        let payload = &samples[p.sample as usize].data;
        !payload.is_empty() && payload.iter().all(|&b| b == 0)
    }

    #[allow(clippy::too_many_arguments)]
    fn add_off(
        note_ni: usize,
        idx: i32,
        items: &[usize],
        plays: &[DacPlay],
        note_time: i64,
        rate: i32,
        out: &[DacSample],
        next_onset: &HashMap<usize, i64>,
        total_samples: i64,
        tick: f64,
        off_items: &mut Vec<(usize, i64)>,
        stats: &mut DacStats,
    ) {
        let last_i = *items.last().unwrap();
        let last = &plays[last_i];
        if !last.source {
            return;
        }
        let span_end = last.onset + last.span;
        let body_end = note_time as f64
            + out[idx as usize].data.len() as f64 * VGM_RATE as f64 / rate as f64;
        let tolerance = 1.0f64.max(tick);
        let next_time = next_onset.get(&last_i).copied().unwrap_or(total_samples);
        if body_end <= span_end as f64 + tolerance {
            return;
        }
        if total_samples != 0 && next_time as f64 <= span_end as f64 + tolerance {
            // The next note already cuts this one on time.
            return;
        }
        off_items.push((note_ni, py_int(py_round(span_end as f64))));
        stats.bump1("dac_cut_offs");
    }

    for &row in &rows {
        let items = &groups[&row];
        if items.iter().all(|&pi| is_silent(samples, &plays[pi])) {
            // Pure-silent row: tail the previous sample with 0x00, no note.
            stats.bump1("dac_silent_groups");
            stats.bump1("dac_silent_notes");
            stats.bump("dac_silent_plays", items.len() as i64);
            stats.bump(
                "dac_silent_bytes",
                items
                    .iter()
                    .map(|&pi| samples[plays[pi].sample as usize].data.len() as i64)
                    .sum(),
            );
            if !notes.is_empty() {
                let (prev_time, prev_idx, prev_pan) = notes[notes.len() - 1];
                let prev = out[prev_idx as usize].clone();
                if !prev.data.is_empty() && prev.data[prev.data.len() - 1] != 0 {
                    let mut target = py_int(py_round(
                        (plays[items[0]].onset - prev_time) as f64 * prev.c4_rate as f64
                            / VGM_RATE as f64,
                    ));
                    target = target.max(prev.data.len() as i64);
                    let mut body = prev.data.clone();
                    let last = prev.data[prev.data.len() - 1];
                    body.extend(
                        std::iter::repeat(last).take((target - prev.data.len() as i64) as usize),
                    );
                    body.push(0);
                    let tail_added = body.len() as i64 - prev.data.len() as i64;
                    let note_i = notes.len() - 1;
                    let new_idx;
                    if notes[..notes.len() - 1]
                        .iter()
                        .any(|&(_t, i, _p)| i == prev_idx)
                    {
                        new_idx = _add_sample(
                            &mut out,
                            &mut by_key,
                            body,
                            prev.c4_rate,
                            prev.onset,
                        );
                    } else {
                        by_key.remove(&(prev.data.clone(), prev.c4_rate));
                        out[prev_idx as usize] = DacSample {
                            start: prev.start,
                            length: body.len() as i64,
                            data: body.clone(),
                            c4_rate: prev.c4_rate,
                            onset: prev.onset,
                        };
                        by_key.insert((body.clone(), prev.c4_rate), prev_idx);
                        new_idx = prev_idx;
                    }
                    notes[note_i] = (prev_time, new_idx, prev_pan);
                    previous_sample = Some(new_idx);
                    previous_time = prev_time;
                    stats.bump("dac_silence_tail_bytes", tail_added);
                }
            }
            continue;
        }
        let first = &plays[items[0]];
        let (first_row, delay) = place(first.onset);
        let note_time = placed_time(first_row, delay);
        let rate = _group_rate(samples, items, plays, &next_onset);
        let mut rate = rate;
        if rate > DAC_RATE_MAX {
            rate = DAC_RATE_MAX;
            stats.bump1("dac_rate_clamped");
        } else if rate < DAC_RATE_MIN {
            rate = DAC_RATE_MIN;
            stats.bump1("dac_rate_clamped");
        }
        let lead = (first.onset - note_time).max(0);
        let lead_frames = py_int(py_round(lead as f64 * rate as f64 / VGM_RATE as f64));
        let mut held = _held_byte(&out, previous_sample, previous_time, note_time);
        if first.fresh {
            // The DAC was off before this run; a lead is silence, not the
            // previous sample's last value.
            held = 0x00;
        }

        // One source play on its own row already has exactly the body the
        // note needs. Reusing it is what keeps a repeated bank sample one
        // module sample; only a same-row concatenation or a lead fill has to
        // build a new body.
        let only = &plays[items[0]];
        let next_time = next_onset.get(&items[0]).copied().unwrap_or(total_samples);
        let has_next = total_samples != 0 && next_time < total_samples;
        let span_frames = if only.span > 0 {
            1.max(py_int(py_round(
                only.span as f64 * rate as f64 / VGM_RATE as f64,
            )))
        } else {
            samples[only.sample as usize].data.len() as i64
        };
        if items.len() == 1
            && only.source
            && rate == samples[only.sample as usize].c4_rate
            && (has_next || span_frames + 1 >= samples[only.sample as usize].data.len() as i64)
        {
            let src = &samples[only.sample as usize];
            let idx = _add_sample(
                &mut out,
                &mut by_key,
                src.data.clone(),
                src.c4_rate,
                note_time,
            );
            notes.push((note_time, idx, only.pan));
            add_off(
                notes.len() - 1,
                idx,
                items,
                plays,
                note_time,
                rate,
                &out,
                &next_onset,
                total_samples,
                tick,
                &mut off_items,
                &mut stats,
            );
            previous_sample = Some(idx);
            previous_time = note_time;
            stats.bump1("dac_groups");
            stats.bump1("dac_reused_notes");
            continue;
        }

        let mut data: Vec<u8> = Vec::new();
        if lead_frames != 0 {
            data.extend(std::iter::repeat(held as u8).take(lead_frames as usize));
            stats.bump("dac_lead_bytes", lead_frames);
        }

        let note_pan = first.pan;
        let last_i = items.len() - 1;
        let last_next = next_onset
            .get(&items[items.len() - 1])
            .copied()
            .unwrap_or(total_samples);
        let last_has_next = total_samples != 0 && last_next < total_samples;
        for (i, &pi) in items.iter().enumerate() {
            let p = &plays[pi];
            let target = py_int(py_round(
                (p.onset - note_time) as f64 * rate as f64 / VGM_RATE as f64,
            ));
            if data.len() as i64 > target {
                let over = data.len() as i64 - target;
                stats.bump("dac_overshoot", over);
                data.truncate(target.max(0) as usize);
                if !data.is_empty() {
                    held = data[data.len() - 1] as i32;
                }
            } else if target > data.len() as i64 {
                let fill = target - data.len() as i64;
                data.extend(std::iter::repeat(held as u8).take(fill as usize));
                stats.bump("dac_hold_bytes", fill);
            }
            let mut payload: &[u8] = &samples[p.sample as usize].data;
            if p.source && (i < last_i || !last_has_next) {
                let mut frames: i64;
                if i < last_i {
                    let nxt = py_int(py_round(
                        (plays[items[i + 1]].onset - note_time) as f64 * rate as f64
                            / VGM_RATE as f64,
                    ));
                    frames = 1.max(nxt - target);
                } else {
                    frames = 1.max(py_int(py_round(
                        p.span as f64 * rate as f64 / VGM_RATE as f64,
                    )));
                    if frames + 1 >= payload.len() as i64 {
                        frames = payload.len() as i64;
                    }
                }
                let take = (frames.max(0) as usize).min(payload.len());
                payload = &payload[..take];
            }
            data.extend_from_slice(payload);
            if !payload.is_empty() {
                held = payload[payload.len() - 1] as i32;
            }
            if i != 0 && p.pan != plays[items[i - 1]].pan {
                // Pan is channel state; land it at the row start.
                let (prow, _pdelay) = place(p.onset);
                pan_events.push((placed_time(prow, 0), p.pan));
            }
        }

        let body = data;
        let idx = _add_sample(&mut out, &mut by_key, body, rate, note_time);
        notes.push((note_time, idx, note_pan));
        add_off(
            notes.len() - 1,
            idx,
            items,
            plays,
            note_time,
            rate,
            &out,
            &next_onset,
            total_samples,
            tick,
            &mut off_items,
            &mut stats,
        );
        previous_sample = Some(idx);
        previous_time = note_time;
        stats.bump1("dac_groups");
    }

    // Bank source bodies are whole audio regions, so two bodies that differ
    // only by a small start offset and a one-shot sequence that never repeats
    // can share storage.  The chunk path must not do this: those bodies are
    // adjacent time slices of one stream.
    if !plays.is_empty() && plays.iter().all(|p| p.source) {
        let r = _split_long_silence(out, notes, off_items, &mut stats);
        out = r.0;
        notes = r.1;
        off_items = r.2;
        let r = _compact_samples(out, notes);
        out = r.0;
        notes = r.1;
        let r = _merge_one_shot_samples(out, notes, off_items, &mut stats);
        out = r.0;
        notes = r.1;
        off_items = r.2;
        let r = _compact_samples(out, notes);
        out = r.0;
        notes = r.1;
        let r = _cluster_similar_samples(out, notes, off_items, &mut stats);
        out = r.0;
        notes = r.1;
        off_items = r.2;
        let r = _compact_samples(out, notes);
        out = r.0;
        notes = r.1;
    }

    // Row grouping can invent a silence tail or a flat DC body that the
    // source-side filter never saw.  No audible note may select one, so drop
    // those slots (and their notes/offs) before the writer sees them.
    let keep: Vec<usize> = if opts.drop_quiet {
        out.iter()
            .enumerate()
            .filter(|(_, smp)| _audible_body(&smp.data))
            .map(|(i, _)| i)
            .collect()
    } else {
        (0..out.len()).collect()
    };
    if keep.len() < out.len() {
        let mut remap_sample: HashMap<i32, i32> = HashMap::new();
        for (new, &old) in keep.iter().enumerate() {
            remap_sample.insert(old as i32, new as i32);
        }
        let mut remap_note: HashMap<usize, usize> = HashMap::new();
        let mut kept_notes: Vec<(i64, i32, i32)> = Vec::new();
        for (ni, &(t, idx, pan)) in notes.iter().enumerate() {
            let new_idx = match remap_sample.get(&idx) {
                Some(&v) => v,
                None => continue,
            };
            remap_note.insert(ni, kept_notes.len());
            kept_notes.push((t, new_idx, pan));
        }
        stats.set_int("dac_quiet_samples_removed", (out.len() - keep.len()) as i64);
        out = keep.iter().map(|&i| out[i].clone()).collect();
        notes = kept_notes;
        off_items = off_items
            .into_iter()
            .filter(|(ni, _)| remap_note.contains_key(ni))
            .map(|(ni, t)| (remap_note[&ni], t))
            .collect();
    }

    if opts.cluster_fragments {
        let r = _cluster_fragments(out, notes, &mut stats, DAC_TARGET_SAMPLES);
        out = r.0;
        notes = r.1;
    }
    let offs: Vec<(i64, i32)> = off_items.iter().map(|&(_ni, t)| (t, 0)).collect();

    let mut warnings: Vec<String> = Vec::new();
    let mut trimmed = 0usize;
    if opts.trim && out.len() > DAC_RELEASE_SAMPLES {
        trimmed = out.len() - DAC_RELEASE_SAMPLES;
        stats.bump("dac_release_trimmed", trimmed as i64);
        out.truncate(DAC_RELEASE_SAMPLES);
        notes.retain(|n| n.1 < DAC_RELEASE_SAMPLES as i32);
    }
    stats.set_int("dac_unique_samples", out.len() as i64);
    stats.set_int("dac_notes", notes.len() as i64);
    stats.set_int(
        "dac_sample_bytes",
        out.iter().map(|s| s.data.len() as i64).sum(),
    );
    stats.set_int(
        "dac_maps",
        ((out.len() as i64 + opts.map_capacity as i64 - 1) / opts.map_capacity as i64) as i64,
    );
    if stats.int("dac_overshoot") != 0 {
        warnings.push(format!(
            "{} YM2612 DAC samples were longer than the gap to the next run; the later bytes start late by that many frames",
            stats.int("dac_overshoot")
        ));
    }
    if trimmed != 0 {
        warnings.push(format!(
            "{trimmed} YM2612 DAC samples were dropped to stay inside the {DAC_RELEASE_SAMPLES}-sample budget; the end of the stream is missing"
        ));
    }
    (out, notes, offs, pan_events, warnings, stats)
}

/// Old 22.05 kHz comparison body.
pub fn _collect_timeline(vgm: &VgmFile) -> (Vec<DacSample>, Vec<DacPlay>, Vec<String>, DacStats) {
    let warnings: Vec<String> = Vec::new();
    let bank = _signed_bank(&vgm.dac_data);
    if bank.is_empty() {
        return (Vec::new(), Vec::new(), warnings, DacStats::new());
    }
    let pan_vec = if _fm6_is_used(vgm) {
        None
    } else {
        Some(_pan_timeline(vgm))
    };
    let pan_timeline = pan_vec.as_deref();
    let mut samples: Vec<DacSample> = Vec::new();
    let mut plays: Vec<DacPlay> = Vec::new();
    let mut start: i64 = 0;
    let mut pos: i64 = 0;
    let mut run_start: Option<i64> = None;
    let mut run_last: i64 = 0;
    let mut out: Vec<u8> = Vec::new();
    let mut cur: u8 = 0x00;
    let mut last_idx: i64 = 0;

    #[allow(clippy::too_many_arguments)]
    fn flush(
        run_start: &mut Option<i64>,
        run_last: i64,
        out: &mut Vec<u8>,
        cur: &mut u8,
        last_idx: &mut i64,
        samples: &mut Vec<DacSample>,
        plays: &mut Vec<DacPlay>,
        pan_timeline: Option<&[(i64, i64)]>,
    ) {
        let run_start_v = match *run_start {
            Some(v) => v,
            None => return,
        };
        let need = py_int(py_round(
            (run_last - run_start_v + 1) as f64 * DAC_TIMELINE_RATE as f64
                / VGM_RATE as f64,
        ));
        if need > out.len() as i64 {
            out.extend(std::iter::repeat(*cur).take((need - out.len() as i64) as usize));
        }
        if out.len() as i64 >= DAC_MIN_SAMPLES {
            let data = out.clone();
            let idx = samples.len() as i32;
            let pan = _pan_at(pan_timeline, run_start_v);
            let span = py_floor_div(
                data.len() as i64 * VGM_RATE,
                DAC_TIMELINE_RATE as i64,
            );
            samples.push(DacSample {
                start: -1,
                length: data.len() as i64,
                data,
                c4_rate: DAC_TIMELINE_RATE,
                onset: run_start_v,
            });
            plays.push(DacPlay::new(run_start_v, idx, pan, span));
        }
        *run_start = None;
        out.clear();
        *cur = 0x00;
        *last_idx = 0;
    }

    for &(t, kind, value) in &vgm.dac_events {
        if kind == 0 {
            start = value.max(0);
            pos = 0;
            continue;
        }
        let byte: u8;
        if kind == 2 {
            byte = ((value - 0x80) & 0xFF) as u8;
        } else if kind == 1 {
            if start + pos >= bank.len() as i64 {
                continue;
            }
            byte = bank[(start + pos) as usize];
            pos += 1;
        } else {
            continue;
        }
        if run_start.is_none() {
            run_start = Some(t);
            run_last = t;
            out.clear();
            cur = byte;
            out.push(byte);
            last_idx = 0;
            continue;
        }
        if t - run_last > DAC_RUN_GAP {
            flush(
                &mut run_start,
                run_last,
                &mut out,
                &mut cur,
                &mut last_idx,
                &mut samples,
                &mut plays,
                pan_timeline,
            );
            run_start = Some(t);
            run_last = t;
            out.clear();
            cur = byte;
            out.push(byte);
            last_idx = 0;
            continue;
        }
        let mut idx = py_int(py_round(
            (t - run_start.unwrap()) as f64 * DAC_TIMELINE_RATE as f64 / VGM_RATE as f64,
        ));
        if idx < last_idx {
            idx = last_idx;
        }
        if idx > out.len() as i64 {
            out.extend(std::iter::repeat(cur).take((idx - out.len() as i64) as usize));
        }
        if idx == out.len() as i64 {
            out.push(byte);
        } else {
            out[idx as usize] = byte;
        }
        last_idx = idx;
        run_last = t;
        cur = byte;
    }
    flush(
        &mut run_start,
        run_last,
        &mut out,
        &mut cur,
        &mut last_idx,
        &mut samples,
        &mut plays,
        pan_timeline,
    );
    let mut stats = DacStats::new();
    stats.set_int("dac_timeline", 1);
    (samples, plays, warnings, stats)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signed_bank_and_audible_checks() {
        assert_eq!(_signed_bank(&[0x00, 0x80, 0xFF]), vec![0x80, 0x00, 0x7F]);
        assert!(!_audible_body(&[]));
        assert!(!_audible_body(&[0, 0, 0]));
        assert!(!_audible_body(&[5, 5, 5]));
        assert!(!_audible_body(&[1, 2, 1]));
        assert!(_audible_body(&[0, 40, 0]));
        assert_eq!(_body_peak(&[0x80, 0x7F, 0x00]), 128);
    }

    #[test]
    fn pan_bytes_match_genesis_bits() {
        assert_eq!(_pan_byte(0xC0), 0xFF);
        assert_eq!(_pan_byte(0x80), 0xF0);
        assert_eq!(_pan_byte(0x40), 0x0F);
        assert_eq!(_pan_byte(0x00), 0);
    }

    #[test]
    fn byte_events_cap_direct_holds_and_track_pointer() {
        let mut vgm = VgmFile::default();
        vgm.dac_data = vec![0x80, 0x90, 0xA0];
        vgm.dac_events = vec![(0, 0, 0), (0, 1, 0), (4, 1, 0), (1000, 1, 0)];
        let mut stats = DacStats::new();
        let words = _byte_events(&vgm, &mut stats, &[]);
        assert_eq!(words.len(), 3);
        assert_eq!(words[0], (0, 0, 0));
        assert_eq!(words[1], (4, 16, 0));
        assert_eq!(words[2].0, 1000);
    }

    #[test]
    fn attack_split_marks_a_silent_rest() {
        let words = vec![(0.0, 40, 0), (10.0, 41, 0), (400.0, 50, 0)];
        let segments = _attack_segments(&words, 64);
        assert_eq!(segments.len(), 2);
        assert_eq!(segments[0].len(), 2);
        assert_eq!(segments[1].len(), 1);
    }

    #[test]
    fn grouped_source_plays_reuse_one_sample() {
        let samples = vec![DacSample {
            start: 0,
            length: 4,
            data: vec![0, 40, 0x80, 40],
            c4_rate: 44100,
            onset: 0,
        }];
        let plays = vec![
            DacPlay::with_flags(0, 0, -1, 40, false, true, false, None),
            DacPlay::with_flags(4410, 0, -1, 40, false, true, false, None),
        ];
        let opts = GroupDacOptions {
            hz: 60.0,
            speed: 1,
            t0: 0,
            total_samples: 20000,
            ..GroupDacOptions::default()
        };
        let (out, notes, offs, pans, warnings, stats) =
            group_dac_plays(&samples, &plays, &opts);
        assert_eq!(out.len(), 1);
        assert_eq!(notes.len(), 2);
        assert!(offs.is_empty());
        assert!(pans.is_empty());
        assert!(warnings.is_empty());
        assert_eq!(stats.int("dac_reused_notes"), 2);
    }
}
