//! Literal port of the top of `vgm2151fur/furwrite.py` (v0.11.0): module
//! constants, the detune tables, channel-name helpers, player volume helpers,
//! the little-endian [`Buf`], and the zlib container helpers.

use std::io::{Read, Write};
use std::sync::LazyLock;

use indexmap::IndexMap;

use crate::model::Song;
use crate::parity::py_floor_div;
use crate::vgm::VgmFile;

// Furnace arcade.cpp: rWrite(DT, dtTable[op.dt]<<4). Inverse of
// dtTable = (7, 6, 5, 0, 1, 2, 3, 4) so YM2151 register DT survives the
// second mapping. Raw DT in the file is heard as the wrong detune.
pub const YM_DT_TO_FURNACE: [i32; 8] = [3, 4, 5, 6, 7, 2, 1, 0];
// OPN DT1 has the same positive 0..3 / negative -1..-3 encoding, but 4 is
// "negative zero" and must behave as 0.  Furnace's dt index 7 maps to raw 4,
// which is positive one step, so use a separate table for OPN.
pub const OPN_DT_TO_FURNACE: [i32; 8] = [3, 4, 5, 6, 3, 2, 1, 0];

pub fn ym_dt_to_furnace(ym_dt: i32) -> i32 {
    YM_DT_TO_FURNACE[(ym_dt & 7) as usize]
}

pub fn opn_dt_to_furnace(ym_dt: i32) -> i32 {
    OPN_DT_TO_FURNACE[(ym_dt & 7) as usize]
}

/// YM2151/YM2612 0x40 or 0x30 register → Furnace DT/MULT byte.
///
/// Python default: `opn=False`.
pub fn pack_dt_mul(dt_mul: i32, opn: bool) -> i32 {
    let dt = (dt_mul >> 4) & 7;
    let index = if opn {
        opn_dt_to_furnace(dt)
    } else {
        ym_dt_to_furnace(dt)
    };
    (index << 4) | (dt_mul & 0x0F)
}

