//! Source-vs-export key-on audit used by the variant verifier.
//!
//! The FM half matches every source 0x28 key-on to an export key-on on the same
//! channel within 100 ms and compares the effective operator state.  The SN76489
//! half does the same for its volume-gated notes.  Only effective bits count:
//! 0x50 bit 5 and 0x60 bits 6-5 are don't-cares, and OPN DT=4 is negative zero.
//!
//! Port of `vgm2151fur/audit.py` (v0.11.0).

use std::collections::BTreeSet;
use std::rc::Rc;

use indexmap::IndexMap;
use serde_json::{json, Value};

use crate::fm::{OpRegs, OPN_OP_TO_MASK};
use crate::sn76489::{collect_sn76489, PsgHit};
use crate::vgm::{assemble_rom, VgmFile, DAC_STREAM_YM2612};
use crate::ym2608::{collect_ym2608_delta_t, collect_ym2608_rhythm, deltat_store_body};
use crate::ym2610::collect_ym2610_adpcm;
use crate::ym2612::YM2612State;

// 100 ms.  A fine grid can delay a note by up to a few EDxx ticks, but a
// wider window pairs too many repeated FM notes with the next instrument.
pub const WINDOW: i64 = 4410;
pub const FIELDS: [&str; 7] = ["dt_mul", "tl", "ks_ar", "am_d1r", "sr", "d1l_rr", "ssg_eg"];

pub fn _canon(field: &str, value: i32) -> i32 {
    if field == "ks_ar" {
        return value & 0xDF;
    }
    if field == "am_d1r" {
        return value & 0x9F;
    }
    if field == "dt_mul" {
        let mut dt = (value >> 4) & 7;
        if dt == 4 {
            dt = 0;
        }
        return (dt << 4) | (value & 15);
    }
    value
}

fn _field(op: &OpRegs, field: &str) -> i32 {
    match field {
        "dt_mul" => op.dt_mul as i32,
        "tl" => op.tl as i32,
        "ks_ar" => op.ks_ar as i32,
        "am_d1r" => op.am_d1r as i32,
        "sr" => op.sr as i32,
        "d1l_rr" => op.d1l_rr as i32,
        "ssg_eg" => op.ssg_eg as i32,
        _ => 0,
    }
}

/// One source/export FM key-on with the register image after the group.
#[derive(Clone, Debug, PartialEq)]
pub struct FmSnapshot {
    pub sample: i64,
    pub ch: i32,
    pub mask: i32,
    pub ops: [OpRegs; 4],
    pub fb_alg: i32,
}

pub fn fm_snapshots(vgm: &VgmFile) -> Vec<FmSnapshot> {
    let mut st = YM2612State::new();
    let mut out: Vec<FmSnapshot> = Vec::new();
    let mut i = 0;
    let ws = &vgm.writes;
    while i < ws.len() {
        let sample = ws[i].sample;
        let mut group: Vec<usize> = Vec::new();
        while i < ws.len() && ws[i].sample == sample {
            group.push(i);
            i += 1;
        }
        for &wi in &group {
            let w = &ws[wi];
            if w.chip == "ym2612" && w.chip_id == 0 {
                st.write(w.reg as i32, w.val as i32);
            }
        }
        for &wi in &group {
            let w = &ws[wi];
            if w.chip != "ym2612" || w.chip_id != 0 {
                continue;
            }
            if w.reg & 0x100 != 0 || (w.reg & 0xFF) != 0x28 {
                continue;
            }
            let mut ch = (w.val & 7) as i32;
            if ch == 3 || ch == 7 {
                continue;
            }
            if ch >= 3 {
                ch -= 1;
            }
            let mask = ((w.val >> 4) & 0xF) as i32;
            if mask == 0 {
                continue;
            }
            out.push(FmSnapshot {
                sample,
                ch,
                mask,
                ops: st.ops[ch as usize].clone(),
                fb_alg: st.fb_alg[ch as usize],
            });
        }
    }
    out
}

fn _match(
    a: &[&FmSnapshot],
    b: &[&FmSnapshot],
    key: impl Fn(&FmSnapshot) -> i32,
    window: i64,
) -> (Vec<(usize, Option<usize>)>, usize) {
    let mut used: BTreeSet<usize> = BTreeSet::new();
    let mut pairs: Vec<(usize, Option<usize>)> = Vec::new();
    for (si, s) in a.iter().enumerate() {
        let mut best: Option<(i64, usize)> = None;
        for (j, e) in b.iter().enumerate() {
            if used.contains(&j) || key(s) != key(e) {
                continue;
            }
            let d = (e.sample - s.sample).abs();
            if d <= window && (best.is_none() || d < best.unwrap().0) {
                best = Some((d, j));
            }
        }
        match best {
            None => pairs.push((si, None)),
            Some((_, j)) => {
                used.insert(j);
                pairs.push((si, Some(j)));
            }
        }
    }
    (pairs, b.len() - used.len())
}

