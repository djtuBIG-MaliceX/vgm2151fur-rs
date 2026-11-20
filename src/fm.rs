//! Shared FM patch model for the OPM (YM2151), OPN (YM2203) and OPL paths.
//!
//! OPM and OPN have four operators per channel and the same eight connection
//! algorithms, so the note events, the instrument list and the .fur FM feature
//! carry one model.  The fields that differ are the register meaning of the
//! per-operator bytes: OPM keeps DT2 and D2R in one register, OPN has a separate
//! sustain rate (0x70) and SSG-EG (0x90), and no DT2.  ``FMPatch.opn`` tells the
//! writer which layout to emit.
//!
//! The OPL family joins it with two operators per channel (four on an OPL3 4-op
//! pair) and its own register split, so ``FMPatch.opl``/``FMPatch.four`` pick the
//! third layout.  An OPL operator has no DT1/DT2/D2R and a 6-bit TL, so the
//! decoded OPL fields ride beside the raw OPM/OPN bytes.

// YM2151 (OPM) carrier sets in its register order (0x40/0x48/0x50/0x58).
// Algorithms 0-3 are chains ending on op4, 5 and 6 add op1-fed carriers, 7 is
// additive. Algorithm 4 outputs on op3 and op4. Across 49 YM2151 rips every
// mid-note TL automation is on that pair, always together, and a driver
// automates what the platform scales. The register order is plain S1..S4 (see
// OPM_OP_TO_MASK), so indices are slot numbers minus one.
pub const CARRIERS: [&[i32]; 8] = [
    &[3],
    &[3],
    &[3],
    &[3],
    &[2, 3],
    &[1, 2, 3],
    &[1, 2, 3],
    &[0, 1, 2, 3],
];

// OPN (YM2203/YM2612) carrier sets in *register* order.  The OPN register
// pages are S1, S3, S2, S4 (0x30/0x34/0x38/0x3C), while the datasheet's
// algorithm diagrams use S1..S4.  Furnace's Genesis/OPN platform uses the
// same register order (opOffs 0,4,8,12) and its isOutput table is:
//   alg 4 -> S2, S4 = registers 0x38, 0x3C -> indices 2, 3
// Using the OPM/datasheet-order pair (1,3) here normalizes S3 and S4 and
// leaves the real carrier S2 at full level, which changes both gain and the
// modulator/carrier balance of every algorithm-4 voice.
pub const OPN_CARRIERS: [&[i32]; 8] = [
    &[3],
    &[3],
    &[3],
    &[3],
    &[2, 3],
    &[1, 2, 3],
    &[1, 2, 3],
    &[0, 1, 2, 3],
];

// OPL/OPL2/OPL3 two-operator channels have one connection bit (0xC0 bit 0):
// 0 is FM (op1 modulates op2) and 1 is additive, so both operators sound.
// Furnace keeps the four-value field of its 4-op mode, and its isOutputL table
// lets algorithms 2 and 3 alias 0 and 1 on a two-operator voice (opl.cpp).
pub const OPL_CARRIERS: [&[i32]; 4] = [&[1], &[0, 1], &[1], &[0, 1]];

// OPL3 4-op algorithms.  The instrument's operator order is not the chain
// order: commitState walks the slot rows with orderedOpsL (0,2,1,3), so row 3
// is instrument operator 3, row 2 is operator 1 and row 1 is operator 2.
// isOutputL[1] marks the rows that reach the output:
//   0 (1>2>3>4) row 3, 1 (1+(2>3>4)) rows 0 and 3,
//   2 ((1>2)+(3>4)) rows 1 and 3, 3 (1+(2>3)+4) rows 0, 2 and 3.
pub const OPL4_CARRIERS: [&[i32]; 4] = [&[3], &[0, 3], &[2, 3], &[0, 1, 3]];

pub fn carriers_for(alg: i32, opn: bool, opl: bool, four: bool) -> Vec<i32> {
    if opl {
        let table: &[&[i32]] = if four { &OPL4_CARRIERS } else { &OPL_CARRIERS };
        match table.get((alg & 3) as usize) {
            Some(ops) => ops.to_vec(),
            // Python: (1,) if not four else (3,)
            None => {
                if four {
                    vec![3]
                } else {
                    vec![1]
                }
            }
        }
    } else {
        let table: &[&[i32]] = if opn { &OPN_CARRIERS } else { &CARRIERS };
        match table.get((alg & 7) as usize) {
            Some(ops) => ops.to_vec(),
            None => vec![3],
        }
    }
}

