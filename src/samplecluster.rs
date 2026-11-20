//! Optional aggressive clustering for short 8-bit PCM sample maps.
//!
//! Port of `vgm2151fur/samplecluster.py` (v0.11.0). This is intentionally
//! lossy: samples always play from frame 0, so a short fragment can only
//! reuse a longer body whose opening frames sound like it. Use it only when
//! the caller asks for it; the normal conversion keeps every body separate.

use std::collections::HashSet;

use indexmap::IndexMap;

/// `DEFAULT_TARGET`: preferred anchor count when the caller gives none.
pub const DEFAULT_TARGET: i32 = 32;
/// `DEFAULT_HOT`: how many of the anchors come from the "hottest" bodies.
pub const DEFAULT_HOT: i32 = 16;
/// `FRAGMENT_LIMIT`: frames compared when matching a fragment to an anchor.
pub const FRAGMENT_LIMIT: i32 = 128;
/// `FRAGMENT_SHIFT`: shifts searched on either side of the opening frames.
pub const FRAGMENT_SHIFT: i32 = 8;

/// Python byte indexing: negative indices count from the end; out-of-range
/// raises `IndexError`.
fn py_index(len: usize, i: i64) -> usize {
    let len = len as i64;
    let j = if i < 0 { len + i } else { i };
    assert!((0..len).contains(&j), "list index out of range");
    j as usize
}

/// Python slice `items[:end]` for a list: negative `end` trims from the tail.
fn py_slice_end(len: usize, end: i64) -> usize {
    if end < 0 {
        (len as i64 + end).max(0) as usize
    } else {
        (end as usize).min(len)
    }
}

/// Python `a[i] if a[i] < 128 else a[i] - 256`: signed 8-bit sample value.
fn signed_byte(x: u8) -> i32 {
    if x < 128 { x as i32 } else { x as i32 - 256 }
}

/// Python `array.array("h")` over the even-length prefix of a byte string.
///
/// `array("h")` uses the native byte order; the port targets little-endian
/// (x86-64), matching `i16::from_le_bytes`.
fn words16(a: &[u8]) -> Vec<i16> {
    let even = &a[..a.len() - a.len() % 2];
    even.chunks_exact(2)
        .map(|c| i16::from_le_bytes([c[0], c[1]]))
        .collect()
}

/// Python `math.log(rates[i] / rates[j])`, including its error cases: zero
/// division and a non-positive logarithm domain.
fn py_log_ratio(num: i32, den: i32) -> f64 {
    assert!(den != 0, "division by zero");
    let ratio = num as f64 / den as f64;
    assert!(ratio > 0.0, "math domain error");
    ratio.ln()
}

/// Mean absolute difference of `a` against `b` shifted by `shift`.
///
/// Replaces `_prefix_mad`. A negative `shift` reads `b` from its tail, since
/// the Python source indexes with `b[i + shift]` and relies on negative list
/// indexing for the first `-shift` frames.
fn prefix_mad(a: &[u8], b: &[u8], shift: i32, limit: i32) -> Option<f64> {
    let shift = shift as i64;
    let n = (a.len() as i64).min(b.len() as i64 - shift).min(limit as i64);
    if n <= 0 {
        return None;
    }
    let mut total: i64 = 0;
    for i in 0..n {
        let av = signed_byte(a[i as usize]);
        let bv = signed_byte(b[py_index(b.len(), i + shift)]);
        total += (av - bv).abs() as i64;
    }
    Some(total as f64 / n as f64)
}

/// 16-bit version: compare opening frames after removing each body's DC.
///
/// Replaces `_prefix_mad16`.
fn prefix_mad16(a: &[u8], b: &[u8], shift: i32, limit: i32) -> Option<f64> {
    let av = words16(a);
    let bv = words16(b);
    if av.is_empty() || bv.is_empty() || shift < 0 || shift as usize >= bv.len() {
        return None;
    }
    let n = (av.len() as i64)
        .min(bv.len() as i64 - shift as i64)
        .min(limit as i64);
    if n <= 0 {
        return None;
    }
    let abase = av[0] as i32;
    let bbase = bv[shift as usize] as i32;
    let mut total: i64 = 0;
    for i in 0..n as usize {
        total +=
            ((av[i] as i32 - abase) - (bv[shift as usize + i] as i32 - bbase)).abs() as i64;
    }
    Some(total as f64 / n as f64)
}