fn map_value(m: &IndexMap<String, i64>) -> Value {
    let mut out = serde_json::Map::new();
    for (k, v) in m {
        out.insert(k.clone(), json!(*v));
    }
    Value::Object(out)
}

fn get_i64(v: &IndexMap<String, Value>, key: &str) -> i64 {
    v.get(key).and_then(|x| x.as_i64()).unwrap_or(0)
}

fn value_i64(v: &Value, key: &str) -> i64 {
    v.get(key).and_then(|x| x.as_i64()).unwrap_or(0)
}

pub fn compare_fm(src: &VgmFile, exp: &VgmFile) -> IndexMap<String, Value> {
    let a = fm_snapshots(src);
    let b = fm_snapshots(exp);
    if a.is_empty() {
        let mut out: IndexMap<String, Value> = IndexMap::new();
        out.insert("matched".to_string(), json!(0));
        out.insert("missed".to_string(), json!(0));
        out.insert("fields".to_string(), json!({}));
        out.insert("startup".to_string(), json!({}));
        out.insert("tl".to_string(), json!(0));
        return out;
    }
    let mut bad: IndexMap<String, i64> = IndexMap::new();
    let mut startup: IndexMap<String, i64> = IndexMap::new();
    let mut matched: i64 = 0;
    let mut missed: i64 = 0;
    for ch in 0..6 {
        let aa: Vec<&FmSnapshot> = a.iter().filter(|s| s.ch == ch).collect();
        let bb: Vec<&FmSnapshot> = b.iter().filter(|e| e.ch == ch).collect();
        let (pairs, _extra) = _match(&aa, &bb, |x| x.ch, WINDOW);
        for (si, ej) in pairs {
            let s = aa[si];
            let e = match ej {
                None => {
                    missed += 1;
                    continue;
                }
                Some(j) => bb[j],
            };
            matched += 1;
            let bucket: &mut IndexMap<String, i64> = if s.sample == 0 {
                &mut startup
            } else {
                &mut bad
            };
            for op in 0..4 {
                if s.mask & (1 << OPN_OP_TO_MASK[op]) == 0 {
                    continue;
                }
                let x = &s.ops[op];
                let y = &e.ops[op];
                for f in FIELDS {
                    if _canon(f, _field(x, f)) != _canon(f, _field(y, f)) {
                        *bucket.entry(f.to_string()).or_insert(0) += 1;
                    }
                }
            }
            if (s.fb_alg & 7) != (e.fb_alg & 7) {
                *bucket.entry("alg".to_string()).or_insert(0) += 1;
            }
            if (s.fb_alg >> 3) != (e.fb_alg >> 3) {
                *bucket.entry("fb".to_string()).or_insert(0) += 1;
            }
        }
    }
    let tl = bad.get("tl").copied().unwrap_or(0);
    let mut out: IndexMap<String, Value> = IndexMap::new();
    out.insert("matched".to_string(), json!(matched));
    out.insert("missed".to_string(), json!(missed));
    out.insert("fields".to_string(), map_value(&bad));
    out.insert("startup".to_string(), map_value(&startup));
    out.insert("tl".to_string(), json!(tl));
    out
}

#[derive(Clone, Debug)]
struct PsgSettled {
    sample: i64,
    voice: i32,
    period: i32,
    vol: i32,
    noise: bool,
    noise_mode: i32,
}

