//! OPL-family (YM3526/YM3812/YMF262) support: pitch mapping and chip state.
//!
//! The OPL chips have two operators per channel (an OPL3 pair can be joined
//! into one four-operator voice), a 6-bit TL and one connection bit. Their
//! register image cannot share the OPM/OPN byte fields. `OPLState` keeps the
//! flat register image a VGM log writes and reads it back through the slot and
//! channel tables of Furnace's own OPL platform (`src/engine/platform/opl.cpp`).
//! A converted channel, operator or drum is the one Furnace will drive.
//!
//! Furnace numbers OPL3 channels by pair, not by the hardware channel order:
//! `CHAN_MAP` carries that permutation and puts the port in bit 8, exactly as
//! the platform's `chanMapOPL3` does. A note is labelled with the channel's own
//! f-number. A four-operator voice reads the registers of its pair's first
//! channel, which is what the chip keys.
//!
//! Pitch works through `DivPitchTable` in frequency mode: the engine turns a
//! note plus `E5xx` (1/128 semitone in linear mode) into one number, and the
//! platform splits that into block/f-num by taking the highest power of two not
//! above it (`toFreq`). `opl_note_for` inverts the pair of steps. A log
//! register therefore comes back as the note/E5 combination that writes it
//! again.

use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex, MutexGuard};

use crate::fm::{FMPatch, OpRegs};
use crate::parity;

pub const OPL_DEFAULT_CLOCK: u32 = 3579545; // YM3526; COLOR_NTSC, Furnace's default
pub const OPL2_DEFAULT_CLOCK: u32 = 3579545; // YM3812; the AdLib/Sound Blaster clock
pub const OPL3_DEFAULT_CLOCK: u32 = 14318180; // YMF262; COLOR_NTSC * 4

pub const KIND_OPL: &str = "opl";
pub const KIND_OPL2: &str = "opl2";
pub const KIND_OPL3: &str = "opl3";
// The YMF278B/OPL4's FM half is OPL3-shaped (two banks, four-operator pairs,
// waveform select) but runs off a different divider: Furnace's DivPlatformOPL
// sets chipFreqBase to 32768*684 and its clock to 33.8688 MHz (opl.cpp:3390).
pub const KIND_OPL4: &str = "opl4";
// The Y8950's FM half is an OPL2 (waveform select and rhythm included); its
// ADPCM voice is handled outside this state.
pub const KIND_Y8950: &str = "y8950";

/// DivPlatformOPL::chipFreqBase, keyed by whether the chip has two banks. The
/// OPL3 runs eighteen channels off four times the prescaler. At its usual
/// 14.318 MHz clock a note lands on the same unit value an OPL2 finds at 3.58.
pub const FREQ_BASE_OPL2: i32 = 32768 * 72;
pub const FREQ_BASE_OPL3: i32 = 32768 * 288;

/// `FREQ_BASE[bool(opl3)]`.
pub fn freq_base(opl3: bool) -> i32 {
    if opl3 { FREQ_BASE_OPL3 } else { FREQ_BASE_OPL2 }
}

// DivPlatformOPL::setOPLType for chipType 4 overrides the OPL3 base with the
// OPL4's divider (32768*684). Using the OPL3 base here puts every FM note more
// than an octave flat, so the pitch path carries it separately.
pub const FREQ_BASE_OPL4: i32 = 32768 * 684;

/// True for the two-bank OPL chips: YMF262 and the OPL4's FM half.
pub fn opl3_like(kind: &str) -> bool {
    kind == KIND_OPL3 || kind == KIND_OPL4
}

// pitchTable.init's maximum, and the f-num ceiling of toFreq.
pub const MAX_UNIT: i32 = 0x1FFFF;
pub const MAX_FNUM: i32 = 0x3FF;
// toFreq starts a block at this unit value (OPLL_C_NUM in opl.cpp).
pub const BLOCK_MIN: i32 = 686;

/// An unused slot, as Furnace spells it.
pub const N: u8 = 255;

// slotMap: operator slot index -> register address (bit 8 selects port 1).
pub const SLOT_ADDR: [u16; 36] = [
    0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x08, 0x09, 0x0A, 0x0B, 0x0C, 0x0D, 0x10, 0x11, 0x12,
    0x13, 0x14, 0x15, 0x100, 0x101, 0x102, 0x103, 0x104, 0x105, 0x108, 0x109, 0x10A, 0x10B, 0x10C,
    0x10D, 0x110, 0x111, 0x112, 0x113, 0x114, 0x115,
];

