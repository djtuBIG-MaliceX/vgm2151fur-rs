//! The analyzer: one `VgmFile` in, one `Song` out.
//!
//! Literal port of `analyze()` (`vgm2151fur/analyze.py`, v0.11.0).  The Python
//! function is a single 7,000-line body with nested closures; here it is an
//! `AnalyzeState` value whose fields are the Python locals that cross a
//! section, a method per nested function, and `run()` reproducing the
//! statement order.

use std::collections::{BTreeMap, BTreeSet, HashSet};

use indexmap::IndexMap;

use crate::fm::{carriers_for, opn_tl_op, OpRegs, FMPatch, OPN_ORDERED_OPS, OPM_OP_TO_MASK};
use crate::model::{
    dual_ids, kc_to_furnace_note, kf_to_e5, pcm_pan_fx, transpose_opm_events, vol15,
    FmSystem, NoteEvent, Song, ToneIns,
};
use crate::parity::py_round;
use crate::vgm::{ChipWrite, VgmFile, VGM_RATE};

/// Furnace's YM2151 chip-wide effects.
pub const FX_LFO_SPEED: i32 = 0x17;
pub const FX_LFO_WAVE: i32 = 0x18;
pub const FX_LFO_AMD: i32 = 0x1E;
pub const FX_LFO_PMD: i32 = 0x1F;

const _OPM_BURST_LOAD_SAMPLES: i64 = 16;
const _ONSET_MERGE_SAMPLES: i64 = 100;
const OPN_BASE_INS_CAP: usize = 200;
const OPN_TL_FOLD: i64 = 512;
const OPN_PATCH_SETTLE: i64 = 512;
/// `analyze.py:3614`. The tone binder also exports it from `tone.rs`; the
/// two must agree or the merge tolerance flips a song too early.
const INS_TARGET: i32 = 250;
const INS_HARD_LIMIT: i32 = 256;
const YM2610_MAP_SLOTS: i32 = 120;
const NES_PCM_MAP_SLOTS: i32 = 120;
const ADPCM_MAP_SLOTS: i32 = 120;
const MSM6258_MAP_NOTE: i32 = 60;
const FURNACE_C4: i32 = 108;
const RF5C_VOICES: i32 = 8;
const YMZ280B_VOICES: i32 = 8;
const MULTIPCM_VOICES: i32 = 28;
const PWM_SIDES: i32 = 2;
const SCC_VOICES: i32 = 5;
const AY_VOICES: i32 = 3;
const NES_CHANNELS: i32 = 5;
const HUC6280_CHANNELS: i32 = 6;
const VSU_CHANNELS: i32 = 6;
const SAA_VOICES: i32 = 6;
const POKEY_VOICES: i32 = 4;
const MIKEY_VOICES: i32 = 4;
const X1_CHANNELS: i32 = 16;
const WS_VOICES: i32 = 4;
const QSOUND_CHANNELS: i32 = 19;
const K005289_VOICES: i32 = 2;
const OPL4_WAVE_SLOTS: i32 = 24;
const OPLL_CHANNELS: i32 = 9;
const OPLL_DRUMS_CHANNELS: i32 = 11;
const OPL4_TAIL_MUTE: i64 = 600;
const C352_REFIT_SAMPLES: i64 = 64;
const K053260_REFIT_SAMPLES: i64 = 64;
const K005289_REFIT_SAMPLES: i64 = 64;
const HUC6280_ATTACK_WINDOW: i64 = 64;
const HUC6280_REFIT_SAMPLES: i64 = 64;
const YM2203_DEFAULT_CLOCK: u32 = 3_579_545;
const YM2612_DEFAULT_CLOCK: u32 = 7_670_453;
const YM2610_DEFAULT_CLOCK: u32 = 8_000_000;
const YM2608_DEFAULT_CLOCK: u32 = 8_000_000;
const PSG_DEFAULT_CLOCK: u32 = 3_579_545;
const SCC_DEFAULT_CLOCK: u32 = 3_579_545;
const AY_DEFAULT_CLOCK: u32 = 1_789_772;
const NES_DEFAULT_CLOCK: u32 = 1_789_772;
const GB_DEFAULT_CLOCK: u32 = 4_194_304;
const VSU_DEFAULT_CLOCK: u32 = 5_000_000;
const SAA_DEFAULT_CLOCK: u32 = 8_000_000;
const POKEY_DEFAULT_CLOCK: u32 = 1_789_772;
const MIKEY_DEFAULT_CLOCK: u32 = 16_000_000;
const X1_010_DEFAULT_CLOCK: u32 = 16_000_000;
const FDS_DEFAULT_CLOCK: u32 = 1_789_772;
const WSWAN_DEFAULT_CLOCK: u32 = 3_072_000;
const K005289_DEFAULT_CLOCK: u32 = 3_579_545;
const C140_DEFAULT_CLOCK: u32 = 12_288_000;
const C352_DEFAULT_CLOCK: u32 = 36_864_000;
const C219_DEFAULT_CLOCK: u32 = 12_288_000;
const K054539_DEFAULT_CLOCK: u32 = 18_432_000;
const GA20_DEFAULT_CLOCK: u32 = 16_000_000;
const MSM5232_DEFAULT_CLOCK: u32 = 2_000_000;
const RF5C68_DEFAULT_CLOCK: u32 = 12_500_000;
const RF5C164_DEFAULT_CLOCK: u32 = 12_500_000;
const YM2203_SSG_DIVIDER: i32 = 16;
const YM2610_SSG_DIVIDER: i32 = 16;
const YM2608_SSG_DIVIDER: i32 = 32;
const GB_PITCH_DIVIDER: i32 = 16384;
const VSU_PITCH_DIVIDER: i32 = 32;
const NES_CPU_DIVIDER: i32 = 16;
// fds.py:17-18: the divider is the register range, and the table tops out
// at the 16-bit register value.
const FDS_FREQ_DIV: i32 = 262_144;
const FDS_MAX_FREQ: i32 = 0xFFFF;
const QSOUND_OUT_RATE: u32 = 44_100;
const QSOUND_FREQ_DIV: i32 = 1;
const QSOUND_MAX_FREQ: i32 = 0xFFFF;
const WS_PITCH_DIVIDER: i32 = 32;
const WS_MAX_PERIOD: i32 = 0x7FF;
const K005289_DIVIDER: i32 = 1;
const SCC_MIN_PERIOD: i32 = 9;
const _AY_MERGE_GAP: i64 = 16;
const _AY_OPEN_WINDOW: i64 = 64;
const EXT_FREQ_OP: [i32; 4] = [1, 2, 0, 3];
const OPN_FREQ_BASE: f64 = 4_720_270.0;
const YM2610_FREQ_BASE: f64 = 9_440_540.0;
const KIND_OPL: &str = "opl";
const KIND_OPL2: &str = "opl2";
const KIND_OPL3: &str = "opl3";
const KIND_OPL4: &str = "opl4";
const KIND_Y8950: &str = "y8950";

/// Register image of one YM2151.
#[derive(Clone, Debug, PartialEq)]
pub struct YM2151State {
    pub rl_fb_con: [i32; 8],
    pub kc: [i32; 8],
    pub kf: [i32; 8],
    pub pms_ams: [i32; 8],
    pub ops: [[OpRegs; 4]; 8],
    pub kon: [bool; 8],
}

impl Default for YM2151State {
    fn default() -> Self {
        Self::new()
    }
}

impl YM2151State {
    pub fn new() -> Self {
        YM2151State {
            rl_fb_con: [0xC0; 8],
            kc: [0; 8],
            kf: [0; 8],
            pms_ams: [0; 8],
            ops: core::array::from_fn(|_| core::array::from_fn(|_| OpRegs::default())),
            kon: [false; 8],
        }
    }

    pub fn write(&mut self, reg: u32, val: u32) {
        let reg = reg as i32;
        let val = val as i32;
        if (0x20..=0x27).contains(&reg) {
            self.rl_fb_con[(reg - 0x20) as usize] = val;
        } else if (0x28..=0x2F).contains(&reg) {
            self.kc[(reg - 0x28) as usize] = val;
        } else if (0x30..=0x37).contains(&reg) {
            self.kf[(reg - 0x30) as usize] = val;
        } else if (0x38..=0x3F).contains(&reg) {
            self.pms_ams[(reg - 0x38) as usize] = val;
        } else if reg >= 0x40 {
            let ch = (reg & 7) as usize;
            let op = ((reg >> 3) & 3) as usize;
            let slot = &mut self.ops[ch][op];
            match reg & 0xE0 {
                0x40 => slot.dt_mul = val as u8,
                0x60 => slot.tl = val as u8,
                0x80 => slot.ks_ar = val as u8,
                0xA0 => slot.am_d1r = val as u8,
                0xC0 => slot.dt2_d2r = val as u8,
                0xE0 => slot.d1l_rr = val as u8,
                _ => {}
            }
        }
    }

    pub fn snapshot(&self, ch: usize) -> FMPatch {
        let con = self.rl_fb_con[ch];
        let pa = self.pms_ams[ch];
        let ops: [OpRegs; 4] = core::array::from_fn(|i| OpRegs {
            dt_mul: self.ops[ch][i].dt_mul,
            tl: self.ops[ch][i].tl,
            ks_ar: self.ops[ch][i].ks_ar,
            am_d1r: self.ops[ch][i].am_d1r,
            dt2_d2r: self.ops[ch][i].dt2_d2r,
            d1l_rr: self.ops[ch][i].d1l_rr,
            ..OpRegs::default()
        });
        FMPatch {
            alg: con & 7,
            fb: (con >> 3) & 7,
            ams: pa & 3,
            pms: (pa >> 4) & 7,
            ops,
            ..FMPatch::default()
        }
    }

    pub fn pan_byte(&self, ch: usize) -> i32 {
        let rl = (self.rl_fb_con[ch] >> 6) & 3;
        match rl {
            3 => 0xFF,
            2 => 0x0F,
            1 => 0xF0,
            _ => 0x00,
        }
    }

    pub fn carrier_tl(&self, ch: usize) -> i32 {
        let alg = (self.rl_fb_con[ch] & 7) as usize;
        let table = carriers_for(alg as i32, false, false, false);
        let tls: Vec<i32> = table
            .iter()
            .map(|i| (self.ops[ch][*i as usize].tl & 0x7F) as i32)
            .collect();
        tls.iter().copied().min().unwrap_or(0x7F)
    }
}

/// Identity key of an interned FM patch.  Python concatenates the mask onto
/// the identity tuple; the Ext path uses a tagged three-tuple.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum PatchKey {
    Plain(Vec<i64>),
    Ext { cid: i32, key0: Vec<i64> },
}

/// One chip-wide effect with the Python tuple's own arity, because the
/// duplicate check compares tuples of different lengths.
#[derive(Clone, Debug, PartialEq)]
pub enum ChipFx {
    Plain(i64, i32, i32),
    Tick(i64, i32, i32, i32),
    Chan(i64, i32, i32, i32, i32),
}

impl ChipFx {
    fn as_song(&self) -> (i64, i32, i32, i32, i32) {
        match *self {
            ChipFx::Plain(s, c, v) => (s, c, v, 0, 0),
            ChipFx::Tick(s, c, v, t) => (s, c, v, t, 0),
            ChipFx::Chan(s, c, v, t, ch) => (s, c, v, t, ch),
        }
    }
}

pub struct AnalyzeOptions<'a> {
    pub speed: Option<i32>,
    pub pcm: bool,
    pub tick_hz: Option<f64>,
    pub tick_samples: Option<f64>,
    pub min_row: Option<f64>,
    pub condense: f64,
    pub grid_cache: Option<&'a mut crate::timing::GridCache>,
    pub c352_quad: bool,
    pub dac_slices: bool,
    pub lfo_loop: bool,
    pub lossy_samples: bool,
}

impl Default for AnalyzeOptions<'_> {
    fn default() -> Self {
        AnalyzeOptions {
            speed: None,
            pcm: true,
            tick_hz: None,
            tick_samples: None,
            min_row: None,
            condense: 1.0,
            grid_cache: None,
            c352_quad: false,
            dac_slices: false,
            lfo_loop: true,
            lossy_samples: false,
        }
    }
}

/// Python locals of `analyze()` that outlive one section.
pub struct AnalyzeState<'a> {
    pub vgm: &'a VgmFile,
    pub opts: AnalyzeOptions<'a>,
    pub tick: f64,
    pub row_floor: f64,
    pub patches: IndexMap<PatchKey, FMPatch>,
    pub patch_order: Vec<PatchKey>,
    pub events: Vec<NoteEvent>,
    pub keyon_times: Vec<i64>,
    pub warnings: Vec<String>,
    pub chip_fx: Vec<ChipFx>,
    pub channel_fx: Vec<(i64, i32, i32, i32)>,
    pub held_lfo: IndexMap<i32, Vec<(Vec<i32>, Vec<i32>, Vec<i32>)>>,
    // --- family discovery ---
    pub chips: BTreeSet<&'static str>,
    pub has_ym2203: bool,
    pub has_ym2151: bool,
    pub has_ym2612: bool,
    pub has_ym2610: bool,
    pub has_ym2608: bool,
    pub has_sn76489: bool,
    pub has_opl: bool,
    pub has_y8950: bool,
    pub has_opl4: bool,
    pub has_opll: bool,
    pub has_nes: bool,
    pub dac_side: bool,
    pub sounds: BTreeMap<&'static str, bool>,
    pub arcade_present: BTreeSet<&'static str>,
    pub arcade_live: BTreeSet<&'static str>,
    pub opl_side: BTreeSet<&'static str>,
    pub opl_live: BTreeSet<&'static str>,
    pub opll_live: bool,
    pub prefer_opl: bool,
    pub opl_keys: i64,
    pub arcade_keys: i64,
    pub is_combo: bool,
    pub arcade_pick: Option<&'static str>,
    pub share_fm: bool,
    pub keep_arcade: bool,
    pub is_opna: bool,
    pub is_opnb: bool,
    pub is_opn2: bool,
    pub is_opn: bool,
    pub is_opl: bool,
    pub opl_kind: Option<&'static str>,
    pub is_opll: bool,
    pub opn2_dac_only: bool,
    pub opn2_dac: bool,
    pub converted: BTreeSet<&'static str>,
    pub selected_fm_chips: Option<BTreeSet<&'static str>>,
    pub opl_source: Option<&'static str>,
    pub opl_dual: bool,
    pub opl_ids: Vec<i32>,
    pub opl_drums: bool,
    pub opl_rhythm: IndexMap<String, i64>,
    pub opl_is_opl3: bool,
    pub opl_clock: f64,
    pub opll_ids: Vec<i32>,
    pub opll_vrc7: bool,
    pub opll_drums: bool,
    pub opll_rhythm: IndexMap<String, i64>,
    pub opll_clock: f64,
    pub ym_ids: Vec<i32>,
    pub ym_clock: u32,
    pub ym2203_ext: bool,
    pub ym2612_ids: Vec<i32>,
    pub ym2612_clock: u32,
    pub ym2612_modes: BTreeSet<u32>,
    pub ym2612_ext: bool,
    pub ym2612_csm: bool,
    pub ym2610_ids: Vec<i32>,
    pub ym2610_clock: u32,
    pub ym2610_b: bool,
    pub ym2610_fm_count: i32,
    pub ym2610_modes: BTreeSet<u32>,
    pub ym2608_ids: Vec<i32>,
    pub ym2608_clock: u32,
    pub ym2608_modes: BTreeSet<u32>,
    pub ym2608_ext: bool,
    pub ym2608_fm_count: i32,
    pub convert_opm: bool,
    pub legato_steps: i32,
    pub legato_ramps: i32,
    pub opm_ids: Vec<i32>,
    pub opm_base: IndexMap<i32, i32>,
    pub opm_end: i32,
    pub ym_chunk: i32,
    pub ym_base: IndexMap<i32, i32>,
    pub ym_count: usize,
    pub ym2203_ssg_count: i32,
    pub ym2610_ssg_count: i32,
    pub ym2608_ssg_count: i32,
    pub opl_state_kind: Option<&'static str>,
    pub opl_fm_count: i32,
    pub opl_chunk: i32,
    pub ym_end: i32,
    pub opl_base: IndexMap<i32, i32>,
    pub opl_end: i32,
    pub opll_chunk: i32,
    pub opll_start: i32,
    pub opll_base: IndexMap<i32, i32>,
    pub opll_end: i32,
    pub fm_end: i32,
    pub fm_systems: Vec<FmSystem>,
    pub psg_ids: Vec<i32>,
    pub psg_t6w28: bool,
    pub psg_stereo: bool,
    pub psg_base: Option<i32>,
    pub psg_clock: u32,
    pub psg_at: IndexMap<i32, i32>,
    // --- OPM walk ---
    pub op_mask: [i32; 8],
    pub opn_dac_writes: i32,
    // --- results ---
    pub t0: i64,
    pub grid_note: String,
    pub row_subdiv: f64,
    pub tone_base: i32,
    pub hz: f64,
    pub use_speed: i32,
    pub hilight: (i32, i32),

    // --- collector results (Python locals that cross sections) ---
    pub pcm_samples: Vec<crate::pcm::PcmSample>,
    pub pcm_ids: Vec<i32>,
    pub audible: Vec<usize>,
    pub oki_samples: Vec<crate::oki::OkiSample>,
    pub adpcm_samples: Vec<crate::oki::AdpcmSample>,
    pub spcm_audible: Vec<crate::segapcm::SegapcmSample>,
    pub spcm_ids: Vec<i32>,
    pub rf5c68_audible: Vec<crate::rf5c::Rf5cSample>,
    pub rf5c164_audible: Vec<crate::rf5c::Rf5cSample>,
    pub rf5c68_ids: Vec<i32>,
    pub rf5c164_ids: Vec<i32>,
    pub c140_audible: Vec<crate::c140::C140Sample>,
    pub c219_samples: Vec<crate::c219::C219Sample>,
    pub c219_ids: Vec<i32>,
    pub c219_events: Vec<(usize, i32)>,
    pub c219_ins_count: i32,
    pub c219_count: i32,
    pub c352_audible: Vec<crate::c352::C352Sample>,
    pub c352_quad_events: bool,
    pub k053260_audible: Vec<crate::k053260::K053260Sample>,
    pub k054539_samples: Vec<crate::k054539::K054539Sample>,
    pub k054539_ids: Vec<i32>,
    pub k054539_events: Vec<(usize, i32)>,
    pub k054539_count: i32,
    pub ymz_audible: Vec<crate::ymz280b::YmzSample>,
    pub ymz_ids: Vec<i32>,
    pub ymz_events: Vec<(usize, i32)>,
    pub ymz_count: i32,
    pub multipcm_audible: Vec<crate::multipcm::MultipcmSample>,
    pub multipcm_ids: Vec<i32>,
    pub multipcm_events: Vec<(usize, i32)>,
    pub multipcm_count: i32,
    pub upd_audible: Vec<crate::upd7759::Upd7759Sample>,
    pub upd_ids: Vec<i32>,
    pub upd_events: Vec<(usize, i32)>,
    pub upd_count: i32,
    pub pwm_audible: Vec<crate::pwm::PwmSample>,
    pub pwm_ids: Vec<i32>,
    pub pwm_events: Vec<(usize, i32)>,
    pub pwm_count: i32,
    pub msm5205_audible: Vec<crate::msm5205::Msm5205Sample>,
    pub msm5205_ids: Vec<i32>,
    pub msm5205_events: Vec<(usize, i32)>,
    pub msm5205_count: i32,
    pub ga20_samples: Vec<crate::ga20::Ga20Sample>,
    pub ga20_ids: Vec<i32>,
    pub ga20_events: Vec<(usize, i32)>,
    pub ga20_count: i32,
    pub msm5232_ids: Vec<i32>,
    pub msm5232_controls: Vec<i32>,
    pub msm5232_events: Vec<(usize, i32)>,
    pub msm5232_count: i32,
    pub channel_vol_events: Vec<(i64, i32, i32)>,
    pub es5503_samples: Vec<crate::es5503::Es5503Sample>,
    pub es5503_ids: Vec<i32>,
    pub es5503_hits: Vec<(usize, usize)>,
    pub es5503_offs: Vec<crate::es5503::Es5503Off>,
    pub es5503_events: Vec<(usize, i32)>,
    pub es5503_oscs: Vec<(i32, i32)>,
    pub es5503_rate: i32,
    pub es5503_count: i32,
    pub dac_samples: Vec<crate::ym2612dac::DacSample>,
    pub dac_plays: Vec<crate::ym2612dac::DacPlay>,
    pub dac_count: i32,
    pub dac_stats: crate::ym2612dac::DacStats,
    pub dac_pan_events: Vec<(i64, i32)>,
    pub n_dac_ins: i32,
    pub psg_dac: Option<crate::psg_dac::PsgDacCollect>,
    pub psg_dac_events: Vec<(usize, i32)>,
    pub psg_dac_count: i32,
    pub psg_count: i32,
    pub opl4_wave_samples: Vec<crate::ymf278b::Opl4WaveSample>,
    pub opl4_wave_events: Vec<(usize, i32)>,
    pub opl4_wave_count: i32,
    pub ym2610_a_samples: Vec<crate::ym2610::AdpcmASample>,
    pub ym2610_b_samples: Vec<crate::ym2610::AdpcmBSample>,
    pub ym2610_a_count: i32,
    pub ym2610_b_count: i32,
    pub ym2610_events: Vec<(usize, i32, i32)>,
    pub ym2608_b_samples: Vec<crate::ym2610::AdpcmBSample>,
    pub ym2608_rhythm_events: Vec<usize>,
    pub ym2608_b_events: Vec<(usize, i32)>,
    pub ym2608_rhythm_count: i32,
    pub ym2608_b_count: i32,
    pub y8950_b_samples: Vec<crate::ym2610::AdpcmBSample>,
    pub y8950_b_events: Vec<(usize, i32)>,
    pub y8950_b_count: i32,
    pub ssg_pcm_samples: Vec<crate::ssgpcm::SsgPcmSample>,
    pub ssg_pcm_events: Vec<(usize, i32)>,
    pub ssg_pcm_count: i32,
    pub scc_ids: Vec<i32>,
    pub scc_waves: Vec<Vec<u8>>,
    pub scc_ins: Vec<ToneIns>,
    pub scc_count: i32,
    pub ay_ids: Vec<i32>,
    pub ay_ins: Vec<ToneIns>,
    pub ay_count: i32,
    pub psg_ins: Vec<ToneIns>,
    pub nes_ins: Vec<ToneIns>,
    pub nes_ids: Vec<i32>,
    pub nes_present: bool,
    pub nes_tones: Vec<crate::nes::NesHit>,
    pub nes_offs: Vec<crate::nes::NesOff>,
    pub nes_dpcm_samples: Vec<crate::nes::NesDpcmSample>,
    pub nes_dpcm_hits: Vec<crate::nes::NesDpcmHit>,
    pub nes_pcm_samples: Vec<crate::nes::NesPcmSample>,
    pub nes_pcm_hits: Vec<crate::nes::NesPcmHit>,
    pub nes_dpcm_events: Vec<(usize, i32)>,
    pub nes_pcm_events: Vec<(usize, i32)>,
    pub huc6280_ids: Vec<i32>,
    pub huc6280_waves: Vec<Vec<u8>>,
    pub huc6280_ins: Vec<ToneIns>,
    pub huc_pcm_samples: Vec<crate::huc6280::Huc6280PcmSample>,
    pub huc_pcm_hits: Vec<crate::huc6280::Huc6280PcmHit>,
    pub huc6280_pcm_events: Vec<(usize, i32)>,
    pub huc6280_count: i32,
    pub huc6280_pcm_count: i32,
    pub gb_ids: Vec<i32>,
    pub gb_waves: Vec<Vec<u8>>,
    pub gb_ins: Vec<ToneIns>,
    pub gb_count: i32,
    pub vsu_ids: Vec<i32>,
    pub vsu_waves: Vec<Vec<u8>>,
    pub vsu_mod_waves: Vec<Vec<u8>>,
    pub vsu_ins: Vec<ToneIns>,
    pub vsu_count: i32,
    pub vsu_dynamic: bool,
    pub saa_ids: Vec<i32>,
    pub saa_ins: Vec<ToneIns>,
    pub saa_count: i32,
    pub pokey_ids: Vec<i32>,
    pub pokey_ins: Vec<ToneIns>,
    pub pokey_count: i32,
    pub mikey_ids: Vec<i32>,
    pub mikey_ins: Vec<ToneIns>,
    pub mikey_samples: Vec<crate::mikey::MikeySample>,
    pub mikey_sample_events: Vec<(usize, i32)>,
    pub mikey_count: i32,
    pub x1_ids: Vec<i32>,
    pub x1_ins: Vec<ToneIns>,
    pub x1_waves: Vec<Vec<u8>>,
    pub x1_envs: Vec<Vec<u8>>,
    pub x1_samples: Vec<crate::x1_010::X1PcmHit>,
    pub x1_sample_events: Vec<(usize, i32)>,
    pub x1_count: i32,
    pub fds_ids: Vec<i32>,
    pub fds_ins: Vec<ToneIns>,
    pub fds_waves: Vec<Vec<u8>>,
    pub fds_mod_waves: Vec<Vec<u8>>,
    pub fds_count: i32,
    pub wonderswan_ids: Vec<i32>,
    pub wonderswan_ins: Vec<ToneIns>,
    pub wonderswan_waves: Vec<Vec<u8>>,
    pub wonderswan_pcm_samples: Vec<crate::wonderswan::WsPcmSample>,
    pub wonderswan_pcm_events: Vec<(usize, i32)>,
    pub wonderswan_count: i32,
    pub wonderswan_pcm_count: i32,
    pub qsound_ids: Vec<i32>,
    pub qsound_ins: Vec<ToneIns>,
    pub qsound_samples: Vec<crate::qsound::QSoundSample>,
    pub qsound_count: i32,
    pub qsound_adpcm_count: i32,
    pub k005289_ids: Vec<i32>,
    pub k005289_waves: Vec<Vec<u8>>,
    pub k005289_ins: Vec<ToneIns>,
    pub k005289_count: i32,
}

pub fn analyze(vgm: &VgmFile, opts: AnalyzeOptions<'_>) -> crate::Result<Song> {
    let tick = opts.tick_samples.map_or(crate::timing::TARGET_TICK_SAMPLES, |v| v);
    let row_floor = opts.min_row.map_or(crate::timing::MIN_ROW_SAMPLES, |v| v);
    if opts.tick_hz.is_some() && !matches!(opts.speed, Some(s) if s >= 1) {
        return Err(crate::Error::Msg(
            "a tick rate needs a manual ticks-per-row".into(),
        ));
    }
    let mut st = AnalyzeState::new(vgm, opts, tick, row_floor);
    st.run()?;
    st.finish()
}

impl<'a> AnalyzeState<'a> {
    fn new(vgm: &'a VgmFile, opts: AnalyzeOptions<'a>, tick: f64, row_floor: f64) -> Self {
        AnalyzeState {
            vgm,
            opts,
            tick,
            row_floor,
            patches: IndexMap::new(),
            patch_order: Vec::new(),
            events: Vec::new(),
            keyon_times: Vec::new(),
            warnings: vgm.warnings.clone(),
            chip_fx: Vec::new(),
            channel_fx: Vec::new(),
            held_lfo: IndexMap::new(),
            chips: BTreeSet::new(),
            has_ym2203: false,
            has_ym2151: false,
            has_ym2612: false,
            has_ym2610: false,
            has_ym2608: false,
            has_sn76489: false,
            has_opl: false,
            has_y8950: false,
            has_opl4: false,
            has_opll: false,
            has_nes: false,
            dac_side: false,
            sounds: BTreeMap::new(),
            arcade_present: BTreeSet::new(),
            arcade_live: BTreeSet::new(),
            opl_side: BTreeSet::new(),
            opl_live: BTreeSet::new(),
            opll_live: false,
            prefer_opl: false,
            opl_keys: 0,
            arcade_keys: 0,
            is_combo: false,
            arcade_pick: None,
            share_fm: false,
            keep_arcade: false,
            is_opna: false,
            is_opnb: false,
            is_opn2: false,
            is_opn: false,
            is_opl: false,
            opl_kind: None,
            is_opll: false,
            opn2_dac_only: false,
            opn2_dac: false,
            converted: BTreeSet::new(),
            selected_fm_chips: None,
            opl_source: None,
            opl_dual: false,
            opl_ids: Vec::new(),
            opl_drums: false,
            opl_rhythm: IndexMap::new(),
            opl_is_opl3: false,
            opl_clock: 0.0,
            opll_ids: Vec::new(),
            opll_vrc7: false,
            opll_drums: false,
            opll_rhythm: IndexMap::new(),
            opll_clock: 0.0,
            ym_ids: Vec::new(),
            ym_clock: 0,
            ym2203_ext: false,
            ym2612_ids: Vec::new(),
            ym2612_clock: 0,
            ym2612_modes: BTreeSet::new(),
            ym2612_ext: false,
            ym2612_csm: false,
            ym2610_ids: Vec::new(),
            ym2610_clock: 0,
            ym2610_b: false,
            ym2610_fm_count: 4,
            ym2610_modes: BTreeSet::new(),
            ym2608_ids: Vec::new(),
            ym2608_clock: 0,
            ym2608_modes: BTreeSet::new(),
            ym2608_ext: false,
            ym2608_fm_count: 6,
            convert_opm: false,
            legato_steps: 0,
            legato_ramps: 0,
            opm_ids: Vec::new(),
            opm_base: IndexMap::new(),
            opm_end: 0,
            ym_chunk: 6,
            ym_base: IndexMap::new(),
            ym_count: 0,
            ym2203_ssg_count: 0,
            ym2610_ssg_count: 0,
            ym2608_ssg_count: 0,
            opl_state_kind: None,
            opl_fm_count: 0,
            opl_chunk: 0,
            ym_end: 0,
            opl_base: IndexMap::new(),
            opl_end: 0,
            opll_chunk: 0,
            opll_start: 0,
            opll_base: IndexMap::new(),
            opll_end: 0,
            fm_end: 0,
            fm_systems: Vec::new(),
            psg_ids: Vec::new(),
            psg_t6w28: false,
            psg_stereo: false,
            psg_base: None,
            psg_clock: 0,
            psg_at: IndexMap::new(),
            op_mask: [0; 8],
            opn_dac_writes: 0,
            t0: 0,
            grid_note: String::new(),
            row_subdiv: 1.0,
            tone_base: 0,
            hz: 0.0,
            use_speed: 0,
            hilight: (4, 16),
            pcm_samples: Vec::new(),
            pcm_ids: Vec::new(),
            audible: Vec::new(),
            oki_samples: Vec::new(),
            adpcm_samples: Vec::new(),
            spcm_audible: Vec::new(),
            spcm_ids: Vec::new(),
            rf5c68_audible: Vec::new(),
            rf5c164_audible: Vec::new(),
            rf5c68_ids: Vec::new(),
            rf5c164_ids: Vec::new(),
            c140_audible: Vec::new(),
            c219_samples: Vec::new(),
            c219_ids: Vec::new(),
            c219_events: Vec::new(),
            c219_ins_count: 0,
            c219_count: 0,
            c352_audible: Vec::new(),
            c352_quad_events: false,
            k053260_audible: Vec::new(),
            k054539_samples: Vec::new(),
            k054539_ids: Vec::new(),
            k054539_events: Vec::new(),
            k054539_count: 0,
            ymz_audible: Vec::new(),
            ymz_ids: Vec::new(),
            ymz_events: Vec::new(),
            ymz_count: 0,
            multipcm_audible: Vec::new(),
            multipcm_ids: Vec::new(),
            multipcm_events: Vec::new(),
            multipcm_count: 0,
            upd_audible: Vec::new(),
            upd_ids: Vec::new(),
            upd_events: Vec::new(),
            upd_count: 0,
            pwm_audible: Vec::new(),
            pwm_ids: Vec::new(),
            pwm_events: Vec::new(),
            pwm_count: 0,
            msm5205_audible: Vec::new(),
            msm5205_ids: Vec::new(),
            msm5205_events: Vec::new(),
            msm5205_count: 0,
            ga20_samples: Vec::new(),
            ga20_ids: Vec::new(),
            ga20_events: Vec::new(),
            ga20_count: 0,
            msm5232_ids: Vec::new(),
            msm5232_controls: Vec::new(),
            msm5232_events: Vec::new(),
            msm5232_count: 0,
            channel_vol_events: Vec::new(),
            es5503_samples: Vec::new(),
            es5503_ids: Vec::new(),
            es5503_hits: Vec::new(),
            es5503_offs: Vec::new(),
            es5503_events: Vec::new(),
            es5503_oscs: Vec::new(),
            es5503_rate: 0,
            es5503_count: 0,
            dac_samples: Vec::new(),
            dac_plays: Vec::new(),
            dac_count: 0,
            dac_stats: crate::ym2612dac::DacStats::new(),
            dac_pan_events: Vec::new(),
            n_dac_ins: 0,
            psg_dac: None,
            psg_dac_events: Vec::new(),
            psg_dac_count: 0,
            psg_count: 0,
            opl4_wave_samples: Vec::new(),
            opl4_wave_events: Vec::new(),
            opl4_wave_count: 0,
            ym2610_a_samples: Vec::new(),
            ym2610_b_samples: Vec::new(),
            ym2610_a_count: 0,
            ym2610_b_count: 0,
            ym2610_events: Vec::new(),
            ym2608_b_samples: Vec::new(),
            ym2608_rhythm_events: Vec::new(),
            ym2608_b_events: Vec::new(),
            ym2608_rhythm_count: 0,
            ym2608_b_count: 0,
            y8950_b_samples: Vec::new(),
            y8950_b_events: Vec::new(),
            y8950_b_count: 0,
            ssg_pcm_samples: Vec::new(),
            ssg_pcm_events: Vec::new(),
            ssg_pcm_count: 0,
            scc_ids: Vec::new(),
            scc_waves: Vec::new(),
            scc_ins: Vec::new(),
            scc_count: 0,
            ay_ids: Vec::new(),
            ay_ins: Vec::new(),
            ay_count: 0,
            psg_ins: Vec::new(),
            nes_ins: Vec::new(),
            nes_ids: Vec::new(),
            nes_present: false,
            nes_tones: Vec::new(),
            nes_offs: Vec::new(),
            nes_dpcm_samples: Vec::new(),
            nes_dpcm_hits: Vec::new(),
            nes_pcm_samples: Vec::new(),
            nes_pcm_hits: Vec::new(),
            nes_dpcm_events: Vec::new(),
            nes_pcm_events: Vec::new(),
            huc6280_ids: Vec::new(),
            huc6280_waves: Vec::new(),
            huc6280_ins: Vec::new(),
            huc_pcm_samples: Vec::new(),
            huc_pcm_hits: Vec::new(),
            huc6280_pcm_events: Vec::new(),
            huc6280_count: 0,
            huc6280_pcm_count: 0,
            gb_ids: Vec::new(),
            gb_waves: Vec::new(),
            gb_ins: Vec::new(),
            gb_count: 0,
            vsu_ids: Vec::new(),
            vsu_waves: Vec::new(),
            vsu_mod_waves: Vec::new(),
            vsu_ins: Vec::new(),
            vsu_count: 0,
            vsu_dynamic: false,
            saa_ids: Vec::new(),
            saa_ins: Vec::new(),
            saa_count: 0,
            pokey_ids: Vec::new(),
            pokey_ins: Vec::new(),
            pokey_count: 0,
            mikey_ids: Vec::new(),
            mikey_ins: Vec::new(),
            mikey_samples: Vec::new(),
            mikey_sample_events: Vec::new(),
            mikey_count: 0,
            x1_ids: Vec::new(),
            x1_ins: Vec::new(),
            x1_waves: Vec::new(),
            x1_envs: Vec::new(),
            x1_samples: Vec::new(),
            x1_sample_events: Vec::new(),
            x1_count: 0,
            fds_ids: Vec::new(),
            fds_ins: Vec::new(),
            fds_waves: Vec::new(),
            fds_mod_waves: Vec::new(),
            fds_count: 0,
            wonderswan_ids: Vec::new(),
            wonderswan_ins: Vec::new(),
            wonderswan_waves: Vec::new(),
            wonderswan_pcm_samples: Vec::new(),
            wonderswan_pcm_events: Vec::new(),
            wonderswan_count: 0,
            wonderswan_pcm_count: 0,
            qsound_ids: Vec::new(),
            qsound_ins: Vec::new(),
            qsound_samples: Vec::new(),
            qsound_count: 0,
            qsound_adpcm_count: 0,
            k005289_ids: Vec::new(),
            k005289_waves: Vec::new(),
            k005289_ins: Vec::new(),
            k005289_count: 0,
        }
    }

    fn songs_subset(names: &[&'static str]) -> BTreeSet<&'static str> {
        names.iter().copied().collect()
    }

    fn _picked(&self, name: &str) -> bool {
        self.keep_arcade && self.arcade_pick == Some(name)
    }

    fn _names(group: &BTreeSet<&'static str>) -> String {
        group
            .iter()
            .map(|n| n.to_uppercase())
            .collect::<Vec<_>>()
            .join("/")
    }

    fn run(&mut self) -> crate::Result<()> {
        let vgm = self.vgm;
        // Channel/family discovery.
        self.chips = vgm.writes.iter().map(|w| w.chip).collect();
        self.has_ym2203 = self.chips.contains("ym2203") || self.chips.contains("ym2203_ssg");
        self.has_ym2151 = self.chips.contains("ym2151");
        self.has_ym2612 = self.chips.contains("ym2612");
        self.has_ym2610 = self.chips.contains("ym2610") || self.chips.contains("ym2610_ssg");
        self.has_ym2608 = self.chips.contains("ym2608") || self.chips.contains("ym2608_ssg");
        self.has_sn76489 = self.chips.contains("sn76489");
        self.has_opl = self
            .chips
            .iter()
            .any(|c| matches!(*c, "opl" | "opl2" | "opl3" | KIND_Y8950));
        self.has_y8950 = self.chips.contains(KIND_Y8950);
        self.has_opl4 = self.chips.contains("ymf278b");
        self.has_opll = self.chips.contains("opll");
        self.has_nes = self.chips.contains("nes");

        self.dac_side = !vgm.dac_stream_plays.is_empty()
            || !vgm.dac_block_plays.is_empty()
            || !vgm.dac_events.is_empty();
        let mut sounds = BTreeMap::new();
        for (name, names) in [
            ("ym2151", &["ym2151"][..]),
            ("ym2203", &["ym2203", "ym2203_ssg"][..]),
            ("ym2612", &["ym2612"][..]),
            ("ym2610", &["ym2610", "ym2610_ssg"][..]),
            ("ym2608", &["ym2608", "ym2608_ssg"][..]),
            ("opl", &["opl"][..]),
            ("opl2", &["opl2"][..]),
            ("opl3", &["opl3"][..]),
            ("y8950", &[KIND_Y8950][..]),
            ("opl4", &["ymf278b"][..]),
            ("opll", &["opll"][..]),
        ] {
            let set = AnalyzeState::songs_subset(names);
            sounds.insert(name, super::_family_sounds(vgm, &set));
        }
        sounds.insert(
            "ym2612",
            sounds["ym2612"] || self.dac_side,
        );
        self.sounds = sounds;

        let mut arcade_present = BTreeSet::new();
        for (name, present) in [
            ("ym2608", self.has_ym2608),
            ("ym2610", self.has_ym2610),
            ("ym2612", self.has_ym2612),
            ("ym2203", self.has_ym2203),
            ("ym2151", self.has_ym2151),
        ] {
            if present {
                arcade_present.insert(name);
            }
        }
        self.arcade_present = arcade_present.clone();
        self.arcade_live = arcade_present
            .iter()
            .copied()
            .filter(|n| *self.sounds.get(n).unwrap_or(&false))
            .collect();
        let mut opl_side: BTreeSet<&'static str> =
            [KIND_OPL, KIND_OPL2, KIND_OPL3, KIND_Y8950]
                .into_iter()
                .filter(|k| self.chips.contains(k))
                .collect();
        if self.has_opl4 {
            opl_side.insert("opl4");
        }
        self.opl_side = opl_side.clone();
        self.opl_live = opl_side
            .iter()
            .copied()
            .filter(|n| *self.sounds.get(n).unwrap_or(&false))
            .collect();
        self.opll_live = self.has_opll && *self.sounds.get("opll").unwrap_or(&false);

        self.prefer_opl = false;
        self.opl_keys = 0;
        self.arcade_keys = 0;
        if self.has_opl && !self.arcade_present.is_empty() {
            self.opl_keys = super::_family_keyons(vgm, &self.opl_side);
            self.arcade_keys = super::_family_keyons(vgm, &self.arcade_present);
            self.prefer_opl = self.opl_keys > self.arcade_keys;
        }

        self.is_combo = (!self.opl_live.is_empty() || self.opll_live)
            && self.arcade_present.len() == 1
            && self.arcade_present.contains("ym2612")
            && super::_family_keyons(vgm, &AnalyzeState::songs_subset(&["ym2612"])) == 0
            && self.dac_side;
        if self.is_combo {
            self.prefer_opl = false;
        }

        let mut arcade_pick: Option<&'static str> = None;
        for name in ["ym2608", "ym2610", "ym2612"] {
            if self.arcade_live.contains(name) {
                arcade_pick = Some(name);
                break;
            }
        }
        if arcade_pick.is_none() && self.arcade_live.contains("ym2203") {
            if !self.arcade_live.contains("ym2151") {
                arcade_pick = Some("ym2203");
            }
        }
        if arcade_pick.is_none() && self.arcade_live.contains("ym2151") {
            arcade_pick = Some("ym2151");
        }
        if let Some(pick) = arcade_pick {
            let family_chips = |name: &str| -> HashSet<&'static str> {
                match name {
                    "ym2608" => ["ym2608", "ym2608_ssg"].into_iter().collect(),
                    "ym2610" => ["ym2610", "ym2610_ssg"].into_iter().collect(),
                    "ym2612" => ["ym2612"].into_iter().collect(),
                    "ym2203" => ["ym2203", "ym2203_ssg"].into_iter().collect(),
                    _ => ["ym2151"].into_iter().collect(),
                }
            };
            let mut keyons: BTreeMap<&'static str, i64> = BTreeMap::new();
            for name in &self.arcade_live {
                keyons.insert(*name, super::_family_keyons(vgm, &family_chips(name)));
            }
            if keyons.get(pick).copied().unwrap_or(0) == 0 {
                let sounding: Vec<(&'static str, i64)> = keyons
                    .iter()
                    .filter(|(_n, v)| **v != 0)
                    .map(|(n, v)| (*n, *v))
                    .collect();
                if !sounding.is_empty() {
                    // Python max(key=(count, name == "ym2203")): first wins ties.
                    let mut best = sounding[0];
                    let mut best_key = (best.1, best.0 == "ym2203");
                    for item in &sounding[1..] {
                        let k = (item.1, item.0 == "ym2203");
                        if k > best_key {
                            best = *item;
                            best_key = k;
                        }
                    }
                    arcade_pick = Some(best.0);
                }
            }
        }
        if arcade_pick.is_none() && self.opl_live.is_empty() && !self.opll_live {
            for name in ["ym2608", "ym2610", "ym2612", "ym2203", "ym2151"] {
                if self.arcade_present.contains(name) {
                    arcade_pick = Some(name);
                    break;
                }
            }
        }
        self.arcade_pick = arcade_pick;
        self.share_fm = matches!(arcade_pick, Some("ym2151" | "ym2612" | "ym2203"))
            && (!self.opl_live.is_empty() || self.opll_live);
        self.keep_arcade = arcade_pick.is_some()
            && (!self.prefer_opl || self.is_combo || self.share_fm);

        self.is_opna = self._picked("ym2608");
        self.is_opnb = self._picked("ym2610");
        self.is_opn2 = self._picked("ym2612");
        self.is_opn = self._picked("ym2203");

        let mut opl_kinds: Vec<&'static str> = [KIND_OPL, KIND_OPL2, KIND_OPL3, KIND_Y8950]
            .into_iter()
            .filter(|k| self.chips.contains(k))
            .collect();
        if self.has_opl4 {
            opl_kinds.push("opl4");
        }
        let mut opl_kind: Option<&'static str> = None;
        if self.prefer_opl
            || self.is_combo
            || self.share_fm
            || self.arcade_pick.is_none()
        {
            let mut live_kinds: Vec<&'static str> = opl_kinds
                .iter()
                .copied()
                .filter(|k| self.opl_live.contains(k))
                .collect();
            if live_kinds.is_empty() {
                live_kinds = opl_kinds.clone();
            }
            if live_kinds.len() > 1 {
                let mut best = live_kinds[0];
                let mut best_n = super::_family_keyons(
                    vgm,
                    &AnalyzeState::songs_subset(&[best]),
                );
                for k in &live_kinds[1..] {
                    let n = super::_family_keyons(vgm, &AnalyzeState::songs_subset(&[*k]));
                    if n > best_n {
                        best = *k;
                        best_n = n;
                    }
                }
                opl_kind = Some(best);
            } else if let Some(k) = live_kinds.first() {
                opl_kind = Some(*k);
            }
        }
        self.opl_kind = opl_kind;
        self.is_opl = opl_kind.is_some();
        self.is_opll = self.opll_live
            && !self.is_opl
            && (self.arcade_pick.is_none() || self.share_fm);
        self.opn2_dac_only = self.arcade_present.contains("ym2612")
            && self.dac_side
            && super::_family_keyons(vgm, &AnalyzeState::songs_subset(&["ym2612"])) == 0;

        let mut all_fm = self.arcade_present.clone();
        all_fm.extend(self.opl_side.iter().copied());
        if self.has_opll {
            all_fm.insert("opll");
        }
        let mut converted: BTreeSet<&'static str> = BTreeSet::new();
        if let Some(k) = self.opl_kind {
            converted.insert(k);
        }
        if self.is_opll {
            converted.insert("opll");
        }
        if self.keep_arcade {
            if let Some(p) = self.arcade_pick {
                converted.insert(p);
            }
        }
        self.opn2_dac = self.opn2_dac_only
            && (converted.contains("ym2151") || self.is_opl || self.is_opll);
        if self.opn2_dac {
            self.is_opn2 = true;
            converted.insert("ym2612");
        }
        let silent: BTreeSet<&'static str> = all_fm
            .iter()
            .copied()
            .filter(|n| !*self.sounds.get(n).unwrap_or(&false))
            .collect();
        let lost: BTreeSet<&'static str> = all_fm
            .iter()
            .copied()
            .filter(|n| *self.sounds.get(n).unwrap_or(&false) && !converted.contains(n))
            .collect();
        if !silent.is_empty() {
            self.warnings.push(format!(
                "{} write no notes; the chip only received its init block, so it gets no system",
                AnalyzeState::_names(&silent)
            ));
        }
        if self.is_opl && opl_kinds.len() > 1 {
            let kind = self.opl_kind.unwrap();
            let dropped: BTreeSet<&'static str> =
                opl_kinds.iter().copied().filter(|k| *k != kind).collect();
            self.warnings.push(format!(
                "more than one OPL family writes in this log; only {kind} was converted, the {} side was dropped",
                AnalyzeState::_names(&dropped)
            ));
        }
        if lost.contains("ym2151") {
            if let Some(pick) = self.arcade_pick {
                self.warnings.push(format!(
                    "YM2151 and {} write in one log; only the {} was converted",
                    pick.to_uppercase(),
                    AnalyzeState::_names(&AnalyzeState::songs_subset(&[pick]))
                ));
            }
        }
        if self.has_opll && lost.contains("opll") {
            let side = if converted.is_empty() {
                "OPL".to_string()
            } else {
                AnalyzeState::_names(&converted)
            };
            self.warnings.push(format!(
                "OPLL and another FM chip write in one log; only the {side} side was converted"
            ));
        }
        for name in &lost {
            let upper = name.to_uppercase();
            if !self.warnings.iter().any(|w| w.contains(&upper)) {
                self.warnings.push(format!(
                    "{upper} writes in this log and gets no system; the module keeps {}",
                    AnalyzeState::_names(&converted)
                ));
            }
        }
        if self.is_combo {
            let other = AnalyzeState::_names(&self.arcade_present);
            let arcade_writes = vgm
                .writes
                .iter()
                .filter(|w| self.arcade_present.contains(w.chip))
                .count();
            self.warnings.push(format!(
                "{other} drives PCM only ({arcade_writes} writes, no FM key-ons) beside the OPL music; both systems were converted"
            ));
        }
        if !converted.is_empty() {
            let mut selected: BTreeSet<&'static str> = BTreeSet::new();
            let names: Vec<&'static str> = converted.iter().copied().collect();
            for name in names {
                selected.insert(name);
                let ssg: &'static str = match name {
                    "ym2203" => "ym2203_ssg",
                    "ym2612" => "ym2612_ssg",
                    "ym2610" => "ym2610_ssg",
                    "ym2608" => "ym2608_ssg",
                    _ => "",
                };
                if !ssg.is_empty() && self.chips.contains(ssg) {
                    selected.insert(ssg);
                }
            }
            self.selected_fm_chips = Some(selected);
        }
        if self.has_opl && !self.arcade_present.is_empty() && !lost.is_empty() {
            let other = AnalyzeState::_names(&self.arcade_present);
            let opl_writes = vgm
                .writes
                .iter()
                .filter(|w| self.opl_side.contains(w.chip))
                .count();
            let arcade_writes = vgm
                .writes
                .iter()
                .filter(|w| self.arcade_present.contains(w.chip))
                .count();
            if self.prefer_opl {
                self.warnings.push(format!(
                    "OPL has more key-ons ({}) than {other} ({}); writes OPL {opl_writes}, {other} {arcade_writes}; only the OPL side was converted",
                    self.opl_keys, self.arcade_keys
                ));
            } else {
                self.warnings.push(format!(
                    "{other} has more key-ons ({}) than OPL ({}); writes {other} {arcade_writes}, OPL {opl_writes}; only the {other} side was converted",
                    self.arcade_keys, self.opl_keys
                ));
            }
        }
        if self.share_fm && !self.is_combo {
            let pick = self.arcade_pick.unwrap();
            let arcade_writes = vgm.writes.iter().filter(|w| w.chip == pick).count();
            let mut home = self.opl_side.clone();
            if self.is_opll {
                home.insert("opll");
            }
            let arcade_keys = super::_family_keyons(vgm, &AnalyzeState::songs_subset(&[pick]));
            let home_keys = super::_family_keyons(vgm, &home);
            self.warnings.push(format!(
                "{} ({arcade_keys} key-ons, {arcade_writes} writes) plays beside {} ({home_keys} key-ons); each keeps its own system",
                AnalyzeState::_names(&AnalyzeState::songs_subset(&[pick])),
                AnalyzeState::_names(&home)
            ));
        }
        self.converted = converted;

        let opl_source: Option<&'static str> = if self.opl_kind == Some("opl4") {
            Some("ymf278b")
        } else {
            self.opl_kind
        };
        self.opl_source = opl_source;
        self.opl_dual = match self.opl_kind {
            Some(KIND_OPL) => vgm.opl_dual,
            Some(KIND_OPL2) => vgm.opl2_dual,
            Some(KIND_OPL3) => vgm.opl3_dual,
            Some(KIND_Y8950) => vgm.y8950_dual,
            Some(KIND_OPL4) => vgm.ymf278b_dual,
            _ => false,
        };
        let opl_id_set: Vec<i32> = if self.is_opl {
            let mut ids: Vec<i32> = vgm
                .writes
                .iter()
                .filter(|w| Some(w.chip) == opl_source)
                .map(|w| w.chip_id as i32)
                .collect();
            ids.sort();
            ids.dedup();
            ids
        } else {
            Vec::new()
        };
        self.opl_ids = dual_ids(&opl_id_set, self.opl_dual && self.is_opl);
        let (drums, rhythm) = if self.is_opl {
            super::_opl_rhythm_plan(
                vgm,
                self.opl_kind.unwrap(),
                if self.opl_kind == Some("opl4") {
                    Some("ymf278b")
                } else {
                    None
                },
            )
        } else {
            (false, IndexMap::new())
        };
        self.opl_drums = drums;
        self.opl_rhythm = rhythm;
        self.opl_is_opl3 = matches!(self.opl_kind, Some(KIND_OPL3 | "opl4"));
        self.opl_clock = if self.is_opl {
            let raw = match self.opl_kind {
                Some(KIND_OPL3) => vgm.opl3_clock,
                Some(KIND_OPL2) => vgm.opl2_clock,
                Some(KIND_Y8950) => vgm.y8950_clock,
                Some("opl4") => vgm.ymf278b_clock,
                _ => vgm.opl_clock,
            };
            crate::opl::opl_module_clock(raw as f64, self.opl_is_opl3)
        } else {
            0.0
        };
        let mut opll_id_set: Vec<i32> = if self.is_opll {
            let mut ids: Vec<i32> = vgm
                .writes
                .iter()
                .filter(|w| w.chip == "opll")
                .map(|w| w.chip_id as i32)
                .collect();
            ids.sort();
            ids.dedup();
            ids
        } else {
            Vec::new()
        };
        opll_id_set.sort();
        self.opll_ids = dual_ids(&opll_id_set, vgm.ym2413_dual && self.is_opll);
        self.opll_vrc7 = self.is_opll && vgm.ym2413_vrc7;
        if self.is_opll && !self.opll_vrc7 {
            let (d, r) = crate::ym2413::opll_rhythm_plan(vgm, &self.opll_ids);
            self.opll_drums = d;
            self.opll_rhythm = r;
        }
        self.opll_clock = if self.is_opll {
            crate::opl::opll_module_clock(if vgm.ym2413_clock != 0 {
                vgm.ym2413_clock as f64
            } else {
                3_579_545.0
            })
        } else {
            0.0
        };
        let ym_id_set: Vec<i32> = if self.is_opn {
            let mut ids: Vec<i32> = vgm
                .writes
                .iter()
                .filter(|w| w.chip == "ym2203" || w.chip == "ym2203_ssg")
                .map(|w| w.chip_id as i32)
                .collect();
            ids.sort();
            ids.dedup();
            ids
        } else {
            Vec::new()
        };
        self.ym_ids = dual_ids(&ym_id_set, vgm.ym2203_dual && self.is_opn);
        self.ym_clock = if vgm.ym2203_clock != 0 {
            vgm.ym2203_clock
        } else {
            YM2203_DEFAULT_CLOCK
        };
        self.ym2203_ext = self.is_opn
            && vgm
                .writes
                .iter()
                .any(|w| w.chip == "ym2203" && (0xA8..=0xAE).contains(&w.reg));
        let ym2612_id_set: Vec<i32> = if self.is_opn2 {
            let mut ids: Vec<i32> = vgm
                .writes
                .iter()
                .filter(|w| w.chip == "ym2612")
                .map(|w| w.chip_id as i32)
                .collect();
            ids.sort();
            ids.dedup();
            ids
        } else {
            Vec::new()
        };
        self.ym2612_ids = dual_ids(&ym2612_id_set, vgm.ym2612_dual && self.is_opn2);
        self.ym2612_clock = if vgm.ym2612_clock != 0 {
            vgm.ym2612_clock
        } else {
            YM2612_DEFAULT_CLOCK
        };
        self.ym2612_modes = vgm
            .writes
            .iter()
            .filter(|w| self.is_opn2 && w.chip == "ym2612" && (w.reg & 0xFF) == 0x27)
            .map(|w| (w.val >> 6) & 0x03)
            .collect();
        self.ym2612_ext = self.is_opn2
            && self.ym2612_modes.iter().any(|m| *m == 1 || *m == 2);
        self.ym2612_csm = self.is_opn2 && self.ym2612_modes.contains(&2);
        let ym2610_id_set: Vec<i32> = if self.is_opnb {
            let mut ids: Vec<i32> = vgm
                .writes
                .iter()
                .filter(|w| w.chip == "ym2610" || w.chip == "ym2610_ssg")
                .map(|w| w.chip_id as i32)
                .collect();
            ids.sort();
            ids.dedup();
            ids
        } else {
            Vec::new()
        };
        self.ym2610_ids = dual_ids(&ym2610_id_set, vgm.ym2610_dual && self.is_opnb);
        self.ym2610_clock = if vgm.ym2610_clock != 0 {
            vgm.ym2610_clock
        } else {
            YM2610_DEFAULT_CLOCK
        };
        self.ym2610_b = vgm.ym2610_b;
        self.ym2610_fm_count = if self.ym2610_b { 6 } else { 4 };
        self.ym2610_modes = vgm
            .writes
            .iter()
            .filter(|w| self.is_opnb && w.chip == "ym2610" && (w.reg & 0xFF) == 0x27)
            .map(|w| (w.val >> 6) & 0x03)
            .collect();
        if self.is_opnb && self.ym2610_modes.iter().any(|m| *m == 1 || *m == 2) {
            self.warnings.push(
                "YM2610 channel-2 special/CSM mode is converted with the base 14/16-channel layout; per-operator FM3 frequencies are not split".into(),
            );
        }
        let ym2608_id_set: Vec<i32> = if self.is_opna {
            let mut ids: Vec<i32> = vgm
                .writes
                .iter()
                .filter(|w| w.chip == "ym2608" || w.chip == "ym2608_ssg")
                .map(|w| w.chip_id as i32)
                .collect();
            ids.sort();
            ids.dedup();
            ids
        } else {
            Vec::new()
        };
        self.ym2608_ids = dual_ids(&ym2608_id_set, vgm.ym2608_dual && self.is_opna);
        self.ym2608_clock = if vgm.ym2608_clock != 0 {
            vgm.ym2608_clock
        } else {
            YM2608_DEFAULT_CLOCK
        };
        self.ym2608_modes = vgm
            .writes
            .iter()
            .filter(|w| {
                self.is_opna
                    && w.chip == "ym2608"
                    && (w.reg & 0x100) == 0
                    && (w.reg & 0xFF) == 0x27
            })
            .map(|w| (w.val >> 6) & 0x03)
            .collect();
        self.ym2608_ext = self.ym2608_modes.iter().any(|m| *m == 1 || *m == 2);
        self.ym2608_fm_count = if self.ym2608_ext { 9 } else { 6 };
        self.convert_opm = self.converted.contains("ym2151");
        let opm_id_set: Vec<i32> = if self.convert_opm {
            let mut ids: Vec<i32> = vgm
                .writes
                .iter()
                .filter(|w| w.chip == "ym2151")
                .map(|w| w.chip_id as i32)
                .collect();
            ids.sort();
            ids.dedup();
            ids
        } else {
            Vec::new()
        };
        self.opm_ids = dual_ids(&opm_id_set, vgm.ym2151_dual && self.convert_opm);
        self.opm_base = self
            .opm_ids
            .iter()
            .enumerate()
            .map(|(i, cid)| (*cid, 8 * i as i32))
            .collect();
        self.opm_end = if self.convert_opm {
            8 * self.opm_ids.len() as i32
        } else {
            0
        };
        let multi_ext = self.ym2203_ext || self.ym2612_ext || self.ym2608_ext;
        self.ym_chunk = if multi_ext { 9 } else { 6 };
        if self.is_opnb {
            self.ym_chunk = self.ym2610_fm_count + 3 + 6 + 1;
            self.ym_base = self
                .ym2610_ids
                .iter()
                .enumerate()
                .map(|(i, cid)| (*cid, self.opm_end + self.ym_chunk * i as i32))
                .collect();
            self.ym_count = self.ym2610_ids.len();
        } else if self.is_opna {
            self.ym_chunk = self.ym2608_fm_count + 3 + 6 + 1;
            self.ym_base = self
                .ym2608_ids
                .iter()
                .enumerate()
                .map(|(i, cid)| (*cid, self.opm_end + self.ym_chunk * i as i32))
                .collect();
            self.ym_count = self.ym2608_ids.len();
        } else {
            let ids = if self.is_opn {
                &self.ym_ids
            } else {
                &self.ym2612_ids
            };
            self.ym_base = ids
                .iter()
                .enumerate()
                .map(|(i, cid)| (*cid, self.opm_end + self.ym_chunk * i as i32))
                .collect();
            self.ym_count = ids.len();
        }
        self.ym2203_ssg_count = 0;
        self.ym2610_ssg_count = 0;
        self.ym2608_ssg_count = 0;
        self.opl_state_kind = self.opl_kind;
        self.opl_fm_count = if self.is_opl {
            crate::opl::channel_count(self.opl_state_kind.unwrap(), self.opl_drums)
        } else {
            0
        };
        self.opl_chunk = if self.is_opl {
            self.opl_fm_count
                + if self.opl_kind == Some(KIND_Y8950) { 1 } else { 0 }
                + if self.opl_kind == Some("opl4") {
                    OPL4_WAVE_SLOTS
                } else {
                    0
                }
        } else {
            0
        };
        self.ym_end = if self.is_opn || self.is_opn2 || self.is_opnb || self.is_opna {
            self.opm_end + self.ym_chunk * self.ym_count as i32
        } else {
            self.opm_end
        };
        self.opl_base = self
            .opl_ids
            .iter()
            .enumerate()
            .map(|(i, cid)| (*cid, self.ym_end + self.opl_chunk * i as i32))
            .collect();
        self.opl_end = self.ym_end + self.opl_chunk * self.opl_ids.len() as i32;
        self.opll_chunk = if self.is_opll {
            if self.opll_vrc7 {
                6
            } else if self.opll_drums {
                OPLL_DRUMS_CHANNELS
            } else {
                OPLL_CHANNELS
            }
        } else {
            0
        };
        self.opll_start = if self.is_opl { self.opl_end } else { self.ym_end };
        self.opll_base = self
            .opll_ids
            .iter()
            .enumerate()
            .map(|(i, cid)| (*cid, self.opll_start + self.opll_chunk * i as i32))
            .collect();
        self.opll_end = self.opll_start + self.opll_chunk * self.opll_ids.len() as i32;

        let mut fm_end = 0;
        if self.is_opnb || self.is_opna || self.is_opn || self.is_opn2 {
            fm_end = self.ym_end;
        } else if self.convert_opm {
            fm_end = 8 * self.opm_ids.len() as i32;
        }
        if self.is_opl {
            fm_end = fm_end.max(self.opl_end);
        }
        if self.is_opll {
            fm_end = fm_end.max(self.opll_end);
        }
        if !(self.is_opl
            || self.is_opll
            || self.convert_opm
            || self.is_opn
            || self.is_opn2
            || self.is_opnb
            || self.is_opna)
        {
            fm_end = 8;
        }
        self.fm_end = fm_end;

        let mut fm_systems: Vec<FmSystem> = Vec::new();
        if self.is_opnb {
            for cid in &self.ym2610_ids {
                fm_systems.push(FmSystem {
                    kind: "ym2610".into(),
                    chip_id: *cid as u8,
                    fm_channels: self.ym2610_fm_count,
                    clock: self.ym2610_clock,
                    ..FmSystem::default()
                });
            }
        } else if self.is_opna {
            for cid in &self.ym2608_ids {
                fm_systems.push(FmSystem {
                    kind: "ym2608".into(),
                    chip_id: *cid as u8,
                    fm_channels: self.ym2608_fm_count,
                    ext: self.ym2608_ext,
                    clock: self.ym2608_clock,
                    ..FmSystem::default()
                });
            }
        } else if self.is_opn {
            for cid in &self.ym_ids {
                fm_systems.push(FmSystem {
                    kind: "ym2203".into(),
                    chip_id: *cid as u8,
                    fm_channels: if self.ym2203_ext { 6 } else { 3 },
                    ext: self.ym2203_ext,
                    clock: self.ym_clock,
                    ..FmSystem::default()
                });
            }
        }
        if self.convert_opm
            || !(self.is_opl || self.is_opll || !self.arcade_present.is_empty())
        {
            let ids: Vec<i32> = if self.opm_ids.is_empty() {
                vec![0]
            } else {
                self.opm_ids.clone()
            };
            for cid in ids {
                fm_systems.push(FmSystem {
                    kind: "ym2151".into(),
                    chip_id: cid as u8,
                    fm_channels: 8,
                    clock: vgm.ym2151_clock,
                    ..FmSystem::default()
                });
            }
        }
        if self.is_opn2 {
            for cid in &self.ym2612_ids {
                fm_systems.push(FmSystem {
                    kind: "ym2612".into(),
                    chip_id: *cid as u8,
                    fm_channels: if self.ym2612_ext { 9 } else { 6 },
                    ext: self.ym2612_ext,
                    csm: self.ym2612_csm,
                    clock: self.ym2612_clock,
                    ..FmSystem::default()
                });
            }
        }
        if self.is_opl {
            for cid in &self.opl_ids {
                fm_systems.push(FmSystem {
                    kind: self.opl_kind.unwrap().to_string(),
                    chip_id: *cid as u8,
                    fm_channels: self.opl_fm_count,
                    drums: self.opl_drums,
                    clock: self.opl_clock as u32,
                    ..FmSystem::default()
                });
            }
        } else if self.is_opll {
            for cid in &self.opll_ids {
                fm_systems.push(FmSystem {
                    kind: "opll".into(),
                    chip_id: *cid as u8,
                    fm_channels: if self.opll_vrc7 {
                        6
                    } else if self.opll_drums {
                        OPLL_DRUMS_CHANNELS
                    } else {
                        OPLL_CHANNELS
                    },
                    drums: self.opll_drums,
                    clock: self.opll_clock as u32,
                    vrc7: self.opll_vrc7,
                    ..FmSystem::default()
                });
            }
        }
        self.fm_systems = fm_systems;

        let psg_ids: Vec<i32> = if self.has_sn76489
            && (self.is_opn2
                || !(!self.arcade_present.is_empty()
                    || self.has_opl
                    || self.has_opll
                    || self.has_opl4
                    || self.has_y8950))
        {
            vec![0]
        } else {
            Vec::new()
        };
        self.psg_ids = psg_ids;
        self.psg_t6w28 = !self.psg_ids.is_empty() && vgm.sn76489_t6w28;
        if !self.psg_ids.is_empty() && !self.psg_t6w28 {
            let mut present: Vec<i32> = vgm
                .writes
                .iter()
                .filter(|w| w.chip == "sn76489")
                .map(|w| w.chip_id as i32)
                .collect();
            present.sort();
            present.dedup();
            if !present.is_empty() {
                self.psg_ids = present;
            }
            self.psg_ids = dual_ids(&self.psg_ids, vgm.sn76489_dual);
        }
        self.psg_stereo = !self.psg_ids.is_empty() && vgm.sn76489_stereo;
        self.psg_base = if self.psg_ids.is_empty() {
            None
        } else {
            Some(self.fm_end)
        };
        self.psg_clock = if vgm.sn76489_clock != 0 {
            vgm.sn76489_clock
        } else {
            PSG_DEFAULT_CLOCK
        };
        self.psg_at = match self.psg_base {
            Some(base) => self
                .psg_ids
                .iter()
                .enumerate()
                .map(|(i, cid)| (*cid, base + 4 * i as i32))
                .collect(),
            None => IndexMap::new(),
        };
        if !self.psg_ids.is_empty() {
            self.fm_end += 4 * self.psg_ids.len() as i32;
        }

        // --- OPM walk ---
        self.run_opm()?;
        self.run_ym2203();
        if self.is_opn2 {
            let ids = self.ym2612_ids.clone();
            self.run_opn2_opna("ym2612", ids, self.ym2612_clock, self.ym2612_ext, self.ym2612_csm, true);
        } else if self.is_opna {
            let ids = self.ym2608_ids.clone();
            self.run_opn2_opna("ym2608", ids, self.ym2608_clock, self.ym2608_ext, false, false);
        }
        self.run_opl();
        self.run_opl4_wave();
        self.run_opll();
        self.run_ym2610();
        self.run_ym2608();
        self.run_y8950();
        self.run_psg();
        self.run_pcm_bank();
        self.run_sample_chips();
        // Python analyze.py:12284 counts *instruments*, not channels: the FM
        // patches, one per audible sample body, one ADPCM/YM2610 map per 120
        // slots. Using the channel widths here put every tone instrument too
        // high whenever an FM log also had a sample engine.
        let mut fm_ins_count = self.patch_order.len() as i32;
        if fm_ins_count == 0 && self.is_opl && !self.opl4_wave_samples.is_empty() {
            fm_ins_count = 1;
        }
        self.tone_base = fm_ins_count
            + if self.opts.pcm {
                self.audible.len() as i32
            } else {
                0
            }
            + self.oki_samples.len() as i32
            + crate::oki::msm6258_ins_count(self.adpcm_samples.len() as i32)
            + self.spcm_audible.len() as i32
            + self.rf5c68_audible.len() as i32
            + self.rf5c164_audible.len() as i32
            + self.c140_audible.len() as i32
            + self.c352_audible.len() as i32
            + self.k053260_audible.len() as i32
            + (self.ym2610_a_samples.len() as i32 + YM2610_MAP_SLOTS - 1)
                / YM2610_MAP_SLOTS
            + (self.ym2610_b_samples.len() as i32 + YM2610_MAP_SLOTS - 1)
                / YM2610_MAP_SLOTS
            + if self.ym2608_rhythm_events.is_empty() { 0 } else { 1 }
            + (self.ym2608_b_samples.len() as i32 + YM2610_MAP_SLOTS - 1)
                / YM2610_MAP_SLOTS
            + (self.y8950_b_samples.len() as i32 + YM2610_MAP_SLOTS - 1)
                / YM2610_MAP_SLOTS
            + self.opl4_wave_samples.len() as i32;
        self.run_msm5232();
        self.tone_base += 8 * self.msm5232_ids.len() as i32;
        self.run_es5503();
        // Python emits the SCC and AY families before the NES block: their
        // ids are part of every later family's channel base, and their notes
        // come first in the event list.
        self.run_scc_ay();
        self.run_nes();
        self.run_huc6280();
        self.run_gb();
        self.run_vsu();
        self.run_saa_pokey();
        self.run_mikey_x1();
        self.run_fds_ws();
        self.run_qsound_k005289();
        Ok(())
    }

    fn run_opm(&mut self) -> crate::Result<()> {
        let vgm = self.vgm;
        if self.convert_opm {
            let ids = self.opm_ids.clone();
            for cid in ids {
                let base = self.opm_base[&cid];
                self.emit_chip_fx(0, FX_LFO_AMD, 0, Some(base), 0);
                self.emit_chip_fx(0, FX_LFO_PMD, 0, Some(base), 0);
            }
            if self.opm_ids.len() > 1 {
                self.warnings.push(format!(
                    "dual YM2151 log: {} chips converted as separate systems",
                    self.opm_ids.len()
                ));
            }
        }

        for opm_cid in self.opm_ids.clone() {
            let mut ym = YM2151State::new();
            let base_ch = self.opm_base[&opm_cid];
            let mut last_lfrq = -1i32;
            let mut last_wave = -1i32;
            let mut last_amd = 0i32;
            let mut last_pmd = 0i32;
            let mut last_keyon_sample = [-1_000_000_000i64; 8];
            let mut last_test_pulse = -1_000_000_000i64;
            let mut pending_off: [Option<i64>; 8] = [None; 8];
            let mut held_ins = [-1i32; 8];
            let mut held_kon = [-1i64; 8];
            let mut pms_ev: [Option<usize>; 8] = [None; 8];
            let mut held_ramp: [Option<Vec<(i64, i32, i32)>>; 8] = core::array::from_fn(|_| None);
            let mut sweep_pts: [Vec<(i64, i32, i32)>; 8] = core::array::from_fn(|_| Vec::new());
            let mut sweep_base = [(0i32, 0i32); 8];
            let mut sweep_ev: [Option<usize>; 8] = [None; 8];
            let mut tl_pts: [Vec<(i64, i32)>; 8] = core::array::from_fn(|_| Vec::new());
            let mut tl_ev: [Option<usize>; 8] = [None; 8];
            let mut tl_last = [0x7Fi32; 8];
            let mut dtmul_pts: [Vec<(i64, i32, i32)>; 8] = core::array::from_fn(|_| Vec::new());
            let mut dtmul_last = [[0i32; 4]; 8];
            let mut dtmul_ev: [Option<usize>; 8] = [None; 8];
            let mut dt2_pts: [Vec<(i64, i32, i32)>; 8] = core::array::from_fn(|_| Vec::new());
            let mut dt2_last = [[0i32; 4]; 8];
            let mut dt2_ev: [Option<usize>; 8] = [None; 8];
            let mut op_mask = [0i32; 8];

            for wi in 0..vgm.writes.len() {
                let w = &vgm.writes[wi];
                if !self.convert_opm || w.chip != "ym2151" || w.chip_id as i32 != opm_cid {
                    continue;
                }
                if w.reg != 0x08 {
                    if w.reg == 0x18 {
                        last_lfrq = (w.val & 0xFF) as i32;
                        self.emit_chip_fx(w.sample, FX_LFO_SPEED, last_lfrq, Some(base_ch), 0);
                    } else if w.reg == 0x01 {
                        if w.val & 0x02 != 0 {
                            last_test_pulse = w.sample;
                            self.emit_chip_fx(w.sample, FX_LFO_SPEED, 0, Some(base_ch), 0);
                            self.emit_chip_fx(
                                w.sample,
                                FX_LFO_SPEED,
                                last_lfrq.max(0),
                                Some(base_ch),
                                1,
                            );
                        } else if w.sample - last_test_pulse > 2 {
                            self.emit_chip_fx(
                                w.sample,
                                FX_LFO_SPEED,
                                last_lfrq.max(0),
                                Some(base_ch),
                                0,
                            );
                        }
                    } else if w.reg == 0x19 {
                        if w.val & 0x80 != 0 {
                            let pmd = (w.val & 0x7F) as i32;
                            if pmd != last_pmd {
                                last_pmd = pmd;
                                self.emit_chip_fx(w.sample, FX_LFO_PMD, pmd, Some(base_ch), 0);
                            }
                        } else {
                            let amd = (w.val & 0x7F) as i32;
                            if amd != last_amd {
                                last_amd = amd;
                                self.emit_chip_fx(w.sample, FX_LFO_AMD, amd, Some(base_ch), 0);
                            }
                        }
                    } else if w.reg == 0x1B {
                        let wave = (w.val & 3) as i32;
                        if wave != last_wave {
                            last_wave = wave;
                            self.emit_chip_fx(w.sample, FX_LFO_WAVE, wave, Some(base_ch), 0);
                        }
                    } else if (0x38..=0x3F).contains(&w.reg) {
                        let ch = (w.reg - 0x38) as usize;
                        ym.write(w.reg, w.val);
                        if let Some(ramp) = &mut held_ramp[ch] {
                            ramp.push((w.sample, ((w.val as i32) >> 4) & 7, (w.val as i32) & 3));
                        }
                        continue;
                    }
                    ym.write(w.reg, w.val);
                    if (0x28..=0x37).contains(&w.reg) {
                        let ch = (w.reg & 7) as usize;
                        if sweep_ev[ch].is_some() {
                            sweep_pts[ch].push((w.sample, ym.kc[ch], ym.kf[ch]));
                        }
                    } else if (0x60..=0x7F).contains(&w.reg) {
                        let ch = (w.reg & 7) as usize;
                        if ym.kon[ch] && tl_ev[ch].is_some() {
                            let cur = ym.carrier_tl(ch);
                            if cur != tl_last[ch] {
                                tl_pts[ch].push((w.sample, cur));
                                tl_last[ch] = cur;
                            }
                        }
                    } else if (0x40..=0x5F).contains(&w.reg) {
                        let ch = (w.reg & 7) as usize;
                        let op = ((w.reg >> 3) & 3) as usize;
                        if ym.kon[ch] && dtmul_ev[ch].is_some() {
                            let cur = ym.ops[ch][op].dt_mul as i32;
                            if cur != dtmul_last[ch][op] {
                                dtmul_pts[ch].push((w.sample, op as i32, cur));
                                dtmul_last[ch][op] = cur;
                            }
                        }
                    } else if (0xC0..=0xDF).contains(&w.reg) {
                        let ch = (w.reg & 7) as usize;
                        let op = ((w.reg >> 3) & 3) as usize;
                        if ym.kon[ch] && dt2_ev[ch].is_some() {
                            let cur = ((ym.ops[ch][op].dt2_d2r as i32) >> 6) & 3;
                            if cur != dt2_last[ch][op] {
                                dt2_pts[ch].push((w.sample, op as i32, cur));
                                dt2_last[ch][op] = cur;
                            }
                        }
                    }
                    continue;
                }
                let ch = (w.val & 7) as usize;
                let mask = ((w.val & 0x78) >> 3) as i32;
                let newly_on = mask & !op_mask[ch];
                op_mask[ch] = mask;
                if mask != 0 && newly_on != 0 {
                    pending_off[ch] = None;
                    Self::close_pms(&mut self.events, &mut self.held_lfo, pms_ev[ch], &mut held_ramp[ch], held_ins[ch], held_kon[ch]);
                    pms_ev[ch] = None;
                    Self::close_sweep(&mut self.events, sweep_ev[ch], &mut sweep_pts[ch], sweep_base[ch]);
                    sweep_ev[ch] = None;
                    Self::close_tl(&mut self.events, tl_ev[ch], &mut tl_pts[ch]);
                    tl_ev[ch] = None;
                    Self::close_dtmul(&mut self.events, dtmul_ev[ch], &mut dtmul_pts[ch]);
                    dtmul_ev[ch] = None;
                    Self::close_dt2(&mut self.events, dt2_ev[ch], &mut dt2_pts[ch]);
                    dt2_ev[ch] = None;
                    if ym.carrier_tl(ch) >= 0x7F {
                        Self::settle_burst_tl(vgm, &mut ym, wi, ch, w.sample, opm_cid);
                    }
                    Self::settle_burst_dtmul(vgm, &mut ym, wi, ch, w.sample, opm_cid);
                    let ins = self.intern_patch(&ym, ch, mask);
                    let note = kc_to_furnace_note(ym.kc[ch]);
                    self.events.push(NoteEvent {
                        sample: w.sample,
                        ch: base_ch + ch as i32,
                        on: true,
                        note,
                        ins,
                        vol: -1,
                        e5: if ym.kf[ch] != 0 { kf_to_e5(ym.kf[ch]) } else { 0x80 },
                        pan: ym.pan_byte(ch),
                        ..NoteEvent::default()
                    });
                    sweep_base[ch] = (ym.kc[ch], ym.kf[ch]);
                    sweep_pts[ch] = Vec::new();
                    sweep_ev[ch] = Some(self.events.len() - 1);
                    pms_ev[ch] = Some(self.events.len() - 1);
                    let pa = ym.pms_ams[ch];
                    held_ramp[ch] = Some(vec![(w.sample, (pa >> 4) & 7, pa & 3)]);
                    tl_pts[ch] = Vec::new();
                    tl_last[ch] = ym.carrier_tl(ch);
                    tl_ev[ch] = Some(self.events.len() - 1);
                    dtmul_pts[ch] = Vec::new();
                    dtmul_last[ch] = [
                        ym.ops[ch][0].dt_mul as i32,
                        ym.ops[ch][1].dt_mul as i32,
                        ym.ops[ch][2].dt_mul as i32,
                        ym.ops[ch][3].dt_mul as i32,
                    ];
                    dtmul_ev[ch] = Some(self.events.len() - 1);
                    dt2_pts[ch] = Vec::new();
                    dt2_last[ch] = [
                        ((ym.ops[ch][0].dt2_d2r as i32) >> 6) & 3,
                        ((ym.ops[ch][1].dt2_d2r as i32) >> 6) & 3,
                        ((ym.ops[ch][2].dt2_d2r as i32) >> 6) & 3,
                        ((ym.ops[ch][3].dt2_d2r as i32) >> 6) & 3,
                    ];
                    dt2_ev[ch] = Some(self.events.len() - 1);
                    self.keyon_times.push(w.sample);
                    last_keyon_sample[ch] = w.sample;
                    ym.kon[ch] = true;
                    held_ins[ch] = ins;
                    held_kon[ch] = w.sample;
                } else if mask != 0 {
                    // Redundant write or per-operator change while held.
                } else if ym.kon[ch] {
                    pending_off[ch] = Some(w.sample);
                    if let Some(idx) = sweep_ev[ch] {
                        if w.sample - self.events[idx].sample
                            < (0.05 * VGM_RATE as f64) as i64
                        {
                            Self::close_sweep(&mut self.events, sweep_ev[ch], &mut sweep_pts[ch], sweep_base[ch]);
                            sweep_ev[ch] = None;
                        }
                    }
                    Self::close_pms(&mut self.events, &mut self.held_lfo, pms_ev[ch], &mut held_ramp[ch], held_ins[ch], held_kon[ch]);
                    pms_ev[ch] = None;
                    Self::close_tl(&mut self.events, tl_ev[ch], &mut tl_pts[ch]);
                    tl_ev[ch] = None;
                    Self::close_dtmul(&mut self.events, dtmul_ev[ch], &mut dtmul_pts[ch]);
                    dtmul_ev[ch] = None;
                    Self::close_dt2(&mut self.events, dt2_ev[ch], &mut dt2_pts[ch]);
                    dt2_ev[ch] = None;
                    ym.kon[ch] = false;
                    held_ins[ch] = -1;
                }
            }
            for ch in 0..8 {
                Self::close_pms(&mut self.events, &mut self.held_lfo, pms_ev[ch], &mut held_ramp[ch], held_ins[ch], held_kon[ch]);
                pms_ev[ch] = None;
                Self::close_sweep(&mut self.events, sweep_ev[ch], &mut sweep_pts[ch], sweep_base[ch]);
                sweep_ev[ch] = None;
                Self::close_tl(&mut self.events, tl_ev[ch], &mut tl_pts[ch]);
                tl_ev[ch] = None;
                Self::close_dtmul(&mut self.events, dtmul_ev[ch], &mut dtmul_pts[ch]);
                dtmul_ev[ch] = None;
                Self::close_dt2(&mut self.events, dt2_ev[ch], &mut dt2_pts[ch]);
                dt2_ev[ch] = None;
            }
        }
        if self.convert_opm && !self.patch_order.is_empty() {
            super::_bind_opm_levels(&mut self.events, &mut self.patches, &self.patch_order);
        }
        self.run_opm_offs();
        Ok(())
    }

    fn run_opm_offs(&mut self) {
        let vgm = self.vgm;
        for opm_cid in self.opm_ids.clone() {
            let mut ym2 = YM2151State::new();
            let base_ch = self.opm_base[&opm_cid];
            let mut last_on = [-1_000_000_000i64; 8];
            let mut next_on_at: [Vec<i64>; 8] = core::array::from_fn(|_| Vec::new());
            let mut on_index: BTreeMap<(i32, i64), usize> = BTreeMap::new();
            for (i, ev) in self.events.iter().enumerate() {
                if self.convert_opm && ev.on && !ev.pcm
                    && base_ch <= ev.ch && ev.ch < base_ch + 8
                {
                    let lch = ev.ch - base_ch;
                    next_on_at[lch as usize].push(ev.sample);
                    on_index.insert((lch, ev.sample), i);
                }
            }
            let mut cursor = [0usize; 8];
            let mut offs: Vec<NoteEvent> = Vec::new();
            let mut sounding = [false; 8];
            for w in vgm.writes.iter() {
                if !self.convert_opm || w.chip != "ym2151" || w.chip_id as i32 != opm_cid {
                    continue;
                }
                let forced_off =
                    w.reg != 0x08 && (0x20..=0x27).contains(&w.reg) && ((w.val >> 6) & 3) == 0;
                if w.reg != 0x08 {
                    ym2.write(w.reg, w.val);
                    if !forced_off {
                        continue;
                    }
                }
                let ch = if w.reg == 0x08 {
                    (w.val & 7) as usize
                } else {
                    (w.reg & 7) as usize
                };
                if !forced_off && (w.val & 0x78) != 0 {
                    last_on[ch] = w.sample;
                    sounding[ch] = true;
                    continue;
                }
                if forced_off && !sounding[ch] {
                    continue;
                }
                sounding[ch] = false;
                let mut nxt: Option<i64> = None;
                let mut idx = cursor[ch];
                let times = &next_on_at[ch];
                while idx < times.len() && times[idx] <= w.sample {
                    idx += 1;
                }
                cursor[ch] = idx;
                if idx < times.len() {
                    nxt = Some(times[idx]);
                }
                if let Some(n) = nxt {
                    if n - w.sample <= 2 {
                        continue;
                    }
                }
                if w.sample - last_on[ch] <= 2 {
                    continue;
                }
                if let Some(note_i) = on_index.get(&(ch as i32, last_on[ch])).copied() {
                    self.events[note_i].end = w.sample;
                }
                offs.push(NoteEvent {
                    sample: w.sample,
                    ch: base_ch + ch as i32,
                    on: false,
                    note: 180,
                    ins: -1,
                    vol: -1,
                    e5: 0x80,
                    pan: 0,
                    ..NoteEvent::default()
                });
            }
            self.events.extend(offs);
        }
    }
}


impl<'a> AnalyzeState<'a> {
    fn close_sweep(
        events: &mut Vec<NoteEvent>,
        ev: Option<usize>,
        pts: &mut Vec<(i64, i32, i32)>,
        base: (i32, i32),
    ) {
        if let Some(idx) = ev {
            super::_finalize_sweep(&mut events[idx], pts, base.0, base.1);
        }
    }

    fn close_pms(
        events: &mut Vec<NoteEvent>,
        held_lfo: &mut IndexMap<i32, Vec<(Vec<i32>, Vec<i32>, Vec<i32>)>>,
        ev: Option<usize>,
        ramp: &mut Option<Vec<(i64, i32, i32)>>,
        ins: i32,
        kon: i64,
    ) {
        if let Some(idx) = ev {
            if let Some(points) = ramp {
                if !points.is_empty() {
                    events[idx].opm_pms_pts = points.clone();
                    let writes: Vec<(i64, i32, i32)> = points.iter().skip(1).copied().collect();
                    if ins >= 0 && !writes.is_empty() {
                        super::_flush_pms_ramp(held_lfo, ins, &writes, kon);
                    }
                }
            }
        }
        *ramp = None;
    }

    fn close_tl(events: &mut Vec<NoteEvent>, ev: Option<usize>, pts: &mut Vec<(i64, i32)>) {
        if let Some(idx) = ev {
            if !pts.is_empty() {
                events[idx].tl_pts = pts.clone();
            }
        }
    }

    fn close_dtmul(
        events: &mut Vec<NoteEvent>,
        ev: Option<usize>,
        pts: &mut Vec<(i64, i32, i32)>,
    ) {
        if let Some(idx) = ev {
            if !pts.is_empty() {
                events[idx].op_dtmul_pts = pts.clone();
            }
        }
    }

    fn close_dt2(
        events: &mut Vec<NoteEvent>,
        ev: Option<usize>,
        pts: &mut Vec<(i64, i32, i32)>,
    ) {
        if let Some(idx) = ev {
            if !pts.is_empty() {
                events[idx].op_dt2_pts = pts.clone();
            }
        }
    }

    fn settle_burst_tl(
        vgm: &VgmFile,
        ym: &mut YM2151State,
        idx: usize,
        ch: usize,
        sample: i64,
        opm_cid: i32,
    ) {
        for j in (idx + 1)..vgm.writes.len() {
            let w2 = &vgm.writes[j];
            if w2.sample - sample > _OPM_BURST_LOAD_SAMPLES {
                break;
            }
            if w2.chip != "ym2151"
                || w2.chip_id as i32 != opm_cid
                || !(0x60..=0x7F).contains(&w2.reg)
            {
                continue;
            }
            if (w2.reg & 7) != ch as u32 {
                continue;
            }
            ym.write(w2.reg, w2.val);
        }
    }

    fn settle_burst_dtmul(
        vgm: &VgmFile,
        ym: &mut YM2151State,
        idx: usize,
        ch: usize,
        sample: i64,
        opm_cid: i32,
    ) {
        for j in (idx + 1)..vgm.writes.len() {
            let w2 = &vgm.writes[j];
            if w2.sample - sample > _OPM_BURST_LOAD_SAMPLES {
                break;
            }
            if w2.chip != "ym2151"
                || w2.chip_id as i32 != opm_cid
                || !((0x40..=0x5F).contains(&w2.reg) || (0xC0..=0xDF).contains(&w2.reg))
            {
                continue;
            }
            if (w2.reg & 7) != ch as u32 {
                continue;
            }
            ym.write(w2.reg, w2.val);
        }
    }

    fn intern_patch(&mut self, ym: &YM2151State, ch: usize, mask: i32) -> i32 {
        let mut snap = ym.snapshot(ch);
        if mask != 0xF {
            snap.ops = core::array::from_fn(|i| {
                let mut op = ym.ops[ch][i].clone();
                if mask & (1 << OPM_OP_TO_MASK[i]) == 0 {
                    op.tl = 0x7F;
                }
                op
            });
        }
        let mut key = snap.identity().0;
        key.push(mask as i64);
        let key = PatchKey::Plain(key);
        if !self.patches.contains_key(&key) {
            snap.name = format!("OPM {:02}", self.patch_order.len());
            self.patches.insert(key.clone(), snap);
            self.patch_order.push(key.clone());
        }
        if let Some(p) = self.patches.get_mut(&key) {
            p.uses += 1;
        }
        Self::patch_index(&self.patch_order, &key)
    }

    fn patch_index(order: &[PatchKey], key: &PatchKey) -> i32 {
        order
            .iter()
            .position(|k| k == key)
            .map(|i| i as i32)
            .unwrap_or(-1)
    }

    fn emit_chip_fx(
        &mut self,
        sample: i64,
        cmd: i32,
        val: i32,
        target_ch: Option<i32>,
        tick_off: i32,
    ) {
        let entry = match (target_ch, tick_off) {
            (Some(ch), _) if ch != 0 => ChipFx::Chan(sample, cmd, val, tick_off, ch),
            (_, t) if t != 0 => ChipFx::Tick(sample, cmd, val, t),
            _ => ChipFx::Plain(sample, cmd, val),
        };
        if self.chip_fx.last() == Some(&entry) {
            return;
        }
        self.chip_fx.push(entry);
    }

    // --- OPN family shared helpers ---

    fn normalized_patch(snap: &FMPatch) -> (FMPatch, i32) {
        let carriers = carriers_for(snap.alg & 7, true, false, false);
        let mut ops = snap.ops.clone();
        let tls: Vec<i32> = carriers
            .iter()
            .map(|i| ops[*i as usize].tl as i32 & 0x7F)
            .collect();
        let floor = tls.iter().copied().min().unwrap_or(0x7F);
        for i in carriers {
            ops[i as usize].tl = ((ops[i as usize].tl as i32 & 0x7F) - floor) as u8;
        }
        let mut out = snap.clone();
        out.ops = ops;
        (out, 127 - floor)
    }

    fn intern_opn_patch_into(
        &mut self,
        snap: FMPatch,
        mask: i32,
    ) -> (i32, i32) {
        let mut snap = snap;
        let mut key = snap.identity().0;
        key.push(mask as i64);
        let mut key = PatchKey::Plain(key);
        let vol = 127 - snap.carrier_tl();
        if !self.patches.contains_key(&key) {
            if self.patch_order.len() >= OPN_BASE_INS_CAP {
                key = super::_opn_reuse_key(&snap, &self.patches, &self.patch_order);
            } else {
                snap.name = format!("OPN {:02}", self.patch_order.len());
                self.patches.insert(key.clone(), snap);
                self.patch_order.push(key.clone());
            }
        }
        if let Some(p) = self.patches.get_mut(&key) {
            p.uses += 1;
        }
        (Self::patch_index(&self.patch_order, &key), vol)
    }

    fn intern_opn_patch_raw(&mut self, snap: FMPatch, mask: i32) -> i32 {
        let mut key = snap.identity().0;
        key.push(mask as i64);
        let mut key = PatchKey::Plain(key);
        let mut snap = snap;

        if !self.patches.contains_key(&key) {
            if self.patch_order.len() >= OPN_BASE_INS_CAP {
                key = super::_opn_reuse_key(&snap, &self.patches, &self.patch_order);
            } else {
                snap.name = format!("OPN {:02}", self.patch_order.len());
                self.patches.insert(key.clone(), snap);
                self.patch_order.push(key.clone());
            }
        }
        if let Some(p) = self.patches.get_mut(&key) {
            p.uses += 1;
        }
        Self::patch_index(&self.patch_order, &key)
    }

    fn intern_ext_patch(
        &mut self,
        snap: FMPatch,
        cid: i32,
        cache: &mut BTreeMap<i32, BTreeMap<Vec<i64>, usize>>,
    ) -> i32 {
        let mut key0: Vec<i64> = vec![(snap.alg & 7) as i64, (snap.fb & 7) as i64];
        for op in &snap.ops {
            key0.extend(op.as_tuple(false));
        }
        let c = cache.entry(cid).or_default();
        if let Some(idx) = c.get(&key0) {
            let key = self.patch_order[*idx].clone();
            if let Some(p) = self.patches.get_mut(&key) {
                p.uses += 1;
            }
            return *idx as i32;
        }
        let key = PatchKey::Ext { cid, key0: key0.clone() };
        let mut snap = snap;
        snap.name = format!("OPN {:02}", self.patch_order.len());
        snap.uses = 1;
        self.patches.insert(key.clone(), snap);
        self.patch_order.push(key);
        let idx = self.patch_order.len() - 1;
        c.insert(key0, idx);
        idx as i32
    }

    fn close_opn_note_2203(
        events: &mut Vec<NoteEvent>,
        held_idx: &mut BTreeMap<(i32, usize, i32), usize>,
        pitch_pts: &mut BTreeMap<(i32, usize, i32), Vec<(i64, i32, i32)>>,
        pitch_base: &mut BTreeMap<(i32, usize, i32), (i32, i32)>,
        opn_tl_pts: &mut BTreeMap<(i32, usize, i32), Vec<(i64, i32)>>,
        opn_offs: &mut Vec<usize>,
        key: (i32, usize, i32),
        sample: i64,
        off: bool,
        clock: u32,
    ) {
        let idx = match held_idx.remove(&key) {
            Some(v) => v,
            None => return,
        };
        let ev = &events[idx];
        let ev_sample = ev.sample;
        let ev_ch = ev.ch;
        let pts = pitch_pts.remove(&key).unwrap_or_default();
        if !pts.is_empty() {
            let mut merged: Vec<(i64, i32, i32)> = Vec::new();
            for p in &pts {
                if let Some(last) = merged.last_mut() {
                    if p.0 - last.0 <= 64 {
                        *last = *p;
                        continue;
                    }
                }
                merged.push(*p);
            }
            if !merged.is_empty()
                && merged[0].0 - ev_sample <= 120
                && (merged[0].1, merged[0].2) != pitch_base[&key]
            {
                let (note, e5) =
                    crate::ym2203::opn_note_for(merged[0].1, merged[0].2, clock);
                events[idx].note = note;
                events[idx].e5 = e5;
                merged[0].0 = ev_sample;
                pitch_base.insert(key, (merged[0].1, merged[0].2));
            }
            let mut distinct: Vec<(i64, i32, i32)> = Vec::new();
            for p in &merged {
                if distinct.is_empty() || (p.1, p.2) != (distinct.last().unwrap().1, distinct.last().unwrap().2) {
                    distinct.push(*p);
                }
            }
            if distinct.len() > 1 {
                events[idx].fm_pitch_pts = distinct;
            }
        }
        let tl = opn_tl_pts.remove(&key).unwrap_or_default();
        if !tl.is_empty() {
            events[idx].tl_pts = tl;
        }
        if off {
            events[idx].end = sample;
            events.push(NoteEvent {
                sample,
                ch: ev_ch,
                on: false,
                note: 180,
                ins: -1,
                vol: -1,
                e5: 0x80,
                pan: 0,
                ..NoteEvent::default()
            });
            opn_offs.push(events.len() - 1);
        }
    }
}


impl<'a> AnalyzeState<'a> {
    fn run_ym2203(&mut self) {
        if !self.is_opn {
            return;
        }
        let vgm = self.vgm;
        let ym_ids = self.ym_ids.clone();
        let ym_clock = self.ym_clock;
        let ym_base = self.ym_base.clone();
        let ym2203_ext = self.ym2203_ext;
        let mut states: BTreeMap<i32, crate::ym2612::YM2612State> = ym_ids
            .iter()
            .map(|cid| (*cid, crate::ym2612::YM2612State::new()))
            .collect();
        let mut held_idx: BTreeMap<(i32, usize, i32), usize> = BTreeMap::new();
        let mut pitch_pts: BTreeMap<(i32, usize, i32), Vec<(i64, i32, i32)>> = BTreeMap::new();
        let mut pitch_base: BTreeMap<(i32, usize, i32), (i32, i32)> = BTreeMap::new();
        let mut opn_tl_pts: BTreeMap<(i32, usize, i32), Vec<(i64, i32)>> = BTreeMap::new();
        let mut opn_tl_last: BTreeMap<(i32, usize, i32), i32> = BTreeMap::new();
        let mut opn_offs: Vec<usize> = Vec::new();
        let mut opn_lfo = 0i32;
        let mut opn_special = 0i32;
        let mut ext_active_freqs: BTreeMap<i32, [(i32, i32); 4]> = BTreeMap::new();
        let mut ext_patch_cache: BTreeMap<i32, BTreeMap<Vec<i64>, usize>> = BTreeMap::new();
        let mut ext_tl_notes: Vec<(usize, i32, i32, i32)> = Vec::new();
        let mut ext_tl_floors: BTreeMap<i32, [i32; 4]> = BTreeMap::new();
        let mut ext_key_op: BTreeMap<(i32, usize, i32), (i32, i32)> = BTreeMap::new();

        let ext_floor = |ins: i32, oi: i32, tl: i32,
                             floors: &mut BTreeMap<i32, [i32; 4]>| {
            let per_op = floors.entry(ins).or_insert([0x7F; 4]);
            if tl < per_op[oi as usize] {
                per_op[oi as usize] = tl;
            }
        };

        for wi in 0..vgm.writes.len() {
            let w = &vgm.writes[wi];
            if w.chip != "ym2203" {
                continue;
            }
            let cid = w.chip_id as i32;
            if !states.contains_key(&cid) {
                continue;
            }
            let reg = w.reg as i32;
            let val = w.val as i32;
            if reg == 0x22 && (val & 0x08) != 0 {
                opn_lfo += 1;
            }
            if matches!(reg, 0xA8 | 0xA9 | 0xAA | 0xAC | 0xAD | 0xAE) {
                opn_special += 1;
            }
            if reg == 0x27 && (val & 0x40) != 0 {
                opn_special += 1;
            }
            if reg == 0x28 {
                let ch = (val & 0x03) as usize;
                if ch >= 3 {
                    continue;
                }
                let mask = (val >> 4) & 0x0F;
                if ym2203_ext && ch == 2 {
                    let prev = states[&cid].op_mask[ch];
                    if mask != 0 {
                        let freqs: [(i32, i32); 4] =
                            core::array::from_fn(|h| states[&cid].frequency_op(ch, h as i32));
                        let active = ext_active_freqs.get(&cid).copied();
                        for c in 0..4 {
                            let bit = 1i32 << c;
                            if (prev & bit) != 0 && (mask & bit) == 0 {
                                Self::close_opn_note_2203(
                                    &mut self.events,
                                    &mut held_idx,
                                    &mut pitch_pts,
                                    &mut pitch_base,
                                    &mut opn_tl_pts,
                                    &mut opn_offs,
                                    (cid, ch, c),
                                    w.sample,
                                    true,
                                    ym_clock,
                                );
                            }
                        }
                        let mut emitted = false;
                        for c in 0..4 {
                            let bit = 1i32 << c;
                            if (mask & bit) == 0 {
                                continue;
                            }
                            let fresh = (prev & bit) == 0 || active.is_none();
                            if !fresh {
                                continue;
                            }
                            Self::close_opn_note_2203(
                                &mut self.events,
                                &mut held_idx,
                                &mut pitch_pts,
                                &mut pitch_base,
                                &mut opn_tl_pts,
                                &mut opn_offs,
                                (cid, ch, c),
                                w.sample,
                                false,
                                ym_clock,
                            );
                            let snap = states[&cid].snapshot(2, 0x0F);
                            let ins = self.intern_ext_patch(
                                snap,
                                cid,
                                &mut ext_patch_cache,
                            );
                            let op = EXT_FREQ_OP[c as usize] as usize;
                            let (block, fnum) = freqs[op];
                            let (note, e5) = crate::ym2203::opn_note_for(
                                block,
                                fnum,
                                ym_clock,
                            );
                            self.events.push(NoteEvent {
                                sample: w.sample,
                                ch: ym_base[&cid] + 2 + c,
                                on: true,
                                note,
                                ins,
                                vol: 127,
                                e5,
                                pan: 0,
                                sweep_div: 2.0,
                                e5_lo: 0x00,
                                e5_hi: 0xFF,
                                ..NoteEvent::default()
                            });
                            let key = (cid, ch, c);
                            held_idx.insert(key, self.events.len() - 1);
                            pitch_base.insert(key, (block, fnum));
                            pitch_pts.insert(key, vec![(w.sample, block, fnum)]);
                            opn_tl_pts.insert(key, Vec::new());
                            let tl_op = OPN_ORDERED_OPS[c as usize];
                            let tl0 = states[&cid].ops[ch][tl_op as usize].tl as i32 & 0x7F;
                            ext_floor(ins, tl_op, tl0, &mut ext_tl_floors);
                            ext_tl_notes.push((self.events.len() - 1, ins, tl_op, tl0));
                            ext_key_op.insert(key, (ins, tl_op));
                            opn_tl_last.insert(key, tl0);
                            emitted = true;
                        }
                        ext_active_freqs.insert(cid, freqs);
                        if emitted {
                            self.keyon_times.push(w.sample);
                        }
                        states.get_mut(&cid).unwrap().kon[ch] = true;
                    } else {
                        for c in 0..4 {
                            Self::close_opn_note_2203(
                                &mut self.events,
                                &mut held_idx,
                                &mut pitch_pts,
                                &mut pitch_base,
                                &mut opn_tl_pts,
                                &mut opn_offs,
                                (cid, ch, c),
                                w.sample,
                                true,
                                ym_clock,
                            );
                        }
                        ext_active_freqs.remove(&cid);
                        states.get_mut(&cid).unwrap().kon[ch] = false;
                    }
                    states.get_mut(&cid).unwrap().op_mask[ch] = mask;
                    continue;
                }
                let prev = states[&cid].op_mask[ch];
                let newly_on = mask & !prev;
                states.get_mut(&cid).unwrap().op_mask[ch] = mask;
                let key = (cid, ch, -1);
                if mask != 0 && newly_on != 0 {
                    Self::close_opn_note_2203(
                        &mut self.events,
                        &mut held_idx,
                        &mut pitch_pts,
                        &mut pitch_base,
                        &mut opn_tl_pts,
                        &mut opn_offs,
                        key,
                        w.sample,
                        false,
                        ym_clock,
                    );
                    let (block, fnum) = states[&cid].frequency(ch);
                    let (note, e5) =
                        crate::ym2203::opn_note_for(block, fnum, ym_clock);
                    let snap = states[&cid].snapshot(ch, mask);
                    let ins = self.intern_opn_patch_raw(snap, mask);
                    self.events.push(NoteEvent {
                        sample: w.sample,
                        ch: ym_base[&cid] + ch as i32,
                        on: true,
                        note,
                        ins,
                        vol: -1,
                        e5,
                        pan: 0,
                        sweep_div: 2.0,
                        e5_lo: 0x00,
                        e5_hi: 0xFF,
                        ..NoteEvent::default()
                    });
                    held_idx.insert(key, self.events.len() - 1);
                    pitch_base.insert(key, (block, fnum));
                    pitch_pts.insert(key, vec![(w.sample, block, fnum)]);
                    opn_tl_pts.insert(key, Vec::new());
                    opn_tl_last.insert(key, states[&cid].carrier_tl(ch));
                    self.keyon_times.push(w.sample);
                    states.get_mut(&cid).unwrap().kon[ch] = true;
                } else if mask == 0 && states[&cid].kon[ch] {
                    states.get_mut(&cid).unwrap().kon[ch] = false;
                    Self::close_opn_note_2203(
                        &mut self.events,
                        &mut held_idx,
                        &mut pitch_pts,
                        &mut pitch_base,
                        &mut opn_tl_pts,
                        &mut opn_offs,
                        key,
                        w.sample,
                        true,
                        ym_clock,
                    );
                }
                continue;
            }
            states.get_mut(&cid).unwrap().write(w.reg as i32, w.val as i32);
            let ch: i32 = if (0xA0..=0xA6).contains(&reg) {
                reg & 0x03
            } else if (0xA8..=0xAE).contains(&reg) {
                2
            } else if (0x30..=0x9E).contains(&reg) {
                reg & 0x03
            } else if (0xB0..=0xB2).contains(&reg) {
                reg - 0xB0
            } else {
                continue;
            };
            if ch >= 3 {
                continue;
            }
            if ym2203_ext && ch == 2 {
                if (0x40..=0x4E).contains(&reg) {
                    let tl_op = opn_tl_op(reg);
                    let c = OPN_ORDERED_OPS
                        .iter()
                        .position(|x| *x == tl_op)
                        .unwrap_or(0) as i32;
                    let key = (cid, 2usize, c);
                    if !held_idx.contains_key(&key) {
                        continue;
                    }
                    let tl_now = states[&cid].ops[2][tl_op as usize].tl as i32 & 0x7F;
                    let ins = ext_key_op.get(&key).map(|v| v.0);
                    if let Some(ins) = ins {
                        ext_floor(ins, tl_op, tl_now, &mut ext_tl_floors);
                    }
                    if tl_now != *opn_tl_last.get(&key).unwrap_or(&0x7F) {
                        opn_tl_pts.get_mut(&key).unwrap().push((w.sample, tl_now));
                        opn_tl_last.insert(key, tl_now);
                    }
                    continue;
                }
                if (0xA8..=0xAE).contains(&reg) {
                    let op = reg & 0x03;
                    if op < 3 {
                        // ext_freq_col = {op: c for c, op in enumerate(EXT_FREQ_OP)}
                        let col = EXT_FREQ_OP
                            .iter()
                            .position(|x| *x == op)
                            .unwrap_or(0) as i32;
                        let key = (cid, 2usize, col);
                        if held_idx.contains_key(&key) {
                            let f = states[&cid].frequency_op(2, op);
                            pitch_pts.get_mut(&key).unwrap().push((w.sample, f.0, f.1));
                        }
                    }
                    continue;
                }
                if (0xA0..=0xA6).contains(&reg) {
                    for c in 0..4 {
                        let key = (cid, 2usize, c);
                        if held_idx.contains_key(&key) {
                            let f = states[&cid].frequency_op(2, EXT_FREQ_OP[c as usize]);
                            pitch_pts.get_mut(&key).unwrap().push((w.sample, f.0, f.1));
                        }
                    }
                }
                continue;
            }
            let key = (cid, ch as usize, -1);
            if !held_idx.contains_key(&key) {
                continue;
            }
            if (0xA0..=0xA6).contains(&reg) {
                if ch == 2
                    && states[&cid].special.iter().any(|v| *v != 0)
                    && states[&cid].special_set.iter().any(|v| *v)
                {
                    continue;
                }
                pitch_pts
                    .get_mut(&key)
                    .unwrap()
                    .push((w.sample, states[&cid].block[ch as usize], states[&cid].fnum[ch as usize]));
            } else if (0xA8..=0xAE).contains(&reg) {
                let f = states[&cid].frequency(2);
                pitch_pts.get_mut(&key).unwrap().push((w.sample, f.0, f.1));
            } else if (0x40..=0x4E).contains(&reg) {
                let cur = states[&cid].carrier_tl(ch as usize);
                if cur != *opn_tl_last.get(&key).unwrap_or(&0x7F) {
                    opn_tl_pts.get_mut(&key).unwrap().push((w.sample, cur));
                    opn_tl_last.insert(key, cur);
                }
            }
        }
        let keys: Vec<(i32, usize, i32)> = held_idx.keys().copied().collect();
        for key in keys {
            Self::close_opn_note_2203(
                &mut self.events,
                &mut held_idx,
                &mut pitch_pts,
                &mut pitch_base,
                &mut opn_tl_pts,
                &mut opn_offs,
                key,
                vgm.total_samples,
                false,
                ym_clock,
            );
        }
        if ym2203_ext && !ext_tl_notes.is_empty() {
            super::_bind_ext_levels(
                &mut self.events,
                &mut self.patches,
                &self.patch_order,
                &ext_tl_notes,
                &ext_tl_floors,
            );
        }
        super::_bind_opm_levels(&mut self.events, &mut self.patches, &self.patch_order);
        let mut ons_by_ch: BTreeMap<i32, Vec<i64>> = BTreeMap::new();
        for e in &self.events {
            if e.on {
                ons_by_ch.entry(e.ch).or_default().push(e.sample);
            }
        }
        let mut drop: BTreeSet<usize> = BTreeSet::new();
        for idx in &opn_offs {
            let ev = &self.events[*idx];
            if ons_by_ch
                .get(&ev.ch)
                .map(|ts| ts.iter().any(|t| (t - ev.sample).abs() <= 2))
                .unwrap_or(false)
            {
                drop.insert(*idx);
            }
        }
        self.drop_events(&drop);
        if opn_lfo != 0 {
            self.warnings
                .push(format!("{opn_lfo} YM2203 LFO writes are not converted"));
        }
        if opn_special != 0 && !ym2203_ext {
            self.warnings.push(format!(
                "{opn_special} YM2203 channel-3 special-mode writes; 3-slot chords are approximated by one operator"
            ));
        }
    }
}


impl<'a> AnalyzeState<'a> {
    fn intern_opn_patch_norm(&mut self, snap: FMPatch, mask: i32) -> i32 {
        let mut key = snap.identity().0;
        key.push(mask as i64);
        let key = PatchKey::Plain(key);
        let mut snap = snap;
        if !self.patches.contains_key(&key) {
            if self.patch_order.len() >= OPN_BASE_INS_CAP {
                let k2 = super::_opn_reuse_key(&snap, &self.patches, &self.patch_order);
                if let Some(p) = self.patches.get_mut(&k2) {
                    p.uses += 1;
                }
                return Self::patch_index(&self.patch_order, &k2);
            }
            snap.name = format!("OPN {:02}", self.patch_order.len());
            self.patches.insert(key.clone(), snap);
            self.patch_order.push(key.clone());
        }
        if let Some(p) = self.patches.get_mut(&key) {
            p.uses += 1;
        }
        Self::patch_index(&self.patch_order, &key)
    }

    fn run_opn2_opna(
        &mut self,
        chip: &'static str,
        ids: Vec<i32>,
        clock: u32,
        ext: bool,
        csm: bool,
        is_opn2: bool,
    ) {
        let vgm = self.vgm;
        let ym_base = self.ym_base.clone();
        let mut states: BTreeMap<i32, crate::ym2612::YM2612State> =
            ids.iter().map(|cid| (*cid, crate::ym2612::YM2612State::new())).collect();
        let mut held_idx: BTreeMap<(i32, usize, i32), usize> = BTreeMap::new();
        let mut pitch_pts: BTreeMap<(i32, usize, i32), Vec<(i64, i32, i32)>> = BTreeMap::new();
        let mut pitch_base: BTreeMap<(i32, usize, i32), (i32, i32)> = BTreeMap::new();
        let mut opn_tl_pts: BTreeMap<(i32, usize, i32), Vec<(i64, i32)>> = BTreeMap::new();
        let mut opn_tl_last: BTreeMap<(i32, usize, i32), i32> = BTreeMap::new();
        let mut opn_offs: Vec<usize> = Vec::new();
        let mut ext_active_freqs: BTreeMap<i32, [(i32, i32); 4]> = BTreeMap::new();
        let mut ext_patch_cache: BTreeMap<i32, BTreeMap<Vec<i64>, usize>> = BTreeMap::new();
        let mut ext_tl_notes: Vec<(usize, i32, i32, i32)> = Vec::new();
        let mut ext_tl_floors: BTreeMap<i32, [i32; 4]> = BTreeMap::new();
        let mut ext_key_op: BTreeMap<(i32, usize, i32), (i32, i32)> = BTreeMap::new();
        let mut dac_writes = 0i32;
        let mut lfo_last: BTreeMap<i32, i32> = BTreeMap::new();
        let mut patch_pending: BTreeMap<(i32, usize, i32), (i64, i64)> = BTreeMap::new();

        let ext_floor = |ins: i32, oi: i32, tl: i32, floors: &mut BTreeMap<i32, [i32; 4]>| {
            let per_op = floors.entry(ins).or_insert([0x7F; 4]);
            if tl < per_op[oi as usize] {
                per_op[oi as usize] = tl;
            }
        };

        let event_ch = |cid: i32, ch: usize| -> i32 {
            ym_base[&cid] + if ext && ch >= 3 { ch as i32 + 3 } else { ch as i32 }
        };
        let chan_freqs = |states: &BTreeMap<i32, crate::ym2612::YM2612State>,
                          cid: i32,
                          ch: usize|
         -> [(i32, i32); 4] {
            if ext && ch == 2 {
                core::array::from_fn(|g| states[&cid].frequency_op(2, g as i32))
            } else {
                let f = states[&cid].frequency(ch);
                [f, f, f, f]
            }
        };

        // key_times
        let mut key_times: BTreeMap<(i32, usize), Vec<i64>> = BTreeMap::new();
        for w in vgm.writes.iter() {
            if w.chip != chip || (w.reg & 0x100) != 0 {
                continue;
            }
            let r0 = w.reg & 0xFF;
            if r0 != 0x28 {
                continue;
            }
            let rc = (w.val & 0x07) as usize;
            if rc == 3 || rc == 7 {
                continue;
            }
            key_times
                .entry((w.chip_id as i32, if rc < 3 { rc } else { rc - 1 }))
                .or_default()
                .push(w.sample);
        }

        let orders: Vec<usize> = Self::kon_last_order(vgm, chip);

        for wi in orders {
            let w = &vgm.writes[wi];
            if w.chip != chip {
                continue;
            }
            let cid = w.chip_id as i32;
            if !states.contains_key(&cid) {
                continue;
            }
            // A quiet patch block is a real mid-note change.
            let pending_keys: Vec<(i32, usize, i32)> = patch_pending.keys().copied().collect();
            for pk in pending_keys {
                if pk.0 != cid {
                    continue;
                }
                let (first, last) = patch_pending[&pk];
                if w.sample - last > OPN_PATCH_SETTLE {
                    patch_pending.remove(&pk);
                    self.split_held_note(
                        pk,
                        first,
                        &mut states,
                        &mut held_idx,
                        &mut pitch_pts,
                        &mut pitch_base,
                        &mut opn_tl_pts,
                        &mut opn_tl_last,
                        clock,
                        ext,
                    );
                }
            }
            let reg = w.reg as i32;
            let val = w.val as i32;
            let r = reg & 0xFF;
            let port = (reg & 0x100) != 0;
            if !port && r == 0x22 {
                states.get_mut(&cid).unwrap().write(w.reg as i32, w.val as i32);
                let fx = (if val & 0x08 != 0 { 0x10 } else { 0x00 }) | (val & 0x07);
                if lfo_last.get(&cid).copied() != Some(fx) {
                    self.chip_fx.push(ChipFx::Plain(w.sample, 0x10, fx));
                    lfo_last.insert(cid, fx);
                }
                continue;
            }
            if !port && r == 0x27 {
                states.get_mut(&cid).unwrap().write(w.reg as i32, w.val as i32);
                continue;
            }
            if !port && r == 0x2A {
                states.get_mut(&cid).unwrap().write(w.reg as i32, w.val as i32);
                if is_opn2 {
                    dac_writes += 1;
                }
                continue;
            }
            if !port && r == 0x2B {
                let was = states[&cid].dac_enabled;
                states.get_mut(&cid).unwrap().write(w.reg as i32, w.val as i32);
                if states[&cid].dac_enabled && !was {
                    Self::close_opn_note_2612(
                        &mut self.events,
                        &mut held_idx,
                        &mut pitch_pts,
                        &mut pitch_base,
                        &mut opn_tl_pts,
                        &mut opn_offs,
                        (cid, 5, -1),
                        w.sample,
                        true,
                        clock,
                        crate::ym2612::YM2612_FREQ_BASE,
                    );
                }
                continue;
            }
            if r == 0x28 && !port {
                let raw_ch = (val & 0x07) as usize;
                if raw_ch == 3 || raw_ch == 7 {
                    continue;
                }
                let ch = if raw_ch < 3 { raw_ch } else { raw_ch - 1 };
                let mask = (val >> 4) & 0x0F;
                if !(ext && ch == 2) {
                    let pkey = (cid, ch, -1);
                    if let Some((first, last)) = patch_pending.remove(&pkey) {
                        if w.sample - last > OPN_PATCH_SETTLE {
                            self.split_held_note(
                                pkey,
                                first,
                                &mut states,
                                &mut held_idx,
                                &mut pitch_pts,
                                &mut pitch_base,
                                &mut opn_tl_pts,
                                &mut opn_tl_last,
                                clock,
                                ext,
                            );
                        }
                    }
                }
                if states[&cid].dac_enabled && ch == 5 {
                    if mask == 0 && states[&cid].kon[ch] {
                        states.get_mut(&cid).unwrap().kon[ch] = false;
                        Self::close_opn_note_2612(
                            &mut self.events,
                            &mut held_idx,
                            &mut pitch_pts,
                            &mut pitch_base,
                            &mut opn_tl_pts,
                            &mut opn_offs,
                            (cid, ch, -1),
                            w.sample,
                            true,
                            clock,
                            crate::ym2612::YM2612_FREQ_BASE,
                        );
                    }
                    states.get_mut(&cid).unwrap().op_mask[ch] = mask;
                    continue;
                }
                if ext && ch == 2 {
                    let prev = states[&cid].op_mask[ch];
                    if mask != 0 {
                        let freqs: [(i32, i32); 4] = core::array::from_fn(|g| {
                            states[&cid].frequency_op(2, g as i32)
                        });
                        let active = ext_active_freqs.get(&cid).copied();
                        for c in 0..4 {
                            let bit = 1i32 << c;
                            if (prev & bit) != 0 && (mask & bit) == 0 {
                                Self::close_opn_note_2612(
                                    &mut self.events,
                                    &mut held_idx,
                                    &mut pitch_pts,
                                    &mut pitch_base,
                                    &mut opn_tl_pts,
                                    &mut opn_offs,
                                    (cid, ch, c),
                                    w.sample,
                                    true,
                                    clock,
                                    crate::ym2612::YM2612_FREQ_BASE,
                                );
                            }
                        }
                        let mut emitted = false;
                        for c in 0..4 {
                            let bit = 1i32 << c;
                            if (mask & bit) == 0 {
                                continue;
                            }
                            let fresh = (prev & bit) == 0 || active.is_none();
                            if !fresh {
                                continue;
                            }
                            Self::close_opn_note_2612(
                                &mut self.events,
                                &mut held_idx,
                                &mut pitch_pts,
                                &mut pitch_base,
                                &mut opn_tl_pts,
                                &mut opn_offs,
                                (cid, ch, c),
                                w.sample,
                                false,
                                clock,
                                crate::ym2612::YM2612_FREQ_BASE,
                            );
                            let snap = states[&cid].snapshot(2, 0x0F);
                            let ins = self.intern_ext_patch(snap, cid, &mut ext_patch_cache);
                            let op = EXT_FREQ_OP[c as usize] as usize;
                            let (block, fnum) = freqs[op];
                            let (note, e5) = crate::ym2612::ym2612_note_for(
                                block,
                                fnum,
                                clock,
                                crate::ym2612::YM2612_FREQ_BASE,
                            );
                            self.events.push(NoteEvent {
                                sample: w.sample,
                                ch: ym_base[&cid] + 2 + c,
                                on: true,
                                note,
                                ins,
                                vol: 127,
                                e5,
                                pan: states[&cid].pan_byte(2),
                                sweep_div: 2.0,
                                e5_lo: 0x00,
                                e5_hi: 0xFF,
                                ..NoteEvent::default()
                            });
                            let key = (cid, ch, c);
                            held_idx.insert(key, self.events.len() - 1);
                            pitch_base.insert(key, (block, fnum));
                            pitch_pts.insert(key, vec![(w.sample, block, fnum)]);
                            opn_tl_pts.insert(key, Vec::new());
                            let tl_op = OPN_ORDERED_OPS[c as usize];
                            let tl0 = states[&cid].ops[ch][tl_op as usize].tl as i32 & 0x7F;
                            ext_floor(ins, tl_op, tl0, &mut ext_tl_floors);
                            ext_tl_notes.push((self.events.len() - 1, ins, tl_op, tl0));
                            ext_key_op.insert(key, (ins, tl_op));
                            opn_tl_last.insert(key, tl0);
                            emitted = true;
                        }
                        ext_active_freqs.insert(cid, freqs);
                        if emitted {
                            self.keyon_times.push(w.sample);
                        }
                        states.get_mut(&cid).unwrap().kon[ch] = true;
                    } else {
                        for c in 0..4 {
                            Self::close_opn_note_2612(
                                &mut self.events,
                                &mut held_idx,
                                &mut pitch_pts,
                                &mut pitch_base,
                                &mut opn_tl_pts,
                                &mut opn_offs,
                                (cid, ch, c),
                                w.sample,
                                true,
                                clock,
                                crate::ym2612::YM2612_FREQ_BASE,
                            );
                        }
                        ext_active_freqs.remove(&cid);
                        states.get_mut(&cid).unwrap().kon[ch] = false;
                    }
                    states.get_mut(&cid).unwrap().op_mask[ch] = mask;
                    continue;
                }
                let prev = states[&cid].op_mask[ch];
                let newly_on = mask & !prev;
                states.get_mut(&cid).unwrap().op_mask[ch] = mask;
                let key = (cid, ch, -1);
                if mask != 0 && newly_on != 0 {
                    Self::close_opn_note_2612(
                        &mut self.events,
                        &mut held_idx,
                        &mut pitch_pts,
                        &mut pitch_base,
                        &mut opn_tl_pts,
                        &mut opn_offs,
                        key,
                        w.sample,
                        false,
                        clock,
                        crate::ym2612::YM2612_FREQ_BASE,
                    );
                    let (block, fnum) = states[&cid].frequency(ch);
                    let (note, e5) = crate::ym2612::ym2612_note_for(
                        block,
                        fnum,
                        clock,
                        crate::ym2612::YM2612_FREQ_BASE,
                    );
                    let (snap, vol) = Self::normalized_patch(&states[&cid].snapshot(ch, mask));
                    let ins = self.intern_opn_patch_norm(snap, mask);
                    self.events.push(NoteEvent {
                        sample: w.sample,
                        ch: event_ch(cid, ch),
                        on: true,
                        note,
                        ins,
                        vol,
                        e5,
                        pan: states[&cid].pan_byte(ch),
                        sweep_div: 2.0,
                        e5_lo: 0x00,
                        e5_hi: 0xFF,
                        ..NoteEvent::default()
                    });
                    held_idx.insert(key, self.events.len() - 1);
                    pitch_base.insert(key, (block, fnum));
                    pitch_pts.insert(key, vec![(w.sample, block, fnum)]);
                    opn_tl_pts.insert(key, Vec::new());
                    opn_tl_last.insert(key, states[&cid].carrier_tl(ch));
                    self.keyon_times.push(w.sample);
                    states.get_mut(&cid).unwrap().kon[ch] = true;
                } else if mask == 0 && states[&cid].kon[ch] {
                    states.get_mut(&cid).unwrap().kon[ch] = false;
                    Self::close_opn_note_2612(
                        &mut self.events,
                        &mut held_idx,
                        &mut pitch_pts,
                        &mut pitch_base,
                        &mut opn_tl_pts,
                        &mut opn_offs,
                        key,
                        w.sample,
                        true,
                        clock,
                        crate::ym2612::YM2612_FREQ_BASE,
                    );
                }
                continue;
            }
            let mut ch_opt: Option<usize> = None;
            let valid;
            if (0x30..=0x9E).contains(&r) {
                valid = (r & 0x03) != 3;
                ch_opt = Some(((r & 0x03) + if port { 3 } else { 0 }) as usize);
            } else if (0xA0..=0xA6).contains(&r) {
                valid = (r & 0x03) != 3;
                ch_opt = Some(((r & 0x03) + if port { 3 } else { 0 }) as usize);
            } else if !port && (0xA8..=0xAE).contains(&r) {
                valid = (r & 0x03) != 3;
                ch_opt = Some(2);
            } else if (0xB0..=0xB2).contains(&r) {
                valid = true;
                ch_opt = Some(((r - 0xB0) + if port { 3 } else { 0 }) as usize);
            } else if (0xB4..=0xB6).contains(&r) {
                valid = true;
                ch_opt = Some(((r - 0xB4) + if port { 3 } else { 0 }) as usize);
            } else {
                valid = false;
            }
            let ch = match ch_opt {
                Some(c) if valid && c < 6 => c,
                _ => {
                    states.get_mut(&cid).unwrap().write(w.reg as i32, w.val as i32);
                    continue;
                }
            };
            let key = (cid, ch, -1);
            if !(ext && ch == 2)
                && patch_pending.contains_key(&key)
                && (0xA0..=0xA6).contains(&r)
            {
                let (first, _last) = patch_pending.remove(&key).unwrap();
                self.split_held_note(
                    key,
                    first,
                    &mut states,
                    &mut held_idx,
                    &mut pitch_pts,
                    &mut pitch_base,
                    &mut opn_tl_pts,
                    &mut opn_tl_last,
                    clock,
                    ext,
                );
            }
            let before = chan_freqs(&states, cid, ch);
            let b4 = states[&cid].pan_pms_ams[ch];
            states.get_mut(&cid).unwrap().write(w.reg as i32, w.val as i32);
            let after = chan_freqs(&states, cid, ch);
            if !(ext && ch == 2)
                && Self::patch_identity_write(r)
                && held_idx.contains_key(&key)
            {
                if let Some(pend) = patch_pending.get(&key).copied() {
                    patch_pending.insert(key, (pend.0, w.sample));
                } else {
                    let soon = key_times
                        .get(&(cid, ch))
                        .map(|ts| ts.iter().any(|t| (0..=OPN_PATCH_SETTLE).contains(&(t - w.sample))))
                        .unwrap_or(false);
                    if !soon {
                        patch_pending.insert(key, (w.sample, w.sample));
                    }
                }
            }
            if ext && ch == 2 {
                for c in 0..4 {
                    let key = (cid, ch, c);
                    let group = EXT_FREQ_OP[c as usize] as usize;
                    if held_idx.contains_key(&key) && after[group] != before[group] {
                        pitch_pts.get_mut(&key).unwrap().push((
                            w.sample,
                            after[group].0,
                            after[group].1,
                        ));
                    }
                }
                if (r & 0xF0) == 0x40 {
                    let tl_op = opn_tl_op(r);
                    let c = OPN_ORDERED_OPS
                        .iter()
                        .position(|x| *x == tl_op)
                        .unwrap_or(0) as i32;
                    let key = (cid, ch, c);
                    if held_idx.contains_key(&key) {
                        let tl_now = states[&cid].ops[ch][tl_op as usize].tl as i32 & 0x7F;
                        if let Some((ins_x, _)) = ext_key_op.get(&key).copied() {
                            ext_floor(ins_x, tl_op, tl_now, &mut ext_tl_floors);
                        }
                        if tl_now != *opn_tl_last.get(&key).unwrap_or(&0x7F) {
                            opn_tl_pts.get_mut(&key).unwrap().push((w.sample, tl_now));
                            opn_tl_last.insert(key, tl_now);
                        }
                    }
                }
            } else {
                let key = (cid, ch, -1);
                if held_idx.contains_key(&key) && after[0] != before[0] {
                    pitch_pts
                        .get_mut(&key)
                        .unwrap()
                        .push((w.sample, after[0].0, after[0].1));
                }
                if (0x40..=0x4E).contains(&r)
                    && held_idx.contains_key(&key)
                    && !patch_pending.contains_key(&key)
                {
                    let cur = states[&cid].carrier_tl(ch);
                    if cur != *opn_tl_last.get(&key).unwrap_or(&0x7F) {
                        opn_tl_pts.get_mut(&key).unwrap().push((w.sample, cur));
                        opn_tl_last.insert(key, cur);
                    }
                }
            }
            if (0xB4..=0xB6).contains(&r) && !(ch == 5 && states[&cid].dac_enabled) {
                let new_b4 = states[&cid].pan_pms_ams[ch];
                if new_b4 != b4 {
                    let target = if ext && ch == 2 {
                        ym_base[&cid] + 2
                    } else {
                        event_ch(cid, ch)
                    };
                    let pan = states[&cid].pan_byte(ch);
                    if (b4 ^ new_b4) & 0xC0 != 0 {
                        self.channel_fx.push((w.sample, target, 0x08, pan));
                    }
                    if (b4 ^ new_b4) & 0x30 != 0 {
                        self.channel_fx
                            .push((w.sample, target, 0x63, (new_b4 >> 4) & 0x03));
                    }
                    if (b4 ^ new_b4) & 0x07 != 0 {
                        self.channel_fx
                            .push((w.sample, target, 0x62, new_b4 & 0x07));
                    }
                }
            }
        }

        let keys: Vec<(i32, usize, i32)> = held_idx.keys().copied().collect();
        for key in keys {
            Self::close_opn_note_2612(
                &mut self.events,
                &mut held_idx,
                &mut pitch_pts,
                &mut pitch_base,
                &mut opn_tl_pts,
                &mut opn_offs,
                key,
                vgm.total_samples,
                false,
                clock,
                crate::ym2612::YM2612_FREQ_BASE,
            );
        }
        if ext && !ext_tl_notes.is_empty() {
            super::_bind_ext_levels(
                &mut self.events,
                &mut self.patches,
                &self.patch_order,
                &ext_tl_notes,
                &ext_tl_floors,
            );
        }
        let mut ons_by_ch: BTreeMap<i32, Vec<i64>> = BTreeMap::new();
        for e in &self.events {
            if e.on {
                ons_by_ch.entry(e.ch).or_default().push(e.sample);
            }
        }
        let mut drop: BTreeSet<usize> = BTreeSet::new();
        for idx in &opn_offs {
            let ev = &self.events[*idx];
            if ons_by_ch
                .get(&ev.ch)
                .map(|ts| ts.iter().any(|t| (t - ev.sample).abs() <= 2))
                .unwrap_or(false)
            {
                drop.insert(*idx);
            }
        }
        self.drop_events(&drop);
        if csm {
            self.warnings.push(
                "YM2612 CSM mode: per-operator frequencies are converted, but timer-generated key-ons are not synthesized".into(),
            );
        }
        self.opn_dac_writes = dac_writes;
    }
}


impl<'a> AnalyzeState<'a> {
    fn patch_identity_write(r: i32) -> bool {
        (0x30..=0x3E).contains(&r) || (0x90..=0x9E).contains(&r) || (0xB0..=0xB2).contains(&r)
    }

    fn kon_last_order(vgm: &VgmFile, chip: &str) -> Vec<usize> {
        let mut ordered = Vec::with_capacity(vgm.writes.len());
        let mut i = 0;
        while i < vgm.writes.len() {
            let mut j = i + 1;
            while j < vgm.writes.len() && vgm.writes[j].sample == vgm.writes[i].sample {
                j += 1;
            }
            let is_kon = |w: &ChipWrite| {
                w.chip == chip && (w.reg & 0x100) == 0 && (w.reg & 0xFF) == 0x28
            };
            for k in i..j {
                if !is_kon(&vgm.writes[k]) {
                    ordered.push(k);
                }
            }
            for k in i..j {
                if is_kon(&vgm.writes[k]) {
                    ordered.push(k);
                }
            }
            i = j;
        }
        ordered
    }

    #[allow(clippy::too_many_arguments)]
    fn close_opn_note_2612(
        events: &mut Vec<NoteEvent>,
        held_idx: &mut BTreeMap<(i32, usize, i32), usize>,
        pitch_pts: &mut BTreeMap<(i32, usize, i32), Vec<(i64, i32, i32)>>,
        pitch_base: &mut BTreeMap<(i32, usize, i32), (i32, i32)>,
        opn_tl_pts: &mut BTreeMap<(i32, usize, i32), Vec<(i64, i32)>>,
        opn_offs: &mut Vec<usize>,
        key: (i32, usize, i32),
        sample: i64,
        off: bool,
        clock: u32,
        freq_base: f64,
    ) {
        let idx = match held_idx.remove(&key) {
            Some(v) => v,
            None => return,
        };
        let ev_sample = events[idx].sample;
        let ev_ch = events[idx].ch;
        let pts = pitch_pts.remove(&key).unwrap_or_default();
        if !pts.is_empty() {
            let mut merged: Vec<(i64, i32, i32)> = Vec::new();
            for p in &pts {
                if let Some(last) = merged.last_mut() {
                    if p.0 - last.0 <= 64 {
                        *last = *p;
                        continue;
                    }
                }
                merged.push(*p);
            }
            if !merged.is_empty()
                && merged[0].0 - ev_sample <= 120
                && (merged[0].1, merged[0].2) != pitch_base[&key]
            {
                let (note, e5) =
                    crate::ym2612::ym2612_note_for(merged[0].1, merged[0].2, clock, freq_base);
                events[idx].note = note;
                events[idx].e5 = e5;
                merged[0].0 = ev_sample;
                pitch_base.insert(key, (merged[0].1, merged[0].2));
            }
            let mut distinct: Vec<(i64, i32, i32)> = Vec::new();
            for p in &merged {
                if distinct.is_empty()
                    || (p.1, p.2) != (distinct.last().unwrap().1, distinct.last().unwrap().2)
                {
                    distinct.push(*p);
                }
            }
            if distinct.len() > 1 {
                events[idx].fm_pitch_pts = distinct;
            }
        }
        let mut tl = opn_tl_pts.remove(&key).unwrap_or_default();
        if !tl.is_empty() {
            let keyon_tl = 127 - events[idx].vol.max(0);
            if (0..=OPN_TL_FOLD).contains(&(tl[0].0 - ev_sample)) && tl[0].1 < keyon_tl {
                events[idx].vol = (127 - tl[0].1).clamp(0, 127);
                tl.remove(0);
            }
            if !tl.is_empty() {
                events[idx].tl_pts = tl;
            }
        }
        if off {
            events[idx].end = sample;
            events.push(NoteEvent {
                sample,
                ch: ev_ch,
                on: false,
                note: 180,
                ins: -1,
                vol: -1,
                e5: 0x80,
                pan: 0,
                ..NoteEvent::default()
            });
            opn_offs.push(events.len() - 1);
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn split_held_note(
        &mut self,
        key: (i32, usize, i32),
        sample: i64,
        states: &mut BTreeMap<i32, crate::ym2612::YM2612State>,
        held_idx: &mut BTreeMap<(i32, usize, i32), usize>,
        pitch_pts: &mut BTreeMap<(i32, usize, i32), Vec<(i64, i32, i32)>>,
        pitch_base: &mut BTreeMap<(i32, usize, i32), (i32, i32)>,
        opn_tl_pts: &mut BTreeMap<(i32, usize, i32), Vec<(i64, i32)>>,
        opn_tl_last: &mut BTreeMap<(i32, usize, i32), i32>,
        clock: u32,
        ext: bool,
    ) {
        let idx = match held_idx.get(&key).copied() {
            Some(v) => v,
            None => return,
        };
        let (cid, ch, _op) = key;
        let ev_ch = self.events[idx].ch;
        if let Some(pts) = pitch_pts.get_mut(&key) {
            pts.retain(|p| p.0 < sample);
        }
        self.events[idx].end = sample;
        Self::close_opn_note_2612(
            &mut self.events,
            held_idx,
            pitch_pts,
            pitch_base,
            opn_tl_pts,
            &mut Vec::new(),
            key,
            sample,
            false,
            clock,
            crate::ym2612::YM2612_FREQ_BASE,
        );
        let (block, fnum) = states[&cid].frequency(ch);
        let (note, e5) =
            crate::ym2612::ym2612_note_for(block, fnum, clock, crate::ym2612::YM2612_FREQ_BASE);
        let (snap, vol) = Self::normalized_patch(&states[&cid].snapshot(ch, states[&cid].op_mask[ch]));
        let ins = self.intern_opn_patch_norm(snap, states[&cid].op_mask[ch]);
        let pan = if ext && ch == 2 {
            states[&cid].pan_byte(2)
        } else {
            states[&cid].pan_byte(ch)
        };
        self.events.push(NoteEvent {
            sample,
            ch: ev_ch,
            on: true,
            note,
            ins,
            vol,
            e5,
            pan,
            sweep_div: 2.0,
            e5_lo: 0x00,
            e5_hi: 0xFF,
            legato: true,
            ..NoteEvent::default()
        });
        held_idx.insert(key, self.events.len() - 1);
        pitch_base.insert(key, (block, fnum));
        pitch_pts.insert(key, vec![(sample, block, fnum)]);
        opn_tl_pts.insert(key, Vec::new());
        opn_tl_last.insert(key, states[&cid].carrier_tl(ch));
    }
}


impl<'a> AnalyzeState<'a> {
    fn run_opl(&mut self) {
        if !self.is_opl {
            return;
        }
        let vgm = self.vgm;
        let kind = self.opl_state_kind.unwrap();
        let chip_name = if self.opl_kind == Some(KIND_OPL4) {
            Some("ymf278b")
        } else {
            None
        };
        let freq_base = if self.opl_kind == Some(KIND_OPL4) {
            Some(crate::opl::FREQ_BASE_OPL4)
        } else {
            None
        };
        let (stats, warn) = super::_collect_opl(
            vgm,
            &mut self.events,
            &mut self.patches,
            &mut self.patch_order,
            &mut self.keyon_times,
            &mut self.channel_fx,
            &mut self.chip_fx,
            kind,
            &self.opl_ids,
            &self.opl_base,
            self.opl_clock,
            self.opl_drums,
            chip_name,
            freq_base,
        );
        self.warnings.extend(warn);
        if stats.get("four_op").copied().unwrap_or(0) != 0 {
            self.warnings.push(format!(
                "{} note-on(s) read as an OPL3 four-operator voice (0x104 pair bit)",
                stats["four_op"]
            ));
        }
        if stats.get("ins_reused").copied().unwrap_or(0) != 0 {
            self.warnings.push(format!(
                "{} note(s) past the 200-instrument limit share a patch of the same carrier shape; a Furnace module stores only 256 instruments in total",
                stats["ins_reused"]
            ));
        }
        self.opl_rhythm = stats;
    }

    fn run_opl4_wave(&mut self) {
        if !self.is_opl || self.opl_kind != Some(KIND_OPL4) {
            return;
        }
        let vgm = self.vgm;
        let rom = crate::vgm::assemble_rom(vgm, "ymf278b_rom", 0, 0, false, true);
        let rom = if rom.is_empty() {
            crate::ymf278b::load_yrw801(None)
        } else {
            rom
        };
        let ram = crate::vgm::assemble_rom(vgm, "ymf278b_ram", 0, 0, false, true);
        if self.opts.pcm {
            let (samples, warn) = crate::ymf278b::collect_opl4_wave(vgm, &rom, &ram);
            self.opl4_wave_samples = samples;
            self.warnings.extend(warn);
        }
        let mut wave_base: BTreeMap<i32, i32> = BTreeMap::new();
        for cid in &self.opl_ids {
            wave_base.insert(*cid, self.opl_base[cid] + self.opl_fm_count);
        }
        let opl_clock = self.opl_clock;
        for i in 0..self.opl4_wave_samples.len() {
            let smp = self.opl4_wave_samples[i].clone();
            let ref_rate = crate::ymf278b::opl4_rate(opl_clock as u32, smp.mode_step());
            let base_ch = wave_base.get(&0).copied().unwrap_or(0);
            for h in &smp.hits {
                self.opl4_wave_count += 1;
                let (note, e5) =
                    crate::pcm::rate_to_note(crate::ymf278b::opl4_rate(opl_clock as u32, h.step), ref_rate);
                let wave_pan = crate::ymf278b::opl4_pan_fx(h.pan);
                let mut vol = (0x7F - (h.level & 0x7F)).clamp(0, 127);
                if (h.pan & 0x0F) == 8 {
                    vol = 0;
                }
                let mut motion: Vec<(i64, i32, i32)> = h
                    .vol_pts
                    .iter()
                    .map(|(t, level)| (*t, 0, *level))
                    .collect();
                motion.extend(h.pan_pts.iter().map(|(t, pan)| (*t, 1, *pan)));
                motion.sort_by_key(|p| p.0);
                let mut last_vol = vol;
                let mut last_pan = h.pan & 0x0F;
                let mut written_vol = vol;
                let mut written_pan = last_pan;
                let mut vol_pts: Vec<(i64, i32, Option<i32>)> = Vec::new();
                for (when, kind, value) in motion {
                    if when <= h.sample_time {
                        continue;
                    }
                    if h.end_time != 0 && when >= h.end_time {
                        break;
                    }
                    if kind == 0 {
                        last_vol = (0x7F - (value & 0x7F)).clamp(0, 127);
                    } else {
                        last_pan = value & 0x0F;
                    }
                    let target_vol = if last_pan == 8 { 0 } else { last_vol };
                    if target_vol == written_vol && last_pan == written_pan {
                        continue;
                    }
                    let pan_fx = if last_pan != 8 && last_pan != written_pan {
                        Some(crate::ymf278b::opl4_pan_fx(last_pan))
                    } else {
                        None
                    };
                    vol_pts.push((when, target_vol, pan_fx));
                    written_vol = target_vol;
                    written_pan = last_pan;
                }
                while !vol_pts.is_empty()
                    && vol_pts.last().unwrap().1 == 0
                    && h.end_time != 0
                    && h.end_time - vol_pts.last().unwrap().0
                        <= crate::ymf278b::OPL4_TAIL_MUTE
                {
                    vol_pts.pop();
                }
                let mut pitch_steps: Vec<(i64, i32)> = Vec::new();
                if h.step != 0 {
                    let mut previous = 0;
                    for (when, step) in &h.pitch_pts {
                        if *when <= h.sample_time {
                            continue;
                        }
                        if h.end_time != 0 && *when >= h.end_time {
                            break;
                        }
                        let delta = py_round(64.0 * 12.0 * (*step as f64 / h.step as f64).log2())
                            as i32;
                        if delta != previous {
                            pitch_steps.push((*when, delta));
                            previous = delta;
                        }
                    }
                }
                let mut ev = NoteEvent {
                    sample: h.sample_time,
                    ch: base_ch + h.slot as i32,
                    on: true,
                    note,
                    ins: -1,
                    vol,
                    e5,
                    pan: wave_pan,
                    pcm: true,
                    end: h.end_time,
                    vol_pts,
                    pitch_steps,
                    ..NoteEvent::default()
                };
                if !ev.pitch_steps.is_empty() {
                    ev.e5_lo = 0x00;
                    ev.e5_hi = 0xFF;
                }
                self.events.push(ev);
                self.opl4_wave_events.push((self.events.len() - 1, i as i32));
            }
        }
        for smp in self.opl4_wave_samples.clone() {
            let base_ch = wave_base.get(&0).copied().unwrap_or(0);
            for h in &smp.hits {
                if h.end_time != 0 && h.end_time > h.sample_time {
                    self.events.push(NoteEvent {
                        sample: h.end_time,
                        ch: base_ch + h.slot as i32,
                        on: false,
                        note: 180,
                        ins: -1,
                        vol: -1,
                        e5: 0x80,
                        pan: 0,
                        pcm: true,
                        ..NoteEvent::default()
                    });
                }
            }
        }
    }

    fn run_opll(&mut self) {
        if !self.is_opll {
            return;
        }
        let vgm = self.vgm;
        let (stats, warn) = super::_collect_opll(
            vgm,
            &mut self.events,
            &mut self.patches,
            &mut self.patch_order,
            &mut self.keyon_times,
            &mut self.channel_fx,
            &mut self.chip_fx,
            &self.opll_ids,
            &self.opll_base,
            self.opll_clock,
            self.opll_drums,
        );
        self.warnings.extend(warn);
        if let Some(melodic) = self.opll_rhythm.get("melodic_on_kit").copied() {
            if self.opll_drums && melodic != 0 {
                self.warnings.push(format!(
                    "{melodic} melodic key-on(s) on OPLL channels 7-9 landed while rhythm mode was off"
                ));
            }
        }
        self.opll_rhythm = stats;
    }
}


impl<'a> AnalyzeState<'a> {
    fn run_ym2610(&mut self) {
        if !self.is_opnb {
            return;
        }
        let vgm = self.vgm;
        let ids = self.ym2610_ids.clone();
        let clock = self.ym2610_clock;
        let fm_count = self.ym2610_fm_count;
        let ym_base = self.ym_base.clone();
        let b = self.ym2610_b;
        let mut states: BTreeMap<i32, crate::ym2610::YM2610State> =
            ids.iter().map(|cid| (*cid, crate::ym2610::YM2610State::new(b))).collect();
        let mut held_idx: BTreeMap<(i32, usize, i32), usize> = BTreeMap::new();
        let mut pitch_pts: BTreeMap<(i32, usize, i32), Vec<(i64, i32, i32)>> = BTreeMap::new();
        let mut pitch_base: BTreeMap<(i32, usize, i32), (i32, i32)> = BTreeMap::new();
        let mut opn_tl_pts: BTreeMap<(i32, usize, i32), Vec<(i64, i32)>> = BTreeMap::new();
        let mut opn_tl_last: BTreeMap<(i32, usize, i32), i32> = BTreeMap::new();
        let mut opn_offs: Vec<usize> = Vec::new();
        let mut lfo_last: BTreeMap<i32, i32> = BTreeMap::new();
        for wi in Self::kon_last_order(vgm, "ym2610") {
            let w = &vgm.writes[wi];
            if w.chip != "ym2610" {
                continue;
            }
            let cid = w.chip_id as i32;
            if !states.contains_key(&cid) {
                continue;
            }
            let reg = w.reg as i32;
            let val = w.val as i32;
            let r = reg & 0xFF;
            let port = (reg & 0x100) != 0;
            if !port && r == 0x22 {
                states.get_mut(&cid).unwrap().write(w.reg as i32, w.val as i32);
                let fx = (if val & 0x08 != 0 { 0x10 } else { 0x00 }) | (val & 0x07);
                if lfo_last.get(&cid).copied() != Some(fx) {
                    self.chip_fx.push(ChipFx::Plain(w.sample, 0x10, fx));
                    lfo_last.insert(cid, fx);
                }
                continue;
            }
            if !port && r == 0x27 {
                states.get_mut(&cid).unwrap().write(w.reg as i32, w.val as i32);
                continue;
            }
            if (port && r <= 0x2D) || (!port && (0x10..=0x1F).contains(&r)) {
                states.get_mut(&cid).unwrap().write(w.reg as i32, w.val as i32);
                continue;
            }
            if !port && r == 0x28 {
                let raw = (val & 7) as i32;
                let ch = if b {
                    match raw {
                        0 => Some(0usize),
                        1 => Some(1),
                        2 => Some(2),
                        4 => Some(3),
                        5 => Some(4),
                        6 => Some(5),
                        _ => None,
                    }
                } else {
                    match raw {
                        1 => Some(0usize),
                        2 => Some(1),
                        5 => Some(2),
                        6 => Some(3),
                        _ => None,
                    }
                };
                let ch = match ch {
                    Some(c) => c,
                    None => {
                        states.get_mut(&cid).unwrap().write(w.reg as i32, w.val as i32);
                        continue;
                    }
                };
                let prev = states[&cid].op_mask[ch];
                let mask = (val >> 4) & 0x0F;
                states.get_mut(&cid).unwrap().write(w.reg as i32, w.val as i32);
                let newly_on = mask & !prev;
                let key = (cid, ch, -1);
                if mask != 0 && newly_on != 0 {
                    Self::close_opn_note_2612(
                        &mut self.events,
                        &mut held_idx,
                        &mut pitch_pts,
                        &mut pitch_base,
                        &mut opn_tl_pts,
                        &mut opn_offs,
                        key,
                        w.sample,
                        false,
                        clock,
                        YM2610_FREQ_BASE,
                    );
                    let (block, fnum) = states[&cid].frequency(ch);
                    let (note, e5) =
                        crate::ym2610::ym2610_note_for(block, fnum, clock);
                    let (snap, vol) = Self::normalized_patch(&states[&cid].snapshot(ch, mask));
                    let ins = self.intern_opn_patch_norm(snap, mask);
                    self.events.push(NoteEvent {
                        sample: w.sample,
                        ch: ym_base[&cid] + ch as i32,
                        on: true,
                        note,
                        ins,
                        vol,
                        e5,
                        pan: states[&cid].pan_byte_ch(ch),
                        sweep_div: 2.0,
                        e5_lo: 0x00,
                        e5_hi: 0xFF,
                        ..NoteEvent::default()
                    });
                    held_idx.insert(key, self.events.len() - 1);
                    pitch_base.insert(key, (block, fnum));
                    pitch_pts.insert(key, vec![(w.sample, block, fnum)]);
                    opn_tl_pts.insert(key, Vec::new());
                    opn_tl_last.insert(key, states[&cid].carrier_tl(ch));
                    self.keyon_times.push(w.sample);
                    states.get_mut(&cid).unwrap().kon[ch] = true;
                } else if mask == 0 && states[&cid].kon[ch] {
                    states.get_mut(&cid).unwrap().kon[ch] = false;
                    Self::close_opn_note_2612(
                        &mut self.events,
                        &mut held_idx,
                        &mut pitch_pts,
                        &mut pitch_base,
                        &mut opn_tl_pts,
                        &mut opn_offs,
                        key,
                        w.sample,
                        true,
                        clock,
                        YM2610_FREQ_BASE,
                    );
                }
                continue;
            }
            let ch = match states[&cid].channel_of(reg) {
                Some(c) => c,
                None => {
                    states.get_mut(&cid).unwrap().write(w.reg as i32, w.val as i32);
                    continue;
                }
            };
            let before = states[&cid].frequency(ch);
            let b4 = states[&cid].pan_pms_ams[ch];
            states.get_mut(&cid).unwrap().write(w.reg as i32, w.val as i32);
            let after = states[&cid].frequency(ch);
            let key = (cid, ch, -1);
            if held_idx.contains_key(&key) && after != before {
                pitch_pts
                    .get_mut(&key)
                    .unwrap()
                    .push((w.sample, after.0, after.1));
            }
            if (0x40..=0x4E).contains(&r) && held_idx.contains_key(&key) {
                let cur = states[&cid].carrier_tl(ch);
                if cur != *opn_tl_last.get(&key).unwrap_or(&0x7F) {
                    opn_tl_pts.get_mut(&key).unwrap().push((w.sample, cur));
                    opn_tl_last.insert(key, cur);
                }
            }
            if (0xB4..=0xB6).contains(&r) {
                let new_b4 = states[&cid].pan_pms_ams[ch];
                if new_b4 != b4 {
                    let target = ym_base[&cid] + ch as i32;
                    let pan = states[&cid].pan_byte_ch(ch);
                    if (b4 ^ new_b4) & 0xC0 != 0 {
                        self.channel_fx.push((w.sample, target, 0x08, pan));
                    }
                    if (b4 ^ new_b4) & 0x30 != 0 {
                        self.channel_fx
                            .push((w.sample, target, 0x63, (new_b4 >> 4) & 0x03));
                    }
                    if (b4 ^ new_b4) & 0x07 != 0 {
                        self.channel_fx
                            .push((w.sample, target, 0x62, new_b4 & 0x07));
                    }
                }
            }
        }
        let keys: Vec<(i32, usize, i32)> = held_idx.keys().copied().collect();
        for key in keys {
            Self::close_opn_note_2612(
                &mut self.events,
                &mut held_idx,
                &mut pitch_pts,
                &mut pitch_base,
                &mut opn_tl_pts,
                &mut opn_offs,
                key,
                vgm.total_samples,
                false,
                clock,
                YM2610_FREQ_BASE,
            );
        }
        let mut ons_by_ch: BTreeMap<i32, Vec<i64>> = BTreeMap::new();
        for e in &self.events {
            if e.on {
                ons_by_ch.entry(e.ch).or_default().push(e.sample);
            }
        }
        let mut drop: BTreeSet<usize> = BTreeSet::new();
        for idx in &opn_offs {
            let ev = &self.events[*idx];
            if ons_by_ch
                .get(&ev.ch)
                .map(|ts| ts.iter().any(|t| (t - ev.sample).abs() <= 2))
                .unwrap_or(false)
            {
                drop.insert(*idx);
            }
        }
        self.drop_events(&drop);
        let rom_a = if self.opts.pcm {
            crate::vgm::assemble_rom(vgm, "ym2610_adpcm", 0, 0, false, true)
        } else {
            Vec::new()
        };
        let rom_b = if self.opts.pcm {
            crate::vgm::assemble_rom(vgm, "ym2610_deltat", 0, 0, false, true)
        } else {
            Vec::new()
        };
        if self.opts.pcm {
            if !rom_a.is_empty() || !rom_b.is_empty() {
                let (a_samples, b_samples, ym_warn) = crate::ym2610::collect_ym2610_adpcm(
                    vgm,
                    &rom_a,
                    if rom_b.is_empty() { None } else { Some(&rom_b) },
                );
                self.warnings.extend(ym_warn);
                self.ym2610_a_samples = a_samples;
                self.ym2610_b_samples = b_samples;
                for i in 0..self.ym2610_a_samples.len() {
                    let smp = self.ym2610_a_samples[i].clone();
                    for h in &smp.hits {
                        self.ym2610_a_count += 1;
                        if h.pan == 0 && !h.pan_pts.iter().any(|p| p.1 != 0) {
                            continue;
                        }
                        let mut ev = NoteEvent {
                            sample: h.sample_time,
                            ch: ym_base[&(h.chip_id as i32)] + fm_count + 3 + h.channel,
                            on: true,
                            note: 60 + (i as i32 % YM2610_MAP_SLOTS),
                            ins: -1,
                            vol: if h.pan == 0 {
                                0
                            } else {
                                h.vol.clamp(0, 31)
                            },
                            e5: 0x80,
                            pan: h.pan,
                            pcm: true,
                            end: h.end_time,
                            ..NoteEvent::default()
                        };
                        let levels = super::_pcm_levels(h);
                        if !levels.is_empty() {
                            ev.vol_pts = levels;
                        }
                        self.events.push(ev);
                        self.ym2610_events.push((self.events.len() - 1, 0, i as i32));
                        if h.end_time != 0 && h.end_time > h.sample_time {
                            let ch = self.events.last().unwrap().ch;
                            self.events.push(NoteEvent {
                                sample: h.end_time,
                                ch,
                                on: false,
                                note: 180,
                                ins: -1,
                                vol: -1,
                                e5: 0x80,
                                pan: 0,
                                pcm: true,
                                ..NoteEvent::default()
                            });
                        }
                    }
                }
                for i in 0..self.ym2610_b_samples.len() {
                    let smp = self.ym2610_b_samples[i].clone();
                    for h in &smp.hits {
                        self.ym2610_b_count += 1;
                        if h.pan == 0 && !h.pan_pts.iter().any(|p| p.1 != 0) {
                            continue;
                        }
                        let (pitch_steps, vol_pts) = super::_adpcm_b_motion(h);
                        let mut ev = NoteEvent {
                            sample: h.sample_time,
                            ch: ym_base[&(h.chip_id as i32)] + fm_count + 9,
                            on: true,
                            note: 60 + (i as i32 % YM2610_MAP_SLOTS),
                            ins: -1,
                            vol: if h.pan == 0 {
                                0
                            } else {
                                h.vol.clamp(0, 255)
                            },
                            e5: 0x80,
                            pan: h.pan,
                            pcm: true,
                            end: h.end_time,
                            ..NoteEvent::default()
                        };
                        if !pitch_steps.is_empty() {
                            ev.pitch_steps = pitch_steps;
                            ev.e5_lo = 0x00;
                            ev.e5_hi = 0xFF;
                        }
                        if !vol_pts.is_empty() {
                            ev.vol_pts = vol_pts;
                        }
                        self.events.push(ev);
                        self.ym2610_events.push((self.events.len() - 1, 1, i as i32));
                        if h.end_time != 0 && h.end_time > h.sample_time {
                            let ch = self.events.last().unwrap().ch;
                            self.events.push(NoteEvent {
                                sample: h.end_time,
                                ch,
                                on: false,
                                note: 180,
                                ins: -1,
                                vol: -1,
                                e5: 0x80,
                                pan: 0,
                                pcm: true,
                                ..NoteEvent::default()
                            });
                        }
                    }
                }
            } else {
                self.warnings
                    .push("YM2610 ADPCM ROM block missing; sample channels skipped".into());
            }
        }
    }

    fn run_ym2608(&mut self) {
        if !self.is_opna {
            return;
        }
        let vgm = self.vgm;
        let ym_base = self.ym_base.clone();
        let fm_count = self.ym2608_fm_count;
        let (rhythm_hits, rh_warn) = crate::ym2608::collect_ym2608_rhythm(vgm);
        self.warnings.extend(rh_warn);
        let mut rom_b = if self.opts.pcm {
            crate::vgm::assemble_rom(vgm, "ym2608_deltat", 0, 0, false, true)
        } else {
            Vec::new()
        };
        if self.opts.pcm {
            rom_b = crate::ym2608::replay_deltat_memory(vgm, &rom_b);
            let (samples, dt_warn) = crate::ym2608::collect_ym2608_delta_t(vgm, &rom_b);
            self.ym2608_b_samples = samples;
            self.warnings.extend(dt_warn);
        }
        for h in &rhythm_hits {
            self.ym2608_rhythm_count += 1;
            if h.pan == 0 && !h.pan_pts.iter().any(|p| p.1 != 0) {
                continue;
            }
            let mut ev = NoteEvent {
                sample: h.sample_time,
                ch: ym_base[&(h.chip_id as i32)] + fm_count + 3 + h.voice as i32,
                on: true,
                note: 60,
                ins: -1,
                vol: h.vol.clamp(0, 31),
                e5: 0x80,
                pan: h.pan,
                pcm: true,
                end: h.end_time,
                ..NoteEvent::default()
            };
            let levels = super::_pcm_levels(h);
            if !levels.is_empty() {
                ev.vol_pts = levels;
            }
            self.events.push(ev);
            self.ym2608_rhythm_events.push(self.events.len() - 1);
            if h.end_time != 0 && h.end_time > h.sample_time {
                let ch = self.events.last().unwrap().ch;
                self.events.push(NoteEvent {
                    sample: h.end_time,
                    ch,
                    on: false,
                    note: 180,
                    ins: -1,
                    vol: -1,
                    e5: 0x80,
                    pan: 0,
                    pcm: true,
                    ..NoteEvent::default()
                });
            }
        }
        if !rhythm_hits.is_empty() {
            for w in vgm.writes.iter() {
                if w.chip != "ym2608" || (w.reg & 0x100) != 0 || (w.reg & 0xFF) != 0x11 {
                    continue;
                }
                if !ym_base.contains_key(&(w.chip_id as i32)) {
                    continue;
                }
                let target = ym_base[&(w.chip_id as i32)] + fm_count + 3;
                self.channel_fx
                    .push((w.sample, target, 0x1F, (w.val & 0x3F) as i32));
            }
        }
        for i in 0..self.ym2608_b_samples.len() {
            let smp = self.ym2608_b_samples[i].clone();
            for h in &smp.hits {
                self.ym2608_b_count += 1;
                if h.pan == 0 && !h.pan_pts.iter().any(|p| p.1 != 0) {
                    continue;
                }
                let (pitch_steps, vol_pts) = super::_adpcm_b_motion(h);
                let mut ev = NoteEvent {
                    sample: h.sample_time,
                    ch: ym_base[&(h.chip_id as i32)] + fm_count + 9,
                    on: true,
                    note: 60 + (i as i32 % YM2610_MAP_SLOTS),
                    ins: -1,
                    vol: if h.pan == 0 { 0 } else { h.vol.clamp(0, 255) },
                    e5: 0x80,
                    pan: h.pan,
                    pcm: true,
                    end: h.end_time,
                    ..NoteEvent::default()
                };
                if !pitch_steps.is_empty() {
                    ev.pitch_steps = pitch_steps;
                    ev.e5_lo = 0x00;
                    ev.e5_hi = 0xFF;
                }
                if !vol_pts.is_empty() {
                    ev.vol_pts = vol_pts;
                }
                self.events.push(ev);
                self.ym2608_b_events.push((self.events.len() - 1, i as i32));
                if h.end_time != 0 && h.end_time > h.sample_time {
                    let ch = self.events.last().unwrap().ch;
                    self.events.push(NoteEvent {
                        sample: h.end_time,
                        ch,
                        on: false,
                        note: 180,
                        ins: -1,
                        vol: -1,
                        e5: 0x80,
                        pan: 0,
                        pcm: true,
                        ..NoteEvent::default()
                    });
                }
            }
        }
    }

    fn run_y8950(&mut self) {
        if !self.is_opl || self.opl_kind != Some(KIND_Y8950) {
            return;
        }
        let vgm = self.vgm;
        let fm_count = self.opl_fm_count;
        let opl_base = self.opl_base.clone();
        let rom_b = if self.opts.pcm {
            crate::vgm::assemble_rom(vgm, "y8950_deltat", 0, 0, false, true)
        } else {
            Vec::new()
        };
        if self.opts.pcm {
            let (samples, warn) = crate::y8950::collect_y8950_delta_t(vgm, Some(rom_b.as_slice()));
            self.y8950_b_samples = samples;
            self.warnings.extend(warn);
        }
        for i in 0..self.y8950_b_samples.len() {
            let smp = self.y8950_b_samples[i].clone();
            for h in &smp.hits {
                self.y8950_b_count += 1;
                if h.pan == 0 && !h.pan_pts.iter().any(|p| p.1 != 0) {
                    continue;
                }
                let (pitch_steps, vol_pts) = super::_adpcm_b_motion(h);
                let mut ev = NoteEvent {
                    sample: h.sample_time,
                    ch: opl_base.get(&(h.chip_id as i32)).copied().unwrap_or(0) + fm_count,
                    on: true,
                    note: 60 + (i as i32 % YM2610_MAP_SLOTS),
                    ins: -1,
                    vol: h.vol.clamp(0, 255),
                    e5: 0x80,
                    pan: h.pan,
                    pcm: true,
                    end: h.end_time,
                    ..NoteEvent::default()
                };
                if !pitch_steps.is_empty() {
                    ev.pitch_steps = pitch_steps;
                    ev.e5_lo = 0x00;
                    ev.e5_hi = 0xFF;
                }
                if !vol_pts.is_empty() {
                    ev.vol_pts = vol_pts;
                }
                self.events.push(ev);
                self.y8950_b_events.push((self.events.len() - 1, i as i32));
                if h.end_time != 0 && h.end_time > h.sample_time {
                    let ch = self.events.last().unwrap().ch;
                    self.events.push(NoteEvent {
                        sample: h.end_time,
                        ch,
                        on: false,
                        note: 180,
                        ins: -1,
                        vol: -1,
                        e5: 0x80,
                        pan: 0,
                        pcm: true,
                        ..NoteEvent::default()
                    });
                }
            }
        }
    }
}


impl<'a> AnalyzeState<'a> {
    fn run_psg(&mut self) {
        let vgm = self.vgm;
        let psg_dac = crate::psg_dac::collect_psg_dac(vgm, crate::psg_dac::PSG_DAC_RATE, false);
        self.psg_dac = Some(psg_dac);
        if self.psg_base.is_none() {
            return;
        }
        let psg_base = self.psg_base.unwrap();
        let psg_clock = self.psg_clock;
        let psg_at = self.psg_at.clone();
        let psg_t6w28 = self.psg_t6w28;
        let psg_stereo = self.psg_stereo;
        let (psg_hits, psg_offs, psg_warn) =
            crate::sn76489::collect_sn76489(vgm, Some(psg_clock as i64));
        self.warnings.extend(psg_warn);
        let mut psg_events: Vec<NoteEvent> = Vec::new();
        for h in &psg_hits {
            let ch = psg_at.get(&h.chip).copied().unwrap_or(psg_base) + h.voice;
            let hit_clock = crate::sn76489::psg_module_clock(if h.clock != 0 {
                Some(h.clock as f64)
            } else {
                None
            });
            let mut vol_pts: Vec<(i64, i32, Option<i32>)> = Vec::new();
            let tone_div;
            let note;
            let e5;
            let pitch;
            if psg_t6w28 {
                if h.noise {
                    let (n, e) = crate::sn76489::t6w28_noise_note(h.period, psg_clock as i64);
                    note = n;
                    e5 = e;
                    pitch = h.pitch_pts.clone();
                    let fmode = if (h.noise_mode & 4) != 0 { 1 } else { 0 };
                    self.channel_fx.push((h.sample_time, ch, 0x20, fmode));
                    for (t, mode) in &h.mode_pts {
                        self.channel_fx
                            .push((*t, ch, 0x20, if (mode & 4) != 0 { 1 } else { 0 }));
                    }
                    tone_div = 15;
                } else {
                    let (n, e) = crate::sn76489::t6w28_note(h.period, psg_clock as i64);
                    note = n;
                    e5 = e;
                    pitch = h.pitch_pts.clone();
                    tone_div = 16;
                }
                vol_pts = h
                    .mix_pts
                    .iter()
                    .map(|(t, v, pan)| (*t, (*v).clamp(0, 15), Some(*pan)))
                    .collect();
            } else {
                tone_div = crate::sn76489::PSG_DIVIDER;
                if h.noise {
                    let spec = h.noise_mode & 3;
                    if spec == 3 {
                        let (n, e) = crate::sn76489::psg_note(h.period, Some(hit_clock));
                        note = n;
                        e5 = e;
                        pitch = h.pitch_pts.clone();
                    } else {
                        note = 62 - spec;
                        e5 = 0x80;
                        pitch = Vec::new();
                    }
                    let fmode = ((h.noise_mode >> 2) & 1) | if spec == 3 { 0x10 } else { 0 };
                    self.channel_fx.push((h.sample_time, ch, 0x20, fmode));
                    for (t, mode) in &h.mode_pts {
                        let mf = ((mode >> 2) & 1)
                            | if (mode & 3) == 3 { 0x10 } else { 0 };
                        self.channel_fx.push((*t, ch, 0x20, mf));
                    }
                } else {
                    let (n, e) = crate::sn76489::psg_note(h.period, Some(hit_clock));
                    note = n;
                    e5 = e;
                    pitch = h.pitch_pts.clone();
                }
                if psg_stereo {
                    for (t, pan) in &h.pan_pts {
                        if *pan != 0 {
                            self.channel_fx.push((*t, ch, 0x08, *pan));
                        }
                    }
                }
                vol_pts = h
                    .vol_pts
                    .iter()
                    .map(|(t, v)| (*t, (*v).clamp(0, 15), None))
                    .collect();
            }
            psg_events.push(NoteEvent {
                sample: h.sample_time,
                ch,
                on: true,
                note,
                ins: -1,
                vol: h.vol.clamp(0, 15),
                e5,
                pan: h.opened_pan,
                end: h.off_sample,
                vol_pts,
                tone_pitch_pts: pitch,
                tone_chip: 3,
                tone_clock: hit_clock as u32,
                tone_div,
                tone_period_offset: 0,
                legato: h.legato,
                ..NoteEvent::default()
            });
            self.psg_count += 1;
        }
        for off in &psg_offs {
            psg_events.push(NoteEvent {
                sample: off.sample_time,
                ch: psg_at.get(&off.chip).copied().unwrap_or(psg_base) + off.voice,
                on: false,
                note: 180,
                ins: -1,
                vol: -1,
                e5: 0x80,
                pan: 0,
                ..NoteEvent::default()
            });
        }
        psg_events.sort_by_key(|e| (e.sample, if e.on { 1 } else { 0 }));
        self.events.extend(psg_events);
        if self.psg_count == 0 {
            let has_dac = self
                .psg_dac
                .as_ref()
                .map(|d| !d.samples.is_empty())
                .unwrap_or(false);
            if has_dac {
                self.warnings.push(
                    "SN76489 tone voices are silent; the DAC stream is converted as Generic PCM DAC".into(),
                );
            } else {
                self.warnings.push(
                    "SN76489 writes carry no audible note; the PSG system was dropped".into(),
                );
            }
            self.psg_ids.clear();
            self.psg_t6w28 = false;
            self.fm_end -= 4 * (self.psg_at.len() as i32).max(1);
        }
        self.emit_psg_dac();
    }

    /// Python analyze.py 12216-12258: one Generic PCM DAC column per voice,
    /// note 60+body for the 120-slot map, and a cut when the bank body is a
    /// longer variant reused for a shorter attack. Runs after the silent-PSG
    /// reset, so `tone_ch_base()` sees the restored `fm_end`.
    fn emit_psg_dac(&mut self) {
        let Some(psg_dac) = self.psg_dac.clone() else { return };
        let base_ch = self.tone_ch_base();
        let mut at: IndexMap<(i32, i32), i32> = IndexMap::new();
        for (i, key) in psg_dac.voices.iter().enumerate() {
            at.insert(*key, base_ch + i as i32);
        }
        self.psg_dac_events.clear();
        for play in &psg_dac.plays {
            let Some(ch) = at.get(&(play.chip_id, play.voice)).copied() else {
                continue;
            };
            self.events.push(NoteEvent {
                sample: play.sample_time,
                ch,
                on: true,
                note: 60 + (play.body as i32 % 120),
                ins: -1,
                vol: 255,
                e5: 0x80,
                pan: 0,
                pcm: true,
                ..NoteEvent::default()
            });
            self.psg_dac_events
                .push((self.events.len() - 1, play.body as i32));
            if let Some(body) = psg_dac.samples.get(play.body) {
                if play.frames != 0 && body.frames > play.frames {
                    let end_sample = play.sample_time
                        + crate::parity::py_int(crate::parity::py_round(
                            play.frames as f64 * 44100.0 / psg_dac.rate as f64,
                        ));
                    self.events.push(NoteEvent {
                        sample: end_sample,
                        ch,
                        on: false,
                        note: 180,
                        ins: -1,
                        vol: -1,
                        e5: 0x80,
                        pan: 0,
                        pcm: true,
                        ..NoteEvent::default()
                    });
                }
            }
        }
        self.psg_dac_count = self.psg_dac_events.len() as i32;
    }

    fn run_pcm_bank(&mut self) {
        let vgm = self.vgm;
        let (_rom, pcm_samples) = crate::pcm::build_pcm_bank(vgm);
        self.pcm_samples = pcm_samples;
        self.pcm_ids = Vec::new();
        if self.opts.pcm {
            let clock = if vgm.k007232_clock != 0 {
                vgm.k007232_clock
            } else {
                3_579_545
            };
            let audible: Vec<usize> = self
                .pcm_samples
                .iter()
                .enumerate()
                .filter(|(_i, s)| s.length() >= 8)
                .map(|(i, _s)| i)
                .collect();
            self.audible = audible;
            let mut ids: Vec<i32> = self
                .audible
                .iter()
                .map(|i| self.pcm_samples[*i].chip_id as i32)
                .collect();
            ids.sort();
            ids.dedup();
            self.pcm_ids = dual_ids(&ids, vgm.k007232_dual);
            let mut pcm_at: BTreeMap<i32, i32> = BTreeMap::new();
            for (i, cid) in self.pcm_ids.iter().enumerate() {
                pcm_at.insert(*cid, self.fm_end + 2 * i as i32);
            }
            let peak_raw = self
                .audible
                .iter()
                .flat_map(|i| self.pcm_samples[*i].hits.iter())
                .map(|h| h.vol_l.max(h.vol_r))
                .max()
                .unwrap_or(0);
            let vol_scale = if peak_raw > 16 { 255.0 } else { 15.0 };
            let mut by_ch: BTreeMap<(i32, i32), Vec<(usize, usize)>> = BTreeMap::new();
            for (pos, smp_i) in self.audible.iter().enumerate() {
                for (hi, h) in self.pcm_samples[*smp_i].hits.iter().enumerate() {
                    by_ch
                        .entry((self.pcm_samples[*smp_i].chip_id as i32, h.ch))
                        .or_default()
                        .push((pos, hi));
                }
            }
            let mut next_on: BTreeMap<(usize, usize), i64> = BTreeMap::new();
            for hits_for_ch in by_ch.values() {
                let mut hs = hits_for_ch.clone();
                hs.sort_by_key(|(p, hi)| {
                    self.pcm_samples[self.audible[*p]].hits[*hi].sample_time
                });
                for i in 0..hs.len().saturating_sub(1) {
                    let cur = hs[i];
                    let nxt = hs[i + 1];
                    let t = self.pcm_samples[self.audible[nxt.0]].hits[nxt.1].sample_time;
                    next_on.insert(cur, t);
                }
            }
            let total = vgm.total_samples;
            let pcm_ins_base = self.patch_order.len() as i32 + self.n_dac_ins;
            let audible_len = self.audible.len();
            for pos in 0..audible_len {
                let smp_i = self.audible[pos];
                let smp = self.pcm_samples[smp_i].clone();
                let c4p = smp.mode_pitch();
                let ins = pcm_ins_base + pos as i32;
                for (hi, h) in smp.hits.iter().enumerate() {
                    let (note, e5) = crate::pcm::pitch_to_note(h.pitch, c4p, clock);
                    let vl = vol15(h.vol_l, vol_scale);
                    let vr = vol15(h.vol_r, vol_scale);
                    let vol = vl.max(vr);
                    let ch = pcm_at.get(&(smp.chip_id as i32)).copied().unwrap_or(self.fm_end) + h.ch;
                    let vol_pts: Vec<(i64, i32, Option<i32>)> = h
                        .vol_pts
                        .iter()
                        .filter(|(t, _l, _r)| *t > h.sample_time)
                        .map(|(t, l, r)| {
                            (
                                *t,
                                vol15((*l).max(*r), vol_scale),
                                Some(pcm_pan_fx(vol15(*l, vol_scale), vol15(*r, vol_scale))),
                            )
                        })
                        .collect();
                    let fade_in = vol == 0 && vol_pts.iter().any(|(_t, v, _p)| *v > 0);
                    self.events.push(NoteEvent {
                        sample: h.sample_time,
                        ch,
                        on: true,
                        note,
                        ins,
                        vol: if fade_in {
                            0
                        } else if vol != 0 {
                            vol
                        } else {
                            15
                        },
                        e5,
                        pan: if fade_in { 0 } else { pcm_pan_fx(vl, vr) },
                        pcm: true,
                        vol_pts,
                        ..NoteEvent::default()
                    });
                    if !smp.pcm7.is_empty() {
                        let rate = crate::pcm::k007232_rate(clock, h.pitch);
                        let natural = h.sample_time
                            + py_round(
                                smp.pcm7.len() as f64 / rate * VGM_RATE as f64,
                            ) as i64;
                        let nxt = next_on.get(&(pos, hi)).copied();
                        let mut zero_cut: Option<i64> = None;
                        for (i, (t, left, right)) in h.vol_pts.iter().enumerate() {
                            if (*left).max(*right) != 0 {
                                continue;
                            }
                            let after = if i + 1 < h.vol_pts.len() {
                                h.vol_pts[i + 1].0
                            } else {
                                nxt.unwrap_or(natural)
                            };
                            if after - *t >= 64 {
                                zero_cut = Some(*t);
                                break;
                            }
                        }
                        let end;
                        if let Some(z) = zero_cut {
                            end = z;
                        } else if let Some(n) = nxt {
                            if natural >= n {
                                end = 0;
                            } else if !h.loop_ {
                                end = if total != 0 { natural.min(total) } else { natural };
                            } else {
                                end = 0;
                            }
                        } else if !h.loop_ {
                            end = if total != 0 { natural.min(total) } else { natural };
                        } else {
                            end = 0;
                        }
                        if end > h.sample_time {
                            let ch = self.events.last().unwrap().ch;
                            self.events.push(NoteEvent {
                                sample: end,
                                ch,
                                on: false,
                                note: 180,
                                ins: -1,
                                vol: -1,
                                e5: 0x80,
                                pan: 0,
                                pcm: true,
                                ..NoteEvent::default()
                            });
                        }
                    }
                }
            }
            let rom = crate::vgm::assemble_k007232_rom(vgm, 0);
            if rom.is_empty()
                && self
                    .pcm_samples
                    .iter()
                    .any(|s| s.length() >= 8 && !s.hits.is_empty())
            {
                self.warnings.push(
                    "no K007232 ROM block (0x94); PCM notes will be silent until samples are added".into(),
                );
            }
        } else {
            self.pcm_samples = Vec::new();
            self.pcm_ids = Vec::new();
            self.audible = Vec::new();
        }
    }
}


impl<'a> AnalyzeState<'a> {
    fn run_sample_chips(&mut self) {
        if !self.opts.pcm {
            return;
        }
        let vgm = self.vgm;
        let fm_end = self.fm_end;
        let pcm_ids_len = self.pcm_ids.len() as i32;
        let pcm_ins_base = self.patch_order.len() as i32;
        let audible_len = self.audible.len() as i32;
        let (oki_samples, oki_warn) = crate::oki::collect_oki(vgm);
        self.warnings.extend(oki_warn);
        self.oki_samples = oki_samples;
        let (adpcm_samples, adpcm_warn) = crate::oki::collect_msm6258(vgm);
        self.warnings.extend(adpcm_warn);
        self.adpcm_samples = adpcm_samples;
        let sample_base = fm_end + 2 * pcm_ids_len;
        let mut oki_ids: Vec<i32> = Vec::new();
        for smp in &self.oki_samples {
            if !oki_ids.contains(&smp.chip_id) {
                oki_ids.push(smp.chip_id);
            }
        }
        oki_ids = dual_ids(&oki_ids, vgm.msm6295_dual);
        let mut oki_base: BTreeMap<i32, i32> = BTreeMap::new();
        for (i, cid) in oki_ids.iter().enumerate() {
            oki_base.insert(*cid, sample_base + 4 * i as i32);
        }
        let ins0 = pcm_ins_base + audible_len;
        for (i, smp) in self.oki_samples.clone().iter().enumerate() {
            let ins = ins0 + i as i32;
            let dur = smp.data.len() as f64 * 2.0 / smp.rate.max(1) as f64 * VGM_RATE as f64;
            for h in &smp.hits {
                let ch = oki_base[&smp.chip_id] + h.voice;
                self.events.push(NoteEvent {
                    sample: h.sample_time,
                    ch,
                    on: true,
                    note: FURNACE_C4,
                    ins,
                    vol: h.vol,
                    e5: 0x80,
                    pan: 0,
                    pcm: true,
                    no_pan: true,
                    ..NoteEvent::default()
                });
                if h.end != 0 && (h.end as f64 + 64.0) < h.sample_time as f64 + dur {
                    self.events.push(NoteEvent {
                        sample: h.end,
                        ch,
                        on: false,
                        note: 180,
                        ins: -1,
                        vol: -1,
                        e5: 0x80,
                        pan: 0,
                        pcm: true,
                        no_pan: true,
                        ..NoteEvent::default()
                    });
                }
            }
        }
        let adpcm_base = sample_base + 4 * oki_ids.len() as i32;
        let mut adpcm_ids: Vec<i32> = Vec::new();
        for smp in &self.adpcm_samples {
            if !adpcm_ids.contains(&smp.chip_id) {
                adpcm_ids.push(smp.chip_id);
            }
        }
        adpcm_ids = dual_ids(&adpcm_ids, vgm.msm6258_dual);
        let mut adpcm_at: BTreeMap<i32, i32> = BTreeMap::new();
        for (i, cid) in adpcm_ids.iter().enumerate() {
            adpcm_at.insert(*cid, adpcm_base + i as i32);
        }
        let ins1 = ins0 + self.oki_samples.len() as i32;
        let _n_adpcm_ch = adpcm_ids.len() as i32;
        let n_adpcm_ins = crate::oki::msm6258_ins_count(self.adpcm_samples.len() as i32);
        let mut div_now: BTreeMap<i32, Option<i32>> = BTreeMap::new();
        for (i, smp) in self.adpcm_samples.clone().iter().enumerate() {
            let ins = ins1 + (i as i32 / ADPCM_MAP_SLOTS);
            let note = MSM6258_MAP_NOTE + (i as i32 % ADPCM_MAP_SLOTS);
            let ch = adpcm_at[&smp.chip_id];
            let fx = crate::oki::msm6258_div_fx(smp.divider);
            if !div_now.contains_key(&smp.chip_id) {
                div_now.insert(smp.chip_id, fx);
                if let Some(f) = fx {
                    self.channel_fx.push((0, ch, 0x20, f));
                }
            } else if div_now[&smp.chip_id] != fx {
                div_now.insert(smp.chip_id, fx);
                let when = smp.hits.first().map(|h| h.sample_time).unwrap_or(0);
                self.channel_fx
                    .push((when, ch, 0x20, if fx.is_none() { 2 } else { fx.unwrap() }));
            }
            for h in &smp.hits {
                self.events.push(NoteEvent {
                    sample: h.sample_time,
                    ch,
                    on: true,
                    note,
                    ins,
                    vol: 127,
                    e5: 0x80,
                    pan: 0,
                    pcm: true,
                    no_pan: true,
                    ..NoteEvent::default()
                });
                if h.end != 0 && h.end > h.sample_time {
                    self.events.push(NoteEvent {
                        sample: h.end,
                        ch,
                        on: false,
                        note: 180,
                        ins: -1,
                        vol: -1,
                        e5: 0x80,
                        pan: 0,
                        pcm: true,
                        no_pan: true,
                        ..NoteEvent::default()
                    });
                }
            }
        }

        // SegaPCM
        let spcm_clock = if vgm.segapcm_clock != 0 {
            vgm.segapcm_clock
        } else {
            4_000_000
        };
        let (_rom, spcm_samples, spcm_warn) = crate::segapcm::collect_segapcm(vgm);
        self.warnings.extend(spcm_warn);
        self.spcm_audible = spcm_samples.into_iter().filter(|s| s.length() >= 8).collect();
        let mut ids: Vec<i32> = self.spcm_audible.iter().map(|s| s.chip_id).collect();
        ids.sort();
        ids.dedup();
        self.spcm_ids = dual_ids(&ids, vgm.segapcm_dual);
        let spcm_base = adpcm_base + adpcm_ids.len() as i32;
        let mut spcm_at: BTreeMap<i32, i32> = BTreeMap::new();
        for (i, cid) in self.spcm_ids.iter().enumerate() {
            spcm_at.insert(*cid, spcm_base + 16 * i as i32);
        }
        let ins2 = ins1 + n_adpcm_ins;
        let spcm_peak = self
            .spcm_audible
            .iter()
            .flat_map(|s| s.hits.iter())
            .map(|h| h.vol_l.max(h.vol_r))
            .max()
            .unwrap_or(0);
        let spcm_scale = if spcm_peak > 16 { 127.0 } else { 16.0 };
        for (i, smp) in self.spcm_audible.clone().iter().enumerate() {
            let ins = ins2 + i as i32;
            let ref_rate = crate::segapcm::segapcm_rate(spcm_clock, smp.mode_freq());
            for h in &smp.hits {
                let base_f = crate::segapcm::hit_played_freq(h);
                let (note, e5) = crate::pcm::rate_to_note(
                    crate::segapcm::segapcm_rate(spcm_clock, base_f),
                    ref_rate,
                );
                let mut sweep: Vec<(i64, i32)> = Vec::new();
                if !h.freq_pts.is_empty() && base_f != 0 {
                    let deltas: Vec<(i64, i32)> = h
                        .freq_pts
                        .iter()
                        .filter(|(_t, f)| *f != 0)
                        .map(|(t, f)| {
                            (*t, py_round(64.0 * 12.0 * (*f as f64 / base_f as f64).log2()) as i32)
                        })
                        .collect();
                    let mut vals: Vec<i32> = deltas.iter().map(|d| d.1).collect();
                    vals.push(0);
                    if !deltas.is_empty()
                        && *vals.iter().max().unwrap() - *vals.iter().min().unwrap() >= 4
                    {
                        sweep = deltas;
                    }
                }
                let vl = vol15(h.vol_l, spcm_scale);
                let vr = vol15(h.vol_r, spcm_scale);
                let vol = h.vol_l.max(h.vol_r) & 0x7F;
                let pts: Vec<(i64, i32, Option<i32>)> = h
                    .vol_pts
                    .iter()
                    .map(|(t, a, b)| {
                        (
                            *t,
                            (*a).max(*b) & 0x7F,
                            if (*a).max(*b) != 0 {
                                Some(pcm_pan_fx(vol15(*a, spcm_scale), vol15(*b, spcm_scale)))
                            } else {
                                None
                            },
                        )
                    })
                    .collect();
                let fade_in = vol == 0 && pts.iter().any(|p| p.1 > 0);
                self.events.push(NoteEvent {
                    sample: h.sample_time,
                    ch: spcm_at.get(&smp.chip_id).copied().unwrap_or(spcm_base) + h.voice,
                    on: true,
                    note,
                    ins,
                    vol: if fade_in { 0 } else { vol },
                    e5,
                    pan: if fade_in { 0 } else { pcm_pan_fx(vl, vr) },
                    pcm: true,
                    sweep,
                    vol_pts: pts,
                    no_pan: fade_in,
                    ..NoteEvent::default()
                });
            }
        }
        self.run_sample_chips_b(spcm_base);
    }
}


impl<'a> AnalyzeState<'a> {
    fn run_sample_chips_b(&mut self, spcm_base: i32) {
        let vgm = self.vgm;
        let pcm_ins_base = self.patch_order.len() as i32;
        let audible_len = self.audible.len() as i32;
        let n_adpcm_ins = crate::oki::msm6258_ins_count(self.adpcm_samples.len() as i32);
        let ins2 = pcm_ins_base + audible_len + self.oki_samples.len() as i32 + n_adpcm_ins;

        // C140
        let c140_clock = if vgm.c140_clock != 0 { vgm.c140_clock } else { C140_DEFAULT_CLOCK };
        let (_rom, c140_samples, c140_offs, c140_warn) = crate::c140::collect_c140(vgm);
        self.warnings.extend(c140_warn);
        self.c140_audible = c140_samples.into_iter().filter(|s| s.length() >= 8).collect();
        let mut c140_ids: Vec<i32> = Vec::new();
        for smp in &self.c140_audible {
            for h in &smp.hits {
                if !c140_ids.contains(&h.chip_id) {
                    c140_ids.push(h.chip_id);
                }
            }
        }
        c140_ids = dual_ids(&c140_ids, vgm.c140_dual);
        let c140_base = spcm_base + 16 * self.spcm_ids.len() as i32;
        let mut c140_at: BTreeMap<i32, i32> = BTreeMap::new();
        for (i, cid) in c140_ids.iter().enumerate() {
            c140_at.insert(*cid, c140_base + 24 * i as i32);
        }
        let ins3 = ins2 + self.spcm_audible.len() as i32;
        let c140_peak = self
            .c140_audible
            .iter()
            .flat_map(|s| s.hits.iter())
            .map(|h| h.vol_l.max(h.vol_r))
            .max()
            .unwrap_or(0);
        let c140_scale = if c140_peak > 16 { 255.0 } else { 16.0 };
        for (i, smp) in self.c140_audible.clone().iter().enumerate() {
            let ins = ins3 + i as i32;
            let ref_rate = crate::c140::c140_rate(c140_clock, smp.mode_freq());
            for h in &smp.hits {
                let base_f = crate::c140::hit_played_freq(h);
                let steps = crate::c140::merge_step_writes(&h.freq_pts, 600);
                let (note, e5) = crate::pcm::rate_to_note(
                    crate::c140::c140_rate(c140_clock, base_f),
                    ref_rate,
                );
                let mut sweep: Vec<(i64, i32)> = Vec::new();
                if !steps.is_empty() && base_f != 0 {
                    let deltas: Vec<(i64, i32)> = steps
                        .iter()
                        .filter(|(_t, f)| *f != 0)
                        .map(|(t, f)| {
                            (*t, py_round(64.0 * 12.0 * (*f as f64 / base_f as f64).log2()) as i32)
                        })
                        .collect();
                    let mut vals: Vec<i32> = deltas.iter().map(|d| d.1).collect();
                    vals.push(0);
                    if !deltas.is_empty()
                        && *vals.iter().max().unwrap() - *vals.iter().min().unwrap() >= 4
                    {
                        sweep = deltas;
                    }
                }
                let vl = vol15(h.vol_l, c140_scale);
                let vr = vol15(h.vol_r, c140_scale);
                let vol = h.vol_l.max(h.vol_r) & 0xFF;
                let pts: Vec<(i64, i32, Option<i32>)> = h
                    .vol_pts
                    .iter()
                    .map(|(t, a, b)| {
                        (
                            *t,
                            (*a).max(*b) & 0xFF,
                            if (*a).max(*b) != 0 {
                                Some(pcm_pan_fx(vol15(*a, c140_scale), vol15(*b, c140_scale)))
                            } else {
                                None
                            },
                        )
                    })
                    .collect();
                let fade_in = vol == 0 && pts.iter().any(|p| p.1 > 0);
                self.events.push(NoteEvent {
                    sample: h.sample_time,
                    ch: c140_at.get(&h.chip_id).copied().unwrap_or(c140_base) + h.voice,
                    on: true,
                    note,
                    ins,
                    vol: if fade_in {
                        0
                    } else if vol != 0 {
                        vol
                    } else {
                        15
                    },
                    e5,
                    pan: if fade_in { 0 } else { pcm_pan_fx(vl, vr) },
                    pcm: true,
                    end: h.off_sample,
                    sweep,
                    vol_pts: pts,
                    no_pan: fade_in,
                    ..NoteEvent::default()
                });
            }
        }
        for off in &c140_offs {
            let base = match c140_at.get(&off.chip_id) {
                Some(b) => *b,
                None => continue,
            };
            self.events.push(NoteEvent {
                sample: off.sample_time,
                ch: base + off.voice,
                on: false,
                note: 180,
                ins: -1,
                vol: -1,
                e5: 0x80,
                pan: 0,
                pcm: true,
                ..NoteEvent::default()
            });
        }

        // C352
        let c352_clock = if vgm.c352_clock != 0 { vgm.c352_clock } else { C352_DEFAULT_CLOCK };
        let (_rom, c352_samples, c352_offs, c352_warn) =
            crate::c352::collect_c352(vgm, self.opts.c352_quad);
        self.warnings.extend(c352_warn);
        self.c352_audible = c352_samples.into_iter().filter(|s| s.length() >= 8).collect();
        let c352_quad_events = self.opts.c352_quad
            && self.c352_audible.iter().any(|s| {
                s.hits.iter().any(|h| {
                    h.rear_l != 0
                        || h.rear_r != 0
                        || h.quad_pts.iter().any(|pt| pt.3 != 0 || pt.4 != 0)
                })
            });
        self.c352_quad_events = c352_quad_events;
        let mut c352_ids: Vec<i32> = Vec::new();
        for smp in &self.c352_audible {
            for h in &smp.hits {
                if !c352_ids.contains(&h.chip_id) {
                    c352_ids.push(h.chip_id);
                }
            }
        }
        let c352_base = c140_base + 24 * c140_ids.len() as i32;
        let mut c352_at: BTreeMap<i32, i32> = BTreeMap::new();
        for (i, cid) in c352_ids.iter().enumerate() {
            c352_at.insert(*cid, c352_base + 32 * i as i32);
        }
        let ins4 = ins3 + self.c140_audible.len() as i32;
        let c352_peak = self
            .c352_audible
            .iter()
            .flat_map(|s| s.hits.iter())
            .map(|h| {
                let mut m = h.vol_l.max(h.vol_r);
                for (_t, a, b) in &h.vol_pts {
                    m = m.max((*a).max(*b));
                }
                m
            })
            .max()
            .unwrap_or(0);
        let c352_scale = if c352_peak > 16 { 255.0 } else { 16.0 };
        for (i, smp) in self.c352_audible.clone().iter().enumerate() {
            let ins = ins4 + i as i32;
            let ref_rate = crate::c352::c352_rate(c352_clock, smp.mode_freq());
            for h in &smp.hits {
                let base_f = crate::c352::hit_played_freq(h);
                let steps = crate::c352::merge_step_writes(&h.freq_pts, 600);
                let (note, e5) = crate::pcm::rate_to_note(
                    crate::c352::c352_rate(c352_clock, base_f),
                    ref_rate,
                );
                let mut sweep: Vec<(i64, i32)> = Vec::new();
                if !steps.is_empty() && base_f != 0 {
                    let deltas: Vec<(i64, i32)> = steps
                        .iter()
                        .filter(|(_t, f)| *f != 0)
                        .map(|(t, f)| {
                            (*t, py_round(64.0 * 12.0 * (*f as f64 / base_f as f64).log2()) as i32)
                        })
                        .collect();
                    let mut vals: Vec<i32> = deltas.iter().map(|d| d.1).collect();
                    vals.push(0);
                    if !deltas.is_empty()
                        && *vals.iter().max().unwrap() - *vals.iter().min().unwrap() >= 4
                    {
                        sweep = deltas;
                    }
                }
                let mut init_l = h.vol_l;
                let mut init_r = h.vol_r;
                for (t, a, b) in &h.vol_pts {
                    if *t - h.sample_time > crate::c352::C352_REFIT_SAMPLES {
                        break;
                    }
                    init_l = *a;
                    init_r = *b;
                }
                let vl = vol15(init_l, c352_scale);
                let vr = vol15(init_r, c352_scale);
                let mut vol = init_l.max(init_r) & 0xFF;
                let mut pts: Vec<(i64, i32, Option<i32>)> = h
                    .vol_pts
                    .iter()
                    .map(|(t, a, b)| {
                        (
                            *t,
                            (*a).max(*b) & 0xFF,
                            if (*a).max(*b) != 0 {
                                Some(pcm_pan_fx(vol15(*a, c352_scale), vol15(*b, c352_scale)))
                            } else {
                                None
                            },
                        )
                    })
                    .collect();
                let mut fade_in = vol == 0 && pts.iter().any(|p| p.1 > 0);
                let mut quad: Option<(i32, i32, i32, i32)> = None;
                let mut quad_pts: Vec<(i64, i32, i32, i32, i32)> = Vec::new();
                if c352_quad_events {
                    let mut four = (h.front_l, h.front_r, h.rear_l, h.rear_r);
                    for pt in &h.quad_pts {
                        if pt.0 - h.sample_time > crate::c352::C352_REFIT_SAMPLES {
                            break;
                        }
                        four = (pt.1, pt.2, pt.3, pt.4);
                    }
                    quad = Some(four);
                    quad_pts = h.quad_pts.clone();
                    vol = 255;
                    pts = Vec::new();
                    fade_in = false;
                }
                self.events.push(NoteEvent {
                    sample: h.sample_time,
                    ch: c352_at.get(&h.chip_id).copied().unwrap_or(c352_base) + h.voice as i32,
                    on: true,
                    note,
                    ins,
                    vol: if fade_in { 0 } else if vol != 0 { vol } else { 15 },
                    e5,
                    pan: if fade_in { 0 } else { pcm_pan_fx(vl, vr) },
                    pcm: true,
                    end: h.off_sample,
                    sweep,
                    vol_pts: pts,
                    no_pan: fade_in,
                    quad,
                    quad_pts,
                    ..NoteEvent::default()
                });
            }
        }
        for off in &c352_offs {
            let base = match c352_at.get(&off.chip_id) {
                Some(b) => *b,
                None => continue,
            };
            self.events.push(NoteEvent {
                sample: off.sample_time,
                ch: base + off.voice as i32,
                on: false,
                note: 180,
                ins: -1,
                vol: -1,
                e5: 0x80,
                pan: 0,
                pcm: true,
                ..NoteEvent::default()
            });
        }

        // K053260
        let k053260_clock = if vgm.k053260_clock != 0 { vgm.k053260_clock } else { 3_579_545 };
        let (_rom, k053260_samples, k053260_offs, k053260_warn) =
            crate::k053260::collect_k053260(vgm);
        self.warnings.extend(k053260_warn);
        self.k053260_audible = k053260_samples.into_iter().filter(|s| s.length() >= 8).collect();
        let mut k053260_ids: Vec<i32> = Vec::new();
        for smp in &self.k053260_audible {
            for h in &smp.hits {
                if !k053260_ids.contains(&h.chip_id) {
                    k053260_ids.push(h.chip_id);
                }
            }
        }
        k053260_ids = dual_ids(&k053260_ids, vgm.k053260_dual);
        let k053260_base = c352_base + 32 * c352_ids.len() as i32;
        let mut k053260_at: BTreeMap<i32, i32> = BTreeMap::new();
        for (i, cid) in k053260_ids.iter().enumerate() {
            k053260_at.insert(*cid, k053260_base + 4 * i as i32);
        }
        let ins5 = ins4 + self.c352_audible.len() as i32;
        for (i, smp) in self.k053260_audible.clone().iter().enumerate() {
            let ins = ins5 + i as i32;
            let ref_rate = crate::k053260::k053260_rate(k053260_clock, smp.mode_pitch());
            for h in &smp.hits {
                let base_pitch = crate::k053260::hit_played_pitch(h);
                let steps = crate::k053260::merge_step_writes(&h.freq_pts, 600);
                let (note, e5) = crate::pcm::rate_to_note(
                    crate::k053260::k053260_rate(k053260_clock, base_pitch),
                    ref_rate,
                );
                let mut sweep: Vec<(i64, i32)> = Vec::new();
                if !steps.is_empty() && base_pitch != 0 {
                    let base_rate = crate::k053260::k053260_rate(k053260_clock, base_pitch);
                    let deltas: Vec<(i64, i32)> = steps
                        .iter()
                        .filter(|(_t, f)| *f != 0)
                        .map(|(t, f)| {
                            (
                                *t,
                                py_round(
                                    64.0 * 12.0
                                        * (crate::k053260::k053260_rate(k053260_clock, *f)
                                            / base_rate)
                                            .log2(),
                                ) as i32,
                            )
                        })
                        .collect();
                    let mut vals: Vec<i32> = deltas.iter().map(|d| d.1).collect();
                    vals.push(0);
                    if !deltas.is_empty()
                        && *vals.iter().max().unwrap() - *vals.iter().min().unwrap() >= 4
                    {
                        sweep = deltas;
                    }
                }
                let (init_vol, init_pan) = crate::k053260::hit_played_volume(h);
                let mut pts: Vec<(i64, i32, Option<i32>)> = Vec::new();
                let mut last_pan_fx =
                    if init_pan == 0 { None } else { Some(crate::k053260::k053260_pan_fx(init_pan)) };
                for (t, vol_now, pan_now) in &h.state_pts {
                    if *t - h.sample_time <= crate::k053260::K053260_REFIT_SAMPLES {
                        continue;
                    }
                    if *pan_now == 0 {
                        pts.push((*t, 0, None));
                        last_pan_fx = None;
                        continue;
                    }
                    let fx = crate::k053260::k053260_pan_fx(*pan_now);
                    pts.push((*t, vol_now & 0x7F, if Some(fx) != last_pan_fx { Some(fx) } else { None }));
                    last_pan_fx = Some(fx);
                }
                let pan_zero = init_pan == 0;
                self.events.push(NoteEvent {
                    sample: h.sample_time,
                    ch: k053260_at.get(&h.chip_id).copied().unwrap_or(k053260_base) + h.voice as i32,
                    on: true,
                    note,
                    ins,
                    vol: if pan_zero { 0 } else { init_vol & 0x7F },
                    e5,
                    pan: if pan_zero { 0 } else { crate::k053260::k053260_pan_fx(init_pan) },
                    pcm: true,
                    end: h.off_sample,
                    sweep,
                    vol_pts: pts,
                    no_pan: pan_zero,
                    ..NoteEvent::default()
                });
            }
        }
        for off in &k053260_offs {
            let base = match k053260_at.get(&off.chip_id) {
                Some(b) => *b,
                None => continue,
            };
            self.events.push(NoteEvent {
                sample: off.sample_time,
                ch: base + off.voice as i32,
                on: false,
                note: 180,
                ins: -1,
                vol: -1,
                e5: 0x80,
                pan: 0,
                pcm: true,
                ..NoteEvent::default()
            });
        }
        self.run_sample_chips_c(k053260_base, k053260_ids, ins5);
    }
}


impl<'a> AnalyzeState<'a> {
    fn run_sample_chips_c(&mut self, k053260_base: i32, k053260_ids: Vec<i32>, ins5: i32) {
        let vgm = self.vgm;
        // RF5C68 / RF5C164
        let rf5c68_clock = if vgm.rf5c68_clock != 0 { vgm.rf5c68_clock } else { RF5C68_DEFAULT_CLOCK };
        let rf5c164_clock = if vgm.rf5c164_clock != 0 { vgm.rf5c164_clock } else { RF5C164_DEFAULT_CLOCK };
        let mut rf5c68_samples = Vec::new();
        let mut rf5c68_offs = Vec::new();
        let mut ids68: Vec<i32> = vgm
            .writes
            .iter()
            .filter(|w| w.chip == "rf5c68")
            .map(|w| w.chip_id as i32)
            .collect();
        ids68.sort();
        ids68.dedup();
        for cid in ids68 {
            let (_ram, samples, offs, warn) =
                crate::rf5c::collect_rf5c(vgm, "rf5c68", Some(rf5c68_clock), cid);
            rf5c68_samples.extend(samples);
            rf5c68_offs.extend(offs);
            self.warnings.extend(warn);
        }
        let mut rf5c164_samples = Vec::new();
        let mut rf5c164_offs = Vec::new();
        let mut ids164: Vec<i32> = vgm
            .writes
            .iter()
            .filter(|w| w.chip == "rf5c164")
            .map(|w| w.chip_id as i32)
            .collect();
        ids164.sort();
        ids164.dedup();
        for cid in ids164 {
            let (_ram, samples, offs, warn) =
                crate::rf5c::collect_rf5c(vgm, "rf5c164", Some(rf5c164_clock), cid);
            rf5c164_samples.extend(samples);
            rf5c164_offs.extend(offs);
            self.warnings.extend(warn);
        }
        self.rf5c68_audible = rf5c68_samples.into_iter().filter(|s| s.length() >= 8).collect();
        self.rf5c164_audible = rf5c164_samples.into_iter().filter(|s| s.length() >= 8).collect();
        let mut ids: Vec<i32> = self.rf5c68_audible.iter().map(|s| s.chip_id).collect();
        ids.sort();
        ids.dedup();
        self.rf5c68_ids = dual_ids(&ids, vgm.rf5c68_dual);
        let mut ids: Vec<i32> = self.rf5c164_audible.iter().map(|s| s.chip_id).collect();
        ids.sort();
        ids.dedup();
        self.rf5c164_ids = dual_ids(&ids, vgm.rf5c164_dual);
        let rf5c_base = k053260_base + 4 * k053260_ids.len() as i32;
        let ins6 = ins5 + self.k053260_audible.len() as i32;
        let mut rf5c68_at: IndexMap<i32, i32> = IndexMap::new();
        for (i, cid) in self.rf5c68_ids.iter().enumerate() {
            rf5c68_at.insert(*cid, rf5c_base + RF5C_VOICES * i as i32);
        }
        let mut rf5c164_at: IndexMap<i32, i32> = IndexMap::new();
        for (i, cid) in self.rf5c164_ids.iter().enumerate() {
            rf5c164_at.insert(
                *cid,
                rf5c_base + RF5C_VOICES * (self.rf5c68_ids.len() + i) as i32,
            );
        }
        self.emit_rf5c(&self.rf5c68_audible.clone(), rf5c68_clock, &rf5c68_at, ins6, &rf5c68_offs);
        self.emit_rf5c(
            &self.rf5c164_audible.clone(),
            rf5c164_clock,
            &rf5c164_at,
            ins6 + self.rf5c68_audible.len() as i32,
            &rf5c164_offs,
        );

        // YMZ280B
        let (ymz_samples, ymz_offs, ymz_warn) = crate::ymz280b::collect_ymz280b(vgm);
        self.warnings.extend(ymz_warn);
        self.ymz_audible = ymz_samples
            .into_iter()
            .filter(|s| !s.data.is_empty() && !s.hits.is_empty())
            .collect();
        let mut ymz_ids: Vec<i32> = self.ymz_audible.iter().map(|s| s.chip_id).collect();
        ymz_ids.sort();
        ymz_ids.dedup();
        self.ymz_ids = dual_ids(&ymz_ids, vgm.ymz280b_dual);
        let ymz_base = self.fm_end
            + 2 * self.pcm_ids.len() as i32
            + 4 * self.oki_samples.iter().map(|s| s.chip_id).collect::<BTreeSet<_>>().len() as i32
            + self.adpcm_samples.iter().map(|s| s.chip_id).collect::<BTreeSet<_>>().len() as i32
            + 16 * self.spcm_ids.len() as i32
            + RF5C_VOICES * self.rf5c68_ids.len() as i32
            + RF5C_VOICES * self.rf5c164_ids.len() as i32
            + 24 * self.c140_audible.iter().flat_map(|s| s.hits.iter()).map(|h| h.chip_id).collect::<BTreeSet<_>>().len() as i32
            + 32 * self.c352_audible.iter().flat_map(|s| s.hits.iter()).map(|h| h.chip_id).collect::<BTreeSet<_>>().len() as i32
            + 4 * self.k053260_audible.iter().flat_map(|s| s.hits.iter()).map(|h| h.chip_id).collect::<BTreeSet<_>>().len() as i32;
        let mut ymz_at: BTreeMap<i32, i32> = BTreeMap::new();
        for (i, cid) in self.ymz_ids.iter().enumerate() {
            ymz_at.insert(*cid, ymz_base + YMZ280B_VOICES * i as i32);
        }
        for (i, smp) in self.ymz_audible.clone().iter().enumerate() {
            let ref_rate = smp.c4_rate();
            for h in &smp.hits {
                let base_f = crate::ymz280b::hit_played_fnum(h);
                let clock = smp.clock;
                let (note, e5) = crate::pcm::rate_to_note(
                    crate::ymz280b::ymz280b_rate(clock, base_f, smp.depth),
                    ref_rate as f64,
                );
                let mut sweep: Vec<(i64, i32)> = Vec::new();
                if !h.freq_pts.is_empty() && base_f != 0 {
                    let base_rate =
                        crate::ymz280b::ymz280b_rate(clock, base_f, smp.depth);
                    let deltas: Vec<(i64, i32)> = h
                        .freq_pts
                        .iter()
                        .filter(|(_t, f)| *f != 0)
                        .map(|(t, f)| {
                            (
                                *t,
                                py_round(
                                    64.0 * 12.0
                                        * (crate::ymz280b::ymz280b_rate(clock, *f, smp.depth)
                                            / base_rate)
                                            .log2(),
                                ) as i32,
                            )
                        })
                        .collect();
                    let mut vals: Vec<i32> = deltas.iter().map(|d| d.1).collect();
                    vals.push(0);
                    if !deltas.is_empty()
                        && *vals.iter().max().unwrap() - *vals.iter().min().unwrap() >= 4
                    {
                        sweep = deltas;
                    }
                }
                let pts: Vec<(i64, i32, Option<i32>)> =
                    h.vol_pts.iter().map(|(t, v)| (*t, v & 0xFF, None)).collect();
                let natural_end = h.sample_time
                    + py_round(
                        smp.data.len() as f64
                            / crate::ymz280b::ymz280b_rate(clock, base_f, smp.depth)
                            * VGM_RATE as f64,
                    ) as i64;
                let end = if smp.loop_start.is_none() {
                    h.off_sample.min(natural_end)
                } else {
                    h.off_sample
                };
                let ev = NoteEvent {
                    sample: h.sample_time,
                    ch: ymz_at.get(&h.chip_id).copied().unwrap_or(ymz_base) + h.voice as i32,
                    on: true,
                    note,
                    ins: -1,
                    vol: h.vol & 0xFF,
                    e5,
                    pan: crate::ymz280b::ymz_pan_fx(h.pan),
                    pcm: true,
                    end,
                    sweep,
                    vol_pts: pts,
                    ..NoteEvent::default()
                };
                self.events.push(ev);
                self.ymz_events.push((self.events.len() - 1, i as i32));
                self.ymz_count += 1;
                let ch = self.events.last().unwrap().ch;
                for (t, pan) in &h.pan_pts {
                    self.channel_fx
                        .push((*t, ch, 0x08, crate::ymz280b::ymz_pan_fx(*pan)));
                }
            }
        }
        for off in &ymz_offs {
            let base = match ymz_at.get(&off.chip_id) {
                Some(b) => *b,
                None => continue,
            };
            self.events.push(NoteEvent {
                sample: off.sample_time,
                ch: base + off.voice as i32,
                on: false,
                note: 180,
                ins: -1,
                vol: -1,
                e5: 0x80,
                pan: 0,
                pcm: true,
                ..NoteEvent::default()
            });
        }
        self.run_sample_chips_d(ymz_base);
    }

    fn emit_rf5c(
        &mut self,
        samples: &[crate::rf5c::Rf5cSample],
        clock: u32,
        at: &IndexMap<i32, i32>,
        ins_base: i32,
        offs: &[crate::rf5c::Rf5cOff],
    ) {
        for (i, smp) in samples.iter().enumerate() {
            let ins = ins_base + i as i32;
            let ref_rate = crate::rf5c::rf5c_rate(clock, smp.mode_step());
            for h in &smp.hits {
                let (note, e5) =
                    crate::pcm::rate_to_note(crate::rf5c::rf5c_rate(clock, h.step), ref_rate);
                let mut sweep: Vec<(i64, i32)> = Vec::new();
                if !h.step_pts.is_empty() && h.step != 0 {
                    let deltas: Vec<(i64, i32)> = h
                        .step_pts
                        .iter()
                        .filter(|(_t, s)| *s != 0)
                        .map(|(t, step)| {
                            (
                                *t,
                                py_round(
                                    64.0 * 12.0
                                        * (((*step & 0xFFFF) as f64) / h.step as f64).log2(),
                                ) as i32,
                            )
                        })
                        .collect();
                    let mut vals: Vec<i32> = deltas.iter().map(|d| d.1).collect();
                    vals.push(0);
                    if !deltas.is_empty()
                        && *vals.iter().max().unwrap() - *vals.iter().min().unwrap() >= 4
                    {
                        sweep = deltas;
                    }
                }
                let left = h.pan & 0x0F;
                let right = (h.pan >> 4) & 0x0F;
                let peak = left.max(right);
                let vol = py_round(h.env as f64 * peak as f64 / 15.0) as i32 & 0xFF;
                self.events.push(NoteEvent {
                    sample: h.sample_time,
                    ch: at.get(&smp.chip_id).copied().unwrap_or(0) + h.voice,
                    on: true,
                    note,
                    ins,
                    vol,
                    e5,
                    pan: pcm_pan_fx(left, right),
                    pcm: true,
                    sweep,
                    ..NoteEvent::default()
                });
            }
        }
        for off in offs {
            let base = at.get(&off.chip_id).copied().unwrap_or(0);
            self.events.push(NoteEvent {
                sample: off.sample_time,
                ch: base + off.voice,
                on: false,
                note: 180,
                ins: -1,
                vol: -1,
                e5: 0x80,
                pan: 0,
                pcm: true,
                ..NoteEvent::default()
            });
        }
    }
}


impl<'a> AnalyzeState<'a> {
    fn run_sample_chips_d(&mut self, ymz_base: i32) {
        let vgm = self.vgm;
        // MultiPCM
        let (multipcm_samples, multipcm_warn) = crate::multipcm::collect_multipcm(vgm);
        self.warnings.extend(multipcm_warn);
        self.multipcm_audible = multipcm_samples
            .into_iter()
            .filter(|s| !s.hits.is_empty() && !s.data.is_empty())
            .collect();
        let mut ids: Vec<i32> = self.multipcm_audible.iter().map(|s| s.chip_id).collect();
        ids.sort();
        ids.dedup();
        if ids.is_empty() && vgm.multipcm_clock != 0 {
            ids.push(0);
        }
        self.multipcm_ids = dual_ids(&ids, vgm.multipcm_dual);
        let multipcm_base = ymz_base + YMZ280B_VOICES * self.ymz_ids.len() as i32;
        let mut multipcm_at: BTreeMap<i32, i32> = BTreeMap::new();
        for (i, cid) in self.multipcm_ids.iter().enumerate() {
            multipcm_at.insert(*cid, multipcm_base + MULTIPCM_VOICES * i as i32);
        }
        for (i, smp) in self.multipcm_audible.clone().iter().enumerate() {
            let ref_rate = smp.c4_rate();
            for h in &smp.hits {
                let base_rate = crate::multipcm::played_rate(smp.clock, h.octave, h.fnum);
                let (note, e5) = crate::pcm::rate_to_note(base_rate, ref_rate as f64);
                let mut sweep: Vec<(i64, i32)> = Vec::new();
                if !h.freq_pts.is_empty() && base_rate != 0.0 {
                    let deltas: Vec<(i64, i32)> = h
                        .freq_pts
                        .iter()
                        .filter(|(_t, o, f)| crate::multipcm::played_rate(smp.clock, *o, *f) != 0.0)
                        .map(|(t, o, f)| {
                            (
                                *t,
                                py_round(
                                    64.0 * 12.0
                                        * (crate::multipcm::played_rate(smp.clock, *o, *f)
                                            / base_rate)
                                            .log2(),
                                ) as i32,
                            )
                        })
                        .collect();
                    let mut vals: Vec<i32> = deltas.iter().map(|d| d.1).collect();
                    vals.push(0);
                    if !deltas.is_empty()
                        && *vals.iter().max().unwrap() - *vals.iter().min().unwrap() >= 4
                    {
                        sweep = deltas;
                    }
                }
                let mut vol = (127 - h.tl).clamp(0, 127);
                if h.pan == 8 {
                    vol = 0;
                }
                let pts: Vec<(i64, i32, Option<i32>)> = h
                    .vol_pts
                    .iter()
                    .map(|(t, tl, pan)| {
                        let pan_fx = crate::model::multipcm_pan_fx(*pan);
                        (
                            *t,
                            if *pan == 8 { 0 } else { (127 - *tl).clamp(0, 127) },
                            if pan_fx == 0 { None } else { Some(pan_fx) },
                        )
                    })
                    .collect();
                let end = if smp.loop_start.is_none() {
                    let natural_end = h.sample_time
                        + py_round(
                            smp.frames as f64
                                / if base_rate != 0.0 { base_rate } else { ref_rate as f64 }
                                * VGM_RATE as f64,
                        ) as i64;
                    h.off_sample.min(natural_end)
                } else {
                    h.off_sample
                };
                let ev = NoteEvent {
                    sample: h.sample_time,
                    ch: multipcm_at.get(&h.chip_id).copied().unwrap_or(multipcm_base)
                        + h.voice,
                    on: true,
                    note,
                    ins: -1,
                    vol,
                    e5,
                    pan: crate::model::multipcm_pan_fx(h.pan),
                    pcm: true,
                    end,
                    sweep,
                    vol_pts: pts,
                    ..NoteEvent::default()
                };
                self.events.push(ev);
                self.multipcm_events.push((self.events.len() - 1, i as i32));
                self.multipcm_count += 1;
                let ch = self.events.last().unwrap().ch;
                if end != 0 && end < vgm.total_samples {
                    self.events.push(NoteEvent {
                        sample: end,
                        ch,
                        on: false,
                        note: 180,
                        ins: -1,
                        vol: -1,
                        e5: 0x80,
                        pan: 0,
                        pcm: true,
                        ..NoteEvent::default()
                    });
                }
                for (t, code, value) in &h.lfo_pts {
                    self.channel_fx.push((*t, ch, *code, value & 0xFF));
                }
            }
        }

        // uPD7759
        let (upd_samples, upd_warn) =
            crate::upd7759::collect_upd7759(vgm, self.opts.lossy_samples).unwrap_or_default();
        self.warnings.extend(upd_warn);
        self.upd_audible = upd_samples
            .into_iter()
            .filter(|s| !s.hits.is_empty() && s.frames() > 0)
            .collect();
        let mut ids: Vec<i32> = self.upd_audible.iter().map(|s| s.chip_id).collect();
        ids.sort();
        ids.dedup();
        if ids.is_empty() && vgm.upd7759_clock != 0 {
            ids.push(0);
        }
        self.upd_ids = dual_ids(&ids, vgm.upd7759_dual);
        let upd_base = multipcm_base + MULTIPCM_VOICES * self.multipcm_ids.len() as i32;
        let mut upd_at: BTreeMap<i32, i32> = BTreeMap::new();
        for (i, cid) in self.upd_ids.iter().enumerate() {
            upd_at.insert(*cid, upd_base + i as i32);
        }
        let n_upd_ins = (self.upd_audible.len() as i32 + 119) / 120;
        for (i, smp) in self.upd_audible.clone().iter().enumerate() {
            let slot = (i as i32) % 120;
            let ins_slot = (i as i32) / 120;
            for h in &smp.hits {
                let ev = NoteEvent {
                    sample: h.sample_time,
                    ch: upd_at.get(&h.chip_id).copied().unwrap_or(upd_base),
                    on: true,
                    note: 60 + slot,
                    ins: -1,
                    vol: 255,
                    e5: 0x80,
                    pan: 0,
                    pcm: true,
                    end: h.end,
                    ..NoteEvent::default()
                };
                self.events.push(ev);
                self.upd_events.push((self.events.len() - 1, ins_slot));
                self.upd_count += 1;
                let ch = self.events.last().unwrap().ch;
                if h.end != 0 && h.end < vgm.total_samples {
                    self.events.push(NoteEvent {
                        sample: h.end,
                        ch,
                        on: false,
                        note: 180,
                        ins: -1,
                        vol: -1,
                        e5: 0x80,
                        pan: 0,
                        pcm: true,
                        ..NoteEvent::default()
                    });
                }
            }
        }

        // PWM
        let (pwm_samples, pwm_warn) = crate::pwm::collect_pwm(vgm, self.opts.lossy_samples);
        self.warnings.extend(pwm_warn);
        self.pwm_audible = pwm_samples.into_iter().filter(|s| s.frames() > 0).collect();
        let mut ids: Vec<i32> = self.pwm_audible.iter().map(|s| s.chip_id as i32).collect();
        ids.sort();
        ids.dedup();
        if ids.is_empty() && vgm.pwm_clock != 0 {
            ids.push(0);
        }
        self.pwm_ids = dual_ids(&ids, vgm.pwm_dual);
        let pwm_base = upd_base + self.upd_ids.len() as i32;
        let mut pwm_at: BTreeMap<i32, i32> = BTreeMap::new();
        for (i, cid) in self.pwm_ids.iter().enumerate() {
            pwm_at.insert(*cid, pwm_base + PWM_SIDES * i as i32);
        }
        let n_pwm_ins = (self.pwm_audible.len() as i32 + 119) / 120;
        for (i, smp) in self.pwm_audible.clone().iter().enumerate() {
            let slot = (i as i32) % 120;
            let ins_slot = (i as i32) / 120;
            let duration = py_round(smp.frames() as f64 / smp.rate as f64 * VGM_RATE as f64) as i64;
            for hit in &smp.hits {
                let start = hit.time;
                let end = (start + duration).min(if vgm.total_samples != 0 {
                    vgm.total_samples
                } else {
                    start + duration
                });
                let ev = NoteEvent {
                    sample: start,
                    ch: pwm_at.get(&(smp.chip_id as i32)).copied().unwrap_or(pwm_base) + hit.side,
                    on: true,
                    note: 60 + slot,
                    ins: -1,
                    vol: 255,
                    e5: 0x80,
                    pan: if hit.side == 0 { 0xF0 } else { 0x0F },
                    pcm: true,
                    end,
                    ..NoteEvent::default()
                };
                self.events.push(ev);
                self.pwm_events.push((self.events.len() - 1, ins_slot));
                self.pwm_count += 1;
                let ch = self.events.last().unwrap().ch;
                if end != 0 && end < vgm.total_samples {
                    self.events.push(NoteEvent {
                        sample: end,
                        ch,
                        on: false,
                        note: 180,
                        ins: -1,
                        vol: -1,
                        e5: 0x80,
                        pan: 0,
                        pcm: true,
                        ..NoteEvent::default()
                    });
                }
            }
        }

        // MSM5205
        let (msm5205_audible, msm5205_warn) =
            crate::msm5205::collect_msm5205(vgm, self.opts.lossy_samples);
        self.warnings.extend(msm5205_warn);
        self.msm5205_audible = msm5205_audible;
        let mut ids: Vec<i32> = self.msm5205_audible.iter().map(|s| s.chip_id).collect();
        ids.sort();
        ids.dedup();
        if ids.is_empty() && vgm.msm5205_clock != 0 {
            ids.push(0);
        }
        self.msm5205_ids = dual_ids(&ids, vgm.msm5205_dual);
        let msm5205_base = pwm_base + PWM_SIDES * self.pwm_ids.len() as i32;
        let mut msm5205_at: BTreeMap<i32, i32> = BTreeMap::new();
        for (i, cid) in self.msm5205_ids.iter().enumerate() {
            msm5205_at.insert(*cid, msm5205_base + i as i32);
        }
        let n_msm5205_ins = (self.msm5205_audible.len() as i32 + 119) / 120;
        for (i, smp) in self.msm5205_audible.clone().iter().enumerate() {
            let slot = (i as i32) % 120;
            let ins_slot = (i as i32) / 120;
            let duration = smp.frames();
            for hit in &smp.hits {
                let start = hit.time;
                let end = (start + duration).min(if vgm.total_samples != 0 {
                    vgm.total_samples
                } else {
                    start + duration
                });
                let ev = NoteEvent {
                    sample: start,
                    ch: msm5205_at.get(&smp.chip_id).copied().unwrap_or(msm5205_base),
                    on: true,
                    note: 60 + slot,
                    ins: -1,
                    vol: 255,
                    e5: 0x80,
                    pan: 0,
                    pcm: true,
                    end,
                    ..NoteEvent::default()
                };
                self.events.push(ev);
                self.msm5205_events.push((self.events.len() - 1, ins_slot));
                self.msm5205_count += 1;
                let ch = self.events.last().unwrap().ch;
                if end != 0 && end < vgm.total_samples {
                    self.events.push(NoteEvent {
                        sample: end,
                        ch,
                        on: false,
                        note: 180,
                        ins: -1,
                        vol: -1,
                        e5: 0x80,
                        pan: 0,
                        pcm: true,
                        ..NoteEvent::default()
                    });
                }
            }
        }
        self.run_sample_chips_e(msm5205_base, n_msm5205_ins, n_upd_ins, n_pwm_ins, pwm_base);
    }
}


impl<'a> AnalyzeState<'a> {
    fn run_sample_chips_e(
        &mut self,
        msm5205_base: i32,
        n_msm5205_ins: i32,
        n_upd_ins: i32,
        n_pwm_ins: i32,
        pwm_base: i32,
    ) {
        let vgm = self.vgm;
        let _ = (n_msm5205_ins, n_upd_ins, n_pwm_ins);
        // GA20
        let ga20_clock = if vgm.ga20_clock != 0 { vgm.ga20_clock } else { GA20_DEFAULT_CLOCK };
        let (ga20_samples, ga20_offs, ga20_warn) = crate::ga20::collect_ga20(vgm);
        self.warnings.extend(ga20_warn);
        self.ga20_samples = ga20_samples;
        let mut ids: Vec<i32> = self.ga20_samples.iter().map(|s| s.chip_id).collect();
        ids.sort();
        ids.dedup();
        if ids.is_empty() && vgm.ga20_clock != 0 {
            ids.push(0);
        }
        self.ga20_ids = dual_ids(&ids, vgm.ga20_dual);
        let ga20_base = msm5205_base + self.msm5205_ids.len() as i32;
        let mut ga20_at: BTreeMap<i32, i32> = BTreeMap::new();
        for (i, cid) in self.ga20_ids.iter().enumerate() {
            ga20_at.insert(*cid, ga20_base + 4 * i as i32);
        }
        for (i, smp) in self.ga20_samples.clone().iter().enumerate() {
            for h in &smp.hits {
                let (note, e5) = crate::ga20::ga20_note(h, smp, ga20_clock);
                let pts: Vec<(i64, i32, Option<i32>)> = h
                    .vol_pts
                    .iter()
                    .map(|(t, v)| (*t, (*v).clamp(0, 255), None))
                    .collect();
                let ev = NoteEvent {
                    sample: h.sample_time,
                    ch: ga20_at.get(&h.chip_id).copied().unwrap_or(ga20_base) + h.voice as i32,
                    on: true,
                    note,
                    ins: -1,
                    vol: h.volume.clamp(0, 255),
                    e5,
                    pan: 0,
                    pcm: true,
                    end: h.off_sample,
                    vol_pts: pts,
                    no_pan: true,
                    ..NoteEvent::default()
                };
                self.events.push(ev);
                self.ga20_events.push((self.events.len() - 1, i as i32));
                self.ga20_count += 1;
            }
        }
        for off in &ga20_offs {
            let base = match ga20_at.get(&off.chip_id) {
                Some(b) => *b,
                None => continue,
            };
            self.events.push(NoteEvent {
                sample: off.sample_time,
                ch: base + off.voice as i32,
                on: false,
                note: 180,
                ins: -1,
                vol: -1,
                e5: 0x80,
                pan: 0,
                pcm: true,
                ..NoteEvent::default()
            });
        }

        // C219
        let c219_clock = if vgm.c140_clock != 0 { vgm.c140_clock } else { C219_DEFAULT_CLOCK };
        let (c219_collected, c219_offs, c219_warn) = crate::c219::collect_c219(vgm);
        self.warnings.extend(c219_warn);
        let c219_layout = if c219_collected.is_empty() {
            Vec::new()
        } else {
            crate::c219::layout_c219_samples(&c219_collected)
        };
        self.warnings.extend(crate::c219::c219_layout_warnings(&c219_layout));
        self.c219_samples = c219_layout;
        let c219_real: Vec<usize> = self
            .c219_samples
            .iter()
            .enumerate()
            .filter(|(_i, s)| !s.hits.is_empty() && s.length() > 0)
            .map(|(i, _s)| i)
            .collect();
        self.c219_ins_count = c219_real.len() as i32;
        for pos in &c219_real {
            for h in &self.c219_samples[*pos].hits {
                if !self.c219_ids.contains(&h.chip_id) {
                    self.c219_ids.push(h.chip_id);
                }
            }
        }
        self.c219_ids = dual_ids(&self.c219_ids, vgm.c140_dual);
        let c219_base = pwm_base + PWM_SIDES * self.pwm_ids.len() as i32;
        let mut c219_at: BTreeMap<i32, i32> = BTreeMap::new();
        for (i, cid) in self.c219_ids.iter().enumerate() {
            c219_at.insert(*cid, c219_base + 16 * i as i32);
        }
        for (real_idx, pos) in c219_real.iter().enumerate() {
            let smp = self.c219_samples[*pos].clone();
            let ref_rate = smp.c4_rate(c219_clock);
            for h in &smp.hits {
                let base_f = crate::c219::hit_played_freq(h);
                let steps = crate::c219::merge_step_writes(&h.freq_pts, 600);
                let (note, e5) = crate::pcm::rate_to_note(
                    crate::c219::c219_rate(c219_clock, base_f),
                    ref_rate as f64,
                );
                let mut sweep: Vec<(i64, i32)> = Vec::new();
                if !steps.is_empty() && base_f != 0 {
                    let deltas: Vec<(i64, i32)> = steps
                        .iter()
                        .filter(|(_t, f)| *f != 0)
                        .map(|(t, f)| {
                            (*t, py_round(64.0 * 12.0 * (*f as f64 / base_f as f64).log2()) as i32)
                        })
                        .collect();
                    let mut vals: Vec<i32> = deltas.iter().map(|d| d.1).collect();
                    vals.push(0);
                    if !deltas.is_empty()
                        && *vals.iter().max().unwrap() - *vals.iter().min().unwrap() >= 4
                    {
                        sweep = deltas;
                    }
                }
                let vl = vol15(h.vol_l, 255.0);
                let vr = vol15(h.vol_r, 255.0);
                let vol = h.vol_l.max(h.vol_r) & 0xFF;
                let pts: Vec<(i64, i32, Option<i32>)> = h
                    .vol_pts
                    .iter()
                    .map(|(t, a, b)| {
                        (
                            *t,
                            (*a).max(*b) & 0xFF,
                            if (*a).max(*b) != 0 {
                                Some(pcm_pan_fx(vol15(*a, 255.0), vol15(*b, 255.0)))
                            } else {
                                None
                            },
                        )
                    })
                    .collect();
                let fade_in = vol == 0 && pts.iter().any(|p| p.1 > 0);
                let ev = NoteEvent {
                    sample: h.sample_time,
                    ch: c219_at.get(&h.chip_id).copied().unwrap_or(c219_base) + h.voice,
                    on: true,
                    note,
                    ins: -1,
                    vol: if fade_in { 0 } else if vol != 0 { vol } else { 15 },
                    e5,
                    pan: if fade_in { 0 } else { pcm_pan_fx(vl, vr) },
                    pcm: true,
                    end: h.off_sample,
                    sweep,
                    vol_pts: pts,
                    no_pan: fade_in,
                    c219_noise: h.noise,
                    c219_mode: (if h.left_invert { 0x10 } else { 0 })
                        | (if h.invert { 0x01 } else { 0 }),
                    ..NoteEvent::default()
                };
                self.events.push(ev);
                self.c219_events.push((self.events.len() - 1, real_idx as i32));
                self.c219_count += 1;
            }
        }
        for off in &c219_offs {
            let base = match c219_at.get(&off.chip_id) {
                Some(b) => *b,
                None => continue,
            };
            self.events.push(NoteEvent {
                sample: off.sample_time,
                ch: base + off.voice,
                on: false,
                note: 180,
                ins: -1,
                vol: -1,
                e5: 0x80,
                pan: 0,
                pcm: true,
                ..NoteEvent::default()
            });
        }

        // K054539
        let k054539_clock = if vgm.k054539_clock != 0 { vgm.k054539_clock } else { K054539_DEFAULT_CLOCK };
        let (k054539_collected, k054539_offs, k054539_warn) = crate::k054539::collect_k054539(vgm);
        self.warnings.extend(k054539_warn);
        self.k054539_samples = k054539_collected
            .into_iter()
            .filter(|s| !s.hits.is_empty() && !s.data.is_empty())
            .collect();
        for smp in &self.k054539_samples {
            for h in &smp.hits {
                if !self.k054539_ids.contains(&h.chip_id) {
                    self.k054539_ids.push(h.chip_id);
                }
            }
        }
        self.k054539_ids = dual_ids(&self.k054539_ids, vgm.k054539_dual);
        let k054539_base = c219_base + 16 * self.c219_ids.len() as i32;
        let mut k054539_at: BTreeMap<i32, i32> = BTreeMap::new();
        for (i, cid) in self.k054539_ids.iter().enumerate() {
            k054539_at.insert(*cid, k054539_base + 8 * i as i32);
        }
        for (i, smp) in self.k054539_samples.clone().iter().enumerate() {
            let ref_rate = smp.c4_rate(k054539_clock);
            for h in &smp.hits {
                let base_delta = crate::k054539::hit_played_delta(h);
                let (note, e5) = crate::pcm::rate_to_note(
                    crate::k054539::k054539_rate(k054539_clock, base_delta),
                    ref_rate as f64,
                );
                let mut sweep: Vec<(i64, i32)> = Vec::new();
                if !h.freq_pts.is_empty() && base_delta != 0 {
                    let base_rate = crate::k054539::k054539_rate(k054539_clock, base_delta);
                    let deltas: Vec<(i64, i32)> = h
                        .freq_pts
                        .iter()
                        .filter(|(_t, d)| {
                            *d != 0
                                && crate::k054539::k054539_rate(k054539_clock, *d) != 0.0
                        })
                        .map(|(t, d)| {
                            (
                                *t,
                                py_round(
                                    64.0 * 12.0
                                        * (crate::k054539::k054539_rate(k054539_clock, *d)
                                            / base_rate)
                                            .log2(),
                                ) as i32,
                            )
                        })
                        .collect();
                    let mut vals: Vec<i32> = deltas.iter().map(|d| d.1).collect();
                    vals.push(0);
                    if !deltas.is_empty()
                        && *vals.iter().max().unwrap() - *vals.iter().min().unwrap() >= 4
                    {
                        sweep = deltas;
                    }
                }
                let pts: Vec<(i64, i32, Option<i32>)> = h
                    .vol_pts
                    .iter()
                    .map(|(t, v, p)| {
                        (
                            *t,
                            crate::k054539::k054539_volume_fx(*v),
                            if *p != 0 { Some(crate::k054539::k054539_pan_fx(*p)) } else { None },
                        )
                    })
                    .collect();
                let ev = NoteEvent {
                    sample: h.sample_time,
                    ch: k054539_at.get(&h.chip_id).copied().unwrap_or(k054539_base) + h.voice,
                    on: true,
                    note,
                    ins: -1,
                    vol: crate::k054539::k054539_volume_fx(h.vol),
                    e5,
                    pan: crate::k054539::k054539_pan_fx(h.pan),
                    pcm: true,
                    end: h.off_sample,
                    sweep,
                    vol_pts: pts,
                    ..NoteEvent::default()
                };
                self.events.push(ev);
                self.k054539_events.push((self.events.len() - 1, i as i32));
                self.k054539_count += 1;
            }
        }
        for off in &k054539_offs {
            let base = match k054539_at.get(&off.chip_id) {
                Some(b) => *b,
                None => continue,
            };
            self.events.push(NoteEvent {
                sample: off.sample_time,
                ch: base + off.voice,
                on: false,
                note: 180,
                ins: -1,
                vol: -1,
                e5: 0x80,
                pan: 0,
                pcm: true,
                ..NoteEvent::default()
            });
        }

        // ES5503
        let (es5503_samples, es5503_offs, es5503_warn) = crate::es5503::collect_es5503(vgm);
        self.warnings.extend(es5503_warn);
        self.es5503_offs = es5503_offs;
        self.es5503_samples = es5503_samples;
        self.es5503_hits = Vec::new();
        for (si, smp) in self.es5503_samples.iter().enumerate() {
            for (hi, hit) in smp.hits.iter().enumerate() {
                if hit.sample_index >= 0 {
                    self.es5503_hits.push((si, hi));
                }
            }
        }
        let mut ids: Vec<i32> = self.es5503_samples.iter().map(|s| s.chip_id).collect();
        ids.sort();
        ids.dedup();
        self.es5503_ids = ids;
    }
}


impl<'a> AnalyzeState<'a> {
    fn take_song(&mut self) -> Song {
        // Python passes the live `song` object to every emitter, so the note
        // and channel-effect lists keep everything emitted so far.  Hand the
        // accumulated events to the emitter instead of starting a fresh list.
        let mut song = self.partial_song();
        song.events = std::mem::take(&mut self.events);
        song.channel_fx = std::mem::take(&mut self.channel_fx);
        song
    }

    fn put_song(&mut self, song: Song) {
        self.events = song.events;
        self.channel_fx = song.channel_fx;
    }


    fn run_es5503(&mut self) {
        // Python analyze.py 12130-12220: one module channel per used
        // oscillator, notes pitched by the played rate against the body's
        // C-4 rate.
        let samples = self.es5503_samples.clone();
        let offs = self.es5503_offs.clone();
        let mut cids: Vec<i32> = samples.iter().map(|s| s.chip_id).collect();
        cids.sort();
        cids.dedup();
        let mut oscs: Vec<(i32, i32)> = Vec::new();
        for cid in cids {
            let mut used: Vec<i32> = samples
                .iter()
                .filter(|s| s.chip_id == cid)
                .flat_map(|s| s.hits.iter())
                .filter(|h| h.sample_index >= 0 && h.freq > 0)
                .map(|h| h.osc as i32)
                .collect();
            used.sort();
            used.dedup();
            for osc in used {
                oscs.push((cid, osc));
            }
        }
        let base = self.tone_ch_base();
        let mut at: IndexMap<(i32, i32), i32> = IndexMap::new();
        for (i, key) in oscs.iter().enumerate() {
            at.insert(*key, base + i as i32);
        }
        self.es5503_oscs = oscs;
        let output_channels = if self.vgm.es5503_channels != 0 {
            self.vgm.es5503_channels as i32
        } else {
            2
        };
        if !self.es5503_hits.is_empty() {
            // Python: Counter(hit.out_rate for hit in hits).most_common(1).
            let mut counts: IndexMap<i64, i32> = IndexMap::new();
            for smp in &samples {
                for h in &smp.hits {
                    *counts.entry(h.out_rate).or_insert(0) += 1;
                }
            }
            let mut best: (i64, i32) = (0, -1);
            for (rate, n) in &counts {
                if *n > best.1 {
                    best = (*rate, *n);
                }
            }
            self.es5503_rate = best.0 as i32;
            for smp in &samples {
                for hit in &smp.hits {
                    if hit.sample_index < 0 || hit.freq <= 0 {
                        continue;
                    }
                    let Some(ch) = at.get(&(hit.chip_id, hit.osc as i32)).copied() else {
                        continue;
                    };
                    let base_rate = crate::es5503::es5503_play_rate(
                        hit.out_rate,
                        hit.freq,
                        hit.resolution,
                        hit.size_class,
                    );
                    let (note, e5) =
                        crate::pcm::rate_to_note(base_rate, smp.c4_rate as f64);
                    let mut sweep: Vec<(i64, i32)> = Vec::new();
                    if !hit.freq_pts.is_empty() && base_rate != 0.0 {
                        let deltas: Vec<(i64, i32)> = hit
                            .freq_pts
                            .iter()
                            .filter(|(_, freq)| *freq > 0)
                            .map(|(t, freq)| {
                                let r = crate::es5503::es5503_play_rate(
                                    hit.out_rate,
                                    *freq,
                                    hit.resolution,
                                    hit.size_class,
                                );
                                (
                                    *t,
                                    crate::parity::py_int(crate::parity::py_round(
                                        64.0 * 12.0 * (r / base_rate).log2(),
                                    )) as i32,
                                )
                            })
                            .collect();
                        let mut lo = 0;
                        let mut hi = 0;
                        for (_t, d) in &deltas {
                            lo = lo.min(*d);
                            hi = hi.max(*d);
                        }
                        if !deltas.is_empty() && hi - lo >= 4 {
                            sweep = deltas;
                        }
                    }
                    let pts: Vec<(i64, i32, Option<i32>)> = hit
                        .vol_pts
                        .iter()
                        .map(|(t, v)| (*t, crate::es5503::es5503_volume_fx(*v), None))
                        .collect();
                    self.events.push(NoteEvent {
                        sample: hit.sample_time,
                        ch,
                        on: true,
                        note,
                        ins: -1,
                        vol: crate::es5503::es5503_volume_fx(hit.vol),
                        e5,
                        pan: crate::es5503::es5503_pan_fx(hit.chan_mask, output_channels),
                        pcm: true,
                        end: hit.off_sample,
                        sweep,
                        vol_pts: pts,
                        ..NoteEvent::default()
                    });
                    self.es5503_events
                        .push((self.events.len() - 1, hit.sample_index));
                    self.es5503_count += 1;
                }
            }
            for off in &offs {
                let Some(ch) = at.get(&(off.chip_id, off.osc as i32)).copied() else {
                    continue;
                };
                self.events.push(NoteEvent {
                    sample: off.sample_time,
                    ch,
                    on: false,
                    note: 180,
                    ins: -1,
                    vol: -1,
                    e5: 0x80,
                    pan: 0,
                    pcm: true,
                    ..NoteEvent::default()
                });
            }
        }
    }

    fn run_msm5232(&mut self) {
        let vgm = self.vgm;
        let clock = if vgm.msm5232_clock != 0 { vgm.msm5232_clock } else { MSM5232_DEFAULT_CLOCK };
        let _ = clock;
        let (hits, offs, fx, vols, warn) = crate::msm5232::collect_msm5232(&vgm.writes);
        self.warnings.extend(warn);
        let mut ids: Vec<i32> = hits.iter().map(|h| h.chip_id as i32).collect();
        ids.sort();
        ids.dedup();
        if ids.is_empty() && vgm.msm5232_clock != 0 {
            ids.push(0);
        }
        self.msm5232_ids = dual_ids(&ids, vgm.msm5232_dual);
        self.msm5232_controls = if !self.msm5232_ids.is_empty() {
            let mut c: Vec<i32> = hits.iter().map(|h| h.control).collect();
            c.sort();
            c.dedup();
            c
        } else {
            Vec::new()
        };
        if self.msm5232_controls.is_empty() && !self.msm5232_ids.is_empty() {
            self.msm5232_controls = vec![0x0F];
        }
        // Python's `msm5232_base = tone_base` is taken BEFORE its own eight
        // columns are added, so subtract them here (tone_ch_base already
        // counts this family).
        let base = self.tone_ch_base() - 8 * self.msm5232_ids.len() as i32;
        let mut at: IndexMap<i32, i32> = IndexMap::new();
        for (i, cid) in self.msm5232_ids.iter().enumerate() {
            at.insert(*cid, base + 8 * i as i32);
        }
        for h in &hits {
            let b = match at.get(&(h.chip_id as i32)) {
                Some(b) => *b,
                None => continue,
            };
            let pitch = if h.noise { 0 } else { h.pitch.min(0x57) };
            let note = (72 + pitch).min(179);
            let ch = b + h.voice;
            let ev = NoteEvent {
                sample: h.sample_time,
                ch,
                on: true,
                note,
                ins: -1,
                vol: crate::msm5232::msm5232_volume_fx(h.volume),
                e5: 0x80,
                pan: 0,
                end: h.off_sample,
                ..NoteEvent::default()
            };
            self.events.push(ev);
            let idx = self
                .msm5232_controls
                .iter()
                .position(|c| *c == (h.control & 0x1F))
                .unwrap_or(0) as i32;
            self.msm5232_events.push((self.events.len() - 1, idx));
            self.channel_fx
                .push((h.sample_time, ch, 0x11, if h.noise { 1 } else { 0 }));
            self.msm5232_count += 1;
        }
        for off in &offs {
            let b = match at.get(&(off.chip_id as i32)) {
                Some(b) => *b,
                None => continue,
            };
            self.events.push(NoteEvent {
                sample: off.sample_time,
                ch: b + off.voice,
                on: false,
                note: 180,
                ins: -1,
                vol: -1,
                e5: 0x80,
                pan: 0,
                ..NoteEvent::default()
            });
        }
        for f in &fx {
            let b = match at.get(&(f.chip_id as i32)) {
                Some(b) => *b,
                None => continue,
            };
            self.channel_fx
                .push((f.sample_time, b + 4 * f.group, f.code, f.value));
        }
        for v in &vols {
            let b = match at.get(&(v.chip_id as i32)) {
                Some(b) => *b,
                None => continue,
            };
            let level = crate::msm5232::msm5232_volume_fx(v.value);
            for slot in 0..4 {
                self.channel_vol_events
                    .push((v.sample_time, b + 4 * v.group + slot, level));
            }
        }
    }

    fn run_nes(&mut self) {
        let vgm = self.vgm;
        let clock = if vgm.nes_clock != 0 { vgm.nes_clock } else { NES_DEFAULT_CLOCK };
        if !self.has_nes {
            self.nes_ids = Vec::new();
            self.nes_tones = Vec::new();
            self.nes_offs = Vec::new();
            self.nes_dpcm_samples = Vec::new();
            self.nes_dpcm_hits = Vec::new();
            self.nes_pcm_samples = Vec::new();
            self.nes_pcm_hits = Vec::new();
            self.nes_present = false;
            return;
        }
        let (tones, offs, warn) = crate::nes::collect_nes_tones(vgm, Some(clock));
        self.warnings.extend(warn);
        let (dpcm_samples, dpcm_hits, warn) = crate::nes::collect_nes_dpcm(vgm, Some(clock));
        self.warnings.extend(warn);
        let (mut pcm_samples, mut pcm_hits, warn) = crate::nes::collect_nes_pcm(vgm, Some(clock));
        self.warnings.extend(warn);
        if self.opts.lossy_samples {
            let before = pcm_samples.len();
            let (a, b) = crate::nes::cluster_nes_pcm(pcm_samples.clone(), pcm_hits.clone(), 32, 16);
            pcm_samples = a;
            pcm_hits = b;
            if pcm_samples.len() < before {
                self.warnings.push(format!(
                    "NES raw PCM: lossy sample clustering {before} -> {} bodies",
                    pcm_samples.len()
                ));
            }
        }
        let mut ids: Vec<i32> = tones
            .iter()
            .map(|h| h.chip_id)
            .chain(offs.iter().map(|o| o.chip_id))
            .chain(dpcm_hits.iter().map(|h| h.chip_id))
            .chain(pcm_hits.iter().map(|h| h.chip_id))
            .collect();
        ids.sort();
        ids.dedup();
        self.nes_ids = dual_ids(&ids, vgm.nes_dual);
        self.nes_tones = tones;
        self.nes_offs = offs;
        self.nes_dpcm_samples = dpcm_samples;
        self.nes_dpcm_hits = dpcm_hits;
        self.nes_pcm_samples = pcm_samples;
        self.nes_pcm_hits = pcm_hits;
        self.nes_present = true;
        if self.nes_ids.is_empty() {
            return;
        }
        let base = self.tone_ch_base()
            + SCC_VOICES * self.scc_ids.len() as i32
            + AY_VOICES * self.ay_ids.len() as i32;
        let mut at: IndexMap<i32, i32> = IndexMap::new();
        for (i, cid) in self.nes_ids.iter().enumerate() {
            at.insert(*cid, base + NES_CHANNELS * i as i32);
        }
        for h in self.nes_tones.clone() {
            let ch = at.get(&h.chip_id).copied().unwrap_or(base) + h.voice;
            let note;
            let e5;
            let sweep;
            if h.voice == 3 {
                note = Self::nes_noise_note(h.period);
                e5 = 0x80;
                sweep = Vec::new();
            } else {
                let (n, e) = crate::nes::nes_note(clock, h.period);
                note = n;
                e5 = e;
                let deltas: Vec<(i64, i32)> = h
                    .pitch_pts
                    .iter()
                    .map(|(t, per)| (*t, Self::nes_period_delta(h.period, *per)))
                    .collect();
                let mut vals: Vec<i32> = deltas.iter().map(|d| d.1).collect();
                vals.push(0);
                sweep = if !deltas.is_empty()
                    && *vals.iter().max().unwrap() - *vals.iter().min().unwrap() >= 4
                {
                    deltas
                } else {
                    Vec::new()
                };
            }
            let env_mode = h.env_mode | 3;
            let mut vol = if h.voice == 2 { 15 } else { h.vol };
            let mut vol_pts: Vec<(i64, i32, Option<i32>)> =
                h.vol_pts.iter().map(|(t, v)| (*t, *v, None)).collect();
            if h.voice != 2 && (h.env_mode & 1) == 0 {
                let step = ((h.vol & 0x0F) + 1) as f64 * (VGM_RATE as f64 / 240.0);
                vol_pts = (1..15)
                    .map(|k| (h.sample_time + py_round(k as f64 * step) as i64, 15 - k, None))
                    .collect();
                vol = 15;
            }
            let linear = h.linear | 0x80;
            if h.voice < 2 || h.voice == 3 {
                self.channel_fx.push((h.sample_time, ch, 0x12, h.duty));
                for (t, duty) in &h.duty_pts {
                    self.channel_fx.push((*t, ch, 0x12, *duty));
                }
                if h.voice == 3 {
                    self.channel_fx.push((h.sample_time, ch, 0x15, env_mode));
                    for (t, _mode) in &h.env_pts {
                        self.channel_fx.push((*t, ch, 0x15, env_mode));
                    }
                }
            } else {
                self.channel_fx.push((h.sample_time, ch, 0x19, linear));
                for (t, lin) in &h.linear_pts {
                    self.channel_fx.push((*t, ch, 0x19, lin | 0x80));
                }
            }
            if h.voice < 2 {
                self.channel_fx.push((h.sample_time, ch, 0x15, env_mode));
                for (t, _mode) in &h.env_pts {
                    self.channel_fx.push((*t, ch, 0x15, env_mode));
                }
            }
            let ev = NoteEvent {
                sample: h.sample_time,
                ch,
                on: true,
                note,
                ins: -1,
                vol,
                e5,
                pan: 0,
                end: h.off_sample,
                vol_pts,
                sweep,
                sweep_div: 2.0,
                e5_lo: 0x00,
                e5_hi: 0xFF,
                tone_pitch_pts: if h.voice != 3 { h.pitch_pts.clone() } else { Vec::new() },
                tone_clock: clock,
                tone_div: NES_CPU_DIVIDER,
                tone_period_offset: 1,
                tone_chip: 4,
                ..NoteEvent::default()
            };
            self.events.push(ev);
        }
        for off in &self.nes_offs.clone() {
            self.events.push(NoteEvent {
                sample: off.sample_time,
                ch: at.get(&off.chip_id).copied().unwrap_or(base) + off.voice,
                on: false,
                note: 180,
                ins: -1,
                vol: -1,
                e5: 0x80,
                pan: 0,
                ..NoteEvent::default()
            });
        }
        for hit in &self.nes_dpcm_hits.clone() {
            let ch = at.get(&hit.chip_id).copied().unwrap_or(base) + 4;
            self.channel_fx.push((hit.sample_time, ch, 0x18, 1));
            self.channel_fx.push((hit.sample_time, ch, 0x11, 0));
            let ev = NoteEvent {
                sample: hit.sample_time,
                ch,
                on: true,
                note: 60 + (hit.rate_index & 0x0F),
                ins: -1,
                vol: 15,
                e5: 0x80,
                pan: 0,
                end: hit.end_time,
                no_pan: true,
                pcm: true,
                ..NoteEvent::default()
            };
            self.events.push(ev);
            self.nes_dpcm_events
                .push((self.events.len() - 1, hit.sample_index));
            if hit.end_time != 0 {
                self.events.push(NoteEvent {
                    sample: hit.end_time,
                    ch,
                    on: false,
                    note: 180,
                    ins: -1,
                    vol: -1,
                    e5: 0x80,
                    pan: 0,
                    pcm: true,
                    ..NoteEvent::default()
                });
            }
        }
        let mut pcm_mode_set: BTreeSet<i32> = BTreeSet::new();
        for hit in &self.nes_pcm_hits.clone() {
            let ch = at.get(&hit.chip_id).copied().unwrap_or(base) + 4;
            if !pcm_mode_set.contains(&hit.chip_id) {
                self.channel_fx.push((hit.sample_time, ch, 0x18, 0));
                pcm_mode_set.insert(hit.chip_id);
            }
            let ev = NoteEvent {
                sample: hit.sample_time,
                ch,
                on: true,
                note: 60 + (hit.sample_index % NES_PCM_MAP_SLOTS),
                ins: -1,
                vol: 15,
                e5: 0x80,
                pan: 0,
                end: hit.end_time,
                no_pan: true,
                dac: true,
                ..NoteEvent::default()
            };
            self.events.push(ev);
            self.nes_pcm_events
                .push((self.events.len() - 1, hit.sample_index));
            if hit.end_time != 0 {
                // The reference keeps this off out of the PCM row map: the
                // hit ons are dac-not-pcm, so _assign_pcm_rows would find no
                // trigger for it and drop it.
                self.events.push(NoteEvent {
                    sample: hit.end_time,
                    ch,
                    on: false,
                    note: 180,
                    ins: -1,
                    vol: -1,
                    e5: 0x80,
                    pan: 0,
                    ..NoteEvent::default()
                });
            }
        }
    }
}


impl<'a> AnalyzeState<'a> {
    fn run_huc6280(&mut self) {
        let vgm = self.vgm;
        let clock = if vgm.huc6280_clock != 0 { vgm.huc6280_clock } else { 7_159_090 };
        let (notes, offs, wave_list, warn) = crate::huc6280::collect_huc6280(vgm);
        self.warnings.extend(warn);
        let (mut pcm_samples, mut pcm_hits, warn) = if self.opts.pcm {
            crate::huc6280::collect_huc6280_pcm(vgm)
        } else {
            (Vec::new(), Vec::new(), Vec::new())
        };
        self.warnings.extend(warn);
        let _ = &mut pcm_samples;
        let mut ids: Vec<i32> = notes.iter().map(|n| n.hit.chip_id)
            .chain(pcm_hits.iter().map(|h| h.chip_id)).collect();
        ids.sort();
        ids.dedup();
        if ids.is_empty() {
            return;
        }
        self.huc6280_ids = dual_ids(&ids, vgm.huc6280_dual);
        let base = self.tone_ch_base()
            + SCC_VOICES * self.scc_ids.len() as i32
            + AY_VOICES * self.ay_ids.len() as i32
            + NES_CHANNELS * self.nes_ids.len() as i32;
        let mut at: IndexMap<i32, i32> = IndexMap::new();
        for (i, cid) in self.huc6280_ids.iter().enumerate() {
            at.insert(*cid, base + HUC6280_CHANNELS * i as i32);
        }
        self.huc6280_waves = wave_list;
        let mut wave_index: BTreeMap<Vec<u8>, i32> = self
            .huc6280_waves
            .iter()
            .enumerate()
            .map(|(i, w)| (w.clone(), i as i32))
            .collect();
        for n in &notes {
            let h = &n.hit;
            let b = match at.get(&h.chip_id) {
                Some(b) => *b,
                None => continue,
            };
            let voice_ch = b + h.voice as i32;
            let gate_end = if h.off_sample != 0 { h.off_sample } else { vgm.total_samples };
            // Python analyze.py 12718-12811: a noise-capable voice can switch
            // between noise and its waveform inside one gate, so split at every
            // transition; the note boundary then lands on a tick through EDxx.
            let has_noise = h.voice >= 4
                && ((h.noise & crate::huc6280::NOISE_ENABLE) != 0
                    || h
                        .noise_pts
                        .iter()
                        .any(|(_t, reg)| (reg & crate::huc6280::NOISE_ENABLE) != 0));
            if has_noise {
                let mut mode_pts: Vec<(i64, i32)> = vec![(
                    h.sample_time,
                    if (h.noise & crate::huc6280::NOISE_ENABLE) != 0 { 1 } else { 0 },
                )];
                for &(t, reg) in &h.noise_pts {
                    if h.sample_time < t && t < gate_end {
                        let m = if (reg & crate::huc6280::NOISE_ENABLE) != 0 { 1 } else { 0 };
                        if m != mode_pts.last().map(|p| p.1).unwrap_or(0) {
                            mode_pts.push((t, m));
                        }
                    }
                }
                for i in 0..mode_pts.len() {
                    let (st, m) = mode_pts[i];
                    let en = if i + 1 < mode_pts.len() { mode_pts[i + 1].0 } else { gate_end };
                    if en <= st {
                        continue;
                    }
                    let (mut ev_vol, mut ev_pan) = crate::huc6280::pce_effective(
                        super::_value_at(&h.vol_pts, st, h.vol),
                        super::_value_at(&h.balance_pts, st, h.balance),
                    );
                    let mut ev_vol_pts: Vec<(i64, i32, Option<i32>)> =
                        Self::merge_vol_balance(h)
                            .into_iter()
                            .filter(|p| st < p.0 && p.0 < en)
                            .collect();
                    if !ev_vol_pts.is_empty()
                        && ev_vol_pts[0].0 - st <= crate::huc6280::HUC6280_ATTACK_WINDOW
                    {
                        let p = ev_vol_pts.remove(0);
                        ev_vol = p.1;
                        if let Some(pan) = p.2 {
                            ev_pan = pan;
                        }
                    }
                    if m != 0 {
                        let mut regs: Vec<(i64, i32)> = vec![(
                            st,
                            super::_value_at(&h.noise_pts, st, h.noise) & 0x1F,
                        )];
                        let mut last_noise = regs[0].1;
                        for &(t, reg) in &h.noise_pts {
                            if !(st < t && t < en && (reg & crate::huc6280::NOISE_ENABLE) != 0) {
                                continue;
                            }
                            let value = reg & 0x1F;
                            if value != last_noise {
                                regs.push((t, value));
                                last_noise = value;
                            }
                        }
                        let note = crate::huc6280::huc6280_noise_note(super::_value_at(
                            &h.noise_pts,
                            st,
                            h.noise,
                        ));
                        self.events.push(NoteEvent {
                            sample: st,
                            ch: voice_ch,
                            on: true,
                            note,
                            ins: self.tone_base,
                            vol: ev_vol,
                            e5: 0x80,
                            pan: ev_pan,
                            end: en,
                            vol_pts: ev_vol_pts,
                            tone_pitch_pts: if regs.len() > 1 { regs } else { Vec::new() },
                            tone_noise: true,
                            tone_noise_end: 0,
                            tone_wave: n.wave_index as i32,
                            tone_chip: 5,
                            ..NoteEvent::default()
                        });
                    } else {
                        let mut pts: Vec<(i64, i32)> =
                            vec![(st, super::_value_at(&h.pitch_pts, st, h.period))];
                        pts.extend(
                            h.pitch_pts
                                .iter()
                                .copied()
                                .filter(|(t, _p)| st < *t && *t < en),
                        );
                        let real: Vec<(i64, i32)> =
                            pts.into_iter().filter(|(_t, p)| *p > 0).collect();
                        if real.is_empty() {
                            continue;
                        }
                        let (base_t, base_p) = real[0];
                        let (note, e5) = crate::huc6280::huc6280_note(clock, base_p);
                        let tail: Vec<(i64, i32)> =
                            real.iter().copied().filter(|(t, _p)| *t > base_t).collect();
                        self.events.push(NoteEvent {
                            sample: st,
                            ch: voice_ch,
                            on: true,
                            note,
                            ins: self.tone_base,
                            vol: ev_vol,
                            e5,
                            pan: ev_pan,
                            end: en,
                            vol_pts: ev_vol_pts,
                            tone_pitch_pts: tail,
                            tone_wave: n.wave_index as i32,
                            tone_chip: 5,
                            tone_period: base_p,
                            ..NoteEvent::default()
                        });
                    }
                    self.huc6280_count += 1;
                }
                continue;
            }
            if !h.wave.iter().any(|x| *x != 0) {
                continue;
            }
            if h.voice >= 4 {
                self.channel_fx.push((h.sample_time, voice_ch, 0x11, 0));
            }
            let mut seg_pts: Vec<(i64, i32)> = vec![(h.sample_time, h.period)];
            seg_pts.extend(h.pitch_pts.iter().copied());
            let segs = super::segment_periods(
                &seg_pts,
                gate_end,
                clock as f64,
                32.0,
                0,
                _AY_MERGE_GAP,
                // Python keeps the default min_hold (one 60 Hz frame), so a
                // frame-rate glide stays intra-note and becomes row effects.
                super::util::_NOTE_HOLD_SAMPLES,
                false,
                0x7FF,
            );
            for (i, (st, period, intra)) in segs.iter().enumerate() {
                let nxt = if i + 1 < segs.len() { segs[i + 1].0 } else { 0 };
                let seg_end = if nxt != 0 { nxt } else { h.off_sample };
                let wave = Self::value_at_bytes(&h.wave_pts, *st, &h.wave);
                if !wave.iter().any(|x| *x != 0) {
                    self.events.push(NoteEvent {
                        sample: *st,
                        ch: voice_ch,
                        on: false,
                        note: 180,
                        ins: -1,
                        vol: -1,
                        e5: 0x80,
                        pan: 0,
                        ..NoteEvent::default()
                    });
                    continue;
                }
                let bound = if nxt != 0 { nxt } else { gate_end };
                let (mut vol, mut balance) = crate::huc6280::pce_effective(
                    super::_value_at(&h.vol_pts, *st, h.vol),
                    super::_value_at(&h.balance_pts, *st, h.balance),
                );
                let mut vol_pts: Vec<(i64, i32, Option<i32>)> =
                    Self::merge_vol_balance(h)
                        .into_iter()
                        .filter(|p| *st < p.0 && p.0 < bound)
                        .collect();
                if !vol_pts.is_empty()
                    && vol_pts[0].0 - *st <= crate::huc6280::HUC6280_ATTACK_WINDOW
                {
                    let p = vol_pts.remove(0);
                    vol = p.1;
                    if let Some(pan) = p.2 {
                        balance = pan;
                    }
                }
                let steps = super::_tone_steps(
                    intra,
                    *st,
                    if i == 0 { crate::huc6280::HUC6280_REFIT_SAMPLES } else { 0 },
                    _AY_MERGE_GAP,
                );
                let mut wave_steps: Vec<(i64, i32)> = Vec::new();
                let wpts: Vec<(i64, Vec<u8>)> =
                    h.wave_pts.iter().filter(|(t, _)| *st < *t && *t < bound).cloned().collect();
                for (t, img) in Self::merge_step_writes_bytes(wpts, 64) {
                    let idx = match wave_index.get(&img) {
                        Some(i) => *i,
                        None => {
                            let i = self.huc6280_waves.len() as i32;
                            wave_index.insert(img.clone(), i);
                            self.huc6280_waves.push(img);
                            i
                        }
                    };
                    if wave_steps.last().map(|p| p.1) == Some(idx) {
                        continue;
                    }
                    wave_steps.push((t, idx));
                }
                let wi = match wave_index.get(&wave) {
                    Some(i) => *i,
                    None => {
                        let i = self.huc6280_waves.len() as i32;
                        wave_index.insert(wave.clone(), i);
                        self.huc6280_waves.push(wave.clone());
                        i
                    }
                };
                let (note, e5) = crate::huc6280::huc6280_note(clock, *period);
                self.events.push(NoteEvent {
                    sample: *st,
                    ch: voice_ch,
                    on: true,
                    note,
                    ins: self.tone_base,
                    vol,
                    e5,
                    pan: balance,
                    end: seg_end,
                    vol_pts,
                    tone_pitch_pts: steps,
                    tone_wave_pts: wave_steps,
                    tone_wave: wi,
                    tone_period_offset: 0,
                    tone_chip: 5,
                    tone_period: *period,
                    ..NoteEvent::default()
                });
                self.huc6280_count += 1;
            }
        }
        // Python analyze.py 12886-12909: a long gate splits at its volume
        // peaks, then the balance ratio moves into 08xy channel points so the
        // envelope can be the instrument's register-level macro.
        self.events =
            super::tone::_split_pce_volume_peaks(&self.events, clock, vgm.total_samples);
        {
            let events = &mut self.events;
            let channel_fx = &mut self.channel_fx;
            for ev in events.iter_mut() {
                if ev.tone_chip != 5 || ev.vol_pts.is_empty() {
                    continue;
                }
                let mut last_pan = ev.pan;
                let mut pts: Vec<(i64, i32, Option<i32>)> =
                    Vec::with_capacity(ev.vol_pts.len());
                for &(t, value, pan) in ev.vol_pts.iter() {
                    if let Some(pan) = pan {
                        if pan != last_pan {
                            channel_fx.push((t, ev.ch, 0x08, pan & 0xFF));
                            last_pan = pan;
                        }
                    }
                    pts.push((t, value, None));
                }
                ev.vol_pts = pts;
            }
        }
        // PCE noise voices are handled by the same note walk above when their
        // waveform is silent; the reference splits them at mode changes.
        let _ = &mut pcm_hits;
        for off in &offs {
            let b = match at.get(&off.chip_id) {
                Some(b) => *b,
                None => continue,
            };
            self.events.push(NoteEvent {
                sample: off.sample_time,
                ch: b + off.voice as i32,
                on: false,
                note: 180,
                ins: -1,
                vol: -1,
                e5: 0x80,
                pan: 0,
                ..NoteEvent::default()
            });
        }
        for hit in &pcm_hits {
            let b = at.get(&hit.chip_id).copied().unwrap_or(base);
            let ref_rate = if hit.sample_index < pcm_samples.len() {
                pcm_samples[hit.sample_index].rate
            } else {
                0
            };
            let (note, e5) = crate::model::pcm_rate_note(
                (if hit.rate != 0 { hit.rate } else { ref_rate }) as i64,
                ref_rate as i64,
            );
            let ev = NoteEvent {
                sample: hit.sample_time,
                ch: b + hit.voice as i32,
                on: true,
                note,
                ins: -1,
                vol: 31,
                e5,
                pan: 0,
                end: hit.end_time,
                no_pan: true,
                pcm: true,
                ..NoteEvent::default()
            };
            self.events.push(ev);
            self.huc6280_pcm_events
                .push((self.events.len() - 1, hit.sample_index as i32));
        }
        self.huc_pcm_samples = pcm_samples;
        self.huc6280_pcm_count = pcm_hits.len() as i32;
    }

    /// Drop the given events and remap every recorded event index.
    ///
    /// Python's events are objects, so a filter never invalidates the
    /// `(NoteEvent, index)` lists the emitters keep. The Rust port stores
    /// indices, so a removal has to rewrite them or every later binding points
    /// at the wrong event.
    fn drop_events(&mut self, drop: &BTreeSet<usize>) {
        if drop.is_empty() {
            return;
        }
        let mut map: Vec<Option<usize>> = vec![None; self.events.len()];
        let mut out: Vec<NoteEvent> = Vec::with_capacity(self.events.len());
        for (i, e) in self.events.drain(..).enumerate() {
            if !drop.contains(&i) {
                map[i] = Some(out.len());
                out.push(e);
            }
        }
        self.events = out;
        self.remap_indices(&map);
    }

    /// Rewrite every recorded event index through `map` (None drops the entry).
    fn remap_indices(&mut self, map: &[Option<usize>]) {
        fn remap2(list: &mut Vec<(usize, i32)>, map: &[Option<usize>]) {
            list.retain_mut(|p| match map[p.0] {
                Some(n) => {
                    p.0 = n;
                    true
                }
                None => false,
            });
        }
        fn remap3(list: &mut Vec<(usize, i32, i32)>, map: &[Option<usize>]) {
            list.retain_mut(|p| match map[p.0] {
                Some(n) => {
                    p.0 = n;
                    true
                }
                None => false,
            });
        }
        remap2(&mut self.c219_events, &map);
        remap2(&mut self.k054539_events, &map);
        remap2(&mut self.ymz_events, &map);
        remap2(&mut self.multipcm_events, &map);
        remap2(&mut self.upd_events, &map);
        remap2(&mut self.pwm_events, &map);
        remap2(&mut self.msm5205_events, &map);
        remap2(&mut self.ga20_events, &map);
        remap2(&mut self.msm5232_events, &map);
        remap2(&mut self.es5503_events, &map);
        remap2(&mut self.opl4_wave_events, &map);
        remap2(&mut self.ym2608_b_events, &map);
        remap2(&mut self.y8950_b_events, &map);
        remap2(&mut self.ssg_pcm_events, &map);
        remap2(&mut self.nes_dpcm_events, &map);
        remap2(&mut self.nes_pcm_events, &map);
        remap2(&mut self.huc6280_pcm_events, &map);
        remap2(&mut self.mikey_sample_events, &map);
        remap2(&mut self.x1_sample_events, &map);
        remap2(&mut self.wonderswan_pcm_events, &map);
        remap2(&mut self.psg_dac_events, &map);
        remap3(&mut self.ym2610_events, &map);
        self.ym2608_rhythm_events.retain_mut(|i| match map[*i] {
            Some(n) => {
                *i = n;
                true
            }
            None => false,
        });
    }

    fn merge_vol_balance(h: &crate::huc6280::Huc6280Hit) -> Vec<(i64, i32, Option<i32>)> {
        let mut evs: Vec<(i64, i32, i32)> =
            h.vol_pts.iter().map(|(t, v)| (*t, 0, *v)).collect();
        evs.extend(h.balance_pts.iter().map(|(t, b)| (*t, 1, *b)));
        evs.sort_by_key(|x| (x.0, x.1));
        let mut out = Vec::new();
        let mut cur_vol = h.vol;
        let mut cur_bal = h.balance;
        for (t, kind, val) in evs {
            if kind == 0 {
                cur_vol = val;
            } else {
                cur_bal = val;
            }
            let (v, p) = crate::huc6280::pce_effective(cur_vol, cur_bal);
            out.push((t, v, Some(p)));
        }
        out
    }
}


impl<'a> AnalyzeState<'a> {
    fn run_gb(&mut self) {
        let vgm = self.vgm;
        let clock = if vgm.gb_dmg_clock != 0 { vgm.gb_dmg_clock } else { GB_DEFAULT_CLOCK };
        let (hits, offs, wave_list, warn) = crate::gb::collect_gb(vgm, Some(clock));
        self.warnings.extend(warn);
        if hits.is_empty() && offs.is_empty() {
            return;
        }
        self.gb_ids = dual_ids(&[0], vgm.gb_dmg_dual);
        let base = self.tone_ch_base()
            + SCC_VOICES * self.scc_ids.len() as i32
            + AY_VOICES * self.ay_ids.len() as i32
            + NES_CHANNELS * self.nes_ids.len() as i32
            + if !self.huc6280_ids.is_empty() { HUC6280_CHANNELS } else { 0 };
        self.gb_waves = wave_list;
        let mut wave_index: BTreeMap<Vec<u8>, i32> = self
            .gb_waves
            .iter()
            .enumerate()
            .map(|(i, w)| (w.clone(), i as i32))
            .collect();
        for h in &hits {
            let ch = base + h.voice as i32;
            let note;
            let e5;
            let pitch_pts;
            if h.voice == 3 {
                note = crate::gb::gb_noise_note(h.period);
                e5 = 0x80;
                let mut offsets: Vec<(i64, i32)> = Vec::new();
                for (t, nr43) in &h.pitch_pts {
                    let pos = crate::gb::gb_noise_note(*nr43) - 60;
                    if offsets.last().map(|p| p.1) == Some(pos) {
                        continue;
                    }
                    offsets.push((*t, pos));
                }
                pitch_pts = offsets;
            } else {
                let (n, e) = crate::gb::gb_note(clock, h.period);
                note = n;
                e5 = e;
                pitch_pts = h.pitch_pts.iter().map(|(t, p)| (*t, 2048 - *p)).collect();
            }
            if h.voice != 2 {
                self.channel_fx.push((h.sample_time, ch, 0x12, h.duty));
                for (t, d) in &h.duty_pts {
                    self.channel_fx.push((*t, ch, 0x12, *d));
                }
            }
            if h.voice == 0 && (h.sweep & 0x7F) != 0 {
                self.channel_fx.push((
                    h.sample_time,
                    ch,
                    0x13,
                    (((h.sweep >> 4) & 7) << 4) | (h.sweep & 7),
                ));
                self.channel_fx
                    .push((h.sample_time, ch, 0x14, (h.sweep >> 3) & 1));
                for (t, sw) in &h.sweep_pts {
                    self.channel_fx
                        .push((*t, ch, 0x13, (((*sw >> 4) & 7) << 4) | (*sw & 7)));
                    self.channel_fx.push((*t, ch, 0x14, (*sw >> 3) & 1));
                }
            }
            for (t, p) in &h.pan_pts {
                self.channel_fx.push((*t, ch, 0x08, *p));
            }
            let mut wave_steps: Vec<(i64, i32)> = Vec::new();
            if h.voice == 2 {
                for (t, w) in &h.wave_pts {
                    let body = self
                        .gb_waves
                        .get(*w as usize)
                        .cloned()
                        .unwrap_or_default();
                    let idx = match wave_index.get(&body) {
                        Some(i) => *i,
                        None => {
                            let i = self.gb_waves.len() as i32;
                            wave_index.insert(body.clone(), i);
                            self.gb_waves.push(body);
                            i
                        }
                    };
                    if wave_steps.last().map(|p| p.1) == Some(idx) {
                        continue;
                    }
                    wave_steps.push((*t, idx));
                }
            }
            let vol_pts: Vec<(i64, i32, Option<i32>)> =
                h.vol_pts.iter().map(|(t, v)| (*t, *v, None)).collect();
            self.events.push(NoteEvent {
                sample: h.sample_time,
                ch,
                on: true,
                note,
                ins: -1,
                vol: if h.voice != 2 && !h.soft_env { 15 } else { h.vol },
                e5,
                pan: h.pan,
                end: h.off_sample,
                vol_pts,
                tone_pitch_pts: pitch_pts,
                tone_wave_pts: wave_steps,
                tone_wave: if h.voice == 2 { h.wave as i32 } else { -1 },
                tone_clock: clock,
                tone_div: GB_PITCH_DIVIDER,
                tone_period_offset: 0,
                tone_chip: 6,
                tone_gb_wave: h.voice == 2,
                tone_noise: h.voice == 3,
                gb_soft_env: h.soft_env,
                gb_env_vol: h.env_vol,
                gb_env_dir: h.env_dir,
                gb_env_len: h.env_len,
                gb_sound_len: h.sound_len,
                legato: h.legato,
                ..NoteEvent::default()
            });
            self.gb_count += 1;
        }
        for off in &offs {
            self.events.push(NoteEvent {
                sample: off.sample_time,
                ch: base + off.voice as i32,
                on: false,
                note: 180,
                ins: -1,
                vol: -1,
                e5: 0x80,
                pan: 0,
                ..NoteEvent::default()
            });
        }
    }

    fn run_vsu(&mut self) {
        let vgm = self.vgm;
        let _clock = if vgm.vsu_clock != 0 { vgm.vsu_clock } else { VSU_DEFAULT_CLOCK };
        let (hits, offs, wave_list, mod_list, dynamic, warn) = crate::vsu::collect_vsu(vgm);
        self.warnings.extend(warn);
        if hits.is_empty() && offs.is_empty() {
            return;
        }
        let mut ids: Vec<i32> = hits.iter().map(|h| h.chip_id).collect();
        ids.sort();
        ids.dedup();
        self.vsu_ids = dual_ids(&ids, vgm.vsu_dual);
        self.vsu_waves = wave_list;
        self.vsu_mod_waves = mod_list;
        self.vsu_dynamic = dynamic;
        let base = self.tone_ch_base()
            + SCC_VOICES * self.scc_ids.len() as i32
            + AY_VOICES * self.ay_ids.len() as i32
            + NES_CHANNELS * self.nes_ids.len() as i32
            + if !self.huc6280_ids.is_empty() { HUC6280_CHANNELS } else { 0 }
            + 4 * self.gb_ids.len() as i32;
        let mut at: IndexMap<i32, i32> = IndexMap::new();
        for (i, cid) in self.vsu_ids.iter().enumerate() {
            at.insert(*cid, base + VSU_CHANNELS * i as i32);
        }
        for h in &hits {
            let ch = match at.get(&h.chip_id) {
                Some(c) => *c,
                None => continue,
            };
            let clock = if vgm.vsu_clock != 0 { vgm.vsu_clock } else { VSU_DEFAULT_CLOCK };
            let (note, e5) = crate::vsu::vsu_note(clock, h.period);
            let vol_pts: Vec<(i64, i32, Option<i32>)> =
                h.vol_pts.iter().map(|(t, v)| (*t, *v, None)).collect();
            self.events.push(NoteEvent {
                sample: h.sample_time,
                ch: ch + h.voice as i32,
                on: true,
                note,
                ins: -1,
                vol: h.vol,
                e5,
                pan: h.pan,
                end: h.off_sample,
                vol_pts,
                tone_pitch_pts: h.pitch_pts.clone(),
                tone_clock: clock,
                tone_div: VSU_PITCH_DIVIDER,
                tone_period_offset: 0,
                tone_chip: 7,
                tone_wave: if h.voice != 5 { h.wave } else { -1 },
                tone_mod: if h.voice == 4 { h.mod_ } else { 0 },
                ..NoteEvent::default()
            });
            self.vsu_count += 1;
            let chn = ch + h.voice as i32;
            if h.voice != 5 {
                for (t, idx) in &h.wave_pts {
                    self.channel_fx
                        .push((*t, chn, 0x10, 0x1000 | (*idx & 0xFF)));
                }
            }
            for (t, p) in &h.pan_pts {
                self.channel_fx.push((*t, chn, 0x08, p & 0xFF));
            }
            if h.voice == 5 {
                self.channel_fx.push((h.sample_time, chn, 0x11, h.tap & 7));
                for (t, tap) in &h.tap_pts {
                    self.channel_fx.push((*t, chn, 0x11, tap & 7));
                }
            }
            if h.voice == 4 {
                {
                    let effect = super::_vsu_sw_effect(h.sweep, h.ev1);
                    self.channel_fx.push((h.sample_time, chn, effect.0, effect.1));
                }
                for (t, sweep, ev1) in &h.sw_pts {
                    {
                        let effect = super::_vsu_sw_effect(*sweep, *ev1);
                        self.channel_fx.push((*t, chn, effect.0, effect.1));
                    }
                }
            }
        }
        for off in &offs {
            let base = match at.get(&off.chip_id) {
                Some(b) => *b,
                None => continue,
            };
            self.events.push(NoteEvent {
                sample: off.sample_time,
                ch: base + off.voice as i32,
                on: false,
                note: 180,
                ins: -1,
                vol: -1,
                e5: 0x80,
                pan: 0,
                ..NoteEvent::default()
            });
        }
    }

    fn run_saa_pokey(&mut self) {
        let vgm = self.vgm;
        let saa_clock = if vgm.saa1099_clock != 0 { vgm.saa1099_clock } else { SAA_DEFAULT_CLOCK };
        let (saa_hits, saa_offs, saa_warn) = crate::saa1099::collect_saa1099(vgm);
        self.warnings.extend(saa_warn);
        if !saa_hits.is_empty() || !saa_offs.is_empty() {
            let mut ids: Vec<i32> = vgm
                .writes
                .iter()
                .filter(|w| w.chip == "saa1099")
                .map(|w| w.chip_id as i32)
                .collect();
            ids.sort();
            ids.dedup();
            if ids.is_empty() {
                ids = saa_hits.iter().map(|h| h.chip_id as i32).collect();
                ids.sort();
                ids.dedup();
            }
            self.saa_ids = dual_ids(&ids, vgm.saa1099_dual);
            let base = self.tone_ch_base()
                + SCC_VOICES * self.scc_ids.len() as i32
                + AY_VOICES * self.ay_ids.len() as i32
                + NES_CHANNELS * self.nes_ids.len() as i32
                + if !self.huc6280_ids.is_empty() { HUC6280_CHANNELS } else { 0 }
                + 4 * self.gb_ids.len() as i32
                + VSU_CHANNELS * self.vsu_ids.len() as i32;
            let mut at: IndexMap<i32, i32> = IndexMap::new();
            for (i, cid) in self.saa_ids.iter().enumerate() {
                at.insert(*cid, base + SAA_VOICES * i as i32);
            }
            let mut song = self.take_song();
            let n = super::_emit_saa_notes(
                &mut song,
                &saa_hits,
                &saa_offs,
                &at,
                saa_clock as f64,
                vgm.total_samples,
            );
            self.put_song(song);
            self.saa_count = n;
        }
        let pokey_clock = if vgm.pokey_clock != 0 { vgm.pokey_clock } else { POKEY_DEFAULT_CLOCK };
        let (pokey_hits, pokey_offs, audctl, waves, pokey_warn) = crate::pokey::collect_pokey(vgm);
        self.warnings.extend(pokey_warn);
        let mut ids: Vec<i32> = pokey_hits
            .iter()
            .map(|h| h.chip_id)
            .chain(pokey_offs.iter().map(|o| o.chip_id))
            .collect();
        ids.sort();
        ids.dedup();
        self.pokey_ids = dual_ids(&ids, vgm.pokey_dual);
        if !self.pokey_ids.is_empty() {
            let base = self.tone_ch_base()
                + SCC_VOICES * self.scc_ids.len() as i32
                + AY_VOICES * self.ay_ids.len() as i32
                + NES_CHANNELS * self.nes_ids.len() as i32
                + if !self.huc6280_ids.is_empty() { HUC6280_CHANNELS } else { 0 }
                + 4 * self.gb_ids.len() as i32
                + VSU_CHANNELS * self.vsu_ids.len() as i32
                + SAA_VOICES * self.saa_ids.len() as i32;
            let mut at: IndexMap<i32, i32> = IndexMap::new();
            for (i, cid) in self.pokey_ids.iter().enumerate() {
                at.insert(*cid, base + POKEY_VOICES * i as i32);
            }
            let mut song = self.take_song();
            self.pokey_count = super::_emit_pokey_notes(
                &mut song,
                &pokey_hits,
                &pokey_offs,
                &at,
                &audctl,
                &waves,
                self.tone_base,
                vgm.total_samples,
            );
            self.put_song(song);
        }
        let _ = pokey_clock;
    }

    fn run_mikey_x1(&mut self) {
        let vgm = self.vgm;
        let mikey_clock = if vgm.mikey_clock != 0 { vgm.mikey_clock } else { MIKEY_DEFAULT_CLOCK };
        let (mikey_hits, mikey_offs, mut mikey_samples, mikey_warn) =
            crate::mikey::collect_mikey(vgm, self.opts.lossy_samples);
        self.warnings.extend(mikey_warn);
        if !self.opts.pcm {
            mikey_samples = Vec::new();
        }
        let mut ids: Vec<i32> = mikey_hits
            .iter()
            .map(|h| h.chip_id)
            .chain(mikey_samples.iter().map(|s| s.chip_id))
            .collect();
        ids.sort();
        ids.dedup();
        self.mikey_ids = dual_ids(&ids, vgm.mikey_dual);
        if vgm.mikey_dual {
            self.warnings.push(
                "Mikey header declares a second chip, but VGM command 0x40 has no instance bit; both voices play on the first chip".into(),
            );
        }
        let n_mikey_ins = (mikey_samples.len() as i32 + 119) / 120;
        self.mikey_samples = mikey_samples;
        let mikey_base = if self.mikey_ids.is_empty() {
            0
        } else {
            self.tone_ch_base()
                + SCC_VOICES * self.scc_ids.len() as i32
                + AY_VOICES * self.ay_ids.len() as i32
                + NES_CHANNELS * self.nes_ids.len() as i32
                + if !self.huc6280_ids.is_empty() { HUC6280_CHANNELS } else { 0 }
                + 4 * self.gb_ids.len() as i32
                + VSU_CHANNELS * self.vsu_ids.len() as i32
                + SAA_VOICES * self.saa_ids.len() as i32
                + POKEY_VOICES * self.pokey_ids.len() as i32
        };
        if !self.mikey_ids.is_empty() {
            let mut at: IndexMap<i32, i32> = IndexMap::new();
            for (i, cid) in self.mikey_ids.iter().enumerate() {
                at.insert(*cid, mikey_base + MIKEY_VOICES * i as i32);
            }
            let mut song = self.take_song();
            self.mikey_count = super::_emit_mikey_notes(
                &mut song,
                &mikey_hits,
                &mikey_offs,
                &at,
                vgm.total_samples,
            );
            self.put_song(song);
            let total = vgm.total_samples;
            for (i, smp) in self.mikey_samples.clone().iter().enumerate() {
                let slot = (i as i32) % 120;
                let ins_slot = (i as i32) / 120;
                let duration =
                    py_round(smp.frames() as f64 / smp.rate as f64 * VGM_RATE as f64) as i64;
                                for hit in &smp.hits {
                    let start = hit.time;
                    let end = (start + duration)
                        .min(if total != 0 { total } else { start + duration });
                    let ev = NoteEvent {
                        sample: start,
                        ch: at.get(&smp.chip_id).copied().unwrap_or(mikey_base) + hit.voice,
                        on: true,
                        note: 60 + slot,
                        ins: -1,
                        vol: 127,
                        e5: 0x80,
                        pan: hit.pan,
                        pcm: true,
                        end,
                        ..NoteEvent::default()
                    };
                    self.events.push(ev);
                    self.mikey_sample_events.push((self.events.len() - 1, ins_slot));
                    self.mikey_count += 1;
                    let ch = self.events.last().unwrap().ch;
                    if end != 0 && end < total {
                        self.events.push(NoteEvent {
                            sample: end,
                            ch,
                            on: false,
                            note: 180,
                            ins: -1,
                            vol: -1,
                            e5: 0x80,
                            pan: 0,
                            pcm: true,
                            ..NoteEvent::default()
                        });
                    }
                }
            }
        }
        let _ = (mikey_clock, n_mikey_ins);

        // X1-010
        let (x1_wave_hits, mut x1_pcm_hits, x1_offs, x1_warn) =
            crate::x1_010::collect_x1_010(vgm);
        self.warnings.extend(x1_warn);
        if !self.opts.pcm {
            x1_pcm_hits = Vec::new();
        }
        let _x1_clock = if vgm.x1_010_clock != 0 { vgm.x1_010_clock } else { X1_010_DEFAULT_CLOCK };
        let mut ids: Vec<i32> = x1_wave_hits
            .iter()
            .map(|h| h.chip_id)
            .chain(x1_pcm_hits.iter().map(|h| h.chip_id))
            .collect();
        ids.sort();
        ids.dedup();
        self.x1_ids = dual_ids(&ids, vgm.x1_010_dual);
        if vgm.x1_010_dual {
            self.warnings.push(
                "X1-010 header declares a second chip, but VGM command 0xC8 has no instance bit; all writes play on the first chip".into(),
            );
        }
        let mut wave_map: BTreeMap<Vec<u8>, i32> = BTreeMap::new();
        let mut env_map: BTreeMap<Vec<u8>, i32> = BTreeMap::new();
        let bank_wave = |body: Vec<u8>,
                             waves: &mut Vec<Vec<u8>>,
                             map: &mut BTreeMap<Vec<u8>, i32>| -> i32 {
            if let Some(i) = map.get(&body) {
                return *i;
            }
            let i = waves.len() as i32;
            map.insert(body.clone(), i);
            waves.push(body);
            i
        };
        for h in &x1_wave_hits {
            bank_wave(h.wave_data.clone(), &mut self.x1_waves, &mut wave_map);
            bank_wave(h.env_data.clone(), &mut self.x1_envs, &mut env_map);
            for (_t, body) in &h.wave_pts {
                bank_wave(body.clone(), &mut self.x1_waves, &mut wave_map);
            }
            for (_t, body) in &h.env_pts {
                bank_wave(body.clone(), &mut self.x1_envs, &mut env_map);
            }
        }
        if x1_wave_hits.iter().any(|h| h.env_period == 0) {
            let const_env = vec![0xFFu8; 128];
            bank_wave(const_env, &mut self.x1_envs, &mut env_map);
        }
        let mut sample_index: BTreeMap<Vec<u8>, i32> = BTreeMap::new();
        for h in &x1_pcm_hits {
            if !sample_index.contains_key(&h.data) {
                let i = self.x1_samples.len() as i32;
                sample_index.insert(h.data.clone(), i);
                self.x1_samples.push(h.clone());
            }
        }
        let x1_mem: usize = self
            .x1_samples
            .iter()
            .map(|s| (s.data.len() + 4095) & !4095)
            .sum();
        if x1_mem > 0x100000 {
            self.warnings.push(format!(
                "X1-010 sample bodies need {x1_mem} bytes; the module's 1 MB memory truncates the last bodies"
            ));
        }
        if self.x1_ids.is_empty() {
            return;
        }
        let x1_base = self.tone_ch_base()
            + SCC_VOICES * self.scc_ids.len() as i32
            + AY_VOICES * self.ay_ids.len() as i32
            + NES_CHANNELS * self.nes_ids.len() as i32
            + if !self.huc6280_ids.is_empty() { HUC6280_CHANNELS } else { 0 }
            + 4 * self.gb_ids.len() as i32
            + VSU_CHANNELS * self.vsu_ids.len() as i32
            + SAA_VOICES * self.saa_ids.len() as i32
            + POKEY_VOICES * self.pokey_ids.len() as i32
            + MIKEY_VOICES * self.mikey_ids.len() as i32;
        let mut at: IndexMap<i32, i32> = IndexMap::new();
        for (i, cid) in self.x1_ids.iter().enumerate() {
            at.insert(*cid, x1_base + X1_CHANNELS * i as i32);
        }
        // Wavetable notes: emit through the shared helper.
        let mut wave_map_i: IndexMap<Vec<u8>, i32> = wave_map.into_iter().collect();
        let mut env_map_i: IndexMap<Vec<u8>, i32> = env_map.into_iter().collect();
        let mut song = self.take_song();
        self.x1_count = super::_emit_x1_wave_notes(
            &mut song,
            &x1_wave_hits,
            &x1_offs,
            &at,
            &wave_map_i,
            &env_map_i,
            vgm.total_samples,
        );
        self.put_song(song);
        let _ = (&mut wave_map_i, &mut env_map_i);
        let total = vgm.total_samples;
        for h in &x1_pcm_hits {
            let idx = sample_index[&h.data];
            let slot = idx % 120;
            let ins_slot = idx / 120;
            let ch = at.get(&h.chip_id).copied().unwrap_or(x1_base) + h.voice as i32;
            let natural_end = h.sample_time
                + py_round(h.data.len() as f64 / h.rate() as f64 * VGM_RATE as f64) as i64;
            let end = h
                .off_sample
                .min(natural_end)
                .min(if total != 0 { total } else { natural_end });
            let left = (h.pan >> 4) & 0x0F;
            let right = h.pan & 0x0F;
            let ev = NoteEvent {
                sample: h.sample_time,
                ch,
                on: true,
                note: 60 + slot,
                ins: -1,
                vol: left.max(right),
                e5: 0x80,
                pan: pcm_pan_fx(left, right),
                pcm: true,
                end,
                vol_pts: h
                    .lr_pts
                    .iter()
                    .filter(|(t, _l, _r)| h.sample_time < *t && *t < end)
                    .map(|(t, l, r)| (*t, (*l).max(*r), Some(pcm_pan_fx(*l, *r))))
                    .collect(),
                ..NoteEvent::default()
            };
            self.events.push(ev);
            self.x1_sample_events.push((self.events.len() - 1, ins_slot));
            self.x1_count += 1;
            self.channel_fx.push((h.sample_time, ch, 0x20, h.freq));
            for (t, freq) in &h.freq_pts {
                self.channel_fx.push((*t, ch, 0x20, *freq));
            }
            if end != 0 && end < total {
                self.events.push(NoteEvent {
                    sample: end,
                    ch,
                    on: false,
                    note: 180,
                    ins: -1,
                    vol: -1,
                    e5: 0x80,
                    pan: 0,
                    pcm: true,
                    ..NoteEvent::default()
                });
            }
        }
    }
}


impl<'a> AnalyzeState<'a> {
    fn run_fds_ws(&mut self) {
        let vgm = self.vgm;
        let fds_clock = if vgm.nes_clock != 0 { vgm.nes_clock } else { FDS_DEFAULT_CLOCK };
        let (fds_hits, fds_warn) = crate::fds::collect_fds(vgm);
        self.warnings.extend(fds_warn);
        if !fds_hits.is_empty() {
            let mut ids: Vec<i32> = fds_hits.iter().map(|h| h.chip_id).collect();
            ids.sort();
            ids.dedup();
            self.fds_ids = dual_ids(&ids, vgm.nes_dual);
            let base = self.tone_ch_base()
                + SCC_VOICES * self.scc_ids.len() as i32
                + AY_VOICES * self.ay_ids.len() as i32
                + NES_CHANNELS * self.nes_ids.len() as i32
                + if !self.huc6280_ids.is_empty() { HUC6280_CHANNELS } else { 0 }
                + 4 * self.gb_ids.len() as i32
                + VSU_CHANNELS * self.vsu_ids.len() as i32
                + SAA_VOICES * self.saa_ids.len() as i32
                + POKEY_VOICES * self.pokey_ids.len() as i32
                + MIKEY_VOICES * self.mikey_ids.len() as i32
                + X1_CHANNELS * self.x1_ids.len() as i32;
            let mut at: IndexMap<i32, i32> = IndexMap::new();
            for (i, cid) in self.fds_ids.iter().enumerate() {
                at.insert(*cid, base + i as i32);
            }
            let mut wave_index: BTreeMap<Vec<u8>, i32> = BTreeMap::new();
            let mut mod_index: BTreeMap<Vec<u8>, i32> = BTreeMap::new();
            for h in &fds_hits {
                let wi = match wave_index.get(&h.wave) {
                    Some(i) => *i,
                    None => {
                        let i = self.fds_waves.len() as i32;
                        wave_index.insert(h.wave.clone(), i);
                        self.fds_waves.push(h.wave.clone());
                        i
                    }
                };
                let mi = match mod_index.get(&h.mod_table) {
                    Some(i) => *i,
                    None => {
                        let i = self.fds_mod_waves.len() as i32;
                        mod_index.insert(h.mod_table.clone(), i);
                        self.fds_mod_waves.push(h.mod_table.clone());
                        i
                    }
                };
                let ch = at.get(&h.chip_id).copied().unwrap_or(base);
                let (note, e5) = crate::pitch::furnace_frequency_note(
                    fds_clock as f64,
                    FDS_FREQ_DIV as f64,
                    h.freq.max(1),
                    FDS_MAX_FREQ,
                );
                let mut wave_pts_list: Vec<(i64, i32)> = Vec::new();
                for (t, body) in &h.wave_pts {
                    let idx = match wave_index.get(body) {
                        Some(i) => *i,
                        None => {
                            let i = self.fds_waves.len() as i32;
                            wave_index.insert(body.clone(), i);
                            self.fds_waves.push(body.clone());
                            i
                        }
                    };
                    wave_pts_list.push((*t, idx));
                }
                self.events.push(NoteEvent {
                    sample: h.sample_time,
                    ch,
                    on: true,
                    note,
                    ins: -1,
                    vol: h.volume.clamp(0, 32),
                    e5,
                    pan: 0,
                    end: h.off_sample,
                    vol_pts: h.vol_pts.iter().map(|(t, v)| (*t, (*v).clamp(0, 32), None)).collect(),
                    tone_pitch_pts: h.freq_pts.clone(),
                    tone_chip: 12,
                    tone_clock: fds_clock,
                    tone_div: FDS_FREQ_DIV,
                    tone_period: h.freq,
                    tone_wave: wi,
                    tone_wave_pts: wave_pts_list,
                    tone_fds_mod: mi,
                    tone_fds_depth: h.mod_depth,
                    tone_fds_speed: h.mod_speed,
                    tone_fds_pos: h.mod_pos,
                    ..NoteEvent::default()
                });
                for (t, cmd, val) in &h.mod_pts {
                    match cmd {
                        0x11 => self.channel_fx.push((*t, ch, 0x11, val & 0x3F)),
                        0x12 => self.channel_fx.push((
                            *t,
                            ch,
                            0x12,
                            (if h.mod_depth != 0 { 0x10 } else { 0 }) | (val & 0x0F),
                        )),
                        0x13 => self.channel_fx.push((*t, ch, 0x13, val & 0xFF)),
                        0x14 => self.channel_fx.push((*t, ch, 0x14, val & 0x7F)),
                        _ => {}
                    }
                }
                self.fds_count += 1;
                if h.off_sample != 0
                    && (vgm.total_samples == 0 || h.off_sample < vgm.total_samples)
                {
                    self.events.push(NoteEvent {
                        sample: h.off_sample,
                        ch,
                        on: false,
                        note: 180,
                        ins: -1,
                        vol: -1,
                        e5: 0x80,
                        pan: 0,
                        ..NoteEvent::default()
                    });
                }
            }
        }

        // WonderSwan
        let ws_clock = if vgm.wonderswan_clock != 0 { vgm.wonderswan_clock } else { WSWAN_DEFAULT_CLOCK };
        let (ws_hits, ws_pcm, ws_warn) = crate::wonderswan::collect_wonderswan(vgm);
        self.warnings.extend(ws_warn);
        if !ws_hits.is_empty() || !ws_pcm.is_empty() {
            let mut ids: Vec<i32> = ws_hits
                .iter()
                .map(|h| h.chip_id)
                .chain(ws_pcm.iter().map(|s| s.chip_id))
                .collect();
            ids.sort();
            ids.dedup();
            self.wonderswan_ids = dual_ids(&ids, vgm.wonderswan_dual);
            let base = self.tone_ch_base()
                + SCC_VOICES * self.scc_ids.len() as i32
                + AY_VOICES * self.ay_ids.len() as i32
                + NES_CHANNELS * self.nes_ids.len() as i32
                + if !self.huc6280_ids.is_empty() { HUC6280_CHANNELS } else { 0 }
                + 4 * self.gb_ids.len() as i32
                + VSU_CHANNELS * self.vsu_ids.len() as i32
                + SAA_VOICES * self.saa_ids.len() as i32
                + POKEY_VOICES * self.pokey_ids.len() as i32
                + MIKEY_VOICES * self.mikey_ids.len() as i32
                + X1_CHANNELS * self.x1_ids.len() as i32
                + self.fds_ids.len() as i32;
            let mut at: IndexMap<i32, i32> = IndexMap::new();
            for (i, cid) in self.wonderswan_ids.iter().enumerate() {
                at.insert(*cid, base + WS_VOICES * i as i32);
            }
            let mut wave_index: BTreeMap<Vec<u8>, i32> = BTreeMap::new();
            for h in &ws_hits {
                let wi = match wave_index.get(&h.wave) {
                    Some(i) => *i,
                    None => {
                        let i = self.wonderswan_waves.len() as i32;
                        wave_index.insert(h.wave.clone(), i);
                        self.wonderswan_waves.push(h.wave.clone());
                        i
                    }
                };
                let ch = at.get(&h.chip_id).copied().unwrap_or(base) + h.voice;
                let (note, e5) = crate::pitch::furnace_period_note(
                    ws_clock as f64,
                    WS_PITCH_DIVIDER as f64,
                    h.period,
                    0,
                    WS_MAX_PERIOD,
                );
                let mut wave_pts_list: Vec<(i64, i32)> = Vec::new();
                for (t, body) in &h.wave_pts {
                    let idx = match wave_index.get(body) {
                        Some(i) => *i,
                        None => {
                            let i = self.wonderswan_waves.len() as i32;
                            wave_index.insert(body.clone(), i);
                            self.wonderswan_waves.push(body.clone());
                            i
                        }
                    };
                    wave_pts_list.push((*t, idx));
                }
                self.events.push(NoteEvent {
                    sample: h.sample_time,
                    ch,
                    on: true,
                    note,
                    ins: -1,
                    vol: 15,
                    e5,
                    pan: 0,
                    end: h.off_sample,
                    tone_pitch_pts: h.period_pts.clone(),
                    tone_chip: 13,
                    tone_clock: ws_clock,
                    tone_div: WS_PITCH_DIVIDER,
                    tone_period: h.period,
                    tone_wave: wi,
                    tone_wave_pts: wave_pts_list,
                    tone_pan_l: h.vol_l,
                    tone_pan_r: h.vol_r,
                    tone_amp_pts: h.vol_pts.clone(),
                    ..NoteEvent::default()
                });
                self.wonderswan_count += 1;
                if h.voice == 3 {
                    self.channel_fx.push((h.sample_time, ch, 0x11, h.noise));
                }
                for (t, noise) in &h.noise_pts {
                    self.channel_fx.push((*t, ch, 0x11, *noise));
                }
                if h.sweep_time != 0 || h.sweep_amount != 0 {
                    self.channel_fx.push((
                        h.sample_time,
                        ch,
                        0x12,
                        if h.sweep_time == 0 { 0 } else { (h.sweep_time + 1).min(255) },
                    ));
                    self.channel_fx
                        .push((h.sample_time, ch, 0x13, h.sweep_amount & 0xFF));
                }
                for (t, stime, amount) in &h.sweep_pts {
                    self.channel_fx.push((
                        *t,
                        ch,
                        0x12,
                        if *stime == 0 { 0 } else { (*stime + 1).min(255) },
                    ));
                    self.channel_fx.push((*t, ch, 0x13, amount & 0xFF));
                }
                if h.off_sample != 0
                    && (vgm.total_samples == 0 || h.off_sample < vgm.total_samples)
                {
                    self.events.push(NoteEvent {
                        sample: h.off_sample,
                        ch,
                        on: false,
                        note: 180,
                        ins: -1,
                        vol: -1,
                        e5: 0x80,
                        pan: 0,
                        ..NoteEvent::default()
                    });
                }
            }
            self.wonderswan_pcm_samples = ws_pcm.clone();
            for (si, smp) in ws_pcm.iter().enumerate() {
                let base_ch = at.get(&smp.chip_id).copied().unwrap_or(base) + 1;
                for hit in &smp.hits {
                    let ev = NoteEvent {
                        sample: hit.sample_time,
                        ch: base_ch,
                        on: true,
                        note: 108,
                        ins: -1,
                        vol: hit.volume,
                        e5: 0x80,
                        pan: (hit.pan_l << 4) | hit.pan_r,
                        pcm: true,
                        end: hit.off_sample,
                        vol_pts: hit
                            .vol_pts
                            .iter()
                            .map(|&(t, v, fx)| (t, v, Some(fx)))
                            .collect(),
                        ..NoteEvent::default()
                    };
                    self.events.push(ev);
                    self.wonderswan_pcm_events
                        .push((self.events.len() - 1, si as i32));
                    self.wonderswan_pcm_count += 1;
                    if hit.off_sample != 0
                        && (vgm.total_samples == 0 || hit.off_sample < vgm.total_samples)
                    {
                        self.events.push(NoteEvent {
                            sample: hit.off_sample,
                            ch: base_ch,
                            on: false,
                            note: 180,
                            ins: -1,
                            vol: -1,
                            e5: 0x80,
                            pan: 0,
                            pcm: true,
                            ..NoteEvent::default()
                        });
                    }
                }
            }
        }

    }
}


impl<'a> AnalyzeState<'a> {
    /// Python `analyze.py:12045` `tone_base`: the first pitched-chip column.
    /// It counts *channels* after the FM block and every sample engine, not
    /// the instrument slots the writer's `tone_ins_base` counts.
    fn tone_ch_base(&self) -> i32 {
        let oki_cids: BTreeSet<i32> = self.oki_samples.iter().map(|s| s.chip_id).collect();
        let mut adpcm_cids: Vec<i32> =
            self.adpcm_samples.iter().map(|s| s.chip_id).collect();
        adpcm_cids.sort();
        adpcm_cids.dedup();
        let adpcm_cids = dual_ids(&adpcm_cids, self.vgm.msm6258_dual);
        let c140_cids: BTreeSet<i32> = self
            .c140_audible
            .iter()
            .flat_map(|s| s.hits.iter().map(|h| h.chip_id))
            .collect();
        let c352_cids: BTreeSet<i32> = self
            .c352_audible
            .iter()
            .flat_map(|s| s.hits.iter().map(|h| h.chip_id))
            .collect();
        let k053_cids: BTreeSet<i32> = self
            .k053260_audible
            .iter()
            .flat_map(|s| s.hits.iter().map(|h| h.chip_id))
            .collect();
        self.fm_end
            + 2 * self.pcm_ids.len() as i32
            + 4 * oki_cids.len() as i32
            + adpcm_cids.len() as i32
            + 16 * self.spcm_ids.len() as i32
            + RF5C_VOICES * self.rf5c68_ids.len() as i32
            + RF5C_VOICES * self.rf5c164_ids.len() as i32
            + 24 * c140_cids.len() as i32
            + 32 * c352_cids.len() as i32
            + 4 * k053_cids.len() as i32
            + YMZ280B_VOICES * self.ymz_ids.len() as i32
            + MULTIPCM_VOICES * self.multipcm_ids.len() as i32
            + self.upd_ids.len() as i32
            + PWM_SIDES * self.pwm_ids.len() as i32
            + self.msm5205_ids.len() as i32
            + 4 * self.ga20_ids.len() as i32
            + 16 * self.c219_ids.len() as i32
            + 8 * self.k054539_ids.len() as i32
            + 8 * self.msm5232_ids.len() as i32
    }

    fn tone_base_now(&self) -> i32 {
        self.tone_ch_base()
            + SCC_VOICES * self.scc_ids.len() as i32
            + AY_VOICES * self.ay_ids.len() as i32
            + NES_CHANNELS * self.nes_ids.len() as i32
            + if !self.huc6280_ids.is_empty() { HUC6280_CHANNELS } else { 0 }
            + 4 * self.gb_ids.len() as i32
            + VSU_CHANNELS * self.vsu_ids.len() as i32
            + SAA_VOICES * self.saa_ids.len() as i32
            + POKEY_VOICES * self.pokey_ids.len() as i32
            + MIKEY_VOICES * self.mikey_ids.len() as i32
            + X1_CHANNELS * self.x1_ids.len() as i32
            + self.fds_ids.len() as i32
            + WS_VOICES * self.wonderswan_ids.len() as i32
    }

    fn run_qsound_k005289(&mut self) {
        // The SCC/AY block already ran in Python's order (before the NES);
        // QSound and the K005289 follow every tone family.
        let vgm = self.vgm;
        let (qs_pcm, qs_adpcm, qs_samples, qs_warn) = crate::qsound::collect_qsound(vgm);
        self.warnings.extend(qs_warn);
        if !qs_pcm.is_empty() || !qs_adpcm.is_empty() {
            let mut ids: Vec<i32> = qs_pcm
                .iter()
                .map(|h| h.chip_id)
                .chain(qs_adpcm.iter().map(|h| h.chip_id))
                .collect();
            ids.sort();
            ids.dedup();
            self.qsound_ids = ids;
            self.qsound_samples = qs_samples;
            let base = self.tone_base_now();
            let mut at: IndexMap<i32, i32> = IndexMap::new();
            for (i, cid) in self.qsound_ids.iter().enumerate() {
                at.insert(*cid, base + QSOUND_CHANNELS * i as i32);
            }
            let pan_fx = |raw: i32| -> (i32, i32) {
                let (linear, surround) = crate::qsound::qsound_pan(raw);
                let (l, r) = crate::qsound::qsound_pan_split(linear);
                ((l << 4) | r, surround)
            };
            for h in &qs_pcm {
                let ch = at.get(&h.chip_id).copied().unwrap_or(base) + h.voice as i32;
                let (note, e5) = crate::pitch::furnace_frequency_note(
                    QSOUND_OUT_RATE as f64,
                    QSOUND_FREQ_DIV as f64,
                    h.freq.max(1),
                    QSOUND_MAX_FREQ,
                );
                let (pfx, surround) = pan_fx(h.pan);
                self.events.push(NoteEvent {
                    sample: h.sample_time,
                    ch,
                    on: true,
                    note,
                    ins: -1,
                    vol: h.volume,
                    e5,
                    pan: pfx,
                    pcm: true,
                    end: h.off_sample,
                    vol_pts: h.vol_pts.iter().map(|(t, v)| (*t, *v, None)).collect(),
                    tone_pitch_pts: h.freq_pts.clone(),
                    tone_chip: 14,
                    tone_clock: QSOUND_OUT_RATE,
                    tone_div: QSOUND_FREQ_DIV,
                    tone_period: h.freq,
                    tone_sample: h.sample.map(|x| x as i32).unwrap_or(-1),
                    tone_sample_kind: "pcm".into(),
                    ..NoteEvent::default()
                });
                self.channel_fx.push((h.sample_time, ch, 0x08, pfx));
                self.channel_fx.push((h.sample_time, ch, 0x12, surround));
                self.channel_fx
                    .push((h.sample_time, ch, 0x11, crate::qsound::_echo_fx(h.echo)));
                for (t, raw) in &h.pan_pts {
                    let (pfx, sur) = pan_fx(*raw);
                    self.channel_fx.push((*t, ch, 0x08, pfx));
                    self.channel_fx.push((*t, ch, 0x12, sur));
                }
                for (t, raw) in &h.echo_pts {
                    self.channel_fx
                        .push((*t, ch, 0x11, crate::qsound::_echo_fx(*raw)));
                }
                self.qsound_count += 1;
                if h.off_sample != 0
                    && (vgm.total_samples == 0 || h.off_sample < vgm.total_samples)
                {
                    self.events.push(NoteEvent {
                        sample: h.off_sample,
                        ch,
                        on: false,
                        note: 180,
                        ins: -1,
                        vol: -1,
                        e5: 0x80,
                        pan: 0,
                        pcm: true,
                        ..NoteEvent::default()
                    });
                }
            }
            for h in &qs_adpcm {
                let ch = at.get(&h.chip_id).copied().unwrap_or(base) + 16 + h.voice as i32;
                let (pfx, surround) = pan_fx(h.pan);
                self.events.push(NoteEvent {
                    sample: h.sample_time,
                    ch,
                    on: true,
                    note: 60,
                    ins: -1,
                    vol: h.volume,
                    e5: 0x80,
                    pan: pfx,
                    pcm: true,
                    end: h.off_sample,
                    vol_pts: h.vol_pts.iter().map(|(t, v)| (*t, *v, None)).collect(),
                    tone_chip: 15,
                    tone_sample: h.sample.map(|x| x as i32).unwrap_or(-1),
                    tone_sample_kind: "adpcm".into(),
                    ..NoteEvent::default()
                });
                self.channel_fx.push((h.sample_time, ch, 0x08, pfx));
                self.channel_fx.push((h.sample_time, ch, 0x12, surround));
                for (t, raw) in &h.pan_pts {
                    let (pfx, sur) = pan_fx(*raw);
                    self.channel_fx.push((*t, ch, 0x08, pfx));
                    self.channel_fx.push((*t, ch, 0x12, sur));
                }
                self.qsound_adpcm_count += 1;
                if h.off_sample != 0
                    && (vgm.total_samples == 0 || h.off_sample < vgm.total_samples)
                {
                    self.events.push(NoteEvent {
                        sample: h.off_sample,
                        ch,
                        on: false,
                        note: 180,
                        ins: -1,
                        vol: -1,
                        e5: 0x80,
                        pan: 0,
                        pcm: true,
                        ..NoteEvent::default()
                    });
                }
            }
            let qs_fx_ch = at.get(&self.qsound_ids[0]).copied().unwrap_or(base);
            for w in vgm.writes.iter() {
                if w.chip != "qsound" || w.chip_id as i32 != self.qsound_ids[0] {
                    continue;
                }
                if w.reg == 0x93 {
                    self.channel_fx.push((
                        w.sample,
                        qs_fx_ch,
                        0x10,
                        crate::qsound::_feedback_fx(w.val as i32),
                    ));
                } else if w.reg == 0xD9 {
                    let delay = crate::qsound::_delay_fx(w.val as i32);
                    self.channel_fx.push((
                        w.sample,
                        qs_fx_ch,
                        0x30 | ((delay >> 8) & 0x0F),
                        delay & 0xFF,
                    ));
                }
            }
        }

        // K005289
        let (k_notes, k_offs, k_wave_list, k_warn) = crate::k005289::collect_k005289(vgm);
        self.warnings.extend(k_warn);
        if !k_notes.is_empty() {
            let mut ids: Vec<i32> = k_notes.iter().map(|n| n.hit.chip_id).collect();
            ids.sort();
            ids.dedup();
            self.k005289_ids = ids;
            self.k005289_waves = k_wave_list;
            let mut wave_index: BTreeMap<Vec<u8>, i32> = self
                .k005289_waves
                .iter()
                .enumerate()
                .map(|(i, w)| (w.clone(), i as i32))
                .collect();
            let base = self.tone_base_now()
                + QSOUND_CHANNELS * self.qsound_ids.len() as i32;
            let mut at: IndexMap<i32, i32> = IndexMap::new();
            for (i, cid) in self.k005289_ids.iter().enumerate() {
                at.insert(*cid, base + K005289_VOICES * i as i32);
            }
            let clock = if vgm.k005289_clock != 0 {
                vgm.k005289_clock
            } else {
                K005289_DEFAULT_CLOCK
            };
            for n in &k_notes {
                let h = &n.hit;
                let ch = match at.get(&h.chip_id) {
                    Some(c) => *c,
                    None => continue,
                };
                if !h.wave.iter().any(|x| *x != 0) {
                    continue;
                }
                let gate_end = if h.off_sample != 0 { h.off_sample } else { vgm.total_samples };
                let mut pts: Vec<(i64, i32)> = vec![(h.sample_time, h.period)];
                pts.extend(h.freq_pts.iter().copied());
                let segs = super::segment_periods(&pts, gate_end, clock as f64, K005289_DIVIDER as f64, 1, super::_NOTE_HOLD_SAMPLES, super::_NOTE_HOLD_SAMPLES, true, 0xFFF);
                for (i, (st, period, intra)) in segs.iter().enumerate() {
                    let nxt = if i + 1 < segs.len() { segs[i + 1].0 } else { 0 };
                    let seg_end = if nxt != 0 { nxt } else { h.off_sample };
                    let wave = Self::value_at_bytes(&h.wave_pts, *st, &h.wave);
                    if !wave.iter().any(|x| *x != 0) {
                        self.events.push(NoteEvent {
                            sample: *st,
                            ch: ch + h.voice,
                            on: false,
                            note: 180,
                            ins: -1,
                            vol: -1,
                            e5: 0x80,
                            pan: 0,
                            ..NoteEvent::default()
                        });
                        continue;
                    }
                    let bound = if nxt != 0 { nxt } else { gate_end };
                    let mut vol = super::_value_at(&h.vol_pts, *st, h.vol);
                    let mut vol_pts: Vec<(i64, i32, Option<i32>)> = h
                        .vol_pts
                        .iter()
                        .filter(|(t, _)| *st < *t && *t < bound)
                        .map(|(t, v)| (*t, *v, None))
                        .collect();
                    if let Some((first_t, first_v, _)) = vol_pts.first().copied() {
                        if first_t <= *st + crate::k005289::K005289_REFIT_SAMPLES && first_v > vol {
                            vol = first_v;
                            vol_pts.remove(0);
                        }
                    }
                    let steps = super::_tone_steps(
                        intra,
                        *st,
                        if i == 0 { crate::k005289::K005289_REFIT_SAMPLES } else { 0 },
                        600,
                    );
                    let mut wave_steps: Vec<(i64, i32)> = Vec::new();
                    let wpts: Vec<(i64, Vec<u8>)> =
                        h.wave_pts.iter().filter(|(t, _)| *st < *t && *t < bound).cloned().collect();
                    for (t, img) in Self::merge_step_writes_bytes(wpts, 64) {
                        let idx = match wave_index.get(&img) {
                            Some(i) => *i,
                            None => {
                                let i = self.k005289_waves.len() as i32;
                                wave_index.insert(img.clone(), i);
                                self.k005289_waves.push(img);
                                i
                            }
                        };
                        if wave_steps.last().map(|p| p.1) == Some(idx) {
                            continue;
                        }
                        wave_steps.push((t, idx));
                    }
                    let wi = match wave_index.get(&wave) {
                        Some(i) => *i,
                        None => {
                            let i = self.k005289_waves.len() as i32;
                            wave_index.insert(wave.clone(), i);
                            self.k005289_waves.push(wave.clone());
                            i
                        }
                    };
                    let (note, e5) = crate::k005289::k005289_note(clock, *period);
                    self.events.push(NoteEvent {
                        sample: *st,
                        ch: ch + h.voice,
                        on: true,
                        note,
                        ins: self.tone_base,
                        vol,
                        e5,
                        pan: 0,
                        end: seg_end,
                        vol_pts,
                        tone_pitch_pts: steps,
                        tone_wave_pts: wave_steps,
                        tone_wave: wi,
                        tone_period_offset: 1,
                        tone_period: *period,
                        tone_clock: clock,
                        tone_div: K005289_DIVIDER,
                        tone_chip: 16,
                        ..NoteEvent::default()
                    });
                    self.k005289_count += 1;
                }
            }
            for off in &k_offs {
                let b = match at.get(&off.chip_id) {
                    Some(b) => *b,
                    None => continue,
                };
                self.events.push(NoteEvent {
                    sample: off.sample_time,
                    ch: b + off.voice,
                    on: false,
                    note: 180,
                    ins: -1,
                    vol: -1,
                    e5: 0x80,
                    pan: 0,
                    ..NoteEvent::default()
                });
            }
        }
    }

    fn run_scc_ay(&mut self) {
        let vgm = self.vgm;
        let (scc_notes, scc_offs, scc_wave_list, scc_warn) = crate::scc::collect_scc(vgm);
        self.warnings.extend(scc_warn);
        if !scc_notes.is_empty() {
            let mut ids: Vec<i32> = scc_notes.iter().map(|n| n.hit.chip_id).collect();
            ids.sort();
            ids.dedup();
            self.scc_ids = dual_ids(&ids, vgm.k051649_dual);
            self.scc_waves = scc_wave_list;
            let mut wave_index: BTreeMap<Vec<u8>, i32> = self
                .scc_waves
                .iter()
                .enumerate()
                .map(|(i, w)| (w.clone(), i as i32))
                .collect();
            let base = self.tone_ch_base();
            let mut at: IndexMap<i32, i32> = IndexMap::new();
            for (i, cid) in self.scc_ids.iter().enumerate() {
                at.insert(*cid, base + SCC_VOICES * i as i32);
            }
            let clock = if vgm.k051649_clock != 0 {
                vgm.k051649_clock
            } else {
                SCC_DEFAULT_CLOCK
            };
            for n in &scc_notes {
                let h = &n.hit;
                let ch = match at.get(&h.chip_id) {
                    Some(c) => *c,
                    None => continue,
                };
                if h.period <= SCC_MIN_PERIOD || !h.wave.iter().any(|x| *x != 0) {
                    continue;
                }
                let gate_end = if h.off_sample != 0 { h.off_sample } else { vgm.total_samples };
                let mut pts: Vec<(i64, i32)> = vec![(h.sample_time, h.period)];
                pts.extend(h.freq_pts.iter().copied());
                let segs = super::segment_periods(&pts, gate_end, clock as f64, 16.0, 1, super::_NOTE_HOLD_SAMPLES, super::_NOTE_HOLD_SAMPLES, true, 0xFFF);
                for (i, (st, period, intra)) in segs.iter().enumerate() {
                    let nxt = if i + 1 < segs.len() { segs[i + 1].0 } else { 0 };
                    let seg_end = if nxt != 0 { nxt } else { h.off_sample };
                    let wave = Self::value_at_bytes(&h.wave_pts, *st, &h.wave);
                    if *period <= SCC_MIN_PERIOD || !wave.iter().any(|x| *x != 0) {
                        self.events.push(NoteEvent {
                            sample: *st,
                            ch: ch + h.voice,
                            on: false,
                            note: 180,
                            ins: -1,
                            vol: -1,
                            e5: 0x80,
                            pan: 0,
                            ..NoteEvent::default()
                        });
                        continue;
                    }
                    let bound = if nxt != 0 { nxt } else { gate_end };
                    let vol = super::_value_at(&h.vol_pts, *st, h.vol);
                    let vol_pts: Vec<(i64, i32, Option<i32>)> = h
                        .vol_pts
                        .iter()
                        .filter(|(t, _)| *st < *t && *t < bound)
                        .map(|(t, v)| (*t, *v, None))
                        .collect();
                    let steps = super::_tone_steps(
                        intra,
                        *st,
                        if i == 0 { crate::scc::SCC_REFIT_SAMPLES } else { 0 },
                        600,
                    );
                    let mut wave_steps: Vec<(i64, i32)> = Vec::new();
                    let wpts: Vec<(i64, Vec<u8>)> =
                        h.wave_pts.iter().filter(|(t, _)| *st < *t && *t < bound).cloned().collect();
                    for (t, img) in Self::merge_step_writes_bytes(wpts, 64) {
                        let idx = match wave_index.get(&img) {
                            Some(i) => *i,
                            None => {
                                let i = self.scc_waves.len() as i32;
                                wave_index.insert(img.clone(), i);
                                self.scc_waves.push(img);
                                i
                            }
                        };
                        if wave_steps.last().map(|p| p.1) == Some(idx) {
                            continue;
                        }
                        wave_steps.push((t, idx));
                    }
                    let wi = match wave_index.get(&wave) {
                        Some(i) => *i,
                        None => {
                            let i = self.scc_waves.len() as i32;
                            wave_index.insert(wave.clone(), i);
                            self.scc_waves.push(wave.clone());
                            i
                        }
                    };
                    let (note, e5) = crate::scc::scc_note(clock, *period);
                    self.events.push(NoteEvent {
                        sample: *st,
                        ch: ch + h.voice,
                        on: true,
                        note,
                        ins: self.tone_base,
                        vol,
                        e5,
                        pan: 0,
                        end: seg_end,
                        vol_pts,
                        tone_pitch_pts: steps,
                        tone_wave_pts: wave_steps,
                        tone_wave: wi,
                        tone_period_offset: 1,
                        tone_chip: 1,
                        ..NoteEvent::default()
                    });
                    self.scc_count += 1;
                }
            }
            for off in &scc_offs {
                let b = match at.get(&off.chip_id) {
                    Some(b) => *b,
                    None => continue,
                };
                self.events.push(NoteEvent {
                    sample: off.sample_time,
                    ch: b + off.voice,
                    on: false,
                    note: 180,
                    ins: -1,
                    vol: -1,
                    e5: 0x80,
                    pan: 0,
                    ..NoteEvent::default()
                });
            }
        }
        self.run_ay_ssg();
    }
}


impl<'a> AnalyzeState<'a> {
    fn run_ay_ssg(&mut self) {
        let vgm = self.vgm;
        let ay_clock = if vgm.ay8910_clock != 0 { vgm.ay8910_clock } else { AY_DEFAULT_CLOCK };
        let (ay_hits, ay_offs, ay_warn) =
            crate::ay8910::collect_ay8910_with(vgm, "ay8910", Some(ay_clock), 8.0);
        self.warnings.extend(ay_warn);
        let nes_clk = if vgm.nes_clock != 0 { vgm.nes_clock } else { NES_DEFAULT_CLOCK };
        let ay_half = if self.has_nes {
            ay_clock.abs_diff(nes_clk) <= 16
        } else {
            (vgm.ay8910_flags & 0x10) != 0
        };
        let ay_note_clock = if ay_half { ay_clock / 2 } else { ay_clock };
        let mut ay_pcm = Vec::new();
        let mut ay_pcm_hits = Vec::new();
        if self.opts.pcm {
            let (mut got, mut hits, warn) = crate::ssgpcm::collect_ssg_pcm(vgm, "ay8910");
            self.warnings.extend(warn);
            if self.opts.lossy_samples {
                let before = got.len();
                let (a, b) = crate::ssgpcm::cluster_ssg_pcm(got, hits, 32, 16);
                got = a;
                hits = b;
                if got.len() < before {
                    self.warnings.push(format!(
                        "ay8910: lossy sample clustering {before} -> {} bodies",
                        got.len()
                    ));
                }
            }
            ay_pcm = got;
            ay_pcm_hits = hits;
        }
        let mut ay_cids: Vec<i32> = ay_hits
            .iter()
            .map(|h| h.chip_id)
            .chain(ay_pcm_hits.iter().map(|h| h.chip_id as i32))
            .collect();
        ay_cids.sort();
        ay_cids.dedup();
        if !ay_cids.is_empty() {
            for cid in &ay_cids {
                self.ay_ids.push(*cid);
            }
            self.ay_ids = dual_ids(&self.ay_ids, vgm.ay8910_dual);
            let ay_base = self.tone_ch_base() + SCC_VOICES * self.scc_ids.len() as i32;
            let mut at: IndexMap<i32, i32> = IndexMap::new();
            for (i, cid) in self.ay_ids.iter().enumerate() {
                at.insert(*cid, ay_base + AY_VOICES * i as i32);
            }
            if !ay_hits.is_empty() {
                let mut song = self.take_song();
                self.ay_count = super::_emit_ay_notes(
                    &mut song,
                    &ay_hits,
                    &ay_offs,
                    &at,
                    ay_note_clock,
                    8.0,
                    self.tone_base + self.scc_ins.len() as i32,
                    vgm.total_samples,
                );
                self.put_song(song);
            }
            self.emit_pcm(ay_pcm, ay_pcm_hits, &at);
        }

        // YM2203 SSG
        let (ssg_pcm, ssg_hits) = if self.opts.pcm {
            let (mut got, mut hits, warn) = crate::ssgpcm::collect_ssg_pcm(vgm, "ym2203_ssg");
            self.warnings.extend(warn);
            if self.opts.lossy_samples {
                let before = got.len();
                let (a, b) = crate::ssgpcm::cluster_ssg_pcm(got, hits, 32, 16);
                got = a;
                hits = b;
                if got.len() < before {
                    self.warnings.push(format!(
                        "ym2203_ssg: lossy sample clustering {before} -> {} bodies",
                        got.len()
                    ));
                }
            }
            (got, hits)
        } else {
            (Vec::new(), Vec::new())
        };
        let (ssg_notes, ssg_offs, ssg_warn) =
            crate::ay8910::collect_ay8910_with(vgm, "ym2203_ssg", Some(self.ym_clock), YM2203_SSG_DIVIDER as f64);
        self.warnings.extend(ssg_warn);
        if (!ssg_notes.is_empty() || !ssg_hits.is_empty()) && !self.ym_ids.is_empty() {
            let mut at: IndexMap<i32, i32> = IndexMap::new();
            for cid in &self.ym_ids {
                at.insert(*cid, self.ym_base[cid] + if self.ym2203_ext { 6 } else { 3 });
            }
            if !ssg_notes.is_empty() {
                let mut song = self.take_song();
                self.ym2203_ssg_count = super::_emit_ay_notes(
                    &mut song,
                    &ssg_notes,
                    &ssg_offs,
                    &at,
                    self.ym_clock,
                    16.0,
                    self.tone_base + self.scc_ins.len() as i32,
                    vgm.total_samples,
                );
                self.put_song(song);
                self.ay_count += self.ym2203_ssg_count;
            }
            self.emit_pcm(ssg_pcm, ssg_hits, &at);
        }

        // YM2610 SSG
        let (ssg_pcm, ssg_hits) = if self.opts.pcm {
            let (mut got, mut hits, warn) = crate::ssgpcm::collect_ssg_pcm(vgm, "ym2610_ssg");
            self.warnings.extend(warn);
            if self.opts.lossy_samples {
                let before = got.len();
                let (a, b) = crate::ssgpcm::cluster_ssg_pcm(got, hits, 32, 16);
                got = a;
                hits = b;
                if got.len() < before {
                    self.warnings.push(format!(
                        "ym2610_ssg: lossy sample clustering {before} -> {} bodies",
                        got.len()
                    ));
                }
            }
            (got, hits)
        } else {
            (Vec::new(), Vec::new())
        };
        let (ssg_notes, ssg_offs, ssg_warn) =
            crate::ay8910::collect_ay8910_with(vgm, "ym2610_ssg", Some(self.ym2610_clock), YM2610_SSG_DIVIDER as f64);
        self.warnings.extend(ssg_warn);
        if (!ssg_notes.is_empty() || !ssg_hits.is_empty()) && !self.ym2610_ids.is_empty() {
            let mut at: IndexMap<i32, i32> = IndexMap::new();
            for cid in &self.ym2610_ids {
                at.insert(*cid, self.ym_base[cid] + self.ym2610_fm_count);
            }
            if !ssg_notes.is_empty() {
                let mut song = self.take_song();
                let n = super::_emit_ay_notes(
                    &mut song,
                    &ssg_notes,
                    &ssg_offs,
                    &at,
                    self.ym2610_clock,
                    YM2610_SSG_DIVIDER as f64,
                    self.tone_base + self.scc_ins.len() as i32,
                    vgm.total_samples,
                );
                self.put_song(song);
                self.ym2610_ssg_count = n;
                self.ay_count += n;
            }
            self.emit_pcm(ssg_pcm, ssg_hits, &at);
        }

        // YM2608 SSG
        let (ssg_pcm, ssg_hits) = if self.opts.pcm {
            let (mut got, mut hits, warn) = crate::ssgpcm::collect_ssg_pcm(vgm, "ym2608_ssg");
            self.warnings.extend(warn);
            if self.opts.lossy_samples {
                let before = got.len();
                let (a, b) = crate::ssgpcm::cluster_ssg_pcm(got, hits, 32, 16);
                got = a;
                hits = b;
                if got.len() < before {
                    self.warnings.push(format!(
                        "ym2608_ssg: lossy sample clustering {before} -> {} bodies",
                        got.len()
                    ));
                }
            }
            (got, hits)
        } else {
            (Vec::new(), Vec::new())
        };
        let (ssg_notes, ssg_offs, ssg_warn) =
            crate::ay8910::collect_ay8910_with(vgm, "ym2608_ssg", Some(self.ym2608_clock), YM2608_SSG_DIVIDER as f64);
        self.warnings.extend(ssg_warn);
        if (!ssg_notes.is_empty() || !ssg_hits.is_empty()) && !self.ym2608_ids.is_empty() {
            let mut at: IndexMap<i32, i32> = IndexMap::new();
            for cid in &self.ym2608_ids {
                at.insert(*cid, self.ym_base[cid] + self.ym2608_fm_count);
            }
            if !ssg_notes.is_empty() {
                let mut song = self.take_song();
                let n = super::_emit_ay_notes(
                    &mut song,
                    &ssg_notes,
                    &ssg_offs,
                    &at,
                    self.ym2608_clock,
                    YM2608_SSG_DIVIDER as f64,
                    self.tone_base + self.scc_ins.len() as i32,
                    vgm.total_samples,
                );
                self.put_song(song);
                self.ym2608_ssg_count = n;
                self.ay_count += n;
            }
            self.emit_pcm(ssg_pcm, ssg_hits, &at);
        }
        self.md_ssg_pcm_trim();
    }

    fn emit_pcm(
        &mut self,
        got: Vec<crate::ssgpcm::SsgPcmSample>,
        hits: Vec<crate::ssgpcm::SsgPcmHit>,
        at: &IndexMap<i32, i32>,
    ) {
        if hits.is_empty() {
            return;
        }
        let base = self.ssg_pcm_samples.len() as i32;
        self.ssg_pcm_samples.extend(got);
        let mut song = self.take_song();
        let (n, bound) = super::_emit_ssg_pcm_notes(&mut song, &hits, at, base);
        self.put_song(song);
        self.ssg_pcm_count += n;
        self.ssg_pcm_events.extend(bound);
    }

    fn md_ssg_pcm_trim(&mut self) {
        if !self.ssg_pcm_events.is_empty() {
            let mut used: Vec<i32> = self.ssg_pcm_events.iter().map(|(_e, i)| *i).collect();
            used.sort();
            used.dedup();
            if used.len() != self.ssg_pcm_samples.len() {
                let mut remap: BTreeMap<i32, i32> = BTreeMap::new();
                for (new, old) in used.iter().enumerate() {
                    remap.insert(*old, new as i32);
                }
                let samples = self.ssg_pcm_samples.clone();
                self.ssg_pcm_samples = used.iter().map(|i| samples[*i as usize].clone()).collect();
                for (ev, idx) in self.ssg_pcm_events.iter_mut() {
                    *idx = remap[idx];
                    self.events[*ev].note = 60 + (*idx % 120);
                }
            }
        } else {
            self.ssg_pcm_samples = Vec::new();
        }
    }
}


impl<'a> AnalyzeState<'a> {
    fn partial_song(&self) -> Song {
        Song {
            vgm: self.vgm.clone(),
            t0: 0,
            loop_sample: self.vgm.loop_sample,
            speed: 0,
            hz: 0.0,
            fm_patches: Vec::new(),
            pcm_samples: Vec::new(),
            events: Vec::new(),
            warnings: self.warnings.clone(),
            oki_samples: Vec::new(),
            msm6258_samples: Vec::new(),
            segapcm_samples: Vec::new(),
            segapcm_ids: Vec::new(),
            rf5c68_samples: Vec::new(),
            rf5c164_samples: Vec::new(),
            c140_samples: Vec::new(),
            c219_samples: Vec::new(),
            c219_ids: Vec::new(),
            c219_count: 0,
            c352_samples: Vec::new(),
            k053260_samples: Vec::new(),
            k054539_samples: Vec::new(),
            k054539_ids: Vec::new(),
            k054539_count: 0,
            ymz280b_samples: Vec::new(),
            ymz280b_ids: Vec::new(),
            ymz280b_count: 0,
            multipcm_samples: Vec::new(),
            multipcm_ids: Vec::new(),
            multipcm_count: 0,
            upd7759_samples: Vec::new(),
            upd7759_ids: Vec::new(),
            upd7759_count: 0,
            pwm_samples: Vec::new(),
            pwm_ids: Vec::new(),
            pwm_count: 0,
            msm5205_samples: Vec::new(),
            msm5205_ids: Vec::new(),
            msm5205_count: 0,
            ga20_samples: Vec::new(),
            ga20_ids: Vec::new(),
            ga20_count: 0,
            msm5232_ids: Vec::new(),
            msm5232_controls: Vec::new(),
            msm5232_count: 0,
            es5503_samples: Vec::new(),
            es5503_ids: Vec::new(),
            es5503_oscs: Vec::new(),
            es5503_rate: 0,
            es5503_count: 0,
            channel_vol_events: Vec::new(),
            mikey_ids: Vec::new(),
            mikey_ins: Vec::new(),
            mikey_samples: Vec::new(),
            mikey_count: 0,
            x1_010_ids: Vec::new(),
            x1_010_ins: Vec::new(),
            x1_010_samples: Vec::new(),
            x1_010_count: 0,
            x1_waves: Vec::new(),
            x1_envs: Vec::new(),
            fds_ids: Vec::new(),
            fds_ins: Vec::new(),
            fds_waves: Vec::new(),
            fds_mod_waves: Vec::new(),
            fds_count: 0,
            wonderswan_ids: Vec::new(),
            wonderswan_ins: Vec::new(),
            wonderswan_waves: Vec::new(),
            wonderswan_pcm_samples: Vec::new(),
            wonderswan_count: 0,
            wonderswan_pcm_count: 0,
            qsound_ids: Vec::new(),
            qsound_ins: Vec::new(),
            qsound_samples: Vec::new(),
            qsound_count: 0,
            qsound_adpcm_count: 0,
            channel_fx: Vec::new(),
            chip_fx: Vec::new(),
            grid_note: String::new(),
            legato_steps: 0,
            legato_ramps: 0,
            row_subdiv: 1.0,
            hilight_a: 4,
            hilight_b: 16,
            fm_chip: String::new(),
            fm_systems: Vec::new(),
            has_fm_source: false,
            ym2151_ids: Vec::new(),
            ym2203_ids: Vec::new(),
            ym2203_ext: false,
            ym2203_ssg_count: 0,
            ym2612_ids: Vec::new(),
            ym2612_ext: false,
            ym2612_csm: false,
            opl_ids: Vec::new(),
            opl_drums: false,
            opll_ids: Vec::new(),
            opll_drums: false,
            opll_vrc7: false,
            fm_vol_max: 0,
            fm_notes: 0,
            drum_notes: 0,
            ym2610_ids: Vec::new(),
            ym2610_b: false,
            ym2610_ssg_count: 0,
            ym2610_a_samples: Vec::new(),
            ym2610_b_samples: Vec::new(),
            ym2610_a_count: 0,
            ym2610_b_count: 0,
            ym2608_ids: Vec::new(),
            ym2608_ext: false,
            ym2608_ssg_count: 0,
            ym2608_rhythm_count: 0,
            ym2608_rhythm_notes: 0,
            ym2608_b_samples: Vec::new(),
            ym2608_b_count: 0,
            y8950_b_samples: Vec::new(),
            y8950_b_count: 0,
            opl4_wave_samples: Vec::new(),
            opl4_wave_count: 0,
            k007232_volume: 0.0,
            loop_end_sample: None,
            loop_start_row: None,
            loop_end_row: None,
            loop_marker_sample: None,
            loop_cut_sample: None,
            c352_quad: false,
            scc_ids: Vec::new(),
            scc_waves: Vec::new(),
            scc_ins: Vec::new(),
            scc_count: 0,
            k005289_ids: Vec::new(),
            k005289_waves: Vec::new(),
            k005289_ins: Vec::new(),
            k005289_count: 0,
            ay_ids: Vec::new(),
            ay_ins: Vec::new(),
            ay_count: 0,
            pokey_ids: Vec::new(),
            pokey_ins: Vec::new(),
            pokey_count: 0,
            ssg_pcm_samples: Vec::new(),
            ssg_pcm_count: 0,
            nes_ids: Vec::new(),
            nes_present: false,
            nes_ins: Vec::new(),
            nes_dpcm_samples: Vec::new(),
            nes_pcm_samples: Vec::new(),
            nes_count: 0,
            nes_pcm_count: 0,
            huc6280_ids: Vec::new(),
            huc6280_waves: Vec::new(),
            huc6280_ins: Vec::new(),
            huc6280_pcm_samples: Vec::new(),
            huc6280_count: 0,
            huc6280_pcm_count: 0,
            gb_ids: Vec::new(),
            gb_waves: Vec::new(),
            gb_ins: Vec::new(),
            gb_count: 0,
            vsu_ids: Vec::new(),
            vsu_waves: Vec::new(),
            vsu_mod_waves: Vec::new(),
            vsu_ins: Vec::new(),
            vsu_count: 0,
            vsu_dynamic: false,
            saa_ids: Vec::new(),
            saa_ins: Vec::new(),
            saa_count: 0,
            psg_ids: Vec::new(),
            psg_ins: Vec::new(),
            psg_count: 0,
            psg_stereo: false,
            psg_t6w28: false,
            psg_dac_samples: Vec::new(),
            psg_dac_plays: Vec::new(),
            psg_dac_voices: Vec::new(),
            psg_dac_rate: 0,
            psg_dac_count: 0,
            psg_dac_stats: IndexMap::new(),
            dac_samples: Vec::new(),
            dac_count: 0,
            dac_stats: crate::ym2612dac::DacStats::new(),
        }
    }

    fn finish(&mut self) -> crate::Result<Song> {
        self.run_emission();
        Ok(self.build_final())
    }

    fn run_emission(&mut self) {
        let vgm = self.vgm;
        let t0 = 0i64;
        self.t0 = t0;
        // Python sorts `events` (13978) after the collectors, and its
        // `(event, index)` lists hold object references, so the sort cannot
        // disturb them. The port keeps indices, so sort through a permutation
        // and remap every recorded index (stable sort, same order as Python).
        {
            let n = self.events.len();
            let mut order: Vec<usize> = (0..n).collect();
            order.sort_by(|&a, &b| {
                let ea = &self.events[a];
                let eb = &self.events[b];
                (ea.sample, ea.ch, if ea.on { 0 } else { 1 })
                    .cmp(&(eb.sample, eb.ch, if eb.on { 0 } else { 1 }))
            });
            let mut map: Vec<Option<usize>> = vec![None; n];
            for (new, &old) in order.iter().enumerate() {
                map[old] = Some(new);
            }
            let mut slots: Vec<Option<NoteEvent>> =
                self.events.drain(..).map(Some).collect();
            self.events = order
                .iter()
                .map(|&old| slots[old].take().expect("each index once"))
                .collect();
            self.remap_indices(&map);
        }
        let mut pcm_onsets: IndexMap<i32, Vec<i64>> = IndexMap::new();
        for e in &self.events {
            if e.on && (e.pcm || e.ch >= 8) {
                pcm_onsets.entry(e.ch).or_default().push(e.sample);
            }
        }
        for (_ch, times) in pcm_onsets.iter_mut() {
            let mut merged: Vec<i64> = Vec::new();
            for t in times.iter() {
                if let Some(last) = merged.last() {
                    if *t - *last <= _ONSET_MERGE_SAMPLES {
                        continue;
                    }
                }
                merged.push(*t);
            }
            *times = merged;
        }
        let mut tone_hilight = false;
        let mut hz;
        let mut use_speed;
        let mut grid_note;
        let mut row_subdiv = 1.0f64;
        if let Some(speed) = self.opts.speed.filter(|s| *s >= 1) {
            hz = self.opts.tick_hz.unwrap_or(60.0);
            use_speed = speed;
            grid_note = format!(
                "manual {use_speed} ticks/row at {} Hz",
                crate::parity::fmt_g(hz, 6)
            );
            let _ = &mut row_subdiv;
        } else {
            let fm_chips: Option<HashSet<&'static str>> = self
                .selected_fm_chips
                .as_ref()
                .map(|s| s.iter().copied().collect());
            let grid = crate::timing::estimate_grid(
                vgm,
                0.75,
                self.tick,
                self.row_floor,
                None,
                "key-on lattice",
                fm_chips.as_ref(),
            );
            let grid = match grid {
                Some(g) => Some(g),
                None if !pcm_onsets.is_empty() => crate::timing::estimate_grid(
                    vgm,
                    0.75,
                    self.tick,
                    self.row_floor,
                    Some(&pcm_onsets),
                    "sample-trigger lattice",
                    fm_chips.as_ref(),
                ),
                None => None,
            };
            if let Some(grid) = grid {
                let tone_only = (self.scc_count != 0
                    || self.ay_count != 0
                    || self.vsu_count != 0
                    || self.saa_count != 0
                    || self.pokey_count != 0)
                    && self.keyon_times.is_empty()
                    && !self.events.iter().any(|e| e.on && e.pcm);
                if tone_only {
                    let tick_chip: Option<&str> = if self.saa_count != 0 {
                        Some("saa1099")
                    } else if self.pokey_count != 0 {
                        Some("pokey")
                    } else {
                        None
                    };
                    let saa_tick = tick_chip.and_then(|c| super::_saa_tick_interval(vgm, c));
                    if let Some(st) = saa_tick {
                        hz = VGM_RATE as f64 / st;
                        use_speed = 1;
                        row_subdiv = 1.0;
                        grid_note = format!(
                            "{} write lattice ({} samples/tick), row = 1 write tick",
                            if tick_chip == Some("saa1099") { "SAA1099" } else { "POKEY" },
                            crate::parity::fmt_g(st, 6)
                        );
                        tone_hilight = true;
                    } else {
                        let (a, b, _c, d) = super::_musical_row(&grid);
                        let (a, b, c, d) =
                            super::_fit_tone_subdivision(a, b, &self.events, &d);
                        hz = a;
                        use_speed = b;
                        row_subdiv = c;
                        grid_note = d;
                        tone_hilight = true;
                    }
                } else {
                    hz = grid.hz;
                    use_speed = grid.speed;
                    row_subdiv = grid.subdiv as f64;
                    grid_note = format!(
                        "{}, {}% within 6%; row = 1/{} of the measured 16th, {} ticks/row",
                        grid.source,
                        py_round(grid.confidence * 100.0) as i64,
                        crate::parity::fmt_g(grid.subdiv as f64, 6),
                        grid.speed
                    );
                    if self.psg_count != 0
                        && self.keyon_times.is_empty()
                        && !(self.scc_count != 0
                            || self.ay_count != 0
                            || self.vsu_count != 0
                            || self.saa_count != 0
                            || self.pokey_count != 0)
                        && !self.events.iter().any(|e| e.on && e.pcm)
                    {
                        let (a, b, c, snap) =
                            super::_snap_psg_write_lattice(vgm, hz, use_speed, row_subdiv);
                        hz = a;
                        use_speed = b;
                        row_subdiv = c;
                        if !snap.is_empty() {
                            grid_note = format!("{grid_note}; {snap}");
                        }
                    }
                }
            } else {
                hz = 60.0;
                use_speed = super::tone::suggest_speed(&self.keyon_times, hz);
                grid_note = "60 Hz fallback".into();
            }
        }
        let tick_scale = super::_tone_pitch_tick_scale(vgm, hz, use_speed);
        if tick_scale > 1 {
            hz *= tick_scale as f64;
            use_speed = (use_speed * tick_scale).min(255);
            grid_note = if grid_note.is_empty() {
                format!("tone pitch tick x{tick_scale}")
            } else {
                format!("{grid_note}; tone pitch tick x{tick_scale}")
            };
        }
        let mut pcm_gaps: Vec<i64> = Vec::new();
        let collect_gaps = |by: &mut BTreeMap<(i32, i32), Vec<i64>>, gaps: &mut Vec<i64>| {
            for times in by.values_mut() {
                times.sort();
                for w in times.windows(2) {
                    if w[1] > w[0] {
                        gaps.push(w[1] - w[0]);
                    }
                }
            }
        };
        if !self.spcm_audible.is_empty() {
            let mut by: BTreeMap<(i32, i32), Vec<i64>> = BTreeMap::new();
            for smp in &self.spcm_audible {
                for h in &smp.hits {
                    by.entry((0, h.voice)).or_default().push(h.sample_time);
                }
            }
            collect_gaps(&mut by, &mut pcm_gaps);
        }
        if !self.c352_audible.is_empty() {
            let mut by: BTreeMap<(i32, i32), Vec<i64>> = BTreeMap::new();
            for smp in &self.c352_audible {
                for h in &smp.hits {
                    by.entry((h.chip_id, h.voice as i32)).or_default().push(h.sample_time);
                }
            }
            collect_gaps(&mut by, &mut pcm_gaps);
        }
        if !self.k053260_audible.is_empty() {
            let mut by: BTreeMap<(i32, i32), Vec<i64>> = BTreeMap::new();
            for smp in &self.k053260_audible {
                for h in &smp.hits {
                    by.entry((h.chip_id, h.voice as i32)).or_default().push(h.sample_time);
                }
            }
            collect_gaps(&mut by, &mut pcm_gaps);
        }
        if !pcm_gaps.is_empty() {
            let extent = if vgm.total_samples != 0 {
                vgm.total_samples
            } else {
                self.events.iter().map(|e| e.sample).max().unwrap_or(0)
            };
            let (new_hz, new_speed, new_sub, note) = crate::timing::refine_pcm_grid(
                hz,
                use_speed,
                row_subdiv as i32,
                extent,
                &pcm_gaps,
                self.tick,
            );
            if !note.is_empty() {
                hz = new_hz;
                use_speed = new_speed;
                row_subdiv = new_sub as f64;
                grid_note = if grid_note.is_empty() {
                    note.clone()
                } else {
                    format!("{grid_note}; {note}")
                };
            }
        }
        if self.opts.condense > 1.0 {
            let (a, b, c) = crate::timing::condense_row(
                hz,
                use_speed,
                row_subdiv,
                self.opts.condense,
                self.tick,
            );
            hz = a;
            use_speed = b;
            row_subdiv = c;
            grid_note = if grid_note.is_empty() {
                format!("condensed x{}", crate::parity::fmt_g(self.opts.condense, 6))
            } else {
                format!("{grid_note}; condensed x{}", crate::parity::fmt_g(self.opts.condense, 6))
            };
        }
        self.grid_note = grid_note;
        self.row_subdiv = row_subdiv;
        self.hz = hz;
        self.use_speed = use_speed;

        // Python: legato classification right after the grid settles.
        if self.convert_opm {
            let channels: Option<std::collections::HashSet<i32>> = if !(self.is_opn
                || self.is_opn2
                || self.is_opnb
                || self.is_opna
                || self.is_opl)
            {
                None
            } else {
                Some((0..8).collect())
            };
            let (steps, ramps) = super::tone::_classify_legato(
                &mut self.events,
                (crate::vgm::VGM_RATE as f64 / self.hz) * use_speed as f64,
                channels.as_ref(),
            );
            self.legato_steps = steps;
            self.legato_ramps = ramps;
        }

        // DAC after the grid.
        if self.opts.pcm && self.is_opn2 {
            let opts = crate::ym2612dac::CollectDacOptions {
                slices: self.opts.dac_slices,
                timeline: false,
                grid: Some((hz, use_speed, t0)),
                lossy_samples: self.opts.lossy_samples,
                release_samples: None,
            };
            let (samples, plays, warn, stats) = crate::ym2612dac::collect_dac(vgm, &opts);
            // Python analyze.py 14142-14148: DAC writes with nothing playable
            // are reported unless the collector already explained them.
            let no_warn = warn.is_empty();
            let disabled = stats.int("dac_disabled_bytes") != 0;
            self.dac_samples = samples;
            self.dac_plays = plays;
            self.warnings.extend(warn);
            self.dac_stats = stats;
            if self.opn_dac_writes != 0
                && self.dac_plays.is_empty()
                && no_warn
                && !disabled
            {
                self.warnings.push(format!(
                    "{} YM2612 DAC writes but no playable DAC data",
                    self.opn_dac_writes
                ));
            }
        }
        if !self.dac_plays.is_empty() {
            let total = vgm.total_samples;
            let gopts = crate::ym2612dac::GroupDacOptions {
                hz,
                speed: use_speed,
                t0,
                total_samples: total,
                drop_quiet: !self.opts.dac_slices,
                cluster_fragments: self.opts.lossy_samples && !self.opts.dac_slices,
                ..Default::default()
            };
            let (samples, notes, offs, mut pan_events, warn, stats) =
                crate::ym2612dac::group_dac_plays(&self.dac_samples, &self.dac_plays, &gopts);
            self.dac_samples = samples;
            self.warnings.extend(warn);
            self.dac_stats.merge(&stats);
            if self.opts.lossy_samples {
                let clustered = self.dac_stats.int("dac_fragment_clustered");
                if clustered != 0 {
                    self.warnings.push(format!(
                        "YM2612 DAC: lossy sample clustering folded {clustered} bodies into {} slots",
                        self.dac_samples.len()
                    ));
                }
            }
            let n_dac_ins = (self.dac_samples.len() as i32 + 119) / 120;
            self.n_dac_ins = n_dac_ins;
            let dac_ch = self
                .ym_base
                .get(&self.ym2612_ids.first().copied().unwrap_or(0))
                .copied()
                .unwrap_or(0)
                + if self.ym2612_ext { 8 } else { 5 };
            if n_dac_ins != 0 {
                let dac_base = self.patch_order.len() as i32;
                super::_rebase_non_fm_ins(&mut self.events, dac_base, dac_base + n_dac_ins);
                self.tone_base += n_dac_ins;
            }
            let dac_base = self.patch_order.len() as i32;
            for (placed, sample_idx, pan) in notes {
                let no_pan = pan < 0;
                self.events.push(NoteEvent {
                    sample: placed,
                    ch: dac_ch,
                    on: true,
                    note: 60 + sample_idx % 120,
                    ins: dac_base + sample_idx / 120,
                    vol: 127,
                    e5: 0x80,
                    pan: if no_pan { 0 } else { pan },
                    no_pan,
                    dac: true,
                    ..NoteEvent::default()
                });
            }
            for (placed, _pan) in offs {
                self.events.push(NoteEvent {
                    sample: placed,
                    ch: dac_ch,
                    on: false,
                    note: 180,
                    ins: -1,
                    vol: -1,
                    e5: 0x80,
                    pan: 0,
                    dac: true,
                    ..NoteEvent::default()
                });
            }
            for pt in self.dac_stats.points("dac_pan_points") {
                if pt.1 >= 0 {
                    pan_events.push(*pt);
                }
            }
            let mut seen: BTreeSet<(i64, i32)> = BTreeSet::new();
            let mut sorted = pan_events.clone();
            sorted.sort();
            for (placed, pan) in sorted {
                if pan < 0 || seen.contains(&(placed, pan)) {
                    continue;
                }
                seen.insert((placed, pan));
                self.channel_fx.push((placed, dac_ch, 0x08, pan));
            }
            self.events.sort_by(|a, b| {
                (a.sample, a.ch, if a.on { 0 } else { 1 })
                    .cmp(&(b.sample, b.ch, if b.on { 0 } else { 1 }))
            });
            self.dac_count = self.dac_stats.int("dac_notes") as i32;
        }

        let mut hilight_a = 4;
        let mut hilight_b = 16;
        if tone_hilight && row_subdiv >= 1.0 {
            hilight_a = (py_round(4.0 * row_subdiv) as i32).clamp(1, 255);
            hilight_b = (py_round(16.0 * row_subdiv) as i32).max(hilight_a).min(255);
        }
        self.hilight = (hilight_a, hilight_b);

        // LFO macros gathered by the OPM pass.
        let sppt = VGM_RATE as f64 / self.hz;
        let held: Vec<(i32, Vec<(Vec<i32>, Vec<i32>, Vec<i32>)>)> =
            self.held_lfo.iter().map(|(k, v)| (*k, v.clone())).collect();
        for (ins, ramps) in held {
            if !(0..self.patch_order.len() as i32).contains(&ins) {
                continue;
            }
            let base_key = self.patch_order[ins as usize].clone();
            let (fm_pms, ams, macro_speed) = super::_pick_lfo_macros(&ramps, sppt);
            if !fm_pms.is_empty() {
                if let Some(p) = self.patches.get_mut(&base_key) {
                    p.fms_macro = fm_pms;
                    p.ams_macro = ams;
                    p.macro_speed = macro_speed;
                }
            }
        }

        let fm_pitch_clock = if self.is_combo {
            self.opl_clock
        } else if self.is_opnb {
            self.ym2610_clock as f64
        } else if self.is_opna {
            self.ym2608_clock as f64
        } else if self.is_opn2 {
            self.ym2612_clock as f64
        } else if self.is_opl {
            self.opl_clock
        } else {
            self.ym_clock as f64
        };
        let fm_ins_cap = INS_TARGET;
        let total = vgm.total_samples;
        let fm_before_pitch = self.patch_order.len() as i32;
        let mut opll_macro_copies = 0;
        if self.is_opll {
            opll_macro_copies = super::_bind_opll_macros(
                &mut self.events,
                &mut self.patches,
                &mut self.patch_order,
                &mut self.channel_fx,
                hz,
                use_speed,
                t0,
                total,
                fm_ins_cap,
            );
        }
        let (_pms_copies, pms_lost) = super::_bind_pms_macros(
            &mut self.events,
            &mut self.patches,
            &mut self.patch_order,
            hz,
            total,
            fm_ins_cap,
        );
        let (_fm_pitch_copies, fm_pitch_lost) = super::_bind_fm_pitch(
            &mut self.events,
            &mut self.patches,
            &mut self.patch_order,
            fm_pitch_clock,
            hz,
            total,
            fm_ins_cap,
            self.is_opn || self.is_opn2 || self.is_opl || self.is_opnb || self.is_opna,
            use_speed,
            if self.is_opll { 4 } else { 0 },
            self.is_opll,
        );
        let mut opm_dtmul_lost = 0;
        if self.convert_opm {
            let (_c, l) = super::_bind_opm_dtmul_macros(
                &mut self.events,
                &mut self.patches,
                &mut self.patch_order,
                VGM_RATE as f64 / self.hz,
                t0,
                use_speed,
                fm_ins_cap,
            );
            opm_dtmul_lost = l;
        }
        let mut op_tl_lost = 0;
        if self.is_opl {
            let (_c, l) = super::_bind_op_macros(
                &mut self.events,
                &mut self.patches,
                &mut self.patch_order,
                VGM_RATE as f64 / self.hz,
                fm_ins_cap,
            );
            op_tl_lost = l;
        }
        // Any FM macro copy (pitch, PMS/AMS, OPLL level/preset) grows the
        // patch list, so the sample and tone instrument indices shift up.
        let fm_after_pitch = self.patch_order.len() as i32;
        super::_rebase_non_fm_ins(&mut self.events, fm_before_pitch, fm_after_pitch);
        if fm_after_pitch > fm_before_pitch {
            self.tone_base += fm_after_pitch - fm_before_pitch;
        }
        if pms_lost != 0 {
            self.warnings.push(format!(
                "{pms_lost} YM2151 PMS/AMS ramps stayed on the base instrument: instrument limit"
            ));
        }
        if fm_pitch_lost != 0 {
            let chip = if self.is_opnb {
                "YM2610"
            } else if self.is_opna {
                "YM2608"
            } else if self.is_opn2 {
                "YM2612"
            } else if self.is_opl {
                match self.opl_kind {
                    Some(KIND_OPL) => "YM3526",
                    Some(KIND_OPL2) => "YM3812",
                    Some(KIND_OPL3) => "YMF262",
                    _ => "YM2203",
                }
            } else {
                "YM2203"
            };
            self.warnings.push(format!(
                "{fm_pitch_lost} {chip} pitch sweeps encoded as row ramps: instrument limit"
            ));
        }
        if op_tl_lost != 0 {
            self.warnings.push(format!(
                "{op_tl_lost} OPL note(s) scripted a modulator level that stayed flat on the base instrument: operator macro limit"
            ));
        }
        if opm_dtmul_lost != 0 {
            self.warnings.push(format!(
                "{opm_dtmul_lost} YM2151 note(s) retuned an operator inside the note but stayed on the base instrument: operator macro limit"
            ));
        }
        let _ = opll_macro_copies;
        self.bind_tone(hz, use_speed, t0);
    }
}


impl<'a> AnalyzeState<'a> {
    fn bind_tone(&mut self, hz: f64, use_speed: i32, _t0: i64) {
        let vgm = self.vgm;
        let scc_clock = if vgm.k051649_clock != 0 { vgm.k051649_clock } else { SCC_DEFAULT_CLOCK };
        let ay_clock = if vgm.ay8910_clock != 0 { vgm.ay8910_clock } else { AY_DEFAULT_CLOCK };
        let nes_clk = if vgm.nes_clock != 0 { vgm.nes_clock } else { NES_DEFAULT_CLOCK };
        let ay_half = if self.has_nes {
            ay_clock.abs_diff(nes_clk) <= 16
        } else {
            (vgm.ay8910_flags & 0x10) != 0
        };
        let ay_note_clock = if ay_half { ay_clock / 2 } else { ay_clock };
        let psg_clock = self.psg_clock;
        let nes_clock = if vgm.nes_clock != 0 { vgm.nes_clock } else { NES_DEFAULT_CLOCK };
        let huc_clock = if vgm.huc6280_clock != 0 { vgm.huc6280_clock } else { 7_159_090 };
        let gb_clock = if vgm.gb_dmg_clock != 0 { vgm.gb_dmg_clock } else { GB_DEFAULT_CLOCK };
        let vsu_clock = if vgm.vsu_clock != 0 { vgm.vsu_clock } else { VSU_DEFAULT_CLOCK };
        let saa_clock = if vgm.saa1099_clock != 0 { vgm.saa1099_clock } else { SAA_DEFAULT_CLOCK };
        let pokey_clock = if vgm.pokey_clock != 0 { vgm.pokey_clock } else { POKEY_DEFAULT_CLOCK };
        let mikey_clock = if vgm.mikey_clock != 0 { vgm.mikey_clock } else { MIKEY_DEFAULT_CLOCK };
        let x1_clock = if vgm.x1_010_clock != 0 { vgm.x1_010_clock } else { X1_010_DEFAULT_CLOCK };
        let fds_clock = if vgm.nes_clock != 0 { vgm.nes_clock } else { FDS_DEFAULT_CLOCK };
        let ws_clock = if vgm.wonderswan_clock != 0 { vgm.wonderswan_clock } else { WSWAN_DEFAULT_CLOCK };
        let k_clock = if vgm.k005289_clock != 0 { vgm.k005289_clock } else { K005289_DEFAULT_CLOCK };
        let tone_demand_raw = super::_tone_instrument_demand(
            &self.events,
            scc_clock,
            ay_note_clock,
            psg_clock,
            nes_clock,
            huc_clock,
            gb_clock,
            vsu_clock,
            saa_clock,
            pokey_clock,
            mikey_clock,
            x1_clock,
            fds_clock,
            ws_clock,
            k_clock,
            hz,
            vgm.total_samples,
            use_speed,
            self.opts.lfo_loop,
        );
        let _ = tone_demand_raw;
        let n_ssg_pcm = (self.ssg_pcm_samples.len() as i32 + 119) / 120;
        let ssg_pcm_ins_base = self.tone_base;
        self.tone_base += n_ssg_pcm;
        let nes_dpcm_ins_base = self.tone_base;
        self.tone_base += self.nes_dpcm_samples.len() as i32;
        let nes_pcm_ins_base = self.tone_base;
        self.tone_base += (self.nes_pcm_samples.len() as i32 + NES_PCM_MAP_SLOTS - 1) / NES_PCM_MAP_SLOTS;
        let huc_pcm_ins_base = self.tone_base;
        self.tone_base += self.huc_pcm_samples.len() as i32;
        let _ymz_ins_base = self.tone_base;
        self.tone_base += self.ymz_audible.len() as i32;
        let multipcm_ins_base = self.tone_base;
        self.tone_base += self.multipcm_audible.len() as i32;
        let upd_ins_base = self.tone_base;
        self.tone_base += (self.upd_audible.len() as i32 + 119) / 120;
        let pwm_ins_base = self.tone_base;
        self.tone_base += (self.pwm_audible.len() as i32 + 119) / 120;
        let msm5205_ins_base = self.tone_base;
        self.tone_base += (self.msm5205_audible.len() as i32 + 119) / 120;
        let ga20_ins_base = self.tone_base;
        self.tone_base += self.ga20_samples.len() as i32;
        let c219_ins_base = self.tone_base;
        self.tone_base += self.c219_ins_count;
        let k054539_ins_base = self.tone_base;
        self.tone_base += self.k054539_samples.len() as i32;
        let msm5232_ins_base = self.tone_base;
        self.tone_base += self.msm5232_controls.len() as i32;
        let es5503_ins_base = self.tone_base;
        self.tone_base += self.es5503_samples.len() as i32;
        let psg_dac_ins_base = self.tone_base;
        self.tone_base += self
            .psg_dac
            .as_ref()
            .map(|d| (d.samples.len() as i32 + 119) / 120)
            .unwrap_or(0);
        let mikey_ins_base = self.tone_base;
        self.tone_base += (self.mikey_samples.len() as i32 + 119) / 120;
        let ws_pcm_ins_base = self.tone_base;
        self.tone_base += self.wonderswan_pcm_samples.len() as i32;
        let x1_ins_base = self.tone_base;
        self.tone_base += (self.x1_samples.len() as i32 + 119) / 120;
        // The reference sizes the tolerance from the final tone base plus
        // the raw demand; sample-instrument groups sit below this base.
        let tone_merge_tol = if self.tone_base + tone_demand_raw > INS_TARGET {
            1
        } else {
            0
        };
        let merged = super::_bind_tone_instruments(
            &mut self.events,
            &mut self.scc_ins,
            &mut self.ay_ins,
            &mut self.psg_ins,
            &mut self.nes_ins,
            &mut self.huc6280_ins,
            &mut self.gb_ins,
            &mut self.vsu_ins,
            &mut self.saa_ins,
            &mut self.pokey_ins,
            &mut self.mikey_ins,
            &mut self.x1_ins,
            &mut self.fds_ins,
            &mut self.wonderswan_ins,
            &mut self.qsound_ins,
            &mut self.k005289_ins,
            self.tone_base,
            scc_clock,
            ay_note_clock,
            psg_clock,
            nes_clock,
            huc_clock,
            gb_clock,
            vsu_clock,
            saa_clock,
            pokey_clock,
            mikey_clock,
            x1_clock,
            fds_clock,
            ws_clock,
            k_clock,
            hz,
            vgm.total_samples,
            use_speed,
            tone_merge_tol,
            self.opts.lfo_loop,
        );
        if merged != 0 {
            self.warnings.push(format!(
                "instrument limit: {merged} tone note(s) shared a near-identical instrument macro (1-step tolerance)"
            ));
        }

        for (ev, idx) in &self.ssg_pcm_events {
            self.events[*ev].ins = ssg_pcm_ins_base + idx / 120;
        }
        for (ev, idx) in &self.nes_dpcm_events {
            self.events[*ev].ins = nes_dpcm_ins_base + *idx;
        }
        for (ev, idx) in &self.nes_pcm_events {
            self.events[*ev].ins = nes_pcm_ins_base + idx / NES_PCM_MAP_SLOTS;
        }
        for (ev, idx) in &self.huc6280_pcm_events {
            self.events[*ev].ins = huc_pcm_ins_base + *idx;
        }
        for (ev, idx) in &self.ssg_pcm_events {
            let _ = (ev, idx);
        }
        // Python analyze.py 12282-12286: an OPL4 with wave samples but no FM
        // notes keeps the empty FM placeholder at instrument 0, so the wave
        // instruments start at 1.
        let mut fm_ins_count = self.patch_order.len() as i32;
        if fm_ins_count == 0 && self.is_opl && !self.opl4_wave_samples.is_empty() {
            fm_ins_count = 1;
        }
        let final_ymz_base = fm_ins_count
            + self.n_dac_ins
            + self.audible.len() as i32
            + self.oki_samples.len() as i32
            + crate::oki::msm6258_ins_count(self.adpcm_samples.len() as i32)
            + self.spcm_audible.len() as i32
            + self.rf5c68_audible.len() as i32
            + self.rf5c164_audible.len() as i32
            + self.c140_audible.len() as i32
            + self.c352_audible.len() as i32
            + self.k053260_audible.len() as i32
            + (self.ym2610_a_samples.len() as i32 + YM2610_MAP_SLOTS - 1) / YM2610_MAP_SLOTS
            + (self.ym2610_b_samples.len() as i32 + YM2610_MAP_SLOTS - 1) / YM2610_MAP_SLOTS
            + if self.ym2608_rhythm_events.is_empty() { 0 } else { 1 }
            + (self.ym2608_b_samples.len() as i32 + YM2610_MAP_SLOTS - 1) / YM2610_MAP_SLOTS
            + (self.y8950_b_samples.len() as i32 + YM2610_MAP_SLOTS - 1) / YM2610_MAP_SLOTS
            + self.opl4_wave_samples.len() as i32
            + n_ssg_pcm
            + self.nes_dpcm_samples.len() as i32
            + (self.nes_pcm_samples.len() as i32 + NES_PCM_MAP_SLOTS - 1) / NES_PCM_MAP_SLOTS
            + self.huc_pcm_samples.len() as i32;
        for (ev, idx) in &self.ymz_events {
            self.events[*ev].ins = final_ymz_base + *idx;
        }
        for (ev, idx) in &self.multipcm_events {
            self.events[*ev].ins = multipcm_ins_base + *idx;
        }
        for (ev, idx) in &self.upd_events {
            self.events[*ev].ins = upd_ins_base + *idx;
        }
        for (ev, idx) in &self.pwm_events {
            self.events[*ev].ins = pwm_ins_base + *idx;
        }
        for (ev, idx) in &self.msm5205_events {
            self.events[*ev].ins = msm5205_ins_base + *idx;
        }
        for (ev, idx) in &self.ga20_events {
            self.events[*ev].ins = ga20_ins_base + *idx;
        }
        for (ev, idx) in &self.msm5232_events {
            self.events[*ev].ins = msm5232_ins_base + *idx;
        }
        for (ev, idx) in &self.c219_events {
            self.events[*ev].ins = c219_ins_base + *idx;
        }
        for (ev, idx) in &self.k054539_events {
            self.events[*ev].ins = k054539_ins_base + *idx;
        }
        for (ev, idx) in &self.es5503_events {
            self.events[*ev].ins = es5503_ins_base + *idx;
        }
        for (ev, idx) in &self.mikey_sample_events {
            self.events[*ev].ins = mikey_ins_base + *idx;
        }
        for (ev, idx) in &self.wonderswan_pcm_events {
            self.events[*ev].ins = ws_pcm_ins_base + *idx;
        }
        for (ev, idx) in &self.x1_sample_events {
            self.events[*ev].ins = x1_ins_base + *idx;
        }
        let n_ym2610_a_ins =
            (self.ym2610_a_samples.len() as i32 + YM2610_MAP_SLOTS - 1) / YM2610_MAP_SLOTS;
        let n_ym2610_b_ins =
            (self.ym2610_b_samples.len() as i32 + YM2610_MAP_SLOTS - 1) / YM2610_MAP_SLOTS;
        let n_ym2608_b_ins =
            (self.ym2608_b_samples.len() as i32 + YM2610_MAP_SLOTS - 1) / YM2610_MAP_SLOTS;
        let n_ym2608_rhythm_ins = if self.ym2608_rhythm_events.is_empty() { 0 } else { 1 };
        let n_y8950_b_ins =
            (self.y8950_b_samples.len() as i32 + YM2610_MAP_SLOTS - 1) / YM2610_MAP_SLOTS;
        let n_opl4_wave_ins = self.opl4_wave_samples.len() as i32;
        let pre_ssg_tone_base = final_ymz_base - n_ssg_pcm;
        let opl4_wave_ins_base = pre_ssg_tone_base - n_opl4_wave_ins;
        let y8950_b_ins_base = opl4_wave_ins_base - n_y8950_b_ins;
        let ym2610_b_ins_base =
            y8950_b_ins_base - n_ym2610_b_ins - n_ym2608_rhythm_ins - n_ym2608_b_ins;
        let ym2610_a_ins_base = ym2610_b_ins_base - n_ym2610_a_ins;
        let ym2608_rhythm_ins_base =
            y8950_b_ins_base - n_ym2608_b_ins - n_ym2608_rhythm_ins;
        let ym2608_b_ins_base = y8950_b_ins_base - n_ym2608_b_ins;
        for (ev, kind, idx) in &self.ym2610_events {
            let base = if *kind == 0 { ym2610_a_ins_base } else { ym2610_b_ins_base };
            self.events[*ev].ins = base + idx / YM2610_MAP_SLOTS;
        }
        for ev in &self.ym2608_rhythm_events {
            self.events[*ev].ins = ym2608_rhythm_ins_base;
        }
        for (ev, idx) in &self.ym2608_b_events {
            self.events[*ev].ins = ym2608_b_ins_base + idx / YM2610_MAP_SLOTS;
        }
        for (ev, idx) in &self.y8950_b_events {
            self.events[*ev].ins = y8950_b_ins_base + idx / YM2610_MAP_SLOTS;
        }
        for (ev, idx) in &self.opl4_wave_events {
            self.events[*ev].ins = opl4_wave_ins_base + idx;
        }
        for (ev, idx) in &self.psg_dac_events {
            self.events[*ev].ins = psg_dac_ins_base + idx / 120;
        }
    }

    fn build_final(&mut self) -> Song {
        let vgm = self.vgm;
        let mut fm_list: Vec<FMPatch> = self
            .patch_order
            .iter()
            .map(|k| self.patches[k].clone())
            .collect();
        let is_opl = self.is_opl;
        // Python analyze.py 14503-14519: any audible sample engine, tone
        // chip or DAC keeps the legacy placeholder out of the module.
        let has_sample_ins = !self.oki_samples.is_empty()
            || !self.adpcm_samples.is_empty()
            || !self.spcm_audible.is_empty()
            || !self.c140_audible.is_empty()
            || !self.c352_audible.is_empty()
            || !self.k053260_audible.is_empty()
            || !self.rf5c68_audible.is_empty()
            || !self.rf5c164_audible.is_empty()
            || !self.ym2610_a_samples.is_empty()
            || !self.ym2610_b_samples.is_empty()
            || !self.ym2608_b_samples.is_empty()
            || !self.ym2608_rhythm_events.is_empty()
            || !self.y8950_b_samples.is_empty()
            || !self.opl4_wave_samples.is_empty()
            || !self.ssg_pcm_samples.is_empty()
            || !self.nes_ids.is_empty()
            || !self.nes_dpcm_samples.is_empty()
            || !self.nes_pcm_samples.is_empty()
            || !self.huc_pcm_samples.is_empty()
            || !self.ymz_audible.is_empty()
            || !self.multipcm_audible.is_empty()
            || !self.upd_audible.is_empty()
            || !self.pwm_audible.is_empty()
            || self.c219_ins_count != 0
            || !self.k054539_samples.is_empty()
            || !self.es5503_samples.is_empty()
            || self.psg_dac.as_ref().is_some_and(|d| !d.samples.is_empty())
            || !self.mikey_ids.is_empty()
            || !self.x1_ids.is_empty()
            || self.pcm_samples.iter().any(|s| s.length() >= 8)
            || !self.scc_waves.is_empty()
            || self.ay_count != 0
            || !self.dac_samples.is_empty()
            || self.psg_count != 0
            || !self.saa_ids.is_empty()
            || self.saa_count != 0
            || self.pokey_count != 0;
        if fm_list.is_empty() {
            let chip_name = match self.opl_kind {
                Some(KIND_OPL) => "YM3526",
                Some(KIND_OPL2) => "YM3812",
                Some(KIND_OPL3) => "YMF262",
                _ => "YM2151",
            };
            if self.is_opn && self.ym2203_ssg_count != 0 {
                self.warnings
                    .push("no YM2203 FM key-ons found; converting the SSG only".into());
            } else if self.is_opn2 {
                self.warnings.push("no YM2612 key-ons found".into());
            } else if self.is_opl {
                self.warnings.push(format!("no {chip_name} key-ons found"));
            } else if self.is_opnb {
                self.warnings.push("no YM2610 FM key-ons found".into());
            } else if self.is_opna {
                self.warnings.push("no YM2608 FM key-ons found".into());
            } else if !has_sample_ins {
                self.warnings.push(
                    if self.is_opn {
                        "no YM2203 key-ons found"
                    } else {
                        "no YM2151 key-ons found"
                    }
                    .into(),
                );
            }
            if !has_sample_ins || (is_opl && !self.opl4_wave_samples.is_empty()) {
                fm_list = vec![FMPatch {
                    alg: if is_opl { 0 } else { 7 },
                    fb: 0,
                    ams: 0,
                    pms: 0,
                    ops: core::array::from_fn(|_| OpRegs {
                        tl: if is_opl { 63 } else { 0 },
                        ..OpRegs::default()
                    }),
                    name: "empty".into(),
                    opl: is_opl,
                    ..FMPatch::default()
                }];
            }
        }
        let mut skip: Option<HashSet<i32>> = None;
        if !is_opl && (self.ym2203_ext || self.ym2612_ext || self.ym2608_ext) {
            let ids = if self.is_opn {
                &self.ym_ids
            } else if self.is_opna {
                &self.ym2608_ids
            } else {
                &self.ym2612_ids
            };
            let mut s: HashSet<i32> = HashSet::new();
            for cid in ids {
                for col in 0..4 {
                    s.insert(self.ym_base[cid] + 2 + col);
                }
            }
            skip = Some(s);
        }
        super::_apply_opn_carrier_octave(&mut self.events, &mut fm_list, skip.as_ref());
        let n_ym2610_a_ins =
            (self.ym2610_a_samples.len() as i32 + YM2610_MAP_SLOTS - 1) / YM2610_MAP_SLOTS;
        let n_ym2610_b_ins =
            (self.ym2610_b_samples.len() as i32 + YM2610_MAP_SLOTS - 1) / YM2610_MAP_SLOTS;
        let n_ym2608_b_ins =
            (self.ym2608_b_samples.len() as i32 + YM2610_MAP_SLOTS - 1) / YM2610_MAP_SLOTS;
        let n_ym2608_rhythm_ins = if self.ym2608_rhythm_events.is_empty() { 0 } else { 1 };
        let n_ssg_pcm = (self.ssg_pcm_samples.len() as i32 + 119) / 120;
        let non_fm_ins = self.n_dac_ins
            + if self.opts.pcm { self.audible.len() as i32 } else { 0 }
            + self.oki_samples.len() as i32
            + crate::oki::msm6258_ins_count(self.adpcm_samples.len() as i32)
            + self.spcm_audible.len() as i32
            + self.c140_audible.len() as i32
            + self.c352_audible.len() as i32
            + self.k053260_audible.len() as i32
            + n_ym2610_a_ins
            + n_ym2610_b_ins
            + self.scc_ins.len() as i32
            + self.ay_ins.len() as i32
            + self.psg_ins.len() as i32
            + self.vsu_ins.len() as i32
            + n_ym2608_rhythm_ins
            + n_ym2608_b_ins
            + n_ssg_pcm
            + self.ymz_audible.len() as i32
            + self.multipcm_audible.len() as i32
            + (self.upd_audible.len() as i32 + 119) / 120
            + (self.pwm_audible.len() as i32 + 119) / 120
            + self.c219_ins_count
            + self.k054539_samples.len() as i32
            + self
                .psg_dac
                .as_ref()
                .map(|d| (d.samples.len() as i32 + 119) / 120)
                .unwrap_or(0);
        // Before trimming anything for the hard limit, share an instrument
        // whose macros only differ by a longer tail with its shorter copies.
        // A short note keyed off before the extra tail plays exactly the same.
        super::_merge_fm_prefix_macros(&mut self.events, &mut fm_list, VGM_RATE as f64 / self.hz);
        let hard_limit = crate::analyze::tone::INS_HARD_LIMIT;
        let old_fm_count = fm_list.len() as i32;
        let fm_limit = i32::max(1, hard_limit - non_fm_ins);
        let dropped_copies =
            super::_trim_fm_macro_copies(&mut self.events, &mut fm_list, fm_limit as usize);
        if dropped_copies != 0 {
            super::_rebase_non_fm_ins(&mut self.events, old_fm_count, fm_list.len() as i32);
            self.warnings.push(format!(
                "instrument limit: folded {dropped_copies} FM macro copy/copies back onto their base voices to stay within {hard_limit} slots"
            ));
        }
        if fm_list.len() as i32 + non_fm_ins > hard_limit {
            let old_base = fm_list.len() as i32;
            let dropped_base = super::_trim_fm_base_patches(
                &mut self.events,
                &mut fm_list,
                i32::max(1, hard_limit - non_fm_ins) as usize,
            );
            if dropped_base != 0 {
                super::_rebase_non_fm_ins(&mut self.events, old_base, fm_list.len() as i32);
                self.warnings.push(format!(
                    "instrument limit: folded {dropped_base} base FM timbre(s) onto earlier patches to stay within {hard_limit} slots"
                ));
            }
        }
        let n_total_ins = fm_list.len() as i32 + non_fm_ins;
        if n_total_ins > hard_limit {
            self.warnings.push(format!(
                "{n_total_ins} instruments exceed the 0xFF instrument limit; the extra instruments cannot be addressed"
            ));
        }
        let n_total_smp = if self.opts.pcm { self.pcm_samples.len() as i64 } else { 0 }
            + self.oki_samples.len() as i64
            + self.adpcm_samples.len() as i64
            + self.spcm_audible.len() as i64
            + self.c140_audible.len() as i64
            + self.c352_audible.len() as i64
            + self.k053260_audible.len() as i64
            + self.dac_samples.len() as i64
            + self.ym2610_a_samples.len() as i64
            + self.ym2610_b_samples.len() as i64
            + self.ym2608_b_samples.len() as i64
            + self.ssg_pcm_samples.len() as i64
            + self.ymz_audible.len() as i64
            + self.multipcm_audible.len() as i64
            + self.upd_audible.len() as i64
            + self.pwm_audible.len() as i64
            + self.c219_samples.len() as i64
            + self.k054539_samples.len() as i64
            + self
                .psg_dac
                .as_ref()
                .map(|d| d.samples.len() as i64)
                .unwrap_or(0);
        if n_total_smp > 65535 {
            self.warnings.push(format!(
                "{n_total_smp} samples exceed the 16-bit sample-map index range"
            ));
        }
        let _ = vgm;
        let mut song = self.partial_song();
        song.t0 = self.t0;
        song.speed = self.use_speed;
        song.hz = self.hz;
        song.loop_sample = vgm.loop_sample;
        song.events = std::mem::take(&mut self.events);
        song.warnings = self.warnings.clone();
        song.chip_fx = self.chip_fx.iter().map(|e| e.as_song()).collect();
        song.channel_fx = std::mem::take(&mut self.channel_fx);
        song.grid_note = self.grid_note.clone();
        song.row_subdiv = self.row_subdiv;
        song.hilight_a = self.hilight.0;
        song.hilight_b = self.hilight.1;
        song.fm_patches = fm_list;
        song.pcm_samples = if self.opts.pcm { self.pcm_samples.clone() } else { Vec::new() };
        song.oki_samples = self.oki_samples.clone();
        song.msm6258_samples = self.adpcm_samples.clone();
        song.segapcm_samples = self.spcm_audible.clone();
        song.segapcm_ids = self.spcm_ids.clone();
        song.rf5c68_samples = self.rf5c68_audible.clone();
        song.rf5c164_samples = self.rf5c164_audible.clone();
        song.c140_samples = self.c140_audible.clone();
        song.c219_samples = self.c219_samples.clone();
        song.c219_ids = self.c219_ids.clone();
        song.c219_count = self.c219_count;
        song.c352_samples = self.c352_audible.clone();
        song.k053260_samples = self.k053260_audible.clone();
        song.k054539_samples = self.k054539_samples.clone();
        song.k054539_ids = self.k054539_ids.clone();
        song.k054539_count = self.k054539_count;
        song.ymz280b_samples = self.ymz_audible.clone();
        song.ymz280b_ids = self.ymz_ids.clone();
        song.ymz280b_count = self.ymz_count;
        song.multipcm_samples = self.multipcm_audible.clone();
        song.multipcm_ids = self.multipcm_ids.clone();
        song.multipcm_count = self.multipcm_count;
        song.upd7759_samples = self.upd_audible.clone();
        song.upd7759_ids = self.upd_ids.clone();
        song.upd7759_count = self.upd_count;
        song.pwm_samples = self.pwm_audible.clone();
        song.pwm_ids = self.pwm_ids.clone();
        song.pwm_count = self.pwm_count;
        song.msm5205_samples = self.msm5205_audible.clone();
        song.msm5205_ids = self.msm5205_ids.clone();
        song.msm5205_count = self.msm5205_count;
        song.ga20_samples = self.ga20_samples.clone();
        song.ga20_ids = self.ga20_ids.clone();
        song.ga20_count = self.ga20_count;
        song.msm5232_ids = self.msm5232_ids.clone();
        song.msm5232_controls = self.msm5232_controls.clone();
        song.msm5232_count = self.msm5232_count;
        song.es5503_samples = self.es5503_samples.clone();
        song.es5503_ids = self.es5503_ids.clone();
        song.es5503_oscs = self.es5503_oscs.clone();
        song.es5503_rate = self.es5503_rate;
        song.es5503_count = self.es5503_count;
        song.channel_vol_events = self.channel_vol_events.clone();
        if let Some(d) = &self.psg_dac {
            song.psg_dac_samples = d.samples.clone();
            song.psg_dac_plays = d.plays.clone();
            song.psg_dac_voices = d.voices.clone();
            song.psg_dac_rate = d.rate;
            song.psg_dac_stats = d.stats.clone();
        }
        song.psg_dac_count = self.psg_dac_count;
        song.mikey_samples = self.mikey_samples.clone();
        song.mikey_ids = self.mikey_ids.clone();
        song.mikey_ins = self.mikey_ins.clone();
        song.mikey_count = self.mikey_count;
        song.x1_010_samples = self.x1_samples.clone();
        song.x1_010_ids = self.x1_ids.clone();
        song.x1_010_ins = self.x1_ins.clone();
        song.x1_010_count = self.x1_count;
        song.x1_waves = self.x1_waves.clone();
        song.x1_envs = self.x1_envs.clone();
        song.fds_ids = self.fds_ids.clone();
        song.fds_ins = self.fds_ins.clone();
        song.fds_waves = self.fds_waves.clone();
        song.fds_mod_waves = self.fds_mod_waves.clone();
        song.fds_count = self.fds_count;
        song.wonderswan_ids = self.wonderswan_ids.clone();
        song.wonderswan_ins = self.wonderswan_ins.clone();
        song.wonderswan_waves = self.wonderswan_waves.clone();
        song.wonderswan_pcm_samples = self.wonderswan_pcm_samples.clone();
        song.wonderswan_count = self.wonderswan_count;
        song.wonderswan_pcm_count = self.wonderswan_pcm_count;
        song.qsound_ids = self.qsound_ids.clone();
        song.qsound_ins = self.qsound_ins.clone();
        song.qsound_samples = self.qsound_samples.clone();
        song.qsound_count = self.qsound_count;
        song.qsound_adpcm_count = self.qsound_adpcm_count;
        song.scc_ids = self.scc_ids.clone();
        song.scc_waves = self.scc_waves.clone();
        song.scc_ins = self.scc_ins.clone();
        song.scc_count = self.scc_count;
        song.k005289_ids = self.k005289_ids.clone();
        song.k005289_waves = self.k005289_waves.clone();
        song.k005289_ins = self.k005289_ins.clone();
        song.k005289_count = self.k005289_count;
        song.ay_ids = self.ay_ids.clone();
        song.ay_ins = self.ay_ins.clone();
        song.ay_count = self.ay_count;
        song.pokey_ids = self.pokey_ids.clone();
        song.pokey_ins = self.pokey_ins.clone();
        song.pokey_count = self.pokey_count;
        song.ssg_pcm_samples = self.ssg_pcm_samples.clone();
        song.ssg_pcm_count = self.ssg_pcm_count;
        song.nes_ids = self.nes_ids.clone();
        song.nes_present = self.nes_present;
        song.nes_ins = self.nes_ins.clone();
        song.nes_dpcm_samples = self.nes_dpcm_samples.clone();
        song.nes_pcm_samples = self.nes_pcm_samples.clone();
        song.nes_count =
            self.nes_tones.len() as i32 + self.nes_dpcm_hits.len() as i32 + self.nes_pcm_hits.len() as i32;
        song.nes_pcm_count = self.nes_pcm_hits.len() as i32;
        song.huc6280_ids = self.huc6280_ids.clone();
        song.huc6280_waves = self.huc6280_waves.clone();
        song.huc6280_ins = self.huc6280_ins.clone();
        song.huc6280_pcm_samples = self.huc_pcm_samples.clone();
        song.huc6280_count = self.huc6280_count;
        song.huc6280_pcm_count = self.huc6280_pcm_count;
        song.gb_ids = self.gb_ids.clone();
        song.gb_waves = self.gb_waves.clone();
        song.gb_ins = self.gb_ins.clone();
        song.gb_count = self.gb_count;
        song.vsu_ids = self.vsu_ids.clone();
        song.vsu_waves = self.vsu_waves.clone();
        song.vsu_mod_waves = self.vsu_mod_waves.clone();
        song.vsu_ins = self.vsu_ins.clone();
        song.vsu_count = self.vsu_count;
        song.vsu_dynamic = self.vsu_dynamic;
        song.saa_ids = self.saa_ids.clone();
        song.saa_ins = self.saa_ins.clone();
        song.saa_count = self.saa_count;
        song.psg_ids = self.psg_ids.clone();
        song.psg_ins = self.psg_ins.clone();
        song.psg_count = self.psg_count;
        song.psg_stereo = self.psg_stereo;
        song.psg_t6w28 = self.psg_t6w28;
        song.dac_samples = self.dac_samples.clone();
        song.dac_count = self.dac_count;
        song.dac_stats = self.dac_stats.clone();
        song.fm_systems = self.fm_systems.clone();
        song.fm_chip = self.fm_systems.first().map(|s| s.kind.clone()).unwrap_or_else(|| {
            if is_opl {
                self.opl_kind.unwrap_or("").to_string()
            } else if self.is_opll {
                "opll".into()
            } else if self.is_opnb {
                "ym2610".into()
            } else if self.is_opna {
                "ym2608".into()
            } else if self.is_opn {
                "ym2203".into()
            } else if self.is_opn2 {
                "ym2612".into()
            } else {
                "ym2151".into()
            }
        });
        song.has_fm_source = vgm.writes.is_empty()
            || vgm.writes.iter().any(|w| {
                matches!(
                    w.chip,
                    "ym2151" | "ym2203" | "ym2612" | "ym2610" | "ym2608" | "opll" | "opl"
                        | "opl2" | "opl3" | "y8950" | "ymf278b"
                )
            });
        song.ym2151_ids = self.opm_ids.clone();
        song.ym2203_ids = self.ym_ids.clone();
        song.ym2203_ext = self.ym2203_ext;
        song.ym2203_ssg_count = self.ym2203_ssg_count;
        song.ym2612_ids = self.ym2612_ids.clone();
        song.ym2612_ext = self.ym2612_ext;
        song.ym2612_csm = self.ym2612_csm;
        song.opl_ids = self.opl_ids.clone();
        song.opl_drums = self.opl_drums;
        song.opll_ids = self.opll_ids.clone();
        song.opll_drums = self.opll_drums;
        song.opll_vrc7 = self.opll_vrc7;
        song.ym2610_ids = self.ym2610_ids.clone();
        song.ym2610_b = self.ym2610_b;
        song.ym2610_ssg_count = self.ym2610_ssg_count;
        song.ym2610_a_samples = self.ym2610_a_samples.clone();
        song.ym2610_b_samples = self.ym2610_b_samples.clone();
        song.ym2610_a_count = self.ym2610_a_count;
        song.ym2610_b_count = self.ym2610_b_count;
        song.ym2608_ids = self.ym2608_ids.clone();
        song.ym2608_ext = self.ym2608_ext;
        song.ym2608_ssg_count = self.ym2608_ssg_count;
        song.ym2608_rhythm_count = self.ym2608_rhythm_count;
        song.ym2608_rhythm_notes = self.ym2608_rhythm_events.len() as i32;
        song.ym2608_b_samples = self.ym2608_b_samples.clone();
        song.ym2608_b_count = self.ym2608_b_count;
        song.y8950_b_samples = self.y8950_b_samples.clone();
        song.y8950_b_count = self.y8950_b_count;
        song.opl4_wave_samples = self.opl4_wave_samples.clone();
        song.opl4_wave_count = self.opl4_wave_count;
        song.c352_quad = self.c352_quad_events;
        song.fm_vol_max = super::_fm_vol_max(&self.fm_systems, self.is_opll, self.is_opl);
        song.fm_notes = if self.is_opll {
            self.opll_rhythm.get("notes").copied().unwrap_or(0) as i32
        } else if self.is_opl {
            self.opl_rhythm.get("notes").copied().unwrap_or(0) as i32
        } else {
            0
        };
        song.drum_notes = if self.is_opll {
            self.opll_rhythm.get("drums").copied().unwrap_or(0) as i32
        } else if self.is_opl {
            self.opl_rhythm.get("drums").copied().unwrap_or(0) as i32
        } else {
            0
        };
        song.k007232_volume = vgm
            .chip_volume(0x2A)
            .unwrap_or(0.20);
        song.legato_steps = self.legato_steps;
        song.legato_ramps = self.legato_ramps;
        if self.convert_opm {
            transpose_opm_events(
                &mut song.events,
                if vgm.ym2151_clock != 0 { Some(vgm.ym2151_clock) } else { None },
                8 * (self.opm_ids.len() as i32).max(1),
            );
        }
        song
    }
}


impl<'a> AnalyzeState<'a> {
    fn nes_noise_note(period: i32) -> i32 {
        60 + (15 - (period & 0x0F))
    }

    fn nes_period_delta(base: i32, new: i32) -> i32 {
        if base < 0 || new < 0 {
            return 0;
        }
        py_round(64.0 * 12.0 * ((base + 1) as f64 / (new + 1) as f64).log2()) as i32
    }

    fn merge_step_writes_bytes(
        pts: Vec<(i64, Vec<u8>)>,
        gap: i64,
    ) -> Vec<(i64, Vec<u8>)> {
        let mut out: Vec<(i64, Vec<u8>)> = Vec::new();
        for p in pts {
            if let Some(last) = out.last_mut() {
                if p.0 - last.0 <= gap {
                    *last = p;
                    continue;
                }
            }
            out.push(p);
        }
        out
    }
}


impl<'a> AnalyzeState<'a> {
    fn value_at_bytes(pts: &[(i64, Vec<u8>)], start: i64, default: &[u8]) -> Vec<u8> {
        let mut cur = default.to_vec();
        for item in pts {
            if item.0 <= start {
                cur = item.1.clone();
            } else {
                break;
            }
        }
        cur
    }
}

// __MORE__