pub fn compare_psg(src: &VgmFile, exp: &VgmFile) -> IndexMap<String, Value> {
    let (sh, _, _) = collect_sn76489(src, None);
    let (eh, _, _) = collect_sn76489(exp, None);
    if sh.is_empty() {
        let mut out: IndexMap<String, Value> = IndexMap::new();
        out.insert("matched".to_string(), json!(0));
        out.insert("missed".to_string(), json!(0));
        out.insert("extra".to_string(), json!(0));
        out.insert("period".to_string(), json!(0));
        out.insert("vol".to_string(), json!(0));
        out.insert("noise".to_string(), json!(0));
        return out;
    }
    // _match expects dicts; use a dict view of the PSG hits.
    fn settled(h: &PsgHit) -> PsgSettled {
        // Furnace applies a note's E5/mode effect just after the note-on and
        // a driver may write its steady volume a tick later.  Compare the
        // state after 10 ms, not the gate byte.
        let end = h.sample_time + 441;
        let mut period = h.period;
        let mut vol = h.vol;
        let mut mode = h.noise_mode;
        for (t, value) in &h.pitch_pts {
            if *t <= end {
                period = *value;
            }
        }
        for (t, value) in &h.vol_pts {
            if *t <= end {
                vol = *value;
            }
        }
        for (t, value) in &h.mode_pts {
            if *t <= end {
                mode = *value;
            }
        }
        PsgSettled {
            sample: h.sample_time,
            voice: h.voice,
            period,
            vol,
            noise: h.noise,
            noise_mode: mode,
        }
    }

    let sd: Vec<PsgSettled> = sh.iter().map(settled).collect();
    let ed: Vec<PsgSettled> = eh.iter().map(settled).collect();
    let mut used: BTreeSet<usize> = BTreeSet::new();
    let mut pairs: Vec<(usize, Option<usize>)> = Vec::new();
    for (si, s) in sd.iter().enumerate() {
        let mut best: Option<((i64, i64), usize)> = None;
        for (j, e) in ed.iter().enumerate() {
            if used.contains(&j) || e.voice != s.voice {
                continue;
            }
            let dt = (e.sample - s.sample).abs();
            if dt > WINDOW {
                continue;
            }
            let dv = if s.noise {
                ((e.noise_mode & 7) - (s.noise_mode & 7)).abs() as i64
            } else {
                (e.period as i64 - s.period as i64).abs()
            };
            let score = (dv, dt);
            if best.is_none() || score < best.unwrap().0 {
                best = Some((score, j));
            }
        }
        match best {
            None => pairs.push((si, None)),
            Some((_, j)) => {
                used.insert(j);
                pairs.push((si, Some(j)));
            }
        }
    }
    let extra = (ed.len() - used.len()) as i64;
    let mut missed: i64 = 0;
    let mut period: i64 = 0;
    let mut vol: i64 = 0;
    let mut noise: i64 = 0;
    for (si, ej) in &pairs {
        let s = &sd[*si];
        let e = match ej {
            None => {
                missed += 1;
                continue;
            }
            Some(j) => &ed[*j],
        };
        if s.noise {
            if (s.noise_mode & 7) != (e.noise_mode & 7) {
                noise += 1;
            }
        } else if s.period != e.period {
            period += 1;
        }
        if s.vol != e.vol {
            vol += 1;
        }
    }
    let mut out: IndexMap<String, Value> = IndexMap::new();
    out.insert("matched".to_string(), json!(sd.len() as i64 - missed));
    out.insert("missed".to_string(), json!(missed));
    out.insert("extra".to_string(), json!(extra));
    out.insert("period".to_string(), json!(period));
    out.insert("vol".to_string(), json!(vol));
    out.insert("noise".to_string(), json!(noise));
    out
}

fn _dac_starts(exp: &VgmFile, limit: i64) -> Vec<(i64, i64)> {
    // (onset, byte count) for every DAC start in an export.
    //
    // Furnace answers a long PCM channel either way: `$95` replays an embedded
    // block, `$93` streams a range of the bank it uploaded. Both are one note.
    let mut out: Vec<(i64, i64)> = Vec::new();
    for &(t, block, _rate) in &exp.dac_block_plays {
        if t < limit {
            let n = if block >= 0 && (block as usize) < exp.dac_blocks.len() {
                exp.dac_blocks[block as usize].len() as i64
            } else {
                -1
            };
            out.push((t, n));
        }
    }
    for play in &exp.dac_stream_plays {
        if play.sample >= limit || play.chip_type != DAC_STREAM_YM2612 {
            continue;
        }
        out.push((
            play.sample,
            if play.length_mode() == 1 {
                play.length as i64
            } else {
                -1
            },
        ));
    }
    out.sort();
    out
}