// slots[operator row][Furnace channel] -> slot index.  The OPL2 rows are the
// two operators of one channel; the OPL3 rows add the second channel of a
// four-operator pair.
pub const SLOTS_OPL2: &[&[u8]] = &[
    &[0, 1, 2, 6, 7, 8, 12, 13, 14],
    &[3, 4, 5, 9, 10, 11, 15, 16, 17],
];
// BD keeps both of channel 6; then snare, tom, cymbal and hi-hat take the
// single rhythm operator of channels 7 and 8.
pub const SLOTS_OPL2_DRUMS: &[&[u8]] = &[
    &[0, 1, 2, 6, 7, 8, 12, 16, 14, 17, 13],
    &[3, 4, 5, 9, 10, 11, 15, N, N, N, N],
];
pub const SLOTS_OPL3: &[&[u8]] = &[
    &[0, 6, 1, 7, 2, 8, 18, 24, 19, 25, 20, 26, 30, 31, 32, 12, 13, 14],
    &[3, 9, 4, 10, 5, 11, 21, 27, 22, 28, 23, 29, 33, 34, 35, 15, 16, 17],
    &[6, N, 7, N, 8, N, 24, N, 25, N, 26, N, N, N, N, N, N, N],
    &[9, N, 10, N, 11, N, 27, N, 28, N, 29, N, N, N, N, N, N, N],
];
pub const SLOTS_OPL3_DRUMS: &[&[u8]] = &[
    &[0, 6, 1, 7, 2, 8, 18, 24, 19, 25, 20, 26, 30, 31, 32, 12, 16, 14, 17, 13],
    &[3, 9, 4, 10, 5, 11, 21, 27, 22, 28, 23, 29, 33, 34, 35, 15, N, N, N, N],
    &[6, N, 7, N, 8, N, 24, N, 25, N, 26, N, N, N, N, N, N, N, N],
    &[9, N, 10, N, 11, N, 27, N, 28, N, 29, N, N, N, N, N, N, N, N],
];

// chanMap[Furnace channel] + 0xA0/0xB0/0xC0 -> the channel's register block.
pub const CHAN_MAP_OPL2: &[u16] = &[0, 1, 2, 3, 4, 5, 6, 7, 8];
pub const CHAN_MAP_OPL2_DRUMS: &[u16] = &[0, 1, 2, 3, 4, 5, 6, 7, 8, 8, 7];
pub const CHAN_MAP_OPL3: &[u16] = &[
    0, 3, 1, 4, 2, 5, 0x100, 0x103, 0x101, 0x104, 0x102, 0x105, 0x106, 0x107, 0x108, 6, 7, 8,
];
pub const CHAN_MAP_OPL3_DRUMS: &[u16] = &[
    0, 3, 1, 4, 2, 5, 0x100, 0x103, 0x101, 0x104, 0x102, 0x105, 0x106, 0x107, 0x108, 6, 7, 8, 8, 7,
];

// orderedOpsL: slot row -> instrument operator in four-operator mode.
pub const ORDERED_OPS_L: [i32; 4] = [0, 2, 1, 3];

// Channel layout per chip and mode: (channel count, melodic channel count).
pub const LAYOUT: [(&str, bool, i32, i32); 10] = [
    (KIND_OPL, false, 9, 9),
    (KIND_OPL2, false, 9, 9),
    (KIND_OPL3, false, 18, 18),
    (KIND_OPL, true, 11, 6),
    (KIND_OPL2, true, 11, 6),
    (KIND_OPL3, true, 20, 15),
    // The OPL4's FM half, before the caller appends the 24 wave columns.
    (KIND_OPL4, false, 18, 18),
    (KIND_OPL4, true, 20, 15),
    // The Y8950's FM half is OPL2-shaped. Its ADPCM column is appended by the
    // caller, not part of this state.
    (KIND_Y8950, false, 9, 9),
    (KIND_Y8950, true, 11, 6),
];

fn layout(kind: &str, drums: bool) -> (i32, i32) {
    for &(k, d, total, melodic) in LAYOUT.iter() {
        if k == kind && d == drums {
            return (total, melodic);
        }
    }
    panic!("no layout for {:?}", kind);
}

pub fn slots_for(kind: &str, drums: bool) -> &'static [&'static [u8]] {
    if opl3_like(kind) {
        if drums { SLOTS_OPL3_DRUMS } else { SLOTS_OPL3 }
    } else if drums {
        SLOTS_OPL2_DRUMS
    } else {
        SLOTS_OPL2
    }
}

pub fn chan_map_for(kind: &str, drums: bool) -> &'static [u16] {
    if opl3_like(kind) {
        if drums { CHAN_MAP_OPL3_DRUMS } else { CHAN_MAP_OPL3 }
    } else if drums {
        CHAN_MAP_OPL2_DRUMS
    } else {
        CHAN_MAP_OPL2
    }
}

pub fn channel_count(kind: &str, drums: bool) -> i32 {
    layout(kind, drums).0
}

pub fn melodic_count(kind: &str, drums: bool) -> i32 {
    layout(kind, drums).1
}

// Rhythm key-on bits of register 0xBD, in the order Furnace's channels
// follow: bit 4 is the bass drum on channel `melodic`, then each step down
// names the next channel (snare, tom, cymbal, hi-hat).
pub const DRUM_BITS: [(u8, &str); 5] = [
    (1 << 4, "BD"),
    (1 << 3, "SD"),
    (1 << 2, "TM"),
    (1 << 1, "TP"),
    (1 << 0, "HH"),
];

/// Furnace channel a 0xBD trigger bit drives.
pub fn drum_channel(bit: u8, melodic: i32) -> i32 {
    for (i, &(mask, _name)) in DRUM_BITS.iter().enumerate() {
        if mask == bit {
            return melodic + i as i32;
        }
    }
    panic!("not a rhythm bit: {}", bit);
}

/// True when Furnace drives the channel from a DIV_INS_OPL_DRUMS voice.
///
/// The kick keeps two operators and its own channel. It takes a normal OPL
/// instrument, and only the four single-operator columns use the kit voice.
pub fn is_drum_channel(kind: &str, drums: bool, fch: i32) -> bool {
    drums && fch > layout(kind, drums).1
}