// The 0x28 KON mask bits follow the datasheet operator order S1,S2,S3,S4,
// while DivInstrumentFM/OpRegs store the operators in register order
// S1,S3,S2,S4.  Index by *register* operator: which mask bit enables it.
pub const OPN_OP_TO_MASK: [i32; 4] = [0, 2, 1, 3];

// Ext channel-3 layout: which ops[] entry an operator column plays.
// DivPlatformYM2203Ext::dispatch maps its four columns (channels 2..5) as
// `int ch = c.chan - 2; int ordch = orderedOps[ch];` and writes the volume
// column's scaling to `chan[2].state.op[ordch]`. Channel 3 (column 1) therefore
// plays ops[2], not ops[1]. orderedOps is {0,2,1,3} in fmsharedbase.h.
// DIV_CMD_VOLUME applies scaling unconditionally, so every column's level is
// reachable through its own volume. DIV_CMD_FM_TL gates on KVS. The tuple
// matches OPN_OP_TO_MASK but means operator indices per column, not mask bits
// per operator.
pub const OPN_ORDERED_OPS: [i32; 4] = [0, 2, 1, 3];

pub fn opn_tl_op(reg: i32) -> i32 {
    // Pages advance by four within the channel, so $42/$46/$4A/$4E are
    // S1/S3/S2/S4 = ops[0..3].
    (reg >> 2) & 0x03
}

// The YM2151 key-on byte is twisted the same way: DivPlatformArcade::
// commitState composes it as (op[0]?1)|(op[2]?2)|(op[1]?4)|(op[3]?8). Bit1 of
// the logged mask enables operator 3 and bit2 enables operator 2. Its register
// pages are plain S1..S4 ($60/$68/$70/$78 = TL of op1..op4), unlike OPN.
// Reading the mask as identity blanks a keyed carrier and leaves an
// instrument whose every carrier sits at TL 7F.
pub const OPM_OP_TO_MASK: [i32; 4] = [0, 2, 1, 3];

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OpRegs {
    pub dt_mul: u8,
    pub tl: u8,
    pub ks_ar: u8,
    pub am_d1r: u8,
    pub dt2_d2r: u8,
    pub d1l_rr: u8,
    // YM2203 only: register 0x70 (sustain rate) and 0x90 (SSG-EG).  Kept in
    // the identity so a driver that switches an envelope shape starts a new
    // instrument.
    pub sr: u8,
    pub ssg_eg: u8,
    // OPL family only (FMPatch.opl): the decoded register fields.  An OPL
    // operator has no DT1/DT2/D2R and a 6-bit TL, so these cannot share the
    // raw OPM/OPN bytes above.  They take part in the identity.
    pub am: u8,   // 0x20 bit 7
    pub vib: u8,  // 0x20 bit 6
    pub sus: u8,  // 0x20 bit 5 (the period/hold flag, inverted)
    pub ksr: u8,  // 0x20 bit 4
    pub mult: u8, // 0x20 bits 3-0
    pub ksl: u8,  // 0x40 bits 7-6
    pub ar: u8,   // 0x60 bits 7-4
    pub dr: u8,   // 0x60 bits 3-0
    pub sl: u8,   // 0x80 bits 7-4
    pub rr: u8,   // 0x80 bits 3-0
    pub ws: u8,   // 0xE0 (absent on the YM3526)
}

impl Default for OpRegs {
    fn default() -> Self {
        OpRegs {
            dt_mul: 0x01,
            tl: 0x7F,
            ks_ar: 0x1F,
            am_d1r: 0x00,
            dt2_d2r: 0x00,
            d1l_rr: 0xF7,
            sr: 0,
            ssg_eg: 0,
            am: 0,
            vib: 0,
            sus: 0,
            ksr: 0,
            mult: 1,
            ksl: 0,
            ar: 0,
            dr: 15,
            sl: 0,
            rr: 0,
            ws: 0,
        }
    }
}