pub fn compare_dac(src: &VgmFile, exp: &VgmFile) -> IndexMap<String, Value> {
    // Count source DAC notes matched to an exported DAC stream start.
    //
    // `analyze` is the full analyzer entry point from the plan's library API
    // (`analyze(vgm, AnalyzeOptions::default()) -> Result<Song>`); the rest of
    // this module does not need it.
    let song = crate::analyze::analyze(src, Default::default()).expect("analyze failed");
    let notes: Vec<i64> = song
        .events
        .iter()
        .filter(|e| e.dac && e.on)
        .map(|e| e.sample)
        .collect();
    // Ignore the exporter's end-of-file sample flush.
    let limit = if src.total_samples != 0 {
        src.total_samples
    } else {
        notes.iter().copied().max().unwrap_or(0) + 1
    };
    let starts = _dac_starts(exp, limit);
    let streams = src
        .dac_stream_plays
        .iter()
        .filter(|p| limit == 0 || p.sample < limit)
        .count() as i64;
    if notes.is_empty() {
        let mut out: IndexMap<String, Value> = IndexMap::new();
        out.insert("matched".to_string(), json!(0));
        out.insert("missed".to_string(), json!(0));
        out.insert("extra".to_string(), json!(starts.len()));
        out.insert("rate_bad".to_string(), json!(0));
        out.insert("streams".to_string(), json!(streams));
        out.insert("bytes_src".to_string(), json!(0));
        out.insert("bytes_exp".to_string(), json!(0));
        return out;
    }
    let mut used: BTreeSet<usize> = BTreeSet::new();
    let mut missed: i64 = 0;
    for onset in &notes {
        let mut best: Option<(i64, usize)> = None;
        for (j, (t, _n)) in starts.iter().enumerate() {
            if used.contains(&j) {
                continue;
            }
            let d = (t - onset).abs();
            if d <= WINDOW && (best.is_none() || d < best.unwrap().0) {
                best = Some((d, j));
            }
        }
        match best {
            None => missed += 1,
            Some((_, j)) => {
                used.insert(j);
            }
        }
    }
    // Blocks are exported in sample order; allow 2 Hz C-4 rounding.
    let mut rate_bad: i64 = 0;
    let wants: Vec<i32> = song.dac_samples.iter().map(|s| s.c4_rate).collect();
    if exp.dac_blocks.len() == song.dac_samples.len() {
        for &(_t, block, rate) in &exp.dac_block_plays {
            let idx = if block < 0 {
                song.dac_samples.len() as i64 + block
            } else {
                block
            };
            if idx < 0 || idx as usize >= song.dac_samples.len() {
                continue;
            }
            let want = song.dac_samples[idx as usize].c4_rate;
            if want >= 192000 {
                continue;
            }
            if (rate - want as i64).abs() > 2 {
                rate_bad += 1;
            }
        }
    }
    // A stream start carries its own $92 rate. It has to be one of the
    // chunk rates, or the export plays the PCM at the wrong speed.
    for play in &exp.dac_stream_plays {
        if limit != 0 && play.sample >= limit {
            continue;
        }
        if play.rate == 0 || wants.is_empty() {
            continue;
        }
        if !wants
            .iter()
            .any(|want| (play.rate as i64 - *want as i64).abs() <= 2)
        {
            rate_bad += 1;
        }
    }
    // How much PCM actually plays. A `$93` source of one long stream and the
    // chunked module have to cover the same byte count.
    let bytes_src: i64 = song.dac_samples.iter().map(|s| s.data.len() as i64).sum();
    let bytes_exp: i64 = starts.iter().map(|(_t, n)| *n).filter(|n| *n > 0).sum();
    let mut out: IndexMap<String, Value> = IndexMap::new();
    out.insert("matched".to_string(), json!(notes.len() as i64 - missed));
    out.insert("missed".to_string(), json!(missed));
    out.insert("extra".to_string(), json!(starts.len() as i64 - used.len() as i64));
    out.insert("rate_bad".to_string(), json!(rate_bad));
    out.insert("streams".to_string(), json!(streams));
    out.insert("bytes_src".to_string(), json!(bytes_src));
    out.insert("bytes_exp".to_string(), json!(bytes_exp));
    out
}

#[derive(Clone, Debug)]
struct AudHit {
    sample_time: i64,
    vol: i32,
    pan: i32,
    rate: i32,
    /// One identity for ADPCM-A, a voice number for OPNA rhythm, 0 otherwise.
    channel: i32,
    data: Rc<Vec<u8>>,
}

fn audible(pan: i32, pan_pts: &[(i64, i32)]) -> bool {
    pan != 0 || pan_pts.iter().any(|(_t, p)| *p != 0)
}