/// Octave of the loudest carrier relative to the channel f-num.
///
/// The OPL multiplier table is OPN's without the halved values: 0 is a half,
/// 1 unison and then multiples up. Only an exact power of two can be cancelled
/// by a whole-octave note move. A multiplier of 3/5/6/... leaves the note
/// where it is.
///
/// The YM2413 shares that table except at zero: Nuked-OPLL's `pg_multi`
/// starts `1, 2, 4, 6...`, so a multiplier of 0 is unison and never an octave
/// down. An OPLL preset has no readable operators at all, so only the user
/// patch can move.
pub fn opl_carrier_octave(patch: &FMPatch) -> i32 {
    if !patch.opl || patch.drum {
        return 0;
    }
    let opll = patch.opll;
    let mut best_tl: u8 = if !opll { 0x40 } else { 0x10 };
    let mut best_octave: Option<i32> = None;
    for i in patch.carriers() {
        let op = &patch.ops[i as usize];
        let mask: u8 = if opll { 0x0F } else { 0x3F };
        let tl = op.tl & mask;
        if tl >= mask {
            continue;
        }
        let mult = op.mult & 15;
        let ratio = if opll && mult == 0 {
            1.0
        } else if mult == 0 {
            0.5
        } else {
            f64::from(mult)
        };
        let octave = if ratio == 0.5 || ratio == 1.0 || ratio == 2.0 || ratio == 4.0 || ratio == 8.0
        {
            Some(parity::py_int(parity::py_round(ratio.log2())) as i32)
        } else {
            None
        };
        if tl < best_tl {
            best_tl = tl;
            best_octave = octave;
        } else if tl == best_tl {
            if let Some(oct) = octave {
                if best_octave.is_none_or(|best| oct > best) {
                    best_octave = Some(oct);
                }
            }
        }
    }
    best_octave.unwrap_or(0)
}

// --- pitch ------------------------------------------------------------------

// In linear pitch mode `calcFreq` folds the E5 byte into the table offset
// unchanged. One unit is 1/128 semitone, and the instrument pitch macro
// (`pitch2`) is added to the same offset. The `mul` argument the OPL platform
// passes (2) only scales the legacy non-linear path, which this project never
// writes - its modules keep Furnace's default linear pitch.

/// `DivPitchTable` in frequency mode, for one (clock, base) pair.
///
/// `value` returns the engine's frequency unit, which `to_freq` splits into
/// block and f-num. The table holds one octave and the note's octave shifts
/// it, rounding the carry in. A closed form alone misses by a unit here and
/// there.
#[derive(Clone, Debug, PartialEq)]
pub struct FreqPitchTable {
    pub clock: f64,
    pub base: f64,
    pub tuning: f64,
    pub max: i32,
    pub shift: i32,
    pub pitch: Vec<i32>,
    pub diff: Vec<i32>,
}

impl FreqPitchTable {
    pub fn new(clock: f64, base: i32) -> Self {
        Self::with_tuning(clock, base, 440.0, MAX_UNIT)
    }

    pub fn with_tuning(clock: f64, base: i32, tuning: f64, maximum: i32) -> Self {
        let base = f64::from(base);
        let mut shift = 14;
        while shift > 0 {
            let nbase = (shift - 5) * 12;
            let fbase = tuning * 2.0f64.powf(f64::from(nbase + 3) / 12.0);
            if parity::py_round(fbase * (base / clock)) <= f64::from(maximum) {
                break;
            }
            shift -= 1;
        }
        let mut pitch = vec![0i32; 13];
        for (i, slot) in pitch.iter_mut().enumerate() {
            let nbase = i as i32 + (shift - 5) * 12;
            let fbase = tuning * 2.0f64.powf(f64::from(nbase + 3) / 12.0);
            *slot = parity::py_round(fbase * (base / clock)) as i32;
        }
        let diff: Vec<i32> = (0..12).map(|i| pitch[i + 1] - pitch[i]).collect();
        FreqPitchTable { clock, base, tuning, max: maximum, shift, pitch, diff }
    }

    /// The raw table offset of a note, its E5 byte and a pitch macro value.
    pub fn offset(&self, note: i32, e5: i32, pitch2: i32) -> i32 {
        (note << 7) + (e5 - 0x80) + pitch2
    }

    /// The unit Furnace writes for a note, its E5 byte and pitch macro.
    pub fn value(&self, note: i32, e5: i32, pitch2: i32) -> i32 {
        self.value_at(self.offset(note, e5, pitch2))
    }

    fn value_at(&self, offset: i32) -> i32 {
        let mut coarse = offset >> 7;
        if coarse < 0 {
            coarse = 0;
        }
        let fine = offset & 127;
        let index = parity::py_mod(i64::from(coarse), 12) as usize;
        let octave = self.shift - parity::py_floor_div(i64::from(coarse), 12) as i32;
        if octave < 0 {
            return self.max;
        }
        let mut root = self.pitch[index];
        let mut diff = self.diff[index];
        if octave > 0 {
            let mut carry = root & (1 << (octave - 1)) != 0;
            root >>= octave;
            if carry {
                root += 1;
            }
            carry = diff & (1 << (octave - 1)) != 0;
            diff >>= octave;
            if carry {
                diff += 1;
            }
        }
        root + ((diff * fine) >> 7)
    }
}