pub const FUR_MAGIC: &[u8] = b"-Furnace module-";
pub const FUR_VERSION: i32 = 157; // PATN notes; INFO still the old block (not INF2)
pub const CHIP_YM2151: u8 = 0x82;
// Furnace's YM2203 (OPN): three FM channels followed by the AY-compatible
// SSG's three voices, six channels per instance.
pub const CHIP_YM2203: u8 = 0x8D;
// Extended channel 3: FM1, FM2, the four operators of FM3, then PSG1-3.
pub const CHIP_YM2203_EXT: u8 = 0xB6;
// Furnace's YM2612 (OPN2): six FM channels per instance; channel 6 can be a
// sample player.  The Ext layout splits channel 3 into four operator columns
// (9 channels).  The CSM layout adds a timer channel (10 channels).
pub const CHIP_YM2612: u8 = 0x83;
pub const CHIP_YM2612_EXT: u8 = 0xA0;
pub const CHIP_YM2612_CSM: u8 = 0xC1;
// Furnace's YM2610 (4 FM + 3 SSG + 6 ADPCM-A + 1 ADPCM-B = 14 channels) and
// YM2610B (6 FM, 16 channels).
pub const CHIP_YM2610: u8 = 0xA5;
pub const CHIP_YM2610B: u8 = 0x9E;
// Furnace's YM2608 (OPNA): the YM2610B's channel order with rhythm in place of
// ADPCM-A - 6 FM + 3 SSG + 6 rhythm + 1 ADPCM-B = 16 channels, or 19 with
// channel 3 split into four operator columns.
pub const CHIP_YM2608: u8 = 0x8E;
pub const CHIP_YM2608_EXT: u8 = 0xB7;
// TI SN76489 (the SMS platform): three square voices then noise.  A Genesis
// module carries it as a second system after the YM2612.
pub const CHIP_SMS: u8 = 0x03;
// DIV_SYSTEM_T6W28: the Neo Geo Pocket Color's stereo SN76489 variant.
// Four channels (three square + noise), independent panL/panR.
pub const CHIP_T6W28: u8 = 0xBF;
pub const T6W28_DEFAULT_CLOCK: u32 = 3_072_000;
pub const CHIP_MSM6295: u8 = 0xAA;
pub const CHIP_MSM6258: u8 = 0xAB;
pub const CHIP_K007232: u8 = 0xC6;
pub const CHIP_SEGAPCM: u8 = 0x9B;
// Furnace hosts the RF5C68 (FM Towns) and RF5C164 (Sega CD) as one system.
// The chipType flag picks the variant: 0 = RF5C68, 1 = RF5C164.
pub const CHIP_RF5C68: u8 = 0x95;
pub const CHIP_C140: u8 = 0xCE;
// DIV_SYSTEM_C219: sixteen 8-bit/mu-law PCM voices with noise and surround.
pub const CHIP_C219: u8 = 0xCF;
pub const CHIP_K053260: u8 = 0xCC;
// DIV_SYSTEM_YMZ280B: eight PCM/ADPCM sample voices.
pub const CHIP_YMZ280B: u8 = 0xB8;
// Furnace's native Yamaha YMW258-F (MultiPCM): 28 sample voices per instance.
pub const CHIP_MULTIPCM: u8 = 0x92;
// Furnace's Generic PCM DAC, used for streamed sample chips with no native
// platform (uPD7759, Sega 32X PWM).
pub const CHIP_PCM_DAC: u8 = 0xC0;
// DIV_SYSTEM_VBOY: five wavetable voices and one noise voice.
pub const CHIP_VBOY: u8 = 0x9C;
// Furnace format.md reserves 0xD0 for Namco C352, 32 channels, and marks it
// unavailable. 0.6.8.3 has no C352 core and no C352 instrument type.
pub const CHIP_C352: u8 = 0xD0;
// Konami SCC (K051649, voices 3/4 share wave RAM) and SCC+ (K052539,
// independent voice 4 waveform). AY-8910 is the three-channel PSG.
pub const CHIP_K051649: u8 = 0xA1;
pub const CHIP_K052539: u8 = 0xB4;
pub const CHIP_AY8910: u8 = 0x80;
// Konami K005289 (Bubble System WSG): two 32x16 wavetable voices.  Furnace's
// platform uses the SCC instrument editor and defaults to COLOR_NTSC.
pub const CHIP_K005289: u8 = 0xAD;
// DIV_SYSTEM_POKEY: four voices, AUDCTL waveform/filter/join features.
pub const CHIP_POKEY: u8 = 0x94;
// DIV_SYSTEM_LYNX: Atari Lynx Mikey, four LFSR/DAC voices.
pub const CHIP_LYNX: u8 = 0xA8;
pub const INS_MIKEY: i32 = 23;
// DIV_SYSTEM_X1_010: Seta/Allumer X1-010, sixteen wavetable/PCM voices.
pub const CHIP_X1_010: u8 = 0xB0;
pub const INS_X1_010: i32 = 25;
// DIV_SYSTEM_FDS: one 64-step wavetable voice.
pub const CHIP_FDS: u8 = 0x8A;
pub const INS_FDS: i32 = 15;
// DIV_SYSTEM_SWAN: four wavetable/PCM voices.
pub const CHIP_SWAN: u8 = 0x96;
pub const INS_SWAN: i32 = 22;
// DIV_SYSTEM_QSOUND: sixteen PCM and three ADPCM voices.
pub const CHIP_QSOUND: u8 = 0xE0;
pub const INS_QSOUND: i32 = 40;
// DIV_SYSTEM_GA20: Irem's four-voice 8-bit sample player.
pub const CHIP_GA20: u8 = 0xC7;
pub const INS_GA20: i32 = 46;
// DIV_SYSTEM_MSM5232: OKI's eight-voice tone/noise generator.
pub const CHIP_MSM5232: u8 = 0xBC;
pub const INS_MSM5232: i32 = 43;
// DIV_SYSTEM_SAA1099: six square/noise voices, no software timers.  A pair of
// these is the Creative Music System / Game Blaster.
pub const CHIP_SAA1099: u8 = 0x97;
// The OPL family. Furnace gives the rhythm mode its own system: the drums
// layout turns the last melodic channels into kick, snare, tom, cymbal and
// hi-hat columns (9 -> 11 channels on OPL/OPL2, 18 -> 20 on OPL3).
pub const CHIP_OPL: u8 = 0x8F;
pub const CHIP_OPL2: u8 = 0x90;
pub const CHIP_OPL3: u8 = 0x91;
pub const CHIP_OPL_DRUMS: u8 = 0xA2;
pub const CHIP_OPL2_DRUMS: u8 = 0xA3;
pub const CHIP_OPL3_DRUMS: u8 = 0xA4;
// Channels per OPL system and how many of them are melodic. The rhythm systems
// give the kick the first former FM channel and snare, tom, cymbal and hi-hat
// the next four (sysDef.cpp); the kick keeps its two operators, so it plays a
// normal OPL voice.
// The Y8950 is an OPL2 with one ADPCM column after the FM block. The OPL4 is
// an OPL3 with 24 wave columns after it.
pub const CHIP_Y8950: u8 = 0xB2;
pub const CHIP_Y8950_DRUMS: u8 = 0xB3;
pub const CHIP_OPL4: u8 = 0xAE;
pub const CHIP_OPL4_DRUMS: u8 = 0xAF;
/// A Python dict with `(kind, drums)` string keys, kept in insertion order.
///
/// Indexing accepts a `&str` of any lifetime, like a Python string key.
pub struct OplTable<T> {
    pub map: IndexMap<(&'static str, bool), T>,
}

impl<T> OplTable<T> {
    pub fn new(entries: impl IntoIterator<Item = ((&'static str, bool), T)>) -> Self {
        OplTable {
            map: entries.into_iter().collect(),
        }
    }

    pub fn get(&self, kind: &str, drums: bool) -> Option<&T> {
        self.map
            .iter()
            .find(|(k, _)| k.0 == kind && k.1 == drums)
            .map(|(_, v)| v)
    }
}

impl<T> std::ops::Index<&(&str, bool)> for OplTable<T> {
    type Output = T;

    fn index(&self, key: &(&str, bool)) -> &T {
        self.get(key.0, key.1)
            .unwrap_or_else(|| panic!("KeyError: {:?}", key))
    }
}

pub static OPL_CHIP: LazyLock<OplTable<u8>> = LazyLock::new(|| {
    OplTable::new([
        (("opl", false), CHIP_OPL),
        (("opl2", false), CHIP_OPL2),
        (("opl3", false), CHIP_OPL3),
        (("opl", true), CHIP_OPL_DRUMS),
        (("opl2", true), CHIP_OPL2_DRUMS),
        (("opl3", true), CHIP_OPL3_DRUMS),
        (("y8950", false), CHIP_Y8950),
        (("y8950", true), CHIP_Y8950_DRUMS),
        (("opl4", false), CHIP_OPL4),
        (("opl4", true), CHIP_OPL4_DRUMS),
    ])
});
// The YM2413 (OPLL) is its own core and instrument type. Rhythm mode is a
// separate system whose last three hardware channels become five kit columns.
pub const CHIP_OPLL: u8 = 0x89;
pub const CHIP_OPLL_DRUMS: u8 = 0xA7;
// DIV_SYSTEM_VRC7: the same OPLL core with six channels and the VRC7 preset
// ROM.  DIV_SYSTEM_NES: five channels (two pulses, triangle, noise, DPCM).
pub const CHIP_VRC7: u8 = 0x9D;
pub const CHIP_NES: u8 = 0x06;
// DIV_SYSTEM_GB: four channels (two pulses, wavetable, noise). The chip's
// chipType flag selects the emulated model (0 DMG, 2 CGB-E); the converter
// writes CGB-E so the zombie-envelope rule matches.
pub const CHIP_GB: u8 = 0x04;
// DIV_SYSTEM_PCE: six wavetable voices (two pulses, triangle, noise, DPCM on
// the NES sit at 0x06).
pub const CHIP_PCE: u8 = 0x05;
pub const OPLL_CHANNELS: i32 = 9;
pub const OPLL_DRUMS_CHANNELS: i32 = 11;
pub const OPLL_DRUMS_MELODIC: i32 = 6;
pub const OPLL_DRUM_LABELS: [(&str, &str); 5] = [
    ("Kick", "BD"),
    ("Snare", "SD"),
    ("Tom", "TM"),
    ("Top", "TP"),
    ("HiHat", "HH"),
];
// The OPL4's FM half is OPL3-shaped; its 24 wave columns are counted by
// `_system_span` on top of these.
pub static OPL_CHANNELS: LazyLock<OplTable<i32>> = LazyLock::new(|| {
    OplTable::new([
        (("opl", false), 9),
        (("opl2", false), 9),
        (("opl3", false), 18),
        (("opl", true), 11),
        (("opl2", true), 11),
        (("opl3", true), 20),
        (("y8950", false), 9),
        (("y8950", true), 11),
        (("opl4", false), 18),
        (("opl4", true), 20),
    ])
});
pub static OPL_MELODIC: LazyLock<OplTable<i32>> = LazyLock::new(|| {
    OplTable::new([
        (("opl", false), 9),
        (("opl2", false), 9),
        (("opl3", false), 18),
        (("opl", true), 6),
        (("opl2", true), 6),
        (("opl3", true), 15),
        (("y8950", false), 9),
        (("y8950", true), 6),
        (("opl4", false), 18),
        (("opl4", true), 15),
    ])
});
// Furnace's channel labels. An OPL3 pair whose first channel is even can head a
// four-operator voice, and the system names it that way whether or not a note
// uses it.
pub const OPL_DRUM_LABELS: [(&str, &str); 5] = [
    ("Kick/FM 7", "BD"),
    ("Snare", "SD"),
    ("Tom", "TM"),
    ("Top", "TP"),
    ("HiHat", "HH"),
];

/// The long and short channel names of one OPL system (sysDef.cpp).
///
/// The kick is the FM channel rhythm mode takes over, so its label keeps both
/// names. An OPL3 shortens every channel to its number and marks the pairs
/// that can head a four-operator voice.
pub fn opl_channel_names(kind: &str, drums: bool) -> (Vec<String>, Vec<String>) {
    let total = OPL_CHANNELS[&(kind, drums)];
    let melodic = OPL_MELODIC[&(kind, drums)];
    let mut long_names: Vec<String> = Vec::new();
    let mut short_names: Vec<String> = Vec::new();
    for ch in 0..total {
        let (long_name, short_name);
        if drums && ch >= melodic {
            let where_ = (ch - melodic).min(OPL_DRUM_LABELS.len() as i32 - 1);
            long_name = if where_ == 0 {
                format!("Kick/FM {}", melodic + 1)
            } else {
                OPL_DRUM_LABELS[where_ as usize].0.to_string()
            };
            short_name = OPL_DRUM_LABELS[where_ as usize].1.to_string();
        } else if kind == "opl3" {
            long_name = if ch < 12 && (ch & 1) == 0 {
                format!("4OP {}", ch + 1)
            } else {
                format!("FM {}", ch + 1)
            };
            short_name = (ch + 1).to_string();
        } else {
            long_name = format!("FM {}", ch + 1);
            short_name = format!("F{}", ch + 1);
        }
        long_names.push(long_name);
        short_names.push(short_name);
    }
    (long_names, short_names)
}

/// The long and short channel names of the OPLL, VRC7 and drums sibling.
///
/// Python default: `vrc7=False`.
pub fn opll_channel_names(drums: bool, vrc7: bool) -> (Vec<String>, Vec<String>) {
    let total = if vrc7 {
        6
    } else if drums {
        OPLL_DRUMS_CHANNELS
    } else {
        OPLL_CHANNELS
    };
    let mut long_names: Vec<String> = Vec::new();
    let mut short_names: Vec<String> = Vec::new();
    for ch in 0..total {
        if drums && ch >= OPLL_DRUMS_MELODIC {
            let where_ = (ch - OPLL_DRUMS_MELODIC) as usize;
            long_names.push(OPLL_DRUM_LABELS[where_].0.to_string());
            short_names.push(OPLL_DRUM_LABELS[where_].1.to_string());
        } else {
            long_names.push(format!("FM {}", ch + 1));
            short_names.push(format!("F{}", ch + 1));
        }
    }
    (long_names, short_names)
}

pub const INS_OPM: i32 = 33;
// DIV_INS_FM: the generic OPN instrument used by the YM2203.  Furnace reads
// its 0x70/0x90 fields from the same INS2 "FM" feature.
pub const INS_FM: i32 = 1;
// DIV_INS_OPL and its drums sibling. The OPL editor shows two operators per
// voice, and the drums instrument carries the snare/tom/cymbal/hi-hat
// operators in its operator slots.
pub const INS_OPL: i32 = 14;
pub const INS_OPL_DRUMS: i32 = 32;
// DIV_INS_OPLL: the YM2413's own instrument type, two operators and a preset.
pub const INS_OPLL: i32 = 13;
pub const INS_MSM6258: i32 = 35;
pub const INS_MSM6295: i32 = 36;
pub const INS_ADPCMA: i32 = 37;
pub const INS_ADPCMB: i32 = 38;
pub const INS_SEGAPCM: i32 = 39;
// DIV_INS_YMZ280B: one sample body per instrument (or a 120-slot note map
// when the module's instrument budget requires it).
pub const INS_YMZ280B: i32 = 41;
pub const INS_RF5C68: i32 = 42;
// DIV_INS_AMIGA is the Generic Sample instrument used by the Generic PCM DAC.
pub const INS_AMIGA: i32 = 4;
// DIV_INS_MULTIPCM: the OPL4 wave slot's sample-map instrument.
pub const INS_MULTIPCM: i32 = 28;
pub const INS_K007232: i32 = 45;
pub const INS_K053260: i32 = 50;
pub const INS_C140: i32 = 53;
pub const INS_C219: i32 = 54;
// DIV_INS_AMIGA. C140 channels already accept this sample type, so a future
// C352 core copied from c140.cpp can play the bodies. There is no DIV_INS_C352.
pub const INS_C352: i32 = 4;
// DIV_INS_AY (volume/noise/envelope macros) and DIV_INS_SCC (32-byte wavetable
// through the wave synth).
pub const INS_AY: i32 = 6;
pub const INS_STD: i32 = 0;
pub const INS_SCC: i32 = 18;
// DIV_INS_SAA1099: standard macros, with the raw amplitude carried by the
// pan-left/pan-right macros and the envelope by ex1.
pub const INS_SAA: i32 = 9;
// DIV_INS_POKEY: volume and pitch macros; AUDCTL and waveform are 11xx/10xx
// pattern effects (the platform's duty and wave macros share those slots).
pub const INS_POKEY: i32 = 20;
// DIV_INS_PCE: the HuC6280's 32x5-bit wavetable, through the same wave synth
// as the SCC. Its direct-D/A bodies use DIV_INS_AMIGA (INS_C352 above).
pub const INS_PCE: i32 = 5;
// DIV_INS_T6W28: the T6W28's plain square/noise instrument.  It shares the
// standard macro layout; only the platform system differs.
pub const INS_T6W28: i32 = 44;
// DIV_INS_NES: the 2A03 pulse/triangle/noise and DPCM instrument.  The DPCM
// sample rides the same `amiga` sample feature (1-bit DPCM or 8-bit PCM).
pub const INS_NES: i32 = 34;
// DIV_INS_GB: the Game Boy's hardware envelope plus an optional wave-synth
// feature for the wavetable voice.
pub const INS_GB: i32 = 2;
// DIV_INS_VBOY: a wavetable instrument with the FD modulation table and the
// wave-synth feature.  VSU wave changes ride the pattern as 10xx effects.
pub const INS_VBOY: i32 = 16;
pub const SMP_DEPTH_DPCM: i32 = 1;
pub const SMP_DEPTH_ADPCM_K: i32 = 7;
pub const SMP_DEPTH_ADPCM_A: i32 = 5;
pub const SMP_DEPTH_ADPCM_B: i32 = 6;
pub const SMP_DEPTH_YMZ_ADPCM: i32 = 3;
pub const SMP_DEPTH_QSOUND_ADPCM: i32 = 4;
pub const SMP_DEPTH_8BIT: i32 = 8;
pub const SMP_DEPTH_12BIT: i32 = 12;
pub const SMP_DEPTH_16BIT: i32 = 16;
pub const SMP_DEPTH_4BIT: i32 = 15;
pub const SMP_DEPTH_VOX: i32 = 10;
// C219 mu-law bodies are expanded to 8-bit linear in `c219.py`; there is no
// separate sample depth for them here.
// Default sample-upload masks. write_fur narrows this when a MultiPCM system
// is present so unrelated bodies do not fill its 2 MB sample memory.
pub const DEFAULT_RENDER_MASKS: [u32; 4] = [0xFFFFFFFF, 0xFFFFFFFF, 0xFFFFFFFF, 0xFFFFFFFF];
// Furnace's DAC rate is calcFreq(C-4) * sampleRate / getCenterRate(); the
// engine's C-4 reference is a hair sharp, so a 22050 Hz sample exports as
// 22073.  Compensate at write time so the DAC clock matches the source.
pub const DAC_RATE_COMPENSATION: f64 = 22050.0 / 22073.0;
// WonderSwan native sample channel: at note 108 the period table yields 183
// and the platform computes dacRate = (clock/2)/(off*183).  Store the rate
// that makes that expression equal the reconstructed source rate.
pub const WS_PCM_RATE_COMPENSATION: f64 = 2.0 * 183.0 * 8363.0 / 3072000.0;
// Furnace keeps oldCenterRate=true (8363 Hz), while the linear-pitch C-4
// reference is 440 * 2^(4.25) = 8372.  ADPCM-B's rate is derived through
// calcFreq, so its stored centerRate needs the 8372/8363 correction.
pub const ADPCM_B_RATE_COMPENSATION: f64 = 8363.0 / 8372.0;
// Furnace system defaults used to decide when to send customClock.
pub const SEGAPCM_CLOCK_X: u32 = 4000000;
pub const SEGAPCM_CLOCK_Y: u32 = 4026987;
pub const C140_FURNACE_DEFAULT_CLOCK: u32 = 8192000;
pub const NOTE_OFF: i32 = 180;
pub const EMPTY: i32 = 0xFFFF;
// Furnace's pattern length ceiling (engine `defines.h` DIV_MAX_ROWS) and the
// empty rows a pattern keeps past its last content row.
pub const PAT_LEN_MAX: i32 = 256;
pub const PAT_TAIL_ROWS: i32 = 10;
// Chip output levels follow libvgm's VGMPlayer: 8.8 fixed-point defaults with
// 0x100 = YM2151 unity, plus extra-header mix entries (bit 15 = relative).
// SN76489 defaults to 0.500. Absolute extra-header entries override.
pub static _DEV_DEFAULT_VOLUME: LazyLock<IndexMap<u8, i64>> = LazyLock::new(|| {
    IndexMap::from([
        (0x00, 0x80),
        (0x03, 0x100),
        (0x04, 0x180),
        (0x08, 0x100),
        (0x12, 0x100),
        (0x16, 0x11E),
        (0x18, 0x100),
        (0x19, 0xA0),
        (0x1A, 0x100),
        (0x1C, 0x100),
        (0x1D, 0x100),
        (0x27, 0x40),
        (0x14, 0xC0), // NES APU family; the FDS shares its output stage
        (0x0F, 0x100),
        (0x22, 0x100),
        (0x1E, 0x100), // VSU, POKEY
        (0x26, 0x100), // Seta X1-010
        (0x1F, 0x100), // Capcom QSound
        (0x21, 0x100), // Bandai WonderSwan
        (0x11, 0xE0),  // Sega 32X PWM
        (0x15, 0x40),  // YMW258-F / MultiPCM
        (0x28, 0x280), // Irem GA20
        (0x2C, 0x100), // OKI MSM5205
        (0x2D, 0x100), // OKI MSM5232
        (0x2B, 0x100), // Konami K005289 (Bubble System WSG)
    ])
});
pub static _DEV_CORE_PATCH: LazyLock<IndexMap<u8, fn(i64) -> i64>> = LazyLock::new(|| {
    IndexMap::from([
        (0x19, (|v: i64| py_floor_div(v * 8, 5)) as fn(i64) -> i64), // K051649 SCC core scale
        (0x1C, (|v: i64| py_floor_div(v * 2 + 1, 3)) as fn(i64) -> i64), // C140/C219 core scale
    ])
});
// Furnace core output levels relative to the player's, from render comparisons
// (measured on two-chip modules: the player's overall-volume normalizer scales
// a lone chip, and Furnace has no such step).
pub static _FURNACE_VOLUME_SCALE: LazyLock<IndexMap<u8, f64>> = LazyLock::new(|| {
    IndexMap::from([
        (0x00, 1.0),
        (0x03, 1.0),
        (0x04, 1.0),
        (0x12, 1.46),
        (0x16, 0.61),
        (0x18, 1.0),
        (0x1C, 0.7),
        (0x19, 1.04),
        // libvgm's K005289 core writes ``mix * 16``; Furnace's WSG platform
        // writes ``out << 6`` (4x).  A quarter of the player's level matches the
        // source, so the WSG is not 12 dB hot in the module.
        (0x2B, 0.25),
        // YMZ280B's Furnace core is 1.27x quieter than VGMPlay's at the same
        // system volume (measured on the two-chip Stage 4 module).
        (0x0F, 1.27),
        // Furnace's MultiPCM core outputs about 1/8 of the raw sample level at
        // the VGM player's 0.25 default.  Use the chip's full 2.0 mixer range instead of
        // folding that into master (which is capped at 3.0).
        (0x15, 8.0),
        // MAME shifts the voice product by 6 bits, the Furnace core by 7, so the
        // module needs twice the player's system volume to reach the same level.
        (0x1D, 2.0),
        // VBOY's core has a 6x post-amp.  Measured against VGMPlay on Galactic
        // Pinball and Jack Bros: the module is about 2.2x quieter at unity.
        (0x22, 2.2),
        // The new systems stay at the player's own default mix; do not raise the
        // song master above 1.0 for them.
        (0x1F, 1.0),
        (0x21, 1.0),
        (0x14, 1.0),
        (0x28, 1.0),
        (0x2D, 1.0),
    ])
});
// MSM6258's player default is 0x11E; its Furnace core is 1.64x louder, so
// 0.61 lands the sample at the source level. The MSM6295 (0x18) is unity.
pub static CHIP_DEV: LazyLock<IndexMap<u8, u8>> = LazyLock::new(|| {
    IndexMap::from([
        (CHIP_YM2151, 0x03),
        (CHIP_SEGAPCM, 0x04),
        (CHIP_MSM6295, 0x18),
        (CHIP_MSM6258, 0x16),
        (CHIP_C140, 0x1C),
        (CHIP_C219, 0x1C),
        (CHIP_C352, 0x27),
        (CHIP_K053260, 0x1D),
        (CHIP_YMZ280B, 0x0F),
        (CHIP_VBOY, 0x22),
        (CHIP_QSOUND, 0x1F),
        (CHIP_SWAN, 0x21),
        (CHIP_FDS, 0x14),
        (CHIP_SMS, 0x00),
        (CHIP_T6W28, 0x00),
        (CHIP_K051649, 0x19),
        (CHIP_K052539, 0x19),
        (CHIP_AY8910, 0x12),
        (CHIP_K005289, 0x2B),
        (CHIP_POKEY, 0x1E),
        (CHIP_X1_010, 0x26),
        (CHIP_GA20, 0x28),
        (CHIP_MSM5232, 0x2D),
        (CHIP_MULTIPCM, 0x15),
        (CHIP_PCM_DAC, 0x16),
        (CHIP_YM2610, 0x08),
        (CHIP_YM2610B, 0x08),
    ])
});

/// The player's mix level for one chip, the YM2151 default being 1.0.
pub fn _player_chip_volume(vgm: &VgmFile, dev: u8, instances: i64) -> f64 {
    let mut vol = _DEV_DEFAULT_VOLUME[&dev];
    if instances > 1 {
        vol = py_floor_div(vol, instances);
    }
    let entry = vgm.extra_volumes.get(&dev);
    if let Some(&(_flags, value)) = entry {
        let value = value as i64;
        if value & 0x8000 != 0 {
            vol = py_floor_div(vol * (value & 0x7FFF), 0x100);
        } else {
            vol = value;
        }
    }
    if let Some(patch) = _DEV_CORE_PATCH.get(&dev) {
        vol = patch(vol);
    }
    vol as f64 / 256.0
}

/// Output volume for a .fur chip, calibrated to the reference player.
pub fn _sample_chip_volume(song: &Song, chip: u8, instances: i64) -> f64 {
    let Some(&dev) = CHIP_DEV.get(&chip) else {
        return 1.0;
    };
    let mut instances = instances;
    if chip == CHIP_PCM_DAC {
        // Generic PCM DAC output is unity in Furnace.  VGMPlay's uPD7759
        // default is 0x11E; MSM6258 shares that id but has its own core scale,
        // so do not inherit the 0x16 entry from _FURNACE_VOLUME_SCALE.
        return 0.6 * _player_chip_volume(&song.vgm, 0x16, instances);
    }
    if chip == CHIP_C219 {
        // The C219 shares VGMPlay's C140 device volume patch, but its Furnace
        // core scales each voice by one bit more than libvgm's.  Measured on
        // the Slow Brew reference: 1.4x the player level matches the source.
        return 1.4 * _player_chip_volume(&song.vgm, 0x1C, instances);
    }
    if chip == CHIP_MULTIPCM {
        // Match the player's lone-chip MultiPCM output without halving a
        // dual-header pair: both systems run at the 2.0 mixer ceiling.
        instances = 1;
    }
    if chip == CHIP_MSM6295 && song.vgm.msm6295_dual {
        // The player halves every instance when the header carries two,
        // even on a track that only writes to one of them.
        instances = instances.max(2);
    }
    let scale = _FURNACE_VOLUME_SCALE.get(&dev).copied().unwrap_or(1.0);
    if chip == CHIP_YMZ280B {
        // VGMPlay does not halve a YMZ280B pair: both Stage 4 instances run
        // at full output. Furnace also mixes the two systems at full level,
        // so keep the per-chip value independent of the instance count.
        instances = 1;
    }
    scale * _player_chip_volume(&song.vgm, dev, instances)
}

/// Package version, stamped into every .fur comment (stale-file check).
pub fn _version() -> &'static str {
    crate::CONVERTER_VERSION
}

#[derive(Default)]
pub struct Buf {
    pub data: Vec<u8>,
}

impl Buf {
    pub fn new() -> Self {
        Buf { data: Vec::new() }
    }