/// Pair gates by an identity key, then by nearest time inside it.
///
/// `key` is the identity of a hit. Matching keeps the Python candidate order
/// (insertion order unless `sort_group`), while the bisect window always runs
/// over the sorted times, exactly as the reference does.
fn match_aud<K: PartialEq>(
    src: &[AudHit],
    exp: &[AudHit],
    key: impl Fn(&AudHit) -> K,
    include_data: bool,
    sort_group: bool,
) -> (Vec<(usize, Option<usize>)>, usize) {
    let mut groups: Vec<(K, Vec<usize>)> = Vec::new();
    for (j, e) in exp.iter().enumerate() {
        let k = key(e);
        match groups.iter_mut().find(|(gk, _)| *gk == k) {
            Some((_, v)) => v.push(j),
            None => groups.push((k, vec![j])),
        }
    }
    if sort_group {
        for (_, v) in groups.iter_mut() {
            v.sort_by_key(|&j| exp[j].sample_time);
        }
    }
    let mut times: Vec<Vec<i64>> = Vec::new();
    for (_, v) in &groups {
        let mut ts: Vec<i64> = v.iter().map(|&j| exp[j].sample_time).collect();
        ts.sort();
        times.push(ts);
    }
    let mut used: BTreeSet<usize> = BTreeSet::new();
    let mut pairs: Vec<(usize, Option<usize>)> = Vec::new();
    for (si, s) in src.iter().enumerate() {
        let k = key(s);
        let gi = groups.iter().position(|(gk, _)| *gk == k);
        let mut best: Option<((i32, i64), usize)> = None;
        if let Some(gi) = gi {
            let cand = &groups[gi].1;
            let ts = &times[gi];
            let lo = ts.partition_point(|&t| t < s.sample_time - WINDOW);
            let hi = ts.partition_point(|&t| t <= s.sample_time + WINDOW);
            for i in lo..hi {
                let j = cand[i];
                if used.contains(&j) {
                    continue;
                }
                let e = &exp[j];
                let d = (e.sample_time - s.sample_time).abs();
                let score = if include_data {
                    (if *s.data != *e.data { 1i32 } else { 0 }, d)
                } else {
                    (0, d)
                };
                if best.is_none() || score < best.unwrap().0 {
                    best = Some((score, j));
                }
            }
        }
        match best {
            None => pairs.push((si, None)),
            Some((_, j)) => {
                used.insert(j);
                pairs.push((si, Some(j)));
            }
        }
    }
    (pairs, exp.len() - used.len())
}

fn tally_adpcm(
    src: &[AudHit],
    exp: &[AudHit],
    pairs: &[(usize, Option<usize>)],
    extra: usize,
    kind_b: bool,
) -> IndexMap<String, i64> {
    let mut out: IndexMap<String, i64> = IndexMap::new();
    out.insert("matched".to_string(), 0);
    out.insert("missed".to_string(), 0);
    out.insert("extra".to_string(), extra as i64);
    out.insert("data".to_string(), 0);
    out.insert("length".to_string(), 0);
    out.insert("vol".to_string(), 0);
    out.insert("pan".to_string(), 0);
    out.insert("rate".to_string(), 0);
    for (si, ej) in pairs {
        let s = &src[*si];
        let Some(e) = ej.map(|j| &exp[j]) else {
            *out.get_mut("missed").unwrap() += 1;
            continue;
        };
        *out.get_mut("matched").unwrap() += 1;
        if s.data.len() != e.data.len() {
            *out.get_mut("length").unwrap() += 1;
        }
        if *s.data != *e.data {
            *out.get_mut("data").unwrap() += 1;
        }
        if s.vol != e.vol {
            *out.get_mut("vol").unwrap() += 1;
        }
        if s.pan != e.pan {
            // A zero-volume gate is silent on both sides.
            if s.vol != 0 || e.vol != 0 {
                *out.get_mut("pan").unwrap() += 1;
            }
        }
        if kind_b && (s.rate - e.rate).abs() > 4 {
            *out.get_mut("rate").unwrap() += 1;
        }
    }
    out
}

fn tally_opna(
    src: &[AudHit],
    exp: &[AudHit],
    pairs: &[(usize, Option<usize>)],
    extra: usize,
    delta_t: bool,
) -> IndexMap<String, i64> {
    let mut out: IndexMap<String, i64> = IndexMap::new();
    out.insert("matched".to_string(), 0);
    out.insert("missed".to_string(), 0);
    out.insert("extra".to_string(), extra as i64);
    out.insert("data".to_string(), 0);
    out.insert("length".to_string(), 0);
    out.insert("vol".to_string(), 0);
    out.insert("pan".to_string(), 0);
    out.insert("rate".to_string(), 0);
    for (si, ej) in pairs {
        let s = &src[*si];
        let Some(e) = ej.map(|j| &exp[j]) else {
            *out.get_mut("missed").unwrap() += 1;
            continue;
        };
        *out.get_mut("matched").unwrap() += 1;
        if delta_t {
            // Furnace reads `lengthB` bytes, which is the body rounded up to
            // its 256-byte quantum, so that is the span to compare - ours cut
            // the same way. The address itself moves with repacking and says
            // nothing about fidelity.
            let want = deltat_store_body(&s.data);
            if e.data.len() != want.len() {
                *out.get_mut("length").unwrap() += 1;
            }
            let head = &e.data[..std::cmp::min(e.data.len(), want.len())];
            if head != want.as_slice() {
                *out.get_mut("data").unwrap() += 1;
            }
            if (s.rate - e.rate).abs() > 4 {
                *out.get_mut("rate").unwrap() += 1;
            }
        }
        if s.vol != e.vol {
            *out.get_mut("vol").unwrap() += 1;
        }
        if s.pan != e.pan && (s.vol != 0 || e.vol != 0) {
            *out.get_mut("pan").unwrap() += 1;
        }
    }
    out
}

