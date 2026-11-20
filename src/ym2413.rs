//! YM2413 (OPLL) register image: 2-op channels, presets and the user patch.
//!
//! The OPLL is an OPL relative with a much smaller register map: nine channels of
//! two operators, one user instrument in `$00-$07` and fifteen ROM presets
//! selected by the high nibble of `$3x`. `$0E` turns the last three channels
//! into five percussion voices.
//!
//! The chip has no per-channel output level of its own: the low nibble of `$3x`
//! is the carrier's attenuation (0 loudest, 15 quietest) and the carrier TL of a
//! user patch never exists as a separate register. Furnace's OPLL platform reads
//! the pattern volume column into that nibble (`$3x = 15 - outVol`), so a
//! converted note carries its level in the volume column and the carrier operator
//! of the instrument stays at 0.
//!
//! Pitch follows the OPL rules with the OPLL core's own frequency base and a
//! nine-bit f-number; `vgm2151fur.opl` has the tables.

use indexmap::IndexMap;

use crate::fm::{FMPatch, OpRegs};
use crate::vgm::VgmFile;

pub const OPLL_KIND: &str = "opll";
pub const OPLL_DEFAULT_CLOCK: u32 = 3_579_545;
pub const OPLL_CHANNELS: usize = 9;
pub const OPLL_MELODIC: usize = 9;
pub const OPLL_DRUMS_CHANNELS: usize = 11;
pub const OPLL_DRUMS_MELODIC: usize = 6;

/// Furnace's OPLL-with-drums channels, in system order: BD, SD, TM, TP, HH.
/// `hw` is the register channel the chip tunes, `reg`/`shift` give the volume
/// nibble. The chip shares a frequency register between SD/HH and TM/TP.
///
/// Fields: (furnace channel, short name, hw channel, `$3x` register and shift,
/// `$0e` trigger bit).
pub const OPLL_DRUMS: [(i32, &str, i32, u32, u32, u32); 5] = [
    (6, "BD", 6, 0x36, 0, 0x10),
    (7, "SD", 7, 0x37, 0, 0x08),
    (8, "TM", 8, 0x38, 4, 0x04),
    (9, "TP", 8, 0x38, 0, 0x02),
    (10, "HH", 7, 0x37, 4, 0x01),
];

pub const DRUM_NAMES_LONG: [&str; 5] = ["Kick", "Snare", "Tom", "Top", "HiHat"];
pub const DRUM_NAMES_SHORT: [&str; 5] = ["BD", "SD", "TM", "TP", "HH"];
/// Samples a key-on waits before its voice is read. Drivers raise bit 4 of
/// `$2x` and write the f-num's low byte right after; the page at the end of
/// this window is the note. It stays far inside one row (a YM2413 driver ticks
/// every frame or two, and the shortest grid row this project builds is four).
pub const OPLL_SETTLE: i64 = 150;
/// The `$0e` latch bits in Furnace's column order: BD, SD, TM, TP, HH. The grid
/// estimator reads key-onset times from here (timing.py).
pub const KIT_TRIGGERS: [(u32, &str); 5] = [
    (OPLL_DRUMS[0].5, OPLL_DRUMS[0].1),
    (OPLL_DRUMS[1].5, OPLL_DRUMS[1].1),
    (OPLL_DRUMS[2].5, OPLL_DRUMS[2].1),
    (OPLL_DRUMS[3].5, OPLL_DRUMS[3].1),
    (OPLL_DRUMS[4].5, OPLL_DRUMS[4].1),
];

/// An operator the chip never reaches: it stays silent.
fn idle_op() -> OpRegs {
    OpRegs {
        tl: 0x3F,
        mult: 1,
        ..OpRegs::default()
    }
}

/// Silent on its own: the level rides the `$3x` volume nibble.
fn carrier_op() -> OpRegs {
    OpRegs {
        tl: 0,
        mult: 1,
        ..OpRegs::default()
    }
}

/// Register image of one YM2413.
#[derive(Clone, Debug, PartialEq)]
pub struct OPLLState {
    pub drums: bool,
    pub regs: [u8; 0x40],
}

impl Default for OPLLState {
    fn default() -> Self {
        OPLLState {
            drums: false,
            regs: [0; 0x40],
        }
    }
}

impl OPLLState {
    pub fn new(drums: bool) -> Self {
        OPLLState {
            drums,
            regs: [0; 0x40],
        }
    }

    // -- image ---------------------------------------------------------------

    pub fn write(&mut self, reg: u32, val: u32) {
        self.regs[(reg & 0x3F) as usize] = (val & 0xFF) as u8;
    }