    pub fn len(&self) -> usize {
        self.data.len()
    }

    pub fn is_empty(&self) -> bool {
        self.data.is_empty()
    }

    pub fn raw(&mut self, data: &[u8]) {
        self.data.extend_from_slice(data);
    }

    pub fn u8(&mut self, v: impl Into<i64>) {
        self.data.push((v.into() & 0xFF) as u8);
    }

    pub fn u16(&mut self, v: impl Into<i64>) {
        self.data
            .extend_from_slice(&((v.into() & 0xFFFF) as u16).to_le_bytes());
    }

    pub fn s16(&mut self, v: impl Into<i64>) {
        self.data.extend_from_slice(&(v.into() as i16).to_le_bytes());
    }

    pub fn u32(&mut self, v: impl Into<i64>) {
        self.data
            .extend_from_slice(&((v.into() & 0xFFFFFFFF) as u32).to_le_bytes());
    }

    pub fn s32(&mut self, v: impl Into<i64>) {
        self.data.extend_from_slice(&(v.into() as i32).to_le_bytes());
    }

    pub fn f32(&mut self, v: f64) {
        self.data.extend_from_slice(&(v as f32).to_le_bytes());
    }

    pub fn strz(&mut self, s: &str) {
        self.data.extend_from_slice(s.as_bytes());
        self.data.push(0);
    }