pub fn compare_ym2610_samples(src: &VgmFile, exp: &VgmFile) -> IndexMap<String, Value> {
    // Match every source ADPCM-A/B gate to the export and compare it.
    //
    // The export writes the ADPCM-A memory as block 0x82 and the ADPCM-B
    // memory as block 0x83.  The source uses 0x82 for A and 0x83 for B when
    // that block is present.  Matching uses the time window and the extracted
    // payload, and a repacked sample memory does not affect the result.
    if !src
        .writes
        .iter()
        .any(|w| w.chip == "ym2610" || w.chip == "ym2610_ssg")
    {
        return IndexMap::new();
    }

    fn hits(vgm: &VgmFile) -> (Vec<AudHit>, Vec<AudHit>) {
        let rom_a = assemble_rom(vgm, "ym2610_adpcm", 0, 0, false, true);
        let rom_b = assemble_rom(vgm, "ym2610_deltat", 0, 0, false, true);
        let rom_b_arg: &[u8] = if rom_b.is_empty() { &rom_a } else { &rom_b };
        let (a_s, b_s, _w) = collect_ym2610_adpcm(vgm, &rom_a, Some(rom_b_arg));
        let mut av: Vec<AudHit> = Vec::new();
        for smp in &a_s {
            let data = Rc::new(smp.data.clone());
            for h in &smp.hits {
                if !audible(h.pan, &h.pan_pts) {
                    continue;
                }
                av.push(AudHit {
                    sample_time: h.sample_time,
                    vol: h.vol,
                    pan: h.pan,
                    rate: 0,
                    channel: h.channel as i32,
                    data: data.clone(),
                });
            }
        }
        let mut bv: Vec<AudHit> = Vec::new();
        for smp in &b_s {
            let data = Rc::new(smp.data.clone());
            for h in &smp.hits {
                if !audible(h.pan, &h.pan_pts) {
                    continue;
                }
                bv.push(AudHit {
                    sample_time: h.sample_time,
                    vol: h.vol,
                    pan: h.pan,
                    rate: h.rate,
                    channel: 0,
                    data: data.clone(),
                });
            }
        }
        (av, bv)
    }

    let (sa, sb) = hits(src);
    let (ea, eb) = hits(exp);
    let (pa, a_extra) = match_aud(&sa, &ea, |h| h.channel, true, true);
    let (pb, b_extra) = match_aud(&sb, &eb, |_h| 0i32, true, true);
    let mut out: IndexMap<String, Value> = IndexMap::new();
    out.insert(
        "a".to_string(),
        map_value(&tally_adpcm(&sa, &ea, &pa, a_extra, false)),
    );
    out.insert(
        "b".to_string(),
        map_value(&tally_adpcm(&sb, &eb, &pb, b_extra, true)),
    );
    out
}