    pub fn rhythm(&self) -> bool {
        (self.regs[0x0E] & 0x20) != 0
    }

    pub fn triggers(&self) -> u32 {
        u32::from(self.regs[0x0E] & 0x1F)
    }

    // -- channel reads -------------------------------------------------------

    pub fn preset(&self, fch: i32) -> i32 {
        i32::from((self.regs[0x30 + fch as usize] >> 4) & 15)
    }

    pub fn volume(&self, fch: i32) -> i32 {
        i32::from(self.regs[0x30 + fch as usize] & 15)
    }

    pub fn block_fnum(&self, fch: i32) -> (i32, i32) {
        let hi = self.regs[0x20 + fch as usize];
        (
            i32::from((hi >> 1) & 7),
            (i32::from(hi & 1) << 8) | i32::from(self.regs[0x10 + fch as usize]),
        )
    }

    pub fn keyed(&self, fch: i32) -> bool {
        (self.regs[0x20 + fch as usize] & 0x10) != 0
    }

    pub fn sustain(&self, fch: i32) -> i32 {
        if (self.regs[0x20 + fch as usize] & 0x20) != 0 {
            1
        } else {
            0
        }
    }

    pub fn drum_volume(&self, reg: u32, shift: u32) -> i32 {
        i32::from((self.regs[reg as usize] >> shift) & 15)
    }

    // -- instruments ---------------------------------------------------------

    fn custom_ops(&self) -> (OpRegs, OpRegs) {
        let r = &self.regs;

        fn op(
            r: &[u8; 0x40],
            addr: usize,
            ksl_reg: usize,
            ardr_reg: usize,
            slrr_reg: usize,
            tl: u8,
        ) -> OpRegs {
            let amvib = r[addr];
            let ksl_tl = r[ksl_reg];
            let ardr = r[ardr_reg];
            let slrr = r[slrr_reg];
            let et = (amvib >> 5) & 1;
            OpRegs {
                tl,
                ksl: (ksl_tl >> 6) & 3,
                ar: (ardr >> 4) & 15,
                dr: ardr & 15,
                sl: (slrr >> 4) & 15,
                rr: slrr & 15,
                am: (amvib >> 7) & 1,
                vib: (amvib >> 6) & 1,
                ksr: (amvib >> 4) & 1,
                mult: amvib & 15,
                ssg_eg: if et != 0 { 8 } else { 0 },
                ..OpRegs::default()
            }
        }

        (
            op(r, 0x00, 0x02, 0x04, 0x06, r[0x02] & 0x3F),
            // The carrier level is the $3x volume nibble, never $02/$03.
            op(r, 0x01, 0x03, 0x05, 0x07, 0),
        )
    }

    pub fn custom_patch(&self, fch: i32) -> FMPatch {
        let (mod_, car) = self.custom_ops();
        FMPatch {
            alg: self.sustain(fch),
            fb: i32::from(self.regs[0x03] & 7),
            ams: i32::from((self.regs[0x03] >> 3) & 1),
            pms: i32::from((self.regs[0x03] >> 4) & 1),
            ops: [mod_, car, idle_op(), idle_op()],
            opl: true,
            opll: true,
            preset: 0,
            ..FMPatch::default()
        }
    }

    /// A ROM voice. Its operators come from the chip, not the module.
    pub fn preset_patch(&self, fch: i32) -> FMPatch {
        FMPatch {
            alg: self.sustain(fch),
            fb: 0,
            ams: 0,
            pms: 0,
            ops: [idle_op(), carrier_op(), idle_op(), idle_op()],
            opl: true,
            opll: true,
            preset: self.preset(fch),
            ..FMPatch::default()
        }
    }

    pub fn snapshot(&self, fch: i32) -> FMPatch {
        if self.preset(fch) == 0 {
            return self.custom_patch(fch);
        }
        self.preset_patch(fch)
    }

    /// A placeholder voice for a percussion column.
    ///
    /// The chip plays a fixed drum patch in rhythm mode, so the operators are
    /// never loaded; the column only needs an instrument of the right type.
    pub fn drum_patch(&self) -> FMPatch {
        FMPatch {
            alg: 0,
            fb: 0,
            ams: 0,
            pms: 0,
            ops: [idle_op(), carrier_op(), idle_op(), idle_op()],
            opl: true,
            opll: true,
            preset: 0,
            ..FMPatch::default()
        }
    }
}

