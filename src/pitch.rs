//! Furnace's period-mode pitch table, ported, plus its inverse.
//!
//! Furnace turns a note plus the `E5xx` fine pitch into a chip register with
//! `DivPitchTable` (`pitchTable.cpp`): for period chips the table holds
//! `round((clock / f) / divider)` for one octave of notes, a note above it is
//! that value shifted right by whole octaves, and the adjacent-note difference
//! supplies the fine pitch. Platforms whose register is "period - 1" (the SCC)
//! subtract one before writing.
//!
//! Notes use Furnace's own numbering (0 = C-(-5), 108 = C-4); in period mode a
//! note stands for the frequency `(tuning / 16) * 2 ** ((note - 57) / 12)`,
//! which is the convention the AY and SCC platforms share.
//!
//! `furnace_period_note` inverts the table, so a VGM register value can be
//! written back as the note/E5 pair that reproduces it exactly (up to the
//! table's own resolution) instead of through a closed form that ignores the
//! integer shifts.

use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex, MutexGuard};

use crate::parity;

pub const FURNACE_TUNING: f64 = 440.0;
pub const NOTE_MAX: i32 = 179;
pub const E5_CENTER: i32 = 0x80;
/// Furnace's default clock for the AY-8910 and SCC systems: COLOR_NTSC / 2.
/// The engine builds its pitch table from the double value, so writing
/// `customClock` for a rip whose clock is within a hertz of this would change
/// the table by a unit here and there for no audible reason.
pub const FURNACE_CLOCK_DEFAULT: f64 = 315_000_000.0 / 88.0 / 2.0;
pub const CLOCK_DEFAULT_TOLERANCE: i32 = 1; // Hz

/// The clock the module will run at: Furnace's default when the rip's own
/// clock is within a hertz of it, otherwise the rip's clock (which the writer
/// then stores as `customClock`).
pub fn module_clock(clock: f64) -> f64 {
    if clock == 0.0 || (clock - FURNACE_CLOCK_DEFAULT).abs() <= CLOCK_DEFAULT_TOLERANCE as f64 {
        return FURNACE_CLOCK_DEFAULT;
    }
    clock
}

type OffsetCache = Mutex<HashMap<(i32, i32), i32>>;

/// The `PeriodPitchTable` methods that `FrequencyPitchTable` inherits
/// unchanged in Python: `value` and `offset_for` go through the overridden
/// `_value_offset`.
trait PitchTableCore {
    fn value_offset(&self, offset: i32) -> i32;
    fn note_fraction(&self, value: i32) -> f64;
    fn offsets(&self) -> &OffsetCache;

    /// `PeriodPitchTable.value` in pitch.py.
    fn value(&self, note: i32, fine: i32) -> i32 {
        self.value_offset((note << 7) + fine)
    }