pub fn compare_ym2608_samples(src: &VgmFile, exp: &VgmFile) -> IndexMap<String, Value> {
    // Match the OPNA's rhythm hits and delta-T gates to the export.
    //
    // A rhythm voice is named by its column, so the comparison pairs on voice and
    // checks the level each side computes: both sides fold their own `$11` into it,
    // which is what lets the module leave that register at Furnace's full-level
    // default and still sound like the source. A delta-T gate pairs by the span it
    // reads and the step value that drives it, because those - not the address,
    // which Furnace repacks - decide what the chip plays.
    if !src
        .writes
        .iter()
        .any(|w| w.chip == "ym2608" || w.chip == "ym2608_ssg")
    {
        return IndexMap::new();
    }

    // Furnace plays every row it owns, and a module's final pattern is padded to
    // the declared length, so an export can run a second and a half past the end
    // of the recording and key the loop's notes once more. Gates out there have no
    // counterparty in the source, so both sides are cut to the shorter log.
    let limit = std::cmp::min(
        if src.total_samples != 0 {
            src.total_samples
        } else {
            1i64 << 62
        },
        if exp.total_samples != 0 {
            exp.total_samples
        } else {
            1i64 << 62
        },
    );

    fn rhythm(vgm: &VgmFile, limit: i64) -> Vec<AudHit> {
        let (hits, _w) = collect_ym2608_rhythm(vgm);
        let mut out: Vec<AudHit> = Vec::new();
        for h in hits {
            if !audible(h.pan, &h.pan_pts) || h.sample_time > limit {
                continue;
            }
            out.push(AudHit {
                sample_time: h.sample_time,
                vol: h.vol,
                pan: h.pan,
                rate: 0,
                channel: h.voice as i32,
                data: Rc::new(Vec::new()),
            });
        }
        out
    }

    fn deltat(vgm: &VgmFile, limit: i64) -> Vec<AudHit> {
        let rom = assemble_rom(vgm, "ym2608_deltat", 0, 0, false, true);
        let (smp, _w) = collect_ym2608_delta_t(vgm, &rom);
        let mut out: Vec<AudHit> = Vec::new();
        for s in &smp {
            let data = Rc::new(s.data.clone());
            for h in &s.hits {
                if !audible(h.pan, &h.pan_pts) || h.sample_time > limit {
                    continue;
                }
                out.push(AudHit {
                    sample_time: h.sample_time,
                    vol: h.vol,
                    pan: h.pan,
                    rate: h.rate,
                    channel: 0,
                    data: data.clone(),
                });
            }
        }
        out
    }

    let sr = rhythm(src, limit);
    let sd = deltat(src, limit);
    let er = rhythm(exp, limit);
    let ed = deltat(exp, limit);
    let (pr, r_extra) = match_aud(&sr, &er, |h| h.channel, false, false);
    // A delta-T voice is its waveform: the bytes it reads out and how many
    // Furnace will play. Rate is deliberately not part of the key - a wrong step
    // value still plays the same bytes, and tally is where that gets caught.
    let (pd, d_extra) = match_aud(
        &sd,
        &ed,
        |h| {
            (
                h.data[..std::cmp::min(32, h.data.len())].to_vec(),
                deltat_store_body(&h.data).len(),
            )
        },
        false,
        false,
    );
    let mut out: IndexMap<String, Value> = IndexMap::new();
    out.insert(
        "rhythm".to_string(),
        map_value(&tally_opna(&sr, &er, &pr, r_extra, false)),
    );
    out.insert(
        "delta_t".to_string(),
        map_value(&tally_opna(&sd, &ed, &pd, d_extra, true)),
    );
    out
}

pub fn compare_keyons(src: &VgmFile, exp: &VgmFile) -> IndexMap<String, Value> {
    let fm = compare_fm(src, exp);
    let psg = compare_psg(src, exp);
    let dac = compare_dac(src, exp);
    let ym2610 = compare_ym2610_samples(src, exp);
    let ym2608 = compare_ym2608_samples(src, exp);
    let fields = fm
        .get("fields")
        .cloned()
        .unwrap_or_else(|| json!({}));
    let missed = get_i64(&fm, "missed") + get_i64(&psg, "missed");
    let matched = get_i64(&fm, "matched") + get_i64(&psg, "matched");
    let startup = fm.get("startup").cloned().unwrap_or_else(|| json!({}));
    let mut out: IndexMap<String, Value> = IndexMap::new();
    out.insert("matched".to_string(), json!(matched));
    out.insert("missed".to_string(), json!(missed));
    out.insert("fields".to_string(), fields.clone());
    out.insert("startup".to_string(), startup);
    out.insert("tl".to_string(), json!(value_i64(&fields, "tl")));
    out.insert("psg_missed".to_string(), json!(get_i64(&psg, "missed")));
    out.insert("psg_period".to_string(), json!(get_i64(&psg, "period")));
    out.insert("psg_vol".to_string(), json!(get_i64(&psg, "vol")));
    out.insert("psg_noise".to_string(), json!(get_i64(&psg, "noise")));
    out.insert("dac_matched".to_string(), json!(get_i64(&dac, "matched")));
    out.insert("dac_missed".to_string(), json!(get_i64(&dac, "missed")));
    out.insert("dac_extra".to_string(), json!(get_i64(&dac, "extra")));
    out.insert("dac_rate_bad".to_string(), json!(get_i64(&dac, "rate_bad")));
    if !ym2610.is_empty() {
        out.insert("ym2610".to_string(), Value::Object(to_object(&ym2610)));
    }
    if !ym2608.is_empty() {
        out.insert("ym2608".to_string(), Value::Object(to_object(&ym2608)));
    }
    out
}