    /// Python default: `val=0`.
    pub fn pad(&mut self, n: impl Into<i64>, val: impl Into<i64>) {
        let val = val.into() as u8;
        for _ in 0..n.into().max(0) {
            self.data.push(val);
        }
    }

    pub fn patch_u32(&mut self, off: i64, v: impl Into<i64>) {
        let bytes = ((v.into() & 0xFFFFFFFF) as u32).to_le_bytes();
        let len = self.data.len() as i64;
        let start = (off.max(0).min(len)) as usize;
        let end = (off.max(0) + 4).min(len) as usize;
        self.data.splice(start..end, bytes);
    }

    pub fn begin_block(&mut self, magic: &[u8]) -> (i64, i64) {
        self.raw(&magic[..magic.len().min(4)]);
        let size_off = self.data.len() as i64;
        self.u32(0);
        (size_off, self.data.len() as i64)
    }

    pub fn end_block(&mut self, size_off: i64, start: i64) {
        self.patch_u32(size_off, self.data.len() as i64 - start);
    }
}

/// Wrap a raw .fur in the zlib container Furnace's loader expects.
///
/// Python default: `level=5`.
pub fn compress_fur(data: &[u8], level: i32) -> Vec<u8> {
    use flate2::Compression;
    use flate2::write::ZlibEncoder;

    let mut enc = ZlibEncoder::new(Vec::new(), Compression::new(level as u32));
    enc.write_all(data).expect("zlib write");
    enc.finish().expect("zlib finish")
}

/// Undo `compress_fur`, or return an already-raw module unchanged.
pub fn decompress_fur(data: &[u8]) -> Vec<u8> {
    if data.first() != Some(&b'x') {
        return data.to_vec();
    }
    let mut dec = flate2::read::ZlibDecoder::new(data);
    let mut out = Vec::new();
    match dec.read_to_end(&mut out) {
        Ok(_) => out,
        Err(_) => data.to_vec(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn primitive_encoders_emit_little_endian() {
        let mut b = Buf::new();
        b.u8(0x1234);
        b.u16(0x1234);
        b.s16(-2);
        b.u32(0x1234_5678);
        b.s32(-2);
        b.f32(1.5);
        b.strz("hi");
        b.pad(3, 0xAA);
        assert_eq!(
            b.data,
            vec![
                0x34, 0x34, 0x12, 0xFE, 0xFF, 0x78, 0x56, 0x34, 0x12, 0xFE, 0xFF, 0xFF, 0xFF,
                0x00, 0x00, 0xC0, 0x3F, b'h', b'i', 0x00, 0xAA, 0xAA, 0xAA,
            ]
        );
        assert_eq!(b.len(), 23);
    }

    #[test]
    fn patch_and_block_sizes() {
        let mut b = Buf::new();
        b.u16(0xDEAD);
        let (size_off, start) = b.begin_block(b"INFOxxxx");
        assert_eq!(size_off, 6);
        assert_eq!(start, 10);
        b.u8(0x11);
        b.u8(0x22);
        b.end_block(size_off, start);
        assert_eq!(b.data, vec![0xAD, 0xDE, b'I', b'N', b'F', b'O', 2, 0, 0, 0, 0x11, 0x22]);
        // An out-of-range patch replaces only the existing tail, like Python.
        b.patch_u32(10, 0x0102_0304);
        assert_eq!(&b.data[10..], &[0x04, 0x03, 0x02, 0x01]);
    }

    #[test]
    fn pack_dt_mul_maps_opm_and_opn_detune() {
        // DT=2, MUL=5: OPM table 5, OPN table 5 -> both 0x55.
        assert_eq!(pack_dt_mul(0x25, false), 0x55);
        assert_eq!(pack_dt_mul(0x25, true), 0x55);
        // DT=4 is "negative zero" on OPN: OPM 7, OPN 3.
        assert_eq!(pack_dt_mul(0x40, false), 0x70);
        assert_eq!(pack_dt_mul(0x40, true), 0x30);
        // DT=0 maps to 3 on both, MUL low nibble preserved.
        assert_eq!(pack_dt_mul(0x0F, false), 0x3F);
        assert_eq!(pack_dt_mul(0x0F, true), 0x3F);
        assert_eq!(ym_dt_to_furnace(7), 0);
        assert_eq!(opn_dt_to_furnace(7), 0);
        assert_eq!(ym_dt_to_furnace(0), 3);
        assert_eq!(opn_dt_to_furnace(0), 3);
    }

    #[test]
    fn channel_names_match_sysdef() {
        let (long, short) = opl_channel_names("opl3", false);
        assert_eq!(long.len(), 18);
        assert_eq!(long[0], "4OP 1");
        assert_eq!(long[2], "4OP 3");
        assert_eq!(long[11], "FM 12");
        assert_eq!(long[12], "FM 13");
        assert_eq!(short[0], "1");
        let (long, short) = opl_channel_names("opl2", true);
        assert_eq!(long.len(), 11);
        assert_eq!(long[6], "Kick/FM 7");
        assert_eq!(short[6], "BD");
        assert_eq!(long[10], "HiHat");
        assert_eq!(short[10], "HH");
        let (long, short) = opll_channel_names(true, false);
        assert_eq!(long.len(), 11);
        assert_eq!(long[0], "FM 1");
        assert_eq!(short[0], "F1");
        assert_eq!(long[11 - 1], "HiHat");
        let (long, _) = opll_channel_names(false, true);
        assert_eq!(long, vec!["FM 1", "FM 2", "FM 3", "FM 4", "FM 5", "FM 6"]);
    }

    #[test]
    fn compress_decompress_round_trip_and_raw_passthrough() {
        let raw = b"-Furnace module-\x9d\x00\x00\x00".repeat(20);
        let packed = compress_fur(&raw, 5);
        assert_eq!(&packed[..1], b"x");
        assert_ne!(packed, raw);
        assert_eq!(decompress_fur(&packed), raw);
        // Already-raw modules (and corrupt zlib) come back unchanged.
        assert_eq!(decompress_fur(&raw), raw);
        assert_eq!(decompress_fur(b"xnot-zlib"), b"xnot-zlib".to_vec());
        assert_eq!(compress_fur(b"", 5)[0], b'x');
        assert!(decompress_fur(&compress_fur(b"", 5)).is_empty());
    }
}