/// Lossy sample-map reduction for 16-bit PCM (PWM / uPD7759).
///
/// Replaces `cluster_fragments16`. Returns `(keep, remap)`: the surviving
/// original indices in order, and the new index of every old sample (-1 when
/// unused).
pub fn cluster_fragments16(
    bodies: &[Vec<u8>],
    rates: &[i32],
    uses: &IndexMap<i32, i64>,
    target: i32,
    hot: i32,
) -> (Vec<i32>, Vec<i32>) {
    let count = bodies.len();
    if count as i64 <= target as i64 {
        return identity(count);
    }
    assert_eq!(rates.len(), count, "rates must match bodies");
    let anchors = anchors(bodies, uses, target, hot);
    if anchors.is_empty() {
        return identity(count);
    }
    let used_anchors: Vec<i32> = anchors
        .iter()
        .copied()
        .filter(|&i| uses.get(&i).copied().unwrap_or(0) > 0)
        .collect();
    if used_anchors.is_empty() {
        return identity(count);
    }
    let mut keep = used_anchors.clone();
    keep.sort();
    let mut remap = vec![-1i32; count];
    for (new, &old) in keep.iter().enumerate() {
        remap[old as usize] = new as i32;
    }
    let anchor_set: HashSet<i32> = used_anchors.iter().copied().collect();
    for i in 0..count {
        let ii = i as i32;
        if anchor_set.contains(&ii) || uses.get(&ii).copied().unwrap_or(0) <= 0 {
            continue;
        }
        let limit = (bodies[i].len() as i64 / 2).min(FRAGMENT_LIMIT as i64) as i32;
        let mut best: Option<(f64, i32)> = None;
        for &j in &used_anchors {
            let score = (-FRAGMENT_SHIFT..=FRAGMENT_SHIFT)
                .filter_map(|shift| prefix_mad16(&bodies[i], &bodies[j as usize], shift, limit))
                .fold(None, |acc: Option<f64>, v| {
                    Some(match acc {
                        None => v,
                        Some(s) => s.min(v),
                    })
                });
            let score = match score {
                Some(s) => s,
                None => continue,
            };
            let rate_penalty = py_log_ratio(rates[i], rates[j as usize]).abs() * 40.0;
            let length_penalty = if bodies[j as usize].len() < bodies[i].len() {
                40.0
            } else {
                0.0
            };
            let effective = score + rate_penalty + length_penalty;
            if best.is_none() || effective < best.unwrap().0 {
                best = Some((effective, j));
            }
        }
        let chosen = match best {
            Some((_, j)) => j,
            None => used_anchors[0],
        };
        remap[i] = remap[chosen as usize];
    }
    (keep, remap)
}

/// The hottest bodies, then the longest ones, up to `target`.
///
/// Replaces `_anchors`. Both sorts are stable, like Python's `sorted(..., reverse=True)`.
fn anchors(bodies: &[Vec<u8>], uses: &IndexMap<i32, i64>, target: i32, hot: i32) -> Vec<i32> {
    let count = bodies.len();
    if count as i64 <= target as i64 {
        return (0..count as i32).collect();
    }
    let key_uses = |i: i32| (uses.get(&i).copied().unwrap_or(0), bodies[i as usize].len());
    let key_length = |i: i32| (bodies[i as usize].len(), uses.get(&i).copied().unwrap_or(0));

    let mut by_uses: Vec<i32> = (0..count as i32).collect();
    by_uses.sort_by(|&x, &y| key_uses(y).cmp(&key_uses(x)));
    let mut by_length: Vec<i32> = (0..count as i32).collect();
    by_length.sort_by(|&x, &y| key_length(y).cmp(&key_length(x)));

    let mut chosen: Vec<i32> = Vec::new();
    let mut seen: HashSet<i32> = HashSet::new();
    let take = py_slice_end(by_uses.len(), hot.min(target) as i64);
    for &i in &by_uses[..take] {
        chosen.push(i);
        seen.insert(i);
    }
    for &i in &by_length {
        if chosen.len() as i64 >= target as i64 {
            break;
        }
        if seen.contains(&i) {
            continue;
        }
        chosen.push(i);
        seen.insert(i);
    }
    chosen
}