static TABLES: LazyLock<Mutex<HashMap<(u64, i32), Arc<FreqPitchTable>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

// The clocks Furnace gives its OPL systems when no customClock is written:
// COLOR_NTSC and COLOR_NTSC * 4. A rip at the same nominal integer clock still
// has to build its pitch table from these floats, or the last unit of a f-num
// can disagree with what the module computes.
pub const FURNACE_CLOCK: f64 = 315_000_000.0 / 88.0;
pub const FURNACE_CLOCK_OPL3: f64 = FURNACE_CLOCK * 4.0;
pub const CLOCK_DEFAULT_TOLERANCE: i32 = 1; // Hz; the writer omits customClock inside it

// The YM2413 platform's own clock presets (`DivPlatformOPLL::setFlags`).
// `clockSel` is how Furnace names them in the UI, and matching one exactly
// keeps the module on a built-in selection instead of a custom clock.
pub const OPLL_CLOCK_SEL: [(i32, f64); 4] = [
    (0, FURNACE_CLOCK),                             // COLOR_NTSC
    (1, (283.75 * 15625.0 + 25.0) * 4.0 / 5.0),     // COLOR_PAL x4/5
    (2, 4_000_000.0),
    (3, FURNACE_CLOCK / 2.0),
];

fn lock<T>(cache: &Mutex<T>) -> MutexGuard<'_, T> {
    cache.lock().expect("pitch table cache poisoned")
}

/// The clock the module will run at: Furnace's default or the rip's own.
pub fn opl_module_clock(clock: f64, opl3: bool) -> f64 {
    let default = if opl3 { FURNACE_CLOCK_OPL3 } else { FURNACE_CLOCK };
    if clock == 0.0 || (clock - default).abs() <= f64::from(CLOCK_DEFAULT_TOLERANCE) {
        return default;
    }
    clock
}

/// The module's table for one chip and clock.
///
/// `opl3` selects the two-bank layout, and `freq_base` overrides the divider
/// when the variant is not an OPL3 (the OPL4 uses `32768*684`). Neither is
/// derivable from the clock, which a rip may set to anything.
pub fn pitch_table(clock: f64, opl3: bool, freq_base: Option<i32>) -> Arc<FreqPitchTable> {
    let base = freq_base.unwrap_or_else(|| if opl3 { FREQ_BASE_OPL3 } else { FREQ_BASE_OPL2 });
    let key = (parity::py_round_to(opl_module_clock(clock, opl3), 4).to_bits(), base);
    let mut cache = lock(&TABLES);
    if let Some(tbl) = cache.get(&key) {
        return Arc::clone(tbl);
    }
    let tbl = Arc::new(FreqPitchTable::new(f64::from_bits(key.0), base));
    cache.insert(key, Arc::clone(&tbl));
    tbl
}

/// `DivPlatformOPL::toFreq` for an instrument with no fixed block.
///
/// `bsr` in Furnace counts the bits of `unit / OPLL_C_NUM` rather than
/// returning its highest set bit. The block is therefore one higher than an
/// MSB index suggests, and the f-num keeps the next lower half-octave span.
/// Verified against a round trip through Furnace's own export: every note of
/// three and a half octaves lands on the same block and f-num this function
/// predicts.
pub fn to_freq(unit: i32) -> (i32, i32) {
    let mut block = 0i32;
    if unit > 0 {
        let index = parity::py_floor_div(i64::from(unit), i64::from(BLOCK_MIN));
        if index > 0 {
            block = (64 - index.leading_zeros()) as i32;
        }
    }
    block = block.clamp(0, 7);
    let mut fnum = unit >> block;
    if fnum > MAX_FNUM {
        fnum = MAX_FNUM;
    }
    (block, fnum)
}

/// Pitch in 1/64 semitones; only differences are meaningful.
pub fn opl_pitch_units(block: i32, fnum: i32) -> f64 {
    768.0 * ((fnum.max(1) as f64).log2() + f64::from(block))
}

/// The engine frequency unit a note, its E5 byte and a macro value produce.
///
/// `pitch2` is an instrument pitch macro's offset in 1/128 semitone. `get()`
/// folds it into the same offset as the fine tune. A note whose instrument
/// carries an octave compensation therefore still lands on the rip's register.
/// `freq_base` is the variant's divider (OPL4).
pub fn opl_unit_for(
    note: i32,
    clock: f64,
    e5: i32,
    opl3: bool,
    pitch2: i32,
    freq_base: Option<i32>,
) -> i32 {
    pitch_table(clock, opl3, freq_base).value(note, e5, pitch2)
}

/// (block, fnum) Furnace writes for a pattern note on this clock.
pub fn opl_note_fnum(
    note: i32,
    clock: f64,
    e5: i32,
    opl3: bool,
    pitch2: i32,
    freq_base: Option<i32>,
) -> (i32, i32) {
    to_freq(opl_unit_for(note, clock, e5, opl3, pitch2, freq_base))
}

type NoteKey = (i32, i32, u64, bool, i32);

static NOTES: LazyLock<Mutex<HashMap<NoteKey, (i32, i32)>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn candidate_lt(a: &(f64, i32, i32, i32), b: &(f64, i32, i32, i32)) -> bool {
    if a.0 != b.0 {
        return a.0 < b.0;
    }
    if a.1 != b.1 {
        return a.1 < b.1;
    }
    if a.2 != b.2 {
        return a.2 < b.2;
    }
    a.3 < b.3
}