/// Decide whether a log really latches the OPLL drum kit.
///
/// `$0E` bit 5 enables rhythm mode and the low five bits are the level-
/// triggered drum keys. A driver that pokes the enable bit during init and
/// clears it without a key never sounds a drum, so only a trigger that rises
/// while the mode is on counts.
pub fn opll_rhythm_plan(vgm: &VgmFile, ids: &[i32]) -> (bool, IndexMap<String, i64>) {
    let _ = ids;
    let mut stats: IndexMap<String, i64> = IndexMap::new();
    stats.insert("latches".to_string(), 0);
    stats.insert("mode_samples".to_string(), 0);
    stats.insert("melodic_on_kit".to_string(), 0);
    let mut per: IndexMap<u8, Vec<&crate::vgm::ChipWrite>> = IndexMap::new();
    for w in &vgm.writes {
        if w.chip == "opll" {
            per.entry(w.chip_id).or_default().push(w);
        }
    }
    let end: i64 = vgm.total_samples;
    for ws in per.values() {
        let mut mode = false;
        let mut since: i64 = 0;
        let mut trig: u32 = 0;
        // Python iterates `ws + [None]`; the sentinel flushes the open segment
        // and ends this chip's stream.
        for idx in 0..=ws.len() {
            let Some(w) = ws.get(idx) else {
                if mode && end > since {
                    *stats.get_mut("mode_samples").unwrap() += end - since;
                }
                break;
            };
            let r = w.reg & 0xFF;
            if r != 0x0E {
                if (0x20..=0x28).contains(&r) && (w.val & 0x10) != 0 && !mode {
                    // A key-on on a channel the kit takes over.
                    let ch = r - 0x20;
                    if ch >= 6 {
                        *stats.get_mut("melodic_on_kit").unwrap() += 1;
                    }
                }
                continue;
            }
            let now = (w.val & 0x20) != 0;
            if now != mode {
                *stats.entry("segments".to_string()).or_insert(0) += 1;
                if mode {
                    *stats.get_mut("mode_samples").unwrap() += w.sample - since;
                }
                mode = now;
                since = w.sample;
            }
            let bits: u32 = if now { w.val & 0x1F } else { 0 };
            for k in 0..5 {
                let bit: u32 = 1 << k;
                if (bits & bit) != 0 && (trig & bit) == 0 {
                    *stats.get_mut("latches").unwrap() += 1;
                }
            }
            trig = bits;
        }
    }
    (*stats.get("latches").unwrap() != 0, stats)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vgm::ChipWrite;

    #[test]
    fn register_image_reads_channels_and_masks() {
        let mut st = OPLLState::new(false);
        // write() masks the address to $00-$3f and the value to a byte.
        st.write(0x70, 0x1FF);
        assert_eq!(st.regs[0x30], 0xFF);
        st.write(0x30, 0x72);
        assert_eq!(st.preset(0), 7);
        assert_eq!(st.volume(0), 2);
        st.write(0x20, 0x1D);
        st.write(0x10, 0x34);
        assert_eq!(st.block_fnum(0), (6, 0x134));
        assert!(st.keyed(0));
        assert_eq!(st.sustain(0), 0);
        st.write(0x0E, 0x35);
        assert!(st.rhythm());
        assert_eq!(st.triggers(), 0x15);
        st.write(0x36, 0xA5);
        assert_eq!(st.drum_volume(0x36, 0), 5);
        assert_eq!(st.drum_volume(0x36, 4), 10);
        assert!(!st.drums);
    }

    #[test]
    fn custom_patch_decodes_the_user_voice() {
        let mut st = OPLLState::default();
        st.write(0x00, 0xF5); // AM|VIB|ET|KSR, mult 5
        st.write(0x02, 0xBF); // KSL 2, TL $3F
        st.write(0x04, 0xA9); // AR $A, DR 9
        st.write(0x06, 0x53); // SL 5, RR 3
        st.write(0x01, 0x21); // ET, mult 1
        st.write(0x03, 0x59); // KSL 1, FB 1, AMS 1, PMS 1
        st.write(0x05, 0x12); // AR 1, DR 2
        st.write(0x07, 0x34); // SL 3, RR 4
        st.write(0x20, 0x20); // sustain bit on
        let p = st.custom_patch(0);
        assert_eq!(p.alg, 1);
        assert_eq!(p.fb, 1);
        assert_eq!(p.ams, 1);
        assert_eq!(p.pms, 1);
        assert_eq!(p.preset, 0);
        assert!(p.opl && p.opll);
        let m = &p.ops[0];
        assert_eq!(
            (m.tl, m.ksl, m.ar, m.dr, m.sl, m.rr),
            (0x3F, 2, 0xA, 9, 5, 3)
        );
        assert_eq!((m.am, m.vib, m.ksr, m.mult, m.ssg_eg), (1, 1, 1, 5, 8));
        let c = &p.ops[1];
        // The carrier TL is never the $02/$03 register.
        assert_eq!(c.tl, 0);
        assert_eq!((c.ksl, c.ar, c.dr, c.sl, c.rr), (1, 1, 2, 3, 4));
        assert_eq!((c.am, c.vib, c.ksr, c.mult, c.ssg_eg), (0, 0, 0, 1, 8));
        assert_eq!(p.ops[2].tl, 0x3F);
        assert_eq!(p.ops[3].tl, 0x3F);
        assert_eq!(p.carriers(), vec![1]);
        assert_eq!(p.carrier_tl(), 0);
    }

    #[test]
    fn preset_snapshot_keeps_the_rom_voice_opaque() {
        let mut st = OPLLState::default();
        st.write(0x30, 0x72);
        st.write(0x20, 0x00);
        let p = st.snapshot(0);
        assert_eq!(p.preset, 7);
        assert_eq!(p.alg, 0);
        assert_eq!(p.fb, 0);
        assert_eq!(p.ams, 0);
        assert_eq!(p.pms, 0);
        assert_eq!(p.ops[0].tl, 0x3F);
        assert_eq!(p.ops[1].tl, 0);
        assert_eq!(p.ops[1].mult, 1);
        assert!(p.opl && p.opll);
    }

    #[test]
    fn drum_patch_is_a_placeholder_carrier_voice() {
        let st = OPLLState::new(true);
        let p = st.drum_patch();
        assert_eq!((p.alg, p.fb, p.ams, p.pms, p.preset), (0, 0, 0, 0, 0));
        assert_eq!(p.ops[0].tl, 0x3F);
        assert_eq!(p.ops[1].tl, 0);
        assert_eq!(p.ops[1].mult, 1);
        assert!(p.opl && p.opll && !p.opn);
        assert_eq!(DRUM_NAMES_LONG[4], "HiHat");
        assert_eq!(DRUM_NAMES_SHORT[4], "HH");
        assert_eq!(KIT_TRIGGERS[0], (0x10, "BD"));
        assert_eq!(KIT_TRIGGERS[4], (0x01, "HH"));
        assert_eq!(OPLL_DRUMS[2], (8, "TM", 8, 0x38, 4, 0x04));
        assert_eq!(OPLL_SETTLE, 150);
        assert_eq!(OPLL_DEFAULT_CLOCK, 3_579_545);
    }

    #[test]
    fn rhythm_plan_counts_only_rising_latches_in_mode() {
        let mut vgm = VgmFile::default();
        vgm.total_samples = 500;
        vgm.writes = vec![
            // A key-on on a kit channel counts only while rhythm mode is off.
            ChipWrite::new(0, "opll", 0, 0x26, 0x10),
            ChipWrite::new(50, "opll", 0, 0x0E, 0x20), // rhythm on
            ChipWrite::new(150, "opll", 0, 0x0E, 0x30), // BD rises
            ChipWrite::new(250, "opll", 0, 0x0E, 0x20), // BD falls
            ChipWrite::new(320, "opll", 0, 0x0E, 0x00), // rhythm off
            // A latch outside rhythm mode never counts.
            ChipWrite::new(400, "opll", 0, 0x0E, 0x01),
        ];
        let (drums, stats) = opll_rhythm_plan(&vgm, &[0]);
        assert!(drums);
        assert_eq!(stats["latches"], 1);
        assert_eq!(stats["mode_samples"], 270);
        assert_eq!(stats["melodic_on_kit"], 1);
        assert_eq!(stats["segments"], 2);
        assert_eq!(
            stats.keys().map(String::as_str).collect::<Vec<_>>(),
            ["latches", "mode_samples", "melodic_on_kit", "segments"]
        );

        // Enabling the mode without a key leaves every latch at zero.
        let mut vgm = VgmFile::default();
        vgm.total_samples = 400;
        vgm.writes = vec![
            ChipWrite::new(0, "opll", 0, 0x0E, 0x00),
            ChipWrite::new(100, "opll", 0, 0x0E, 0x20),
        ];
        let (drums, stats) = opll_rhythm_plan(&vgm, &[0]);
        assert!(!drums);
        assert_eq!(stats["latches"], 0);
        assert_eq!(stats["mode_samples"], 300);
        assert_eq!(stats["segments"], 1);
    }
}