/// Return `(keep, remap)` for an optional lossy sample-map reduction.
///
/// Replaces `cluster_fragments`. `keep` lists the original sample indices
/// that survive, in order. `remap[i]` is the new index for every old sample,
/// or -1 when the old sample is unused and can disappear. Every used
/// non-anchor is mapped to its best anchor by a loose opening-frames
/// comparison.
///
/// Note: Python's zero-argument `min(..., default=None)` over the shifted
/// scores is folded here; the generator's ascending shift order is preserved
/// and ties are meaningless because the score values are equal.
pub fn cluster_fragments(
    bodies: &[Vec<u8>],
    rates: &[i32],
    uses: &IndexMap<i32, i64>,
    target: i32,
    hot: i32,
) -> (Vec<i32>, Vec<i32>) {
    let count = bodies.len();
    if count as i64 <= target as i64 {
        return identity(count);
    }
    assert_eq!(rates.len(), count, "rates must match bodies");
    let anchors = anchors(bodies, uses, target, hot);
    if anchors.is_empty() {
        return identity(count);
    }
    let used_anchors: Vec<i32> = anchors
        .iter()
        .copied()
        .filter(|&i| uses.get(&i).copied().unwrap_or(0) > 0)
        .collect();
    if used_anchors.is_empty() {
        return identity(count);
    }
    let mut keep = used_anchors.clone();
    keep.sort();
    let mut remap = vec![-1i32; count];
    for (new, &old) in keep.iter().enumerate() {
        remap[old as usize] = new as i32;
    }
    let anchor_set: HashSet<i32> = used_anchors.iter().copied().collect();

    for i in 0..count {
        let ii = i as i32;
        if anchor_set.contains(&ii) || uses.get(&ii).copied().unwrap_or(0) <= 0 {
            continue;
        }
        let body = &bodies[i];
        let limit = (body.len() as i64).min(FRAGMENT_LIMIT as i64) as i32;
        let mut best: Option<(f64, i32)> = None;
        for &j in &used_anchors {
            let other = &bodies[j as usize];
            let score = (-FRAGMENT_SHIFT..=FRAGMENT_SHIFT)
                .filter_map(|shift| prefix_mad(body, other, shift, limit))
                .fold(None, |acc: Option<f64>, v| {
                    Some(match acc {
                        None => v,
                        Some(s) => s.min(v),
                    })
                });
            let score = match score {
                Some(s) => s,
                None => continue,
            };
            let rate_penalty = py_log_ratio(rates[i], rates[j as usize]).abs() * 40.0;
            let length_penalty = if other.len() < body.len() { 40.0 } else { 0.0 };
            let effective = score + rate_penalty + length_penalty;
            if best.is_none() || effective < best.unwrap().0 {
                best = Some((effective, j));
            }
        }
        let chosen = match best {
            Some((_, j)) => j,
            None => used_anchors[0],
        };
        remap[i] = remap[chosen as usize];
    }
    (keep, remap)
}

/// Python `list(range(count)), list(range(count))`.
fn identity(count: usize) -> (Vec<i32>, Vec<i32>) {
    let all: Vec<i32> = (0..count as i32).collect();
    (all.clone(), all)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn uses(pairs: &[(i32, i64)]) -> IndexMap<i32, i64> {
        pairs.iter().copied().collect()
    }

    fn frames16(values: &[i16]) -> Vec<u8> {
        values
            .iter()
            .flat_map(|v| v.to_le_bytes())
            .collect::<Vec<u8>>()
    }

    #[test]
    fn adjacent_fragments_merge() {
        // Bodies 2 and 3 repeat the opening of anchors 0 and 1, so each is
        // folded into its matching anchor instead of surviving on its own.
        let bodies = vec![
            vec![100u8; 16],
            vec![200u8; 16],
            vec![100u8; 8],
            vec![200u8; 8],
        ];
        let rates = [8000i32; 4];
        let uses = uses(&[(0, 10), (1, 5), (2, 3), (3, 2)]);
        let (keep, remap) = cluster_fragments(&bodies, &rates, &uses, 2, 2);
        assert_eq!(keep, vec![0, 1]);
        assert_eq!(remap, vec![0, 1, 0, 1]);

        // A negative shift reads `b` from its tail (Python negative indexing):
        // a[0]=1 against b[-1]=2, then a[1]=2 against b[0]=9 => (1 + 7) / 2.
        assert_eq!(prefix_mad(&[1, 2], &[9, 1, 2], -1, 8), Some(4.0));
    }

    #[test]
    fn fragments_stay_separate_at_or_below_target() {
        // No clustering once the body count fits the target, even if the
        // bodies sound identical: both keep their own slots.
        let bodies = vec![vec![100u8; 16], vec![100u8; 16]];
        let rates = [8000i32; 2];
        let uses = uses(&[(0, 10), (1, 5)]);
        let (keep, remap) = cluster_fragments(&bodies, &rates, &uses, 2, 2);
        assert_eq!(keep, vec![0, 1]);
        assert_eq!(remap, vec![0, 1]);
    }

    #[test]
    fn sixteen_bit_fragment_merges_after_dc_removal() {
        // Body 2 is anchor 0's ramp plus a constant DC offset; removing each
        // body's DC makes the opening frames identical, so it merges to 0.
        let bodies = vec![
            frames16(&[100, 200, 300, 400, 500]),
            frames16(&[0, 0, 0, 0, 0]),
            frames16(&[1000, 1100, 1200, 1300, 1400]),
        ];
        let rates = [8000i32; 3];
        let uses = uses(&[(0, 10), (1, 5), (2, 1)]);
        let (keep, remap) = cluster_fragments16(&bodies, &rates, &uses, 2, 2);
        assert_eq!(keep, vec![0, 1]);
        assert_eq!(remap, vec![0, 1, 0]);
        assert!(remap.iter().all(|&r| r >= 0));
    }

    #[test]
    fn empty_input_is_an_identity_mapping() {
        let bodies: Vec<Vec<u8>> = Vec::new();
        let rates: Vec<i32> = Vec::new();
        let uses = uses(&[]);
        assert_eq!(cluster_fragments(&bodies, &rates, &uses, 32, 16), (vec![], vec![]));
        assert_eq!(
            cluster_fragments16(&bodies, &rates, &uses, 32, 16),
            (vec![], vec![])
        );
    }
}