    /// `PeriodPitchTable.offset_for` in pitch.py.
    fn offset_for(&self, note: i32, target: i32) -> i32 {
        let target = i32::max(1, target);
        let key = (note, target);
        {
            let cache = lock(self.offsets());
            if let Some(&hit) = cache.get(&key) {
                return hit;
            }
        }
        // A semitone of offset moves the register by the local table
        // difference, so a log2 estimate lands within a couple of hundred units
        // and the value moves monotonically from there. Period tables fall
        // with a higher note; frequency tables rise.
        let here = self.value(note, 0);
        let first = self.value(note, -128);
        let last = self.value(note, 127);
        let descending = last < first;
        let ratio = if descending {
            here as f64 / target as f64
        } else {
            target as f64 / here as f64
        };
        let span = if here > 0 && target > 0 {
            128.0 * 12.0 * ratio.log2()
        } else {
            0.0
        };
        let est = ((note as i64) << 7) + parity::py_int(parity::py_round(span));
        let est = est.clamp(-24000, 24000) as i32;
        let mut lo = i32::max(-32768, est - 400);
        let mut hi = i32::min(32768, est + 400);
        while lo < hi {
            let mid = parity::py_floor_div((lo as i64) + (hi as i64), 2) as i32;
            if (self.value_offset(mid) > target) == descending {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        let mut best: Option<(i64, i32, i32)> = None; // (err, |off|, off)
        for off in i32::max(-32768, lo - 4)..=i32::min(32768, lo + 4) {
            let err = ((self.value_offset(off) as i64) - (target as i64)).abs();
            let cand = (err, off.abs(), off);
            if best.is_none_or(|b| cand < b) {
                best = Some(cand);
            }
        }
        let off = best.expect("offset candidate range is non-empty").2;
        lock(self.offsets()).insert(key, off);
        off
    }
}

/// Furnace shifts both values right, rounding the carry in.
fn shift_pair(mut root: i32, mut diff: i32, octave: i32) -> (i32, i32) {
    if octave > 0 {
        let mut carry = (root & (1 << (octave - 1))) != 0;
        root >>= octave;
        if carry {
            root += 1;
        }
        carry = (diff & (1 << (octave - 1))) != 0;
        diff >>= octave;
        if carry {
            diff += 1;
        }
    }
    (root, diff)
}

/// `DivPitchTable` in period mode, for one (tuning, clock, divider).
#[derive(Debug)]
pub struct PeriodPitchTable {
    pub tuning: f64,
    pub clock: f64,
    pub divider: f64,
    pub max: i32,
    pub shift: i32,
    pub pitch: Vec<i32>,
    pub diff: Vec<i32>,
    offsets: OffsetCache,
}

impl PeriodPitchTable {
    pub fn new(tuning: f64, clock: f64, divider: f64, maximum: i32) -> Self {
        let mut shift = 0;
        while shift < 14 {
            let nbase = (shift - 4) * 12;
            let fbase = (tuning * 0.0625) * 2.0f64.powf(((nbase + 3) as f64) / 12.0);
            if parity::py_round((clock / fbase) / divider) <= maximum as f64 {
                break;
            }
            shift += 1;
        }
        let mut pitch = vec![0i32; 13];
        let mut diff = vec![0i32; 13];
        for i in 0..13 {
            let nbase = i + (shift - 5) * 12;
            let fbase = (tuning * 0.0625) * 2.0f64.powf(((nbase + 3) as f64) / 12.0);
            pitch[i as usize] = parity::py_round((clock / fbase) / divider) as i32;
        }
        for i in 0..12 {
            diff[i as usize] = pitch[i as usize + 1] - pitch[i as usize];
        }
        Self {
            tuning,
            clock,
            divider,
            max: maximum,
            shift,
            pitch,
            diff,
            offsets: Mutex::new(HashMap::new()),
        }
    }

    /// Register value Furnace computes for a note and fine pitch.
    ///
    /// `fine` is the engine's `pitch` value, i.e. `E5 - 0x80`: a signed
    /// offset in 1/128 semitone, so `fine=0` is neutral and the pair
    /// `(note, fine)` can carry up to a semitone above or below the note.
    pub fn value(&self, note: i32, fine: i32) -> i32 {
        PitchTableCore::value(self, note, fine)
    }

    /// Note plus fraction a register value stands for, as a float note.
    pub fn note_fraction(&self, value: i32) -> f64 {
        PitchTableCore::note_fraction(self, value)
    }

    /// Pitch offset (the engine's `pitch` units, 1/128 semitone) whose
    /// register is closest to `target`.
    ///
    /// This is what a pitch macro or an `E5` writes: the offset rides on top
    /// of the note, so a value beyond one semitone simply walks into the next
    /// note's entry of the table.
    pub fn offset_for(&self, note: i32, target: i32) -> i32 {
        PitchTableCore::offset_for(self, note, target)
    }
}

impl PitchTableCore for PeriodPitchTable {
    /// `value` for a raw engine offset (note and pitch in one number).
    fn value_offset(&self, offset: i32) -> i32 {
        let mut coarse = offset >> 7;
        if coarse < 0 {
            coarse = 0;
        }
        let frac = offset & 127;
        let index = parity::py_mod(coarse as i64, 12) as i32;
        let octave = parity::py_floor_div(coarse as i64, 12) as i32 - self.shift;
        if octave < 0 {
            return self.max;
        }
        let (root, diff) = shift_pair(
            self.pitch[index as usize],
            self.diff[index as usize],
            octave,
        );
        root + ((diff * frac) >> 7)
    }

    fn note_fraction(&self, value: i32) -> f64 {
        if value <= 0 {
            return NOTE_MAX as f64;
        }
        let freq = self.clock / (self.divider * value as f64);
        return 57.0 + 12.0 * (freq / (self.tuning / 16.0)).log2();
    }

    fn offsets(&self) -> &OffsetCache {
        &self.offsets
    }
}

/// `DivPitchTable` in frequency mode, for one (tuning, clock, divider).
///
/// The X1-010 writes `freq` directly: its register value is the pitch-table
/// value, not a period. Furnace builds this table with
/// `fbase * divider / clock` and shifts the root *down* for higher octaves.
#[derive(Debug)]
pub struct FrequencyPitchTable {
    pub tuning: f64,
    pub clock: f64,
    pub divider: f64,
    pub max: i32,
    pub shift: i32,
    pub pitch: Vec<i32>,
    pub diff: Vec<i32>,
    offsets: OffsetCache,
}

impl FrequencyPitchTable {
    pub fn new(tuning: f64, clock: f64, divider: f64, maximum: i32) -> Self {
        let mut shift = 14;
        while shift > 0 {
            let nbase = (shift - 5) * 12;
            let fbase = tuning * 2.0f64.powf(((nbase + 3) as f64) / 12.0);
            if parity::py_round(fbase * (divider / clock)) <= maximum as f64 {
                break;
            }
            shift -= 1;
        }
        let mut pitch = vec![0i32; 13];
        let mut diff = vec![0i32; 13];
        for i in 0..13 {
            let nbase = i + (shift - 5) * 12;
            let fbase = tuning * 2.0f64.powf(((nbase + 3) as f64) / 12.0);
            pitch[i as usize] = parity::py_round(fbase * (divider / clock)) as i32;
        }
        for i in 0..12 {
            diff[i as usize] = pitch[i as usize + 1] - pitch[i as usize];
        }
        Self {
            tuning,
            clock,
            divider,
            max: maximum,
            shift,
            pitch,
            diff,
            offsets: Mutex::new(HashMap::new()),
        }
    }

    /// Register value Furnace computes for a note and fine pitch.
    pub fn value(&self, note: i32, fine: i32) -> i32 {
        PitchTableCore::value(self, note, fine)
    }

    /// Note plus fraction a frequency-register value stands for.
    pub fn note_fraction(&self, value: i32) -> f64 {
        PitchTableCore::note_fraction(self, value)
    }

    /// Pitch offset whose register is closest to `target`.
    pub fn offset_for(&self, note: i32, target: i32) -> i32 {
        PitchTableCore::offset_for(self, note, target)
    }
}

impl PitchTableCore for FrequencyPitchTable {
    fn value_offset(&self, offset: i32) -> i32 {
        let mut coarse = offset >> 7;
        if coarse < 0 {
            coarse = 0;
        }
        let frac = offset & 127;
        let index = parity::py_mod(coarse as i64, 12) as i32;
        let octave = self.shift - parity::py_floor_div(coarse as i64, 12) as i32;
        if octave < 0 {
            return self.max;
        }
        let (root, diff) = shift_pair(
            self.pitch[index as usize],
            self.diff[index as usize],
            octave,
        );
        root + ((diff * frac) >> 7)
    }

    fn note_fraction(&self, value: i32) -> f64 {
        if value <= 0 {
            return 0.0;
        }
        let freq = value as f64 * self.clock / self.divider;
        return 57.0 + 12.0 * (freq / self.tuning).log2();
    }

    fn offsets(&self) -> &OffsetCache {
        &self.offsets
    }
}

/// A table cache key. Python uses `(round(clock), divider, maximum)`; the
/// divider is a float, so its bits stand in for it (with `-0.0` normalized to
/// `0.0`, which Python's `==` treats as the same key).
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
struct TableKey {
    clock: i64,
    divider_bits: u64,
    maximum: i32,
}

fn divider_bits(divider: f64) -> u64 {
    if divider == 0.0 {
        0.0f64.to_bits()
    } else {
        divider.to_bits()
    }
}

fn table_key(clock: f64, divider: f64, maximum: i32) -> TableKey {
    TableKey {
        clock: parity::py_int(parity::py_round(clock)),
        divider_bits: divider_bits(divider),
        maximum,
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
struct PeriodNoteKey {
    clock: i64,
    divider_bits: u64,
    maximum: i32,
    value: i32,
    period_offset: i32,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
struct FrequencyNoteKey {
    clock: i64,
    divider_bits: u64,
    maximum: i32,
    value: i32,
}

static TABLES: LazyLock<Mutex<HashMap<TableKey, Arc<PeriodPitchTable>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static NOTES: LazyLock<Mutex<HashMap<PeriodNoteKey, (i32, i32)>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static FREQ_TABLES: LazyLock<Mutex<HashMap<TableKey, Arc<FrequencyPitchTable>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static FREQ_NOTES: LazyLock<Mutex<HashMap<FrequencyNoteKey, (i32, i32)>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn lock<T>(cache: &Mutex<T>) -> MutexGuard<'_, T> {
    cache.lock().expect("pitch table cache poisoned")
}

pub fn pitch_table(clock: f64, divider: f64, maximum: i32) -> Arc<PeriodPitchTable> {
    let key = table_key(clock, divider, maximum);
    let mut cache = lock(&TABLES);
    if let Some(tbl) = cache.get(&key) {
        return Arc::clone(tbl);
    }
    let tbl = Arc::new(PeriodPitchTable::new(
        FURNACE_TUNING, clock, divider, maximum,
    ));
    cache.insert(key, Arc::clone(&tbl));
    tbl
}

pub fn frequency_pitch_table(
    clock: f64,
    divider: f64,
    maximum: i32,
) -> Arc<FrequencyPitchTable> {
    let key = table_key(clock, divider, maximum);
    let mut cache = lock(&FREQ_TABLES);
    if let Some(tbl) = cache.get(&key) {
        return Arc::clone(tbl);
    }
    let tbl = Arc::new(FrequencyPitchTable::new(
        FURNACE_TUNING, clock, divider, maximum,
    ));
    cache.insert(key, Arc::clone(&tbl));
    tbl
}

/// (fine, |value - target|) closest to target, by binary search + neighbours.
///
/// `fine` is the signed E5-0x80 offset. The table's value falls as the fine
/// pitch rises (shorter period), and the octave rounding can flatten the
/// difference to zero for high notes; that case has no better fine than the
/// center.
fn closest_fine<T: PitchTableCore + ?Sized>(tbl: &T, note: i32, target: i32) -> (i32, i64) {
    let first = tbl.value(note, -128);
    let last = tbl.value(note, 127);
    if first == last {
        return (0, ((first as i64) - (target as i64)).abs());
    }
    let descending = last < first;
    let mut lo = -128;
    let mut hi = 127;
    while lo < hi {
        let mid = parity::py_floor_div((lo as i64) + (hi as i64), 2) as i32;
        if (tbl.value(note, mid) > target) == descending {
            lo = mid + 1;
        } else {
            hi = mid;
        }
    }
    let mut best: Option<(i32, i64)> = None;
    for fine in i32::max(-128, lo - 3)..=i32::min(127, lo + 3) {
        let err = ((tbl.value(note, fine) as i64) - (target as i64)).abs();
        if best.is_none_or(|(best_fine, best_err)| {
            err < best_err || (err == best_err && fine.abs() < best_fine.abs())
        }) {
            best = Some((fine, err));
        }
    }
    best.expect("fine candidate range is non-empty")
}

/// Inverse of the pitch table: register value -> (Furnace note, E5).
///
/// `period_offset` is what the platform subtracts before writing (1 for the
/// SCC, 0 for the AY), so the table must produce `value + period_offset`.
/// The returned pair is the one whose register is closest to the wanted
/// value; an exact match wins, then the smaller fine pitch, then the note
/// whose frequency is closest. The second element is the E5 effect byte
/// (0x80 = no fine tune), which the engine turns back into `E5 - 0x80`.
pub fn furnace_period_note(
    clock: f64,
    divider: f64,
    value: i32,
    period_offset: i32,
    maximum: i32,
) -> (i32, i32) {
    let cache_key = PeriodNoteKey {
        clock: parity::py_int(parity::py_round(clock)),
        divider_bits: divider_bits(divider),
        maximum,
        value,
        period_offset,
    };
    {
        let cache = lock(&NOTES);
        if let Some(&hit) = cache.get(&cache_key) {
            return hit;
        }
    }
    let tbl = pitch_table(clock, divider, maximum);
    let target = value + period_offset;
    let base = tbl.note_fraction(target);
    let mut best: Option<(i64, i32, i32, i32)> = None; // (err, |fine|, note, fine)
    let center = parity::py_int(base.floor());
    let mut lo = i64::max(0, center - 2);
    let mut hi = i64::min(NOTE_MAX as i64, center + 2);
    if lo > hi {
        // Out of the note range: the closest note the format can hold, with the
        // fine pitch picked to get as near as possible.
        let clamped = center.clamp(0, NOTE_MAX as i64);
        lo = clamped;
        hi = clamped;
    }
    for note in lo..=hi {
        let note = note as i32;
        let (fine, err) = closest_fine(&*tbl, note, target);
        let cand = (err, fine.abs(), note, fine);
        if best.is_none_or(|b| cand < b) {
            best = Some(cand);
        }
    }
    let best = best.expect("note candidate range is non-empty");
    let result = (best.2, E5_CENTER + best.3);
    lock(&NOTES).insert(cache_key, result);
    result
}

/// Inverse of Furnace's frequency-mode pitch table (the X1-010 wave path).
///
/// Returns the `(note, E5)` pair whose register value is closest to
/// `value`. Unlike a period table there is no `period_offset`; the X1
/// platform writes the table value to the chip unchanged.
pub fn furnace_frequency_note(
    clock: f64,
    divider: f64,
    value: i32,
    maximum: i32,
) -> (i32, i32) {
    let cache_key = FrequencyNoteKey {
        clock: parity::py_int(parity::py_round(clock)),
        divider_bits: divider_bits(divider),
        maximum,
        value,
    };
    {
        let cache = lock(&FREQ_NOTES);
        if let Some(&hit) = cache.get(&cache_key) {
            return hit;
        }
    }
    let tbl = frequency_pitch_table(clock, divider, maximum);
    let target = i32::max(1, value);
    let base = tbl.note_fraction(target);
    let mut best: Option<(i64, i32, i32, i32)> = None; // (err, |fine|, note, fine)
    let center = parity::py_int(base.floor());
    let mut lo = i64::max(0, center - 2);
    let mut hi = i64::min(NOTE_MAX as i64, center + 2);
    if lo > hi {
        let clamped = center.clamp(0, NOTE_MAX as i64);
        lo = clamped;
        hi = clamped;
    }
    for note in lo..=hi {
        let note = note as i32;
        let (fine, err) = closest_fine(&*tbl, note, target);
        let cand = (err, fine.abs(), note, fine);
        if best.is_none_or(|b| cand < b) {
            best = Some(cand);
        }
    }
    let best = best.expect("note candidate range is non-empty");
    let result = (best.2, E5_CENTER + best.3);
    lock(&FREQ_NOTES).insert(cache_key, result);
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn module_clock_uses_the_furnace_default_inside_one_hertz() {
        assert_eq!(module_clock(0.0), FURNACE_CLOCK_DEFAULT);
        assert_eq!(module_clock(FURNACE_CLOCK_DEFAULT), FURNACE_CLOCK_DEFAULT);
        assert_eq!(module_clock(1_789_773.0), FURNACE_CLOCK_DEFAULT);
        assert_eq!(module_clock(3_579_545.0), 3_579_545.0);
        assert_eq!(module_clock(1_789_775.0), 1_789_775.0);
    }

    #[test]
    fn period_note_round_trips_a_register_value() {
        let table = pitch_table(3_579_545.0, 8.0, 0xFFF);
        let (note, e5) = furnace_period_note(3_579_545.0, 8.0, 1000, 0, 0xFFF);
        assert_eq!((note, e5), (105, 164));
        assert_eq!(table.value(note, e5 - E5_CENTER), 1000);

        let (note, e5) = furnace_period_note(3_579_545.0, 8.0, 500, 0, 0xFFF);
        assert_eq!((note, e5), (117, 164));
        assert_eq!(table.value(note, e5 - E5_CENTER), 500);
    }

    #[test]
    fn pitch_table_matches_at_low_and_high_notes() {
        let table = pitch_table(3_579_545.0, 8.0, 0xFFF);
        assert_eq!(table.shift, 6);
        assert_eq!(
            table.pitch,
            vec![6841, 6457, 6095, 5753, 5430, 5125, 4837, 4566, 4310, 4068, 3839, 3624, 3420]
        );
        assert_eq!(table.diff[0], -384);
        // Below the table's first octave the value clamps to the maximum.
        assert_eq!(table.value(0, 0), 4095);
        assert_eq!(table.value(108, 0), 855);
        assert_eq!(table.value(179, 127), 13);
    }

    #[test]
    fn default_clock_is_half_the_color_ntsc_clock() {
        assert_eq!(FURNACE_CLOCK_DEFAULT, 315_000_000.0 / 88.0 / 2.0);
        assert!((FURNACE_CLOCK_DEFAULT - 1_789_772.727_272_727_3).abs() < 1e-9);
        assert_eq!(CLOCK_DEFAULT_TOLERANCE, 1);
        assert_eq!(NOTE_MAX, 179);
        assert_eq!(E5_CENTER, 0x80);
    }
}