/// Source (block, fnum) -> (pattern note, E5 byte).
///
/// The pair is searched in the domain the engine actually reaches: a note plus
/// its E5 byte, scaled by `E5_STEP` inside the table. Only reachable values
/// are scored, and what comes back is what Furnace will write. When the target
/// sits between two of them the closer one wins, then the smaller fine tune. A
/// register below note 0 or above B-9 has no pattern cell. The window is
/// clamped into the range, and the closest playable note wins.
pub fn opl_note_for(
    block: i32,
    fnum: i32,
    clock: f64,
    opl3: bool,
    freq_base: Option<i32>,
) -> (i32, i32) {
    let block = block.clamp(0, 7);
    let fnum = fnum.clamp(1, MAX_FNUM);
    let tbl = pitch_table(clock, opl3, freq_base);
    let key = (block, fnum, parity::py_round_to(clock, 4).to_bits(), opl3, tbl.base as i32);
    {
        let cache = lock(&NOTES);
        if let Some(&hit) = cache.get(&key) {
            return hit;
        }
    }
    let want = opl_pitch_units(block, fnum);
    let unit = i32::max(1, fnum << block);
    let est = 57.0
        + 12.0 * ((f64::from(unit) * tbl.clock) / (tbl.tuning * tbl.base)).log2();
    let center = i32::clamp(parity::py_int(est.floor()) as i32, 0, 179);
    let lo_note = i32::clamp(center - 3, 0, 179);
    let hi_note = i32::max(lo_note, i32::min(179, center + 4));
    let mut best: Option<(f64, i32, i32, i32)> = None; // (pitch err, |e5|, note, e5)
    for note in lo_note..=hi_note {
        // Pitch rises with the fine tune, and bisection finds the crossing. The
        // search is in E5 units, one of which moves the table by two steps.
        let mut lo = -128i32;
        let mut hi = 127i32;
        while lo < hi {
            let mid = parity::py_floor_div(i64::from(lo) + i64::from(hi), 2) as i32;
            if opl_pitch_units_wrapped(to_freq(tbl.value(note, 0x80 + mid, 0))) < want {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        for off in i32::max(-128, lo - 2)..=i32::min(127, lo + 2) {
            let got = to_freq(tbl.value(note, 0x80 + off, 0));
            let err = (opl_pitch_units_wrapped(got) - want).abs();
            let cand = (err, off.abs(), note, 0x80 + off);
            if best.is_none_or(|b| candidate_lt(&cand, &b)) {
                best = Some(cand);
            }
        }
    }
    let best = best.expect("note candidate window is non-empty");
    let out = (best.2, best.3);
    lock(&NOTES).insert(key, out);
    out
}

fn opl_pitch_units_wrapped(block_fnum: (i32, i32)) -> f64 {
    opl_pitch_units(block_fnum.0, block_fnum.1)
}

// --- OPLL (YM2413) pitch ----------------------------------------------------
//
// The YM2413 platform is a separate core: DivPlatformOPLL has its own
// CHIP_FREQBASE (1180068), a maximum unit of 0xffff and OPLL_C_NUM 343, and its
// f-num is nine bits wide. The note/E5 search is the same, only the table and
// the split differ.

pub const OPLL_DEFAULT_CLOCK: u32 = 3579545;
pub const OPLL_FREQ_BASE: i32 = 1180068;
pub const OPLL_MAX_UNIT: i32 = 0xFFFF;
pub const OPLL_C_NUM: i32 = 343;
pub const OPLL_MAX_FNUM: i32 = 0x1FF;

static OPLL_TABLES: LazyLock<Mutex<HashMap<u64, Arc<FreqPitchTable>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

type OpllNoteKey = (i32, i32, u64);

static OPLL_NOTES: LazyLock<Mutex<HashMap<OpllNoteKey, (i32, i32)>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// The clock the OPLL module will run at: Furnace's default or the rip's.
pub fn opll_module_clock(clock: f64) -> f64 {
    if clock == 0.0 || (clock - FURNACE_CLOCK).abs() <= f64::from(CLOCK_DEFAULT_TOLERANCE) {
        return FURNACE_CLOCK;
    }
    for &(_sel, hz) in OPLL_CLOCK_SEL.iter() {
        if (clock - hz).abs() <= f64::from(CLOCK_DEFAULT_TOLERANCE) {
            return hz;
        }
    }
    clock
}

/// The chip flag that puts the OPLL platform at `clock`.
///
/// Furnace's four named selections are exact, so a rip whose header matches
/// one of them travels as `clockSel` (the way the tracker itself stores it)
/// and anything else as a custom clock.
pub fn opll_clock_flag(clock: f64) -> String {
    if clock == 0.0 {
        return String::new();
    }
    let hz = opll_module_clock(clock);
    for &(sel, target) in OPLL_CLOCK_SEL.iter() {
        if (hz - target).abs() <= 0.5 {
            return if sel == 0 {
                String::new()
            } else {
                format!("clockSel={}", sel)
            };
        }
    }
    format!("customClock={}", parity::py_int(parity::py_round(hz)))
}

pub fn opll_pitch_table(clock: f64) -> Arc<FreqPitchTable> {
    let key = parity::py_round_to(opll_module_clock(clock), 4).to_bits();
    let mut cache = lock(&OPLL_TABLES);
    if let Some(tbl) = cache.get(&key) {
        return Arc::clone(tbl);
    }
    let tbl = Arc::new(FreqPitchTable::with_tuning(
        f64::from_bits(key),
        OPLL_FREQ_BASE,
        440.0,
        OPLL_MAX_UNIT,
    ));
    cache.insert(key, Arc::clone(&tbl));
    tbl
}

/// `DivPlatformOPLL::toFreq`: the block/f-num a frequency unit becomes.
pub fn opll_to_freq(unit: i32) -> (i32, i32) {
    let mut block = 0i32;
    if unit > 0 {
        let index = parity::py_floor_div(i64::from(unit), i64::from(OPLL_C_NUM));
        if index > 0 {
            block = (64 - index.leading_zeros()) as i32;
        }
    }
    block = block.clamp(0, 7);
    let mut fnum = unit >> block;
    if fnum > OPLL_MAX_FNUM {
        fnum = OPLL_MAX_FNUM;
    }
    (block, fnum)
}

pub fn opll_unit_for(note: i32, clock: f64, e5: i32) -> i32 {
    opll_pitch_table(clock).value(note, e5, 0)
}

/// (block, fnum) Furnace writes for a pattern note on this clock.
pub fn opll_note_fnum(note: i32, clock: f64, e5: i32) -> (i32, i32) {
    opll_to_freq(opll_unit_for(note, clock, e5))
}

/// Source (block, fnum) -> (pattern note, E5 byte), OPLL layout.
pub fn opll_note_for(block: i32, fnum: i32, clock: f64) -> (i32, i32) {
    let block = block.clamp(0, 7);
    let fnum = fnum.clamp(1, OPLL_MAX_FNUM);
    let key = (block, fnum, parity::py_round_to(clock, 4).to_bits());
    {
        let cache = lock(&OPLL_NOTES);
        if let Some(&hit) = cache.get(&key) {
            return hit;
        }
    }
    let tbl = opll_pitch_table(clock);
    let want = opl_pitch_units(block, fnum);
    let unit = i32::max(1, fnum << block);
    let est = 57.0
        + 12.0 * ((f64::from(unit) * tbl.clock) / (tbl.tuning * tbl.base)).log2();
    let center = i32::clamp(parity::py_int(est.floor()) as i32, 0, 179);
    let lo_note = i32::clamp(center - 3, 0, 179);
    let hi_note = i32::max(lo_note, i32::min(179, center + 4));
    let mut best: Option<(f64, i32, i32, i32)> = None;
    for note in lo_note..=hi_note {
        let mut lo = -128i32;
        let mut hi = 127i32;
        while lo < hi {
            let mid = parity::py_floor_div(i64::from(lo) + i64::from(hi), 2) as i32;
            if opl_pitch_units_wrapped(opll_to_freq(tbl.value(note, 0x80 + mid, 0))) < want {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        for off in i32::max(-128, lo - 2)..=i32::min(127, lo + 2) {
            let got = opll_to_freq(tbl.value(note, 0x80 + off, 0));
            let err = (opl_pitch_units_wrapped(got) - want).abs();
            let cand = (err, off.abs(), note, 0x80 + off);
            if best.is_none_or(|b| candidate_lt(&cand, &b)) {
                best = Some(cand);
            }
        }
    }
    let best = best.expect("note candidate window is non-empty");
    let out = (best.2, best.3);
    lock(&OPLL_NOTES).insert(key, out);
    out
}

// --- register image ---------------------------------------------------------

/// An operator the chip never reaches: it stays silent.
fn idle_op() -> OpRegs {
    OpRegs { tl: 0x3F, ..Default::default() }
}

/// Register image of one OPL chip, read through Furnace's channel tables.
///
/// `kind` picks the platform variant (waveform depth, banks and rhythm bits),
/// and `drums` switches to the channel tables of Furnace's drums system. The
/// rhythm flag follows register 0xBD. The log decides when the percussion
/// channels start to mean drums.
#[derive(Clone, Debug)]
pub struct OPLState {
    pub kind: String,
    pub opl3: bool,
    pub drums: bool,
    pub regs: Vec<u8>,
    pub channels: i32,
    pub melodic: i32,
    pub slots: &'static [&'static [u8]],
    pub chan_map: &'static [u16],
}

impl OPLState {
    pub fn new(kind: impl Into<String>, drums: bool) -> Self {
        let kind = kind.into();
        let opl3 = opl3_like(&kind);
        let (total, melodic) = layout(&kind, drums);
        let slots = slots_for(&kind, drums);
        let chan_map = chan_map_for(&kind, drums);
        OPLState {
            kind,
            opl3,
            drums,
            regs: vec![0; 512],
            channels: total,
            melodic,
            slots,
            chan_map,
        }
    }

    // -- image ---------------------------------------------------------------

    pub fn write(&mut self, port: i32, reg: i32, val: i32) {
        let index = ((if self.opl3 { (port & 1) << 8 } else { 0 }) | (reg & 0xFF)) as usize;
        self.regs[index] = (val & 0xFF) as u8;
    }

    /// Rhythm mode as the chip has it (bit 5 of 0xBD).
    pub fn rhythm(&self) -> bool {
        self.regs[0xBD] & 0x20 != 0
    }

    /// The latch-on-write key bits of 0xBD.
    pub fn triggers(&self) -> u8 {
        self.regs[0xBD] & 0x1F
    }

    /// The six pair bits of the OPL3 "new" register 0x104.
    pub fn four_op_mask(&self) -> u8 {
        if self.opl3 { self.regs[0x104] & 0x3F } else { 0 }
    }

    /// True when this channel heads a four-operator pair.
    pub fn is_four_op(&self, fch: i32) -> bool {
        if !self.opl3 || fch >= 12 || (fch & 1) != 0 {
            return false;
        }
        (self.four_op_mask() >> (fch / 2)) & 1 != 0
    }

    pub fn op_count(&self, fch: i32) -> i32 {
        if self.is_four_op(fch) { 4 } else { 2 }
    }

    // -- channel reads -------------------------------------------------------

    fn block_fnum_raw(&self, fch: i32) -> (i32, i32) {
        let cm = self.chan_map[fch as usize] as usize;
        let hi = self.regs[cm + 0xB0];
        (
            i32::from((hi >> 2) & 7),
            (i32::from(hi & 3) << 8) | i32::from(self.regs[cm + 0xA0]),
        )
    }

    /// (block, fnum) the channel's own registers hold.
    ///
    /// In drums mode the four percussion channels share two frequency
    /// registers, exactly as the chip does: snare and hi-hat follow channel 7
    /// and tom and cymbal follow channel 8.
    pub fn frequency(&self, fch: i32) -> (i32, i32) {
        self.block_fnum_raw(fch)
    }

    pub fn keyed(&self, fch: i32) -> bool {
        self.regs[self.chan_map[fch as usize] as usize + 0xB0] & 0x20 != 0
    }

    /// The connection field as the platform stores it.
    ///
    /// A two-operator voice takes one bit; a four-operator pair splits its
    /// two bits over the two channels' 0xCn registers.
    pub fn algorithm(&self, fch: i32) -> i32 {
        let cm = self.chan_map[fch as usize] as usize;
        let mut alg = i32::from(self.regs[cm + 0xC0] & 1);
        if self.is_four_op(fch) {
            let next = self.chan_map[(fch + 1) as usize] as usize;
            alg |= i32::from(self.regs[next + 0xC0] & 1) << 1;
        }
        alg
    }

    pub fn feedback(&self, fch: i32) -> i32 {
        let cm = self.chan_map[fch as usize] as usize;
        i32::from((self.regs[cm + 0xC0] >> 1) & 7)
    }

    /// The hardware L/R enable bits (0 = both off, 3 = both).
    pub fn pan_bits(&self, fch: i32) -> u8 {
        if !self.opl3 {
            return 3;
        }
        let cm = self.chan_map[fch as usize] as usize;
        (self.regs[cm + 0xC0] >> 4) & 3
    }

    /// Furnace 08xy for the channel; 0 means "write nothing".
    ///
    /// The OPL3 gate is one bit per side, and a full nibble carries the level
    /// Furnace scales by. Both sides on must be written explicitly as 0xFF: a
    /// channel that was hard-panned earlier otherwise keeps that pan when the
    /// source returns to centre. Both sides off cannot be written without
    /// Furnace's compatibility rule turning 0x00 back into centre, so it stays
    /// an omitted effect.
    pub fn pan_byte(&self, fch: i32) -> u8 {
        if !self.opl3 {
            return 0;
        }
        let bits = self.pan_bits(fch);
        if bits == 3 {
            return 0xFF;
        }
        if bits == 0 {
            return 0;
        }
        let mut out = 0u8;
        if bits & 1 != 0 {
            out |= 0xF0;
        }
        if bits & 2 != 0 {
            out |= 0x0F;
        }
        out
    }

    /// One operator decoded from its five registers at `addr`.
    pub fn operator(&self, addr: u16) -> OpRegs {
        let regs = &self.regs;
        let addr = addr as usize;
        let amvib = regs[addr + 0x20];
        let tl = regs[addr + 0x40];
        let ardr = regs[addr + 0x60];
        let slrr = regs[addr + 0x80];
        let ws = if self.kind != KIND_OPL { regs[addr + 0xE0] } else { 0 };
        OpRegs {
            tl: tl & 0x3F,
            ksl: (tl >> 6) & 3,
            ar: (ardr >> 4) & 15,
            dr: ardr & 15,
            sl: (slrr >> 4) & 15,
            rr: slrr & 15,
            am: (amvib >> 7) & 1,
            vib: (amvib >> 6) & 1,
            sus: (amvib >> 5) & 1,
            ksr: (amvib >> 4) & 1,
            mult: amvib & 15,
            ws: ws & (if self.opl3 { 7 } else { 3 }),
            ..Default::default()
        }
    }

    fn op_addr(&self, fch: i32, row: usize) -> Option<u16> {
        let slot = self.slots[row][fch as usize];
        if slot == N {
            None
        } else {
            Some(SLOT_ADDR[slot as usize])
        }
    }

    /// The voice a channel holds right now, in Furnace's operator order.
    pub fn snapshot(&self, fch: i32) -> FMPatch {
        let four = self.is_four_op(fch);
        let mut ops: [OpRegs; 4] = core::array::from_fn(|_| idle_op());
        for row in 0..(if four { 4 } else { 2 }) {
            let addr = match self.op_addr(fch, row) {
                Some(addr) => addr,
                None => continue,
            };
            let index = if four { ORDERED_OPS_L[row] as usize } else { row };
            ops[index] = self.operator(addr);
        }
        FMPatch {
            alg: self.algorithm(fch),
            fb: self.feedback(fch),
            ops,
            opl: true,
            four,
            ..Default::default()
        }
    }

    /// The four percussion operators as one DIV_INS_OPL_DRUMS instrument.
    ///
    /// Row 0 of channels `melodic+1` to `melodic+4` is the snare, tom, cymbal
    /// and hi-hat operator, which is the order commitState copies them from an
    /// instrument's operators.
    pub fn drum_snapshot(&self) -> FMPatch {
        let mut ops: Vec<OpRegs> = Vec::with_capacity(4);
        for row in 1..5 {
            let addr = self.op_addr(self.melodic + row, 0);
            ops.push(match addr {
                Some(addr) => self.operator(addr),
                None => idle_op(),
            });
        }
        while ops.len() < 4 {
            ops.push(idle_op());
        }
        FMPatch {
            alg: 0,
            fb: 0,
            ops: [ops[0].clone(), ops[1].clone(), ops[2].clone(), ops[3].clone()],
            opl: true,
            drum: true,
            ..Default::default()
        }
    }

    pub fn carrier_tl(&self, fch: i32) -> i32 {
        self.snapshot(fch).carrier_tl()
    }

    /// Level a volume-column step has to reproduce on this channel.
    ///
    /// The platform scales only the operators that reach the output. A
    /// percussion column is one operator of its own and tracks that operator.
    pub fn channel_tl(&self, fch: i32) -> i32 {
        if self.drums && fch > self.melodic {
            let addr = self.op_addr(fch, 0);
            return match addr {
                None => 63,
                Some(addr) => i32::from(self.operator(addr).tl),
            };
        }
        self.carrier_tl(fch)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CLOCK_OPL2: f64 = 3_579_545.0;
    const CLOCK_OPL3: f64 = 14_318_180.0;

    #[test]
    fn reference_fnum_pairs() {
        let clock = opl_module_clock(CLOCK_OPL2, false);
        assert_eq!(opl_note_fnum(57, clock, 0x80, false, 0, None), (0, 290));
        assert_eq!(opl_note_fnum(69, clock, 0x80, false, 0, None), (0, 580));
        assert_eq!(opl_note_fnum(48, clock, 0x80, false, 0, None), (0, 172));
        assert_eq!(opl_note_fnum(84, clock, 0x80, false, 0, None), (2, 345));
        assert_eq!(opl_note_fnum(96, clock, 0x80, false, 0, None), (3, 344));
    }

    #[test]
    fn opl3_base_is_four_times_the_opl2_one() {
        let clock3 = opl_module_clock(CLOCK_OPL3, true);
        for note in [36, 57, 69, 84, 108] {
            let a = opl_note_fnum(note, opl_module_clock(CLOCK_OPL2, false), 0x80, false, 0, None);
            let b = opl_note_fnum(note, clock3, 0x80, true, 0, None);
            assert_eq!(a.1 << a.0, b.1 << b.0, "note {}", note);
        }
    }

    #[test]
    fn opl3_permutation_pairs_the_bank() {
        let st = OPLState::new(KIND_OPL3, false);
        assert_eq!(&st.chan_map[..8], &[0, 3, 1, 4, 2, 5, 0x100, 0x103]);
        assert_eq!(&st.chan_map[15..], &[6, 7, 8]);
        assert_eq!(channel_count(KIND_OPL3, false), 18);
    }

    #[test]
    fn four_op_bit_selects_the_pair() {
        let mut st = OPLState::new(KIND_OPL3, false);
        assert!(!st.is_four_op(0));
        st.write(1, 0x04, 0x0D); // bank A pairs 0 and 2, bank B pair 0
        assert!(st.is_four_op(0));
        assert!(!st.is_four_op(2));
        assert!(st.is_four_op(4));
        assert!(!st.is_four_op(5)); // an odd column never heads a voice
        assert!(st.is_four_op(6));
        let addrs: Vec<u16> = (0..4).map(|r| SLOT_ADDR[st.slots[r][0] as usize]).collect();
        assert_eq!(addrs, vec![0x00, 0x03, 0x08, 0x0B]);
    }

    #[test]
    fn kit_slots_and_shared_frequency_registers() {
        let st = OPLState::new(KIND_OPL2, true);
        assert_eq!(melodic_count(KIND_OPL2, true), 6);
        let addrs: Vec<u16> = (6..11).map(|f| SLOT_ADDR[st.slots[0][f] as usize]).collect();
        assert_eq!(addrs, vec![0x10, 0x14, 0x12, 0x15, 0x11]);
        assert_eq!(&st.chan_map[6..], &[6, 7, 8, 8, 7]);
        assert_eq!(drum_channel(1 << 4, 6), 6); // kick
        assert_eq!(drum_channel(1 << 0, 6), 10); // hi-hat
        assert!(!is_drum_channel(KIND_OPL2, true, 6));
        assert!(is_drum_channel(KIND_OPL2, true, 7));
    }
}