impl OpRegs {
    /// Flattened form of the Python tuple returned by `as_tuple`.
    ///
    /// Python returns a nested tuple; every component is an integer, and the
    /// nested `opl` tuple is appended last.  Concatenating the same values in
    /// the same order keeps equality/hash behavior identical because all four
    /// operators within one identity use the same `include_tl`.
    pub fn as_tuple(&self, include_tl: bool) -> Vec<i64> {
        let opl = [
            self.am, self.vib, self.sus, self.ksr, self.mult, self.ksl, self.ar, self.dr, self.sl,
            self.rr, self.ws,
        ];
        let mut out: Vec<i64>;
        if include_tl {
            out = vec![
                self.dt_mul,
                self.tl,
                self.ks_ar,
                self.am_d1r,
                self.dt2_d2r,
                self.d1l_rr,
                self.sr,
                self.ssg_eg,
            ]
            .into_iter()
            .map(i64::from)
            .collect();
        } else {
            out = vec![
                self.dt_mul,
                self.ks_ar,
                self.am_d1r,
                self.dt2_d2r,
                self.d1l_rr,
                self.sr,
                self.ssg_eg,
            ]
            .into_iter()
            .map(i64::from)
            .collect();
        }
        out.extend(opl.into_iter().map(i64::from));
        out
    }
}

/// Flat encoding of the tuple Python `FMPatch.identity` returns.
///
/// Python builds differently shaped nested tuples per branch; in Rust the
/// comparison/hash key is one flat `Vec<i64>` in the same preorder, which is
/// equality-equivalent because all four operator tuples have the same length
/// within a single identity.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct FmIdentity(pub Vec<i64>);

#[derive(Clone, Debug, PartialEq)]
pub struct FMPatch {
    pub alg: i32,
    pub fb: i32,
    pub ams: i32,
    pub pms: i32,
    pub ops: [OpRegs; 4], // op1..op4 (register order)
    pub name: String,
    pub uses: i32,
    pub fms_macro: Vec<i32>,
    pub ams_macro: Vec<i32>,
    pub macro_speed: i32,
    // True for a YM2203 (OPN) instrument: no DT2, the 0x70 register is the
    // sustain rate, and 0x90 is SSG-EG.
    pub opn: bool,
    // True for an OPL-family instrument (DIV_INS_OPL). `four` marks an OPL3
    // 4-op voice, whose operators are the pair's four slots; `drum` marks the
    // DIV_INS_OPL_DRUMS instrument that carries snare/tom/cymbal/hi-hat.
    pub opl: bool,
    pub four: bool,
    pub drum: bool,
    // True for a YM2413 (OPLL) instrument (DIV_INS_OPLL). It has two
    // operators and one of sixteen fixed ROM presets or a single user patch
    // (`preset` = the $3x instrument nibble; 0 is the user patch).
    pub opll: bool,
    pub preset: i32,
    // Per-note pitch motion for OPN: absolute pitch2 values in 1/128
    // semitones (the module uses Furnace's default linear pitch mode).
    pub pitch_macro: Vec<i32>,
    pub pitch_speed: i32,
    pub pitch_loop: i32,
    // OPLL only: tick-rate preset / volume / retrigger automation. The chip
    // changes `$3x`'s high nibble through `std.wave`, its low nibble through
    // `std.vol`, and a repeated key-on write through `std.phaseReset`.
    pub wave_macro: Vec<i32>,
    pub wave_speed: i32,
    pub wave_loop: i32,
    pub vol_macro: Vec<i32>,
    pub vol_speed: i32,
    pub vol_loop: i32,
    pub phase_macro: Vec<i32>,
    pub phase_speed: i32,
    pub phase_loop: i32,
    // OPL only: per-operator output level automation as the log wrote it.
    // Entries are (operator index, values, speed): from the key-on the
    // operator takes one absolute `$4n` entry every `speed` ticks and holds
    // the last. The macro starts with the note and has no delay. A level
    // that first moves several ticks in repeats its starting entry across
    // that hold. The volume column reaches only an operator marked KVS. A
    // scripted modulator level has no other path. Furnace reads these from
    // an O1..O4 feature (macro code 6, DIV_MACRO_OP_TL).
    pub op_tl_macro: Vec<(i32, Vec<i32>, i32)>,
    // OPM only: per-operator DT1/MULT automation as raw register values.
    // Entries are (operator index, values, speed) and use macro code 9
    // (DIV_MACRO_OP_DT) and 3 (DIV_MACRO_OP_MULT). A fast multiplier
    // staircase on a modulator or carrier changes the partial's frequency
    // inside one short note; it has no row-level equivalent.
    pub op_dt_macro: Vec<(i32, Vec<i32>, i32)>,
    pub op_dt2_macro: Vec<(i32, Vec<i32>, i32)>,
    pub op_mul_macro: Vec<(i32, Vec<i32>, i32)>,
    // For an instrument copy made by a macro pass: the instrument this one
    // was copied from, or -1 for a base voice. The final budget trim folds a
    // copy back onto its base when the module would otherwise exceed the
    // one-byte instrument index range.
    pub base_ins: i32,
}