fn to_object(m: &IndexMap<String, Value>) -> serde_json::Map<String, Value> {
    m.iter().map(|(k, v)| (k.clone(), v.clone())).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vgm::{ChipWrite, DacStreamPlay};

    fn ym2612_vgm(writes: Vec<ChipWrite>, total_samples: i64) -> VgmFile {
        VgmFile {
            total_samples,
            writes,
            ..VgmFile::default()
        }
    }

    fn psg_write(sample: i64, val: u32) -> ChipWrite {
        // SN76489 writes carry the data byte in `val`; there is no register
        // address.
        ChipWrite::new(sample, "sn76489", 0, 0, val)
    }

    #[test]
    fn canon_only_keeps_effective_register_bits() {
        assert_eq!(_canon("ks_ar", 0x3F), 0x1F);
        assert_eq!(_canon("am_d1r", 0x60), 0x00);
        assert_eq!(_canon("am_d1r", 0x70), 0x10);
        // DT=4 is negative zero.
        assert_eq!(_canon("dt_mul", 0x41), 0x01);
        assert_eq!(_canon("dt_mul", 0x05), 0x05);
        assert_eq!(_canon("sr", 0x1F), 0x1F);
    }

    #[test]
    fn snapshots_apply_group_writes_before_reading_the_keyon() {
        let vgm = ym2612_vgm(
            vec![
                // Operator 0's TL on register channel 4 (port 1, offset 1).
                ChipWrite::new(50, "ym2612", 0, 0x141, 0x20),
                // KON for register channel 5; the audit renumbers 4-6 to
                // FM channels 3-5 (the fourth operator slot is not a channel).
                ChipWrite::new(100, "ym2612", 0, 0x28, 0x75),
            ],
            2000,
        );
        let snaps = fm_snapshots(&vgm);
        assert_eq!(snaps.len(), 1);
        assert_eq!(snaps[0].sample, 100);
        assert_eq!(snaps[0].ch, 4);
        assert_eq!(snaps[0].mask, 0x7);
        assert_eq!(snaps[0].ops[0].tl, 0x20);
    }

    #[test]
    fn compare_fm_pairs_the_same_register_image() {
        let bank = |tl: u32| {
            vec![
                ChipWrite::new(50, "ym2612", 0, 0x40, tl),
                ChipWrite::new(100, "ym2612", 0, 0x28, 0xF0),
            ]
        };
        let src = ym2612_vgm(bank(0x20), 2000);
        let same = ym2612_vgm(bank(0x20), 2000);
        let changed = ym2612_vgm(bank(0x30), 2000);
        let ok = compare_fm(&src, &same);
        assert_eq!(ok["matched"].as_i64(), Some(1));
        assert_eq!(ok["missed"].as_i64(), Some(0));
        assert_eq!(ok["fields"], json!({}));
        let bad = compare_fm(&src, &changed);
        assert_eq!(bad["matched"].as_i64(), Some(1));
        assert_eq!(bad["fields"]["tl"].as_i64(), Some(1));
        assert_eq!(bad["tl"].as_i64(), Some(1));
    }

    #[test]
    fn compare_psg_pairs_the_volume_gated_note() {
        let writes = vec![
            psg_write(0, 0x8F),
            psg_write(0, 0x01),
            psg_write(0, 0x90),
            psg_write(1000, 0x9F),
        ];
        let src = VgmFile {
            sn76489_clock: 3_579_545,
            total_samples: 2000,
            writes: writes.clone(),
            ..VgmFile::default()
        };
        let exp = VgmFile {
            sn76489_clock: 3_579_545,
            total_samples: 2000,
            writes,
            ..VgmFile::default()
        };
        let ok = compare_psg(&src, &exp);
        assert_eq!(ok["matched"].as_i64(), Some(1));
        assert_eq!(ok["missed"].as_i64(), Some(0));
        assert_eq!(ok["period"].as_i64(), Some(0));

        let none = compare_psg(&VgmFile::default(), &VgmFile::default());
        assert_eq!(none["matched"].as_i64(), Some(0));
        assert_eq!(none["extra"].as_i64(), Some(0));
    }

    #[test]
    fn dac_starts_count_blocks_and_ym2612_streams() {
        let exp = VgmFile {
            dac_blocks: vec![vec![1, 2, 3], Vec::new()],
            dac_block_plays: vec![(100, 0, 1000), (200, 5, 1000), (300, 1, 1000)],
            dac_stream_plays: vec![
                DacStreamPlay {
                    sample: 150,
                    chip_type: DAC_STREAM_YM2612,
                    length: 77,
                    mode: 1,
                    ..DacStreamPlay::default()
                },
                DacStreamPlay {
                    sample: 120,
                    chip_type: DAC_STREAM_YM2612,
                    length: 50,
                    mode: 3,
                    ..DacStreamPlay::default()
                },
                // Another chip's stream is not a YM2612 DAC note.
                DacStreamPlay {
                    sample: 130,
                    chip_type: 0,
                    length: 10,
                    mode: 1,
                    ..DacStreamPlay::default()
                },
                DacStreamPlay {
                    sample: 2000,
                    chip_type: DAC_STREAM_YM2612,
                    length: 10,
                    mode: 1,
                    ..DacStreamPlay::default()
                },
            ],
            ..VgmFile::default()
        };
        let starts = _dac_starts(&exp, 1000);
        assert_eq!(
            starts,
            vec![(100, 3), (120, -1), (150, 77), (200, -1), (300, 0)]
        );
    }
}