impl Default for FMPatch {
    /// Python's dataclass has no all-default constructor: `alg`, `fb`, `ams`,
    /// `pms` and `ops` are required.  The required head takes Rust's zero
    /// value so `..FMPatch::default()` can supply the defaulted tail exactly
    /// as the dataclass does; every Python default below is literal.
    fn default() -> Self {
        FMPatch {
            alg: 0,
            fb: 0,
            ams: 0,
            pms: 0,
            ops: core::array::from_fn(|_| OpRegs::default()),
            name: String::new(),
            uses: 0,
            fms_macro: Vec::new(),
            ams_macro: Vec::new(),
            macro_speed: 1,
            opn: false,
            opl: false,
            four: false,
            drum: false,
            opll: false,
            preset: 0,
            pitch_macro: Vec::new(),
            pitch_speed: 1,
            pitch_loop: 255,
            wave_macro: Vec::new(),
            wave_speed: 1,
            wave_loop: 255,
            vol_macro: Vec::new(),
            vol_speed: 1,
            vol_loop: 255,
            phase_macro: Vec::new(),
            phase_speed: 1,
            phase_loop: 255,
            op_tl_macro: Vec::new(),
            op_dt_macro: Vec::new(),
            op_dt2_macro: Vec::new(),
            op_mul_macro: Vec::new(),
            base_ins: -1,
        }
    }
}

impl FMPatch {
    /// Cluster key: full timbre including carrier TLs.
    ///
    /// Furnace applies volume on a log curve, so a stripped TL cannot be
    /// reconstructed from pattern volume. Keep TL on the instrument.
    /// OPN also keeps PMS/AMS: the platform writes 0xB4 from the instrument
    /// at note-on only when the instrument changes, so two patches that
    /// differ only in LFO sensitivity must not merge.
    pub fn identity(&self) -> FmIdentity {
        let op_keys: Vec<Vec<i64>> = (0..4)
            .map(|i| self.ops[i].as_tuple(true))
            .collect();
        let mut w: Vec<i64> = Vec::new();
        if self.opll {
            // The OPLL selects a ROM voice with the instrument nibble. Two
            // presets carry the same idle operators, so the nibble is part of
            // the timbre. `alg` is the per-operator sustain bit here.
            w.extend([
                i64::from(self.alg & 1),
                i64::from(self.fb & 7),
                i64::from(self.pms & 1),
                i64::from(self.ams & 1),
                i64::from(self.opll),
                i64::from(self.preset),
            ]);
        } else if self.opl {
            // The OPL platform has no channel LFO depth register, and 0xB4's
            // PMS/AMS fields do not exist; the connection bit and, on OPL3,
            // the waveforms are all in the operators.
            w.extend([
                i64::from(self.alg & 3),
                i64::from(self.fb & 7),
                i64::from(self.opl),
                i64::from(self.four),
                i64::from(self.drum),
            ]);
        } else if self.opn {
            w.extend([
                i64::from(self.alg & 7),
                i64::from(self.fb & 7),
                i64::from(self.opn),
                i64::from(self.ams),
                i64::from(self.pms),
            ]);
        } else {
            w.extend([
                i64::from(self.alg & 7),
                i64::from(self.fb & 7),
                i64::from(self.opn),
            ]);
        }
        for key in op_keys {
            w.extend(key);
        }
        FmIdentity(w)
    }

    pub fn carriers(&self) -> Vec<i32> {
        if self.drum {
            return vec![0, 1, 2, 3];
        }
        if self.opll {
            // Two operators: op[0] modulates, op[1] is the output. `alg` is
            // the sustain bit on this chip and does not change the connection.
            return vec![1];
        }
        carriers_for(self.alg & 7, self.opn, self.opl, self.four)
    }

    pub fn carrier_tl(&self) -> i32 {
        let mask: u8 = if self.opll {
            0x0F
        } else if self.opl {
            0x3F
        } else {
            0x7F
        };
        let tls: Vec<i32> = self
            .carriers()
            .into_iter()
            .map(|i| i32::from(self.ops[i as usize].tl & mask))
            .collect();
        if tls.is_empty() {
            i32::from(mask)
        } else {
            *tls.iter().min().unwrap()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn carriers_for_algorithm_0() {
        // OPM register-order carrier sets: algorithms 0-3 are chains ending on op4.
        assert_eq!(carriers_for(0, false, false, false), vec![3]);
        assert_eq!(carriers_for(0, true, false, false), vec![3]);
        // OPL two-operator FM connection: only the second operator sounds.
        assert_eq!(carriers_for(0, false, true, false), vec![1]);
        // OPL3 4-op algorithm 0 (1>2>3>4) reaches the output on row 3.
        assert_eq!(carriers_for(0, false, true, true), vec![3]);
    }

    #[test]
    fn carriers_for_algorithm_7() {
        // Algorithm 7 is additive: every operator is a carrier on OPM and OPN.
        assert_eq!(carriers_for(7, false, false, false), vec![0, 1, 2, 3]);
        assert_eq!(carriers_for(7, true, false, false), vec![0, 1, 2, 3]);
        // OPL masks the algorithm with 3, so 7 aliases OPL algorithm 3.
        assert_eq!(carriers_for(7, false, true, false), vec![0, 1]);
        assert_eq!(carriers_for(7, false, true, true), vec![0, 1, 3]);
    }

    #[test]
    fn opn_tl_op_maps_pages_to_register_order() {
        assert_eq!(opn_tl_op(0x42), 0); // S1 -> ops[0]
        assert_eq!(opn_tl_op(0x46), 1); // S3 -> ops[1]
        assert_eq!(opn_tl_op(0x4A), 2); // S2 -> ops[2]
        assert_eq!(opn_tl_op(0x4E), 3); // S4 -> ops[3]
    }

    #[test]
    fn fmpatch_default_matches_python_defaults() {
        let patch = FMPatch::default();
        assert_eq!(patch.name, "");
        assert_eq!(patch.uses, 0);
        assert_eq!(patch.macro_speed, 1);
        assert_eq!(patch.pitch_loop, 255);
        assert_eq!(patch.wave_loop, 255);
        assert_eq!(patch.vol_loop, 255);
        assert_eq!(patch.phase_loop, 255);
        assert_eq!(patch.base_ins, -1);
        assert!(!patch.opn && !patch.opl && !patch.four && !patch.drum && !patch.opll);
        assert!(patch.fms_macro.is_empty());
        assert!(patch.op_tl_macro.is_empty());
        // OpRegs defaults: 0x01 / 0x7F / 0x1F / 0x00 / 0x00 / 0xF7, mult 1, dr 15.
        assert_eq!(patch.ops[0], OpRegs::default());
        assert_eq!(patch.ops[0].dt_mul, 0x01);
        assert_eq!(patch.ops[0].tl, 0x7F);
        assert_eq!(patch.ops[0].ks_ar, 0x1F);
        assert_eq!(patch.ops[0].am_d1r, 0x00);
        assert_eq!(patch.ops[0].dt2_d2r, 0x00);
        assert_eq!(patch.ops[0].d1l_rr, 0xF7);
        assert_eq!(patch.ops[0].mult, 1);
        assert_eq!(patch.ops[0].dr, 15);
        // An all-default OPM patch drives op4 (index 3) and reports its TL.
        assert_eq!(patch.carriers(), vec![3]);
        assert_eq!(patch.carrier_tl(), 0x7F);
    }
}
