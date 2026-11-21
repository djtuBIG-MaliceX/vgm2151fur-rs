//! The `.fur` module assembler: system layout, channel labels, and
//! `write_fur`'s block sequence, ported literally from `furwrite.py`
//! (v0.11.0, lines 4539-7334).
//!
//! Every block writes into one [`Buf`]; the directory pointer arrays are
//! patched after the payloads land, exactly like the Python writer.

use std::collections::{BTreeMap, BTreeSet};

use indexmap::IndexMap;

use crate::fm::FMPatch;
use crate::model::{FmSystem, Song};
use crate::parity::{py_int, py_round};

use super::buf::{
    compress_fur, decompress_fur, Buf, CHIP_AY8910,
    CHIP_C140, CHIP_C219, CHIP_C352, CHIP_FDS, CHIP_GA20, CHIP_GB, CHIP_K005289,
    CHIP_K051649, CHIP_K052539, CHIP_K053260, CHIP_K007232, CHIP_LYNX,
    CHIP_MULTIPCM, CHIP_MSM5232, CHIP_MSM6258, CHIP_MSM6295, CHIP_NES,
    CHIP_OPLL, CHIP_OPLL_DRUMS, CHIP_PCE,
    CHIP_PCM_DAC, CHIP_POKEY, CHIP_QSOUND, CHIP_RF5C68, CHIP_SAA1099,
    CHIP_SEGAPCM, CHIP_SMS, CHIP_SWAN, CHIP_T6W28, CHIP_VBOY, CHIP_VRC7,
    CHIP_X1_010, CHIP_YM2151, CHIP_YM2203, CHIP_YM2203_EXT, CHIP_YM2608,
    CHIP_YM2608_EXT, CHIP_YM2610, CHIP_YM2610B, CHIP_YM2612, CHIP_YM2612_EXT,
    CHIP_YMZ280B, DAC_RATE_COMPENSATION,
    OPL_CHIP,
    DEFAULT_RENDER_MASKS, EMPTY, FUR_MAGIC, FUR_VERSION, INS_ADPCMA, INS_ADPCMB,
    INS_AMIGA, INS_AY, INS_C140, INS_C219, INS_C352, INS_GA20,
    INS_K007232, INS_K053260, INS_MSM5232, INS_MSM6258, INS_MSM6295,
    INS_MULTIPCM, INS_NES, INS_RF5C68, INS_SEGAPCM, INS_STD, INS_T6W28,
    INS_SWAN, INS_X1_010, INS_YMZ280B, OPL_CHANNELS, OPL_MELODIC, OPLL_CHANNELS,
    OPLL_DRUMS_CHANNELS, OPLL_DRUMS_MELODIC, PAT_LEN_MAX, SEGAPCM_CLOCK_X,
    SMP_DEPTH_16BIT, SMP_DEPTH_8BIT, SMP_DEPTH_ADPCM_A,
    SMP_DEPTH_ADPCM_B, SMP_DEPTH_ADPCM_K, SMP_DEPTH_YMZ_ADPCM,
    T6W28_DEFAULT_CLOCK, WS_PCM_RATE_COMPENSATION, ADPCM_B_RATE_COMPENSATION,
    _player_chip_volume, _sample_chip_volume, _version,
};
use super::ins::{
    _pcm_label, _wave_label, _write_empty_adir, _write_flag, _write_ins_ay,
    _write_ins_fds, _write_ins_gb, _write_ins_mikey, _write_ins_nes,
    _write_ins_opm, _write_ins_pcm, _write_ins_pce, _write_ins_pokey,
    _write_ins_psg, _write_ins_qsound, _write_ins_sample_map, _write_ins_saa,
    _write_ins_scc, _write_ins_swan, _write_ins_vsu, _write_ins_x1, _write_sample,
    _write_sample_4bit, _write_sample_adpcm, _write_sample_dpcm,
    _write_sample_k, _write_sample_qsound_adpcm, _write_sample_s12,
    _write_sample_s8, _write_sample_ymz_adpcm, _write_vox, _write_wave,
    ay8910_flag_text, c140_flag_text, c219_flag_text, c352_flag_text,
    ga20_flag_text, k005289_flag_text, k051649_flag_text, k053260_flag_text,
    msm5232_flag_text, multipcm_flag_text, pce_flag_text, pcm_dac_flag_text,
    pokey_flag_text, rf5c68_flag_text, saa1099_flag_text, segapcm_flag_text,
    ymz280b_flag_text,
};
use super::pattern::{_build_patterns, _fit_fx_cols, EmitHooks};
use super::row::{_compat_phase1, _compat_phase2, _write_patn, Row};

use crate::c219::C219_DEFAULT_CLOCK;
use crate::c352::C352_DEFAULT_CLOCK;
use crate::mikey::MIKEY_VOICES;
use crate::nes::nes_dpcm_rate;
use crate::oki::{msm6258_flag_text, oki_flag_text};
use crate::opl::{opl_module_clock, opll_clock_flag, FURNACE_CLOCK, FURNACE_CLOCK_OPL3};
use crate::pokey::POKEY_VOICES;
use crate::qsound::QSOUND_ADPCM_NIBBLE_RATE;
use crate::saa1099::SAA_DEFAULT_CLOCK;
use crate::x1_010::X1_CHANNELS;
use crate::ym2608::{deltat_store_body, RHYTHM_VOICES, YM2608_DEFAULT_CLOCK};
use crate::ymz280b::ymz280b_clock;

use crate::c140::C140Sample;
use crate::c219::C219Sample;
use crate::c352::C352Sample;
use crate::es5503::Es5503Sample;
use crate::ga20::Ga20Sample;
use crate::huc6280::Huc6280PcmSample;
use crate::k053260::K053260Sample;
use crate::k054539::K054539Sample;
use crate::mikey::MikeySample;
use crate::msm5205::Msm5205Sample;
use crate::multipcm::MultipcmSample;
use crate::nes::{NesDpcmSample, NesPcmSample};
use crate::oki::{AdpcmSample, OkiSample};
use crate::pcm::PcmSample;
use crate::psg_dac::PsgDacSample;
use crate::pwm::PwmSample;
use crate::qsound::QSoundSample;
use crate::rf5c::Rf5cSample;
use crate::segapcm::SegapcmSample;
use crate::ssgpcm::SsgPcmSample;
use crate::upd7759::Upd7759Sample;
use crate::wonderswan::WsPcmSample;
use crate::x1_010::X1PcmHit;
use crate::ym2610::{AdpcmASample, AdpcmBSample};
use crate::ym2612dac::DacSample;
use crate::ymf278b::Opl4WaveSample;
use crate::ymz280b::YmzSample;

/// Module columns one FM system occupies, sample engines included.
pub fn _system_span(system: &FmSystem) -> i32 {
    system.channels()
}

/// The volume column's ceiling for every module channel.
///
/// Python default: `ch_offset=0`.
pub fn _channel_vol_max(song: &Song, n_ch: i32, default: i32, ch_offset: i32) -> Vec<i32> {
    let mut table = vec![default; n_ch.max(0) as usize];
    let systems: Vec<&FmSystem> = song.fm_systems.iter().collect();
    let kinds: BTreeSet<&str> = systems.iter().map(|s| s.kind.as_str()).collect();
    // With no FM block in the module the sample chips own these indices, and
    // their fades use their own column scale.
    if ch_offset != 0 || kinds.len() < 2 {
        return table;
    }
    let mut ch: i32 = 0;
    for system in systems {
        let top = if system.kind == "opll" {
            15
        } else if matches!(
            system.kind.as_str(),
            "opl" | "opl2" | "opl3" | "y8950" | "opl4"
        ) {
            63
        } else {
            127
        };
        for _ in 0..system.fm_channels {
            if ch < n_ch {
                if let Some(slot) = table.get_mut(ch as usize) {
                    *slot = top;
                }
            }
            ch += 1;
        }
        // SSG and sample columns ride past that family's FM block.
        ch += _system_span(system) - system.fm_channels;
    }
    table
}

/// One system for a Song built before `fm_systems` existed.
pub fn _legacy_system(song: &Song) -> Vec<FmSystem> {
    let kind = song.fm_chip.as_str();
    if matches!(kind, "opl" | "opl2" | "opl3") {
        let drums = song.opl_drums;
        let mut ids = song.opl_ids.clone();
        if ids.is_empty() {
            ids.push(0);
        }
        return ids
            .into_iter()
            .map(|cid| FmSystem {
                kind: kind.to_string(),
                chip_id: cid as u8,
                fm_channels: OPL_CHANNELS[&(kind, drums)],
                drums,
                ..Default::default()
            })
            .collect();
    }
    if kind == "opll" {
        let drums = song.opll_drums;
        let vrc7 = song.opll_vrc7;
        let mut ids = song.opll_ids.clone();
        if ids.is_empty() {
            ids.push(0);
        }
        let channels = if vrc7 {
            6
        } else if drums {
            OPLL_DRUMS_CHANNELS
        } else {
            OPLL_CHANNELS
        };
        return ids
            .into_iter()
            .map(|cid| FmSystem {
                kind: "opll".to_string(),
                chip_id: cid as u8,
                fm_channels: channels,
                drums,
                vrc7,
                ..Default::default()
            })
            .collect();
    }
    let mut ids: Vec<i32> = match kind {
        "ym2151" => song.ym2151_ids.clone(),
        "ym2203" => song.ym2203_ids.clone(),
        "ym2612" => song.ym2612_ids.clone(),
        "ym2610" => song.ym2610_ids.clone(),
        "ym2608" => song.ym2608_ids.clone(),
        _ => Vec::new(),
    };
    if ids.is_empty() {
        ids.push(0);
    }
    if kind == "ym2610" {
        let fm = if song.ym2610_b { 6 } else { 4 };
        return ids
            .into_iter()
            .map(|cid| FmSystem {
                kind: kind.to_string(),
                chip_id: cid as u8,
                fm_channels: fm,
                ..Default::default()
            })
            .collect();
    }
    if kind == "ym2608" {
        let ext = song.ym2608_ext;
        return ids
            .into_iter()
            .map(|cid| FmSystem {
                kind: kind.to_string(),
                chip_id: cid as u8,
                fm_channels: if ext { 9 } else { 6 },
                ext,
                ..Default::default()
            })
            .collect();
    }
    if kind == "ym2612" {
        let ext = song.ym2612_ext;
        let csm = song.ym2612_csm;
        return ids
            .into_iter()
            .map(|cid| FmSystem {
                kind: kind.to_string(),
                chip_id: cid as u8,
                fm_channels: if ext { 9 } else { 6 },
                ext,
                csm,
                ..Default::default()
            })
            .collect();
    }
    if kind == "ym2203" {
        let ext = song.ym2203_ext;
        return ids
            .into_iter()
            .map(|cid| FmSystem {
                kind: kind.to_string(),
                chip_id: cid as u8,
                fm_channels: if ext { 6 } else { 3 },
                ext,
                ..Default::default()
            })
            .collect();
    }
    vec![FmSystem {
        kind: "ym2151".to_string(),
        chip_id: 0,
        fm_channels: 8,
        ..Default::default()
    }]
}

/// Channel labels for every FM system, in module order.
pub fn _append_system_names(
    song: &Song,
    systems: &[FmSystem],
    names_long: &mut Vec<String>,
    names_short: &mut Vec<String>,
) {
    let mut counts: IndexMap<String, i64> = IndexMap::new();
    for system in systems {
        *counts.entry(system.kind.clone()).or_insert(0) += 1;
    }
    let mixed = counts.len() > 1;
    let mut seen: IndexMap<String, i64> = IndexMap::new();
    let mut ch0: i32 = 0;
    for system in systems {
        let kind = system.kind.as_str();
        *seen.entry(system.kind.clone()).or_insert(0) += 1;
        let instance = seen[system.kind.as_str()];
        let cid = system.chip_id as i32;
        let span = _system_span(system);
        // An instance suffix only appears when the family has more than one
        // chip, matching the long names the older writer produced.
        let suffix = if counts[kind] > 1 {
            format!(" ({})", instance)
        } else {
            String::new()
        };
        let mut longs: Vec<String> = Vec::new();
        let mut shorts: Vec<String> = Vec::new();
        if kind == "ym2610" {
            let fm = if system.fm_channels != 0 {
                system.fm_channels
            } else if song.ym2610_b {
                6
            } else {
                4
            };
            for voice in 0..fm {
                longs.push(format!("FM {}{}", voice + 1, suffix));
                shorts.push(format!("F{}-{}", cid + 1, voice + 1));
            }
            for voice in 0..3 {
                longs.push(format!("PSG {}{}", voice + 1, suffix));
                shorts.push(format!("P{}-{}", cid + 1, voice + 1));
            }
            for voice in 0..6 {
                longs.push(format!("ADPCM-A {}{}", voice + 1, suffix));
                shorts.push(format!("A{}-{}", cid + 1, voice + 1));
            }
            longs.push(format!("ADPCM-B{}", suffix));
            shorts.push(format!("B{}", cid + 1));
        } else if kind == "ym2608" {
            // Furnace names the OPNA's kit columns Kick, Snare, Top, HiHat,
            // Tom and Rim (sysDef.cpp:1365-1370); delta-T is simply ADPCM.
            if system.ext {
                for (voice, label) in ["FM 1", "FM 2"].iter().enumerate() {
                    longs.push(format!("{}{}", label, suffix));
                    shorts.push(format!("F{}-{}", cid + 1, voice + 1));
                }
                for voice in 0..4 {
                    // Column c carries operator (1,2,0,3)[c]'s frequency.
                    let op_label = [1, 2, 0, 3][voice] + 1;
                    longs.push(format!("FM 3 OP{}{}", op_label, suffix));
                    shorts.push(format!("O{}-{}", cid + 1, op_label));
                }
                for voice in 0..3 {
                    longs.push(format!("FM {}{}", voice + 4, suffix));
                    shorts.push(format!("F{}-{}", cid + 1, voice + 4));
                }
            } else {
                for voice in 0..6 {
                    longs.push(format!("FM {}{}", voice + 1, suffix));
                    shorts.push(format!("F{}-{}", cid + 1, voice + 1));
                }
            }
            for voice in 0..3 {
                longs.push(format!("PSG {}{}", voice + 1, suffix));
                shorts.push(format!("P{}-{}", cid + 1, voice + 1));
            }
            for (tag, long_name) in RHYTHM_VOICES.iter() {
                longs.push(format!("{}{}", long_name, suffix));
                shorts.push(format!("{}{}", tag, cid + 1));
            }
            longs.push(format!("ADPCM{}", suffix));
            shorts.push(format!("D{}", cid + 1));
        } else if kind == "ym2612" {
            if system.ext {
                for (voice, label) in ["FM 1", "FM 2"].iter().enumerate() {
                    longs.push(format!("{}{}", label, suffix));
                    shorts.push(format!("F{}-{}", cid + 1, voice + 1));
                }
                for voice in 0..4 {
                    let op_label = [1, 2, 0, 3][voice] + 1;
                    longs.push(format!("FM 3 OP{}{}", op_label, suffix));
                    shorts.push(format!("O{}-{}", cid + 1, op_label));
                }
                for voice in 0..3 {
                    longs.push(format!("FM {}{}", voice + 4, suffix));
                    shorts.push(format!("F{}-{}", cid + 1, voice + 4));
                }
            } else {
                for voice in 0..6 {
                    longs.push(format!("FM {}{}", voice + 1, suffix));
                    shorts.push(format!("F{}-{}", cid + 1, voice + 1));
                }
            }
        } else if kind == "ym2203" {
            if system.ext {
                for (voice, label) in ["FM 1", "FM 2"].iter().enumerate() {
                    longs.push(format!("{}{}", label, suffix));
                    shorts.push(format!("F{}-{}", cid + 1, voice + 1));
                }
                for voice in 0..4 {
                    let op_label = [1, 2, 0, 3][voice] + 1;
                    longs.push(format!("FM 3 OP{}{}", op_label, suffix));
                    shorts.push(format!("O{}-{}", cid + 1, op_label));
                }
                for voice in 0..3 {
                    longs.push(format!("PSG {}{}", voice + 1, suffix));
                    shorts.push(format!("P{}-{}", cid + 1, voice + 1));
                }
            } else {
                for voice in 0..3 {
                    longs.push(format!("FM {}-{}", cid + 1, voice + 1));
                    shorts.push(format!("F{}-{}", cid + 1, voice + 1));
                }
                for voice in 0..3 {
                    longs.push(format!("PSG {}-{}", cid + 1, voice + 1));
                    shorts.push(format!("P{}-{}", cid + 1, voice + 1));
                }
            }
        } else if matches!(kind, "opl" | "opl2" | "opl3" | "y8950" | "opl4") {
            let melodic = OPL_MELODIC[&(kind, system.drums)];
            let (long_names, short_names) = super::buf::opl_channel_names(kind, system.drums);
            for (i, (long_name, short_name)) in
                long_names.iter().zip(short_names.iter()).enumerate()
            {
                longs.push(format!("{}{}", long_name, suffix));
                if mixed && (i as i32) < melodic {
                    // Numbered by module channel, so the OPN family beside it
                    // never answers to the same tag.
                    shorts.push(format!("F{}", ch0 + i as i32 + 1));
                } else if counts[kind] > 1 {
                    // F2-3 is chip 2's FM 3; a kit column keeps the instance.
                    let rest = &short_name[1..];
                    if short_name.starts_with('F')
                        && !rest.is_empty()
                        && rest.chars().all(|c| c.is_ascii_digit())
                    {
                        shorts.push(format!("F{}-{}", cid + 1, rest));
                    } else {
                        shorts.push(format!("{}{}", short_name, instance));
                    }
                } else {
                    shorts.push(short_name.clone());
                }
            }
            if kind == "y8950" {
                // The ADPCM voice is one column after the FM block.
                longs.push(format!("ADPCM{}", suffix));
                shorts.push(if counts[kind] > 1 {
                    format!("P{}", cid + 1)
                } else {
                    "ADPCM".to_string()
                });
            }
            if kind == "opl4" {
                // The 24 wave slots follow the FM block.
                for i in 0..24 {
                    longs.push(format!("PCM {}{}", i + 1, suffix));
                    shorts.push(if counts[kind] > 1 {
                        format!("P{}-{}", cid + 1, i + 1)
                    } else {
                        format!("P{}", i + 1)
                    });
                }
            }
        } else if kind == "opll" {
            let (long_names, short_names) =
                super::buf::opll_channel_names(system.drums, system.vrc7);
            for (i, (long_name, short_name)) in
                long_names.iter().zip(short_names.iter()).enumerate()
            {
                longs.push(format!("{}{}", long_name, suffix));
                if mixed
                    && (i as i32)
                        < (if system.drums {
                            OPLL_DRUMS_MELODIC
                        } else {
                            OPLL_CHANNELS
                        })
                {
                    shorts.push(format!("F{}", ch0 + i as i32 + 1));
                } else if counts[kind] > 1 {
                    let rest = &short_name[1..];
                    if short_name.starts_with('F')
                        && !rest.is_empty()
                        && rest.chars().all(|c| c.is_ascii_digit())
                    {
                        shorts.push(format!("F{}-{}", cid + 1, rest));
                    } else {
                        shorts.push(format!("{}{}", short_name, instance));
                    }
                } else {
                    shorts.push(short_name.clone());
                }
            }
        } else {
            let fm = if system.fm_channels != 0 {
                system.fm_channels
            } else {
                8
            };
            longs = (0..fm).map(|i| format!("FM {}", i + 1)).collect();
            shorts = (0..fm).map(|i| format!("F{}", i + 1)).collect();
        }
        names_long.extend(longs);
        names_short.extend(shorts);
        ch0 += span;
    }
}

/// The index remap behind [`_compact_sample_map_notes`]: selected source
/// indices in ascending order, and the new instrument count.
fn _compact_indices(
    patterns: &mut [(i32, i32, Vec<Option<Row>>)],
    ins_base: i32,
    ins_count: i32,
    sample_len: usize,
) -> (Vec<usize>, i32) {
    if ins_count <= 0 || sample_len == 0 {
        return ((0..sample_len).collect(), ins_count);
    }
    let mut used: BTreeSet<i32> = BTreeSet::new();
    for (_ch, _idx, rows) in patterns.iter() {
        for row in rows.iter().flatten() {
            let ins = row.ins;
            let note = row.note;
            if ins_base <= ins && ins < ins_base + ins_count && (60..180).contains(&note) {
                used.insert((ins - ins_base) * 120 + (note - 60));
            }
        }
    }
    if used.is_empty() {
        return (Vec::new(), 0);
    }
    if *used.iter().next_back().unwrap() >= sample_len as i32 {
        return ((0..sample_len).collect(), ins_count);
    }
    let ordered: Vec<i32> = used.iter().copied().collect();
    let remap: BTreeMap<i32, i32> = ordered
        .iter()
        .enumerate()
        .map(|(new, &old)| (old, new as i32))
        .collect();
    for (_ch, _idx, rows) in patterns.iter_mut() {
        for row in rows.iter_mut().flatten() {
            let ins = row.ins;
            let note = row.note;
            if ins_base <= ins && ins < ins_base + ins_count && (60..180).contains(&note) {
                let old = (ins - ins_base) * 120 + (note - 60);
                let new = remap[&old];
                row.ins = ins_base + new / 120;
                row.note = 60 + new % 120;
            }
        }
    }
    (
        ordered.iter().map(|&i| i as usize).collect(),
        (ordered.len() as i32 + 119) / 120,
    )
}

/// Drop sample-map bodies no placed note selects.
pub fn _compact_sample_map_notes<'a, T>(
    patterns: &mut [(i32, i32, Vec<Option<Row>>)],
    ins_base: i32,
    ins_count: i32,
    samples: &'a [T],
) -> (Vec<&'a T>, i32) {
    let (indices, n_ins) = _compact_indices(patterns, ins_base, ins_count, samples.len());
    (indices.iter().map(|&i| &samples[i]).collect(), n_ins)
}

/// Move later instrument ids when a sample-map group is removed.
pub fn _shift_sample_map_instruments(
    patterns: &mut [(i32, i32, Vec<Option<Row>>)],
    start: i32,
    delta: i32,
) {
    if delta == 0 {
        return;
    }
    for (_ch, _idx, rows) in patterns.iter_mut() {
        for row in rows.iter_mut().flatten() {
            let ins = row.ins;
            if ins != EMPTY && ins >= start {
                row.ins = ins + delta;
            }
        }
    }
}

/// Writer options, mirroring the Python keyword arguments.
pub struct WriteOptions<'a> {
    pub include_pcm: bool,
    pub include_fm: bool,
    pub stats: Option<&'a mut IndexMap<String, i64>>,
    pub compress: bool,
}

impl Default for WriteOptions<'_> {
    fn default() -> Self {
        WriteOptions {
            include_pcm: true,
            include_fm: true,
            stats: None,
            compress: false,
        }
    }
}

/// State for one `write_fur` call: the Python function's locals, in type.
pub struct FurWriter<'a> {
    pub song: &'a Song,
    include_pcm: bool,
    include_fm: bool,
    stats: Option<&'a mut IndexMap<String, i64>>,
    compress: bool,
    b: Buf,

    systems: Vec<FmSystem>,
    kinds: BTreeSet<String>,
    ym2203: bool,
    ym2612: bool,
    ym2610: bool,
    ym2608: bool,
    opll: bool,
    opll_drums: bool,
    opll_ids: Vec<i32>,
    opl_kind: Option<String>,
    opl: bool,
    opl_drums: bool,
    opl_ids: Vec<i32>,
    ym_chip: bool,
    ym_kind: Option<String>,
    ym_ids: Vec<i32>,
    ym_ext: bool,
    fm_present: bool,
    fm_ins: &'a [FMPatch],

    pcm_smps: Vec<&'a PcmSample>,
    pcm_ids: Vec<u8>,
    oki_smps: Vec<&'a OkiSample>,
    adpcm_smps: Vec<&'a AdpcmSample>,
    n_adpcm_ins: i32,
    spcm_smps: Vec<&'a SegapcmSample>,
    spcm_ids: Vec<i32>,
    rf5c68_smps: Vec<&'a Rf5cSample>,
    rf5c164_smps: Vec<&'a Rf5cSample>,
    c140_smps: Vec<&'a C140Sample>,
    c219_all: Vec<&'a C219Sample>,
    c219_real: Vec<(usize, &'a C219Sample)>,
    c219_ids: Vec<i32>,
    c352_smps: Vec<&'a C352Sample>,
    k053260_smps: Vec<&'a K053260Sample>,
    k054539_smps: Vec<&'a K054539Sample>,
    k054539_ids: Vec<i32>,
    ymz_smps: Vec<&'a YmzSample>,
    ymz_ids: Vec<i32>,
    multipcm_smps: Vec<&'a MultipcmSample>,
    multipcm_ids: Vec<i32>,
    upd7759_smps: Vec<&'a Upd7759Sample>,
    upd7759_ids: Vec<i32>,
    pwm_smps: Vec<&'a PwmSample>,
    pwm_ids: Vec<i32>,
    msm5205_smps: Vec<&'a Msm5205Sample>,
    msm5205_ids: Vec<i32>,
    ga20_smps: Vec<&'a Ga20Sample>,
    ga20_ids: Vec<i32>,
    msm5232_ids: Vec<i32>,
    msm5232_controls: Vec<i32>,
    es5503_smps: Vec<&'a Es5503Sample>,
    es5503_oscs: Vec<(i32, i32)>,
    es5503_ids: Vec<i32>,
    n_es5503_ins: i32,
    psg_dac_smps: Vec<&'a PsgDacSample>,
    psg_dac_voices: Vec<(i32, i32)>,
    psg_dac_rate: u32,
    n_psg_dac_ins: i32,
    n_upd_ins: i32,
    n_pwm_ins: i32,
    n_msm5205_ins: i32,
    n_ga20_ins: i32,
    n_msm5232_ins: i32,
    mikey_smps: Vec<&'a MikeySample>,
    mikey_ids: Vec<i32>,
    n_mikey_ins: i32,
    x1_smps: Vec<&'a X1PcmHit>,
    x1_ids: Vec<i32>,
    x1_waves: &'a [Vec<u8>],
    x1_envs: &'a [Vec<u8>],
    n_x1_ins: i32,
    dac_smps: Vec<&'a DacSample>,
    n_dac_ins: i32,
    ym2610_a_smps: Vec<&'a AdpcmASample>,
    ym2610_b_smps: Vec<&'a AdpcmBSample>,
    n_ym2610_a_ins: i32,
    n_ym2610_b_ins: i32,
    ym2608_b_smps: Vec<&'a AdpcmBSample>,
    n_ym2608_b_ins: i32,
    n_ym2608_rhythm_ins: i32,
    y8950_b_smps: Vec<&'a AdpcmBSample>,
    n_y8950_b_ins: i32,
    opl4_wave_smps: Vec<&'a Opl4WaveSample>,
    n_opl4_wave_ins: i32,
    ssg_pcm_smps: Vec<&'a SsgPcmSample>,
    n_ssg_pcm_ins: i32,
    oki_ids: Vec<i32>,
    oki_groups: IndexMap<i32, Vec<&'a OkiSample>>,
    adpcm_ids: Vec<i32>,
    adpcm_groups: IndexMap<i32, Vec<&'a AdpcmSample>>,
    c140_ids: Vec<i32>,
    c352_ids: Vec<i32>,
    k053260_ids: Vec<i32>,

    scc_ids: Vec<i32>,
    scc_waves: &'a [Vec<u8>],
    scc_specs: &'a [crate::model::ToneIns],
    ay_ids: Vec<i32>,
    ay_specs: &'a [crate::model::ToneIns],
    psg_ids: Vec<i32>,
    psg_specs: &'a [crate::model::ToneIns],
    nes_ids: Vec<i32>,
    nes_specs: &'a [crate::model::ToneIns],
    nes_dpcm_smps: Vec<&'a NesDpcmSample>,
    nes_pcm_smps: Vec<&'a NesPcmSample>,
    huc6280_ids: Vec<i32>,
    huc6280_waves: &'a [Vec<u8>],
    huc6280_specs: &'a [crate::model::ToneIns],
    huc6280_pcm_smps: Vec<&'a Huc6280PcmSample>,
    gb_ids: Vec<i32>,
    gb_waves: &'a [Vec<u8>],
    gb_specs: &'a [crate::model::ToneIns],
    vsu_ids: Vec<i32>,
    vsu_waves: &'a [Vec<u8>],
    vsu_mod_waves: &'a [Vec<u8>],
    vsu_specs: &'a [crate::model::ToneIns],
    saa_ids: Vec<i32>,
    saa_specs: &'a [crate::model::ToneIns],
    pokey_ids: Vec<i32>,
    pokey_specs: &'a [crate::model::ToneIns],
    mikey_specs: &'a [crate::model::ToneIns],
    x1_specs: &'a [crate::model::ToneIns],
    fds_ids: Vec<i32>,
    fds_specs: &'a [crate::model::ToneIns],
    fds_waves: &'a [Vec<u8>],
    fds_mod_waves: &'a [Vec<u8>],
    wonderswan_ids: Vec<i32>,
    wonderswan_specs: &'a [crate::model::ToneIns],
    wonderswan_waves: &'a [Vec<u8>],
    wonderswan_pcm_smps: Vec<&'a WsPcmSample>,
    qsound_ids: Vec<i32>,
    qsound_specs: &'a [crate::model::ToneIns],
    qsound_smps: Vec<&'a QSoundSample>,
    scc_chip: u8,
    k005289_ids: Vec<i32>,
    k005289_waves: &'a [Vec<u8>],
    k005289_specs: &'a [crate::model::ToneIns],

    n_scc: i32,
    n_ay: i32,
    n_k005289: i32,
    n_psg: i32,
    n_nes: i32,
    n_huc6280: i32,
    n_gb: i32,
    n_vsu: i32,
    n_saa: i32,
    n_pokey: i32,
    n_mikey: i32,
    n_x1: i32,
    n_fds: i32,
    n_wonderswan: i32,
    n_qsound: i32,
    n_nes_dpcm_ins: i32,
    n_nes_pcm_ins: i32,
    n_huc6280_pcm_ins: i32,

    want_k007: bool,
    n_fm: i32,
    n_ssg: i32,
    n_adpcm_a: i32,
    n_adpcm_b: i32,
    n_k007: i32,
    rf5c68_ids: Vec<i32>,
    rf5c164_ids: Vec<i32>,
    n_rf5c: i32,
    n_ch: i32,
    fx_cols: usize,
    k_clock: u32,
    spcm_clock: u32,
    rf5c68_clock: u32,
    rf5c164_clock: u32,
    c140_clock: u32,
    c219_clock: u32,
    k054539_clock: u32,
    c352_clock: u32,
    k053260_clock: u32,

    n_ins: i32,
    n_smp: i32,
    n_wav: i32,
    vsu_wave_base: i32,
    x1_wave_base: i32,
    x1_env_base: i32,
    fds_wave_base: i32,
    wonderswan_wave_base: i32,
    k005289_wave_base: i32,

    orders: Vec<Vec<i32>>,
    patterns: Vec<(i32, i32, Vec<Option<Row>>)>,
    effect_cols: Vec<i32>,
    pat_len: i32,
    n_ord: i32,
    n_pat: i32,

    dac_smp_base: i32,
    ym2610_a_smp_base: i32,
    ym2610_b_smp_base: i32,
    ym2608_b_smp_base: i32,
    y8950_b_smp_base: i32,
    opl4_wave_smp_base: i32,
    ssg_pcm_ins_base: i32,
    ssg_pcm_smp_base: i32,
    nes_dpcm_smp_base: i32,
    nes_pcm_smp_base: i32,
    huc6280_pcm_smp_base: i32,
    ymz_smp_base: i32,
    multipcm_smp_base: i32,
    upd7759_smp_base: i32,
    pwm_smp_base: i32,
    msm5205_smp_base: i32,
    ga20_smp_base: i32,
    c219_smp_base: i32,
    k054539_smp_base: i32,
    es5503_smp_base: i32,
    psg_dac_smp_base: i32,
    mikey_smp_base: i32,
    wonderswan_pcm_smp_base: i32,
    qsound_smp_base: i32,
    x1_smp_base: i32,

    names_long: Vec<String>,
    names_short: Vec<String>,
    chip_label: String,
    comment: String,

    chips: Vec<u8>,
    flag_texts: Vec<String>,
    ym2610_sys: IndexMap<i32, usize>,
    ym2608_sys: IndexMap<i32, usize>,
    opl_sys: IndexMap<i32, usize>,
    ymz_sys: IndexMap<i32, usize>,
    multipcm_sys: IndexMap<i32, usize>,
    pwm_sys: IndexMap<(i32, i32), usize>,
    msm5205_sys: IndexMap<i32, usize>,
    ga20_sys: IndexMap<i32, usize>,
    k054539_sys: IndexMap<i32, Vec<usize>>,
    msm5232_sys: IndexMap<i32, usize>,
    es5503_sys: IndexMap<(i32, i32), usize>,
    psg_dac_sys: IndexMap<(i32, i32), usize>,
    n_chips: usize,
    opl_first: usize,
    chip_vols: Vec<f64>,
    chip_pans: Vec<f64>,
    master_gain: f64,
    master_out: f64,
    vols: Vec<i64>,
    render_masks: [u32; 4],

    flag_ptr_off: i64,
    adir_off: i64,
    ins_ptr_off: i64,
    wave_ptr_off: i64,
    smp_ptr_off: i64,
    pat_ptr_off: i64,
    ins_ptrs: Vec<usize>,
    smp_ptrs: Vec<usize>,
    wave_ptrs: Vec<usize>,
    pat_ptrs: Vec<usize>,
}

impl<'a> FurWriter<'a> {
    pub fn new(song: &'a Song, opts: WriteOptions<'a>) -> Self {
        FurWriter {
            song,
            include_pcm: opts.include_pcm,
            include_fm: opts.include_fm,
            stats: opts.stats,
            compress: opts.compress,
            b: Buf::new(),
            systems: Vec::new(),
            kinds: BTreeSet::new(),
            ym2203: false,
            ym2612: false,
            ym2610: false,
            ym2608: false,
            opll: false,
            opll_drums: false,
            opll_ids: Vec::new(),
            opl_kind: None,
            opl: false,
            opl_drums: false,
            opl_ids: Vec::new(),
            ym_chip: false,
            ym_kind: None,
            ym_ids: Vec::new(),
            ym_ext: false,
            fm_present: false,
            fm_ins: &[],
            pcm_smps: Vec::new(),
            pcm_ids: Vec::new(),
            oki_smps: Vec::new(),
            adpcm_smps: Vec::new(),
            n_adpcm_ins: 0,
            spcm_smps: Vec::new(),
            spcm_ids: Vec::new(),
            rf5c68_smps: Vec::new(),
            rf5c164_smps: Vec::new(),
            c140_smps: Vec::new(),
            c219_all: Vec::new(),
            c219_real: Vec::new(),
            c219_ids: Vec::new(),
            c352_smps: Vec::new(),
            k053260_smps: Vec::new(),
            k054539_smps: Vec::new(),
            k054539_ids: Vec::new(),
            ymz_smps: Vec::new(),
            ymz_ids: Vec::new(),
            multipcm_smps: Vec::new(),
            multipcm_ids: Vec::new(),
            upd7759_smps: Vec::new(),
            upd7759_ids: Vec::new(),
            pwm_smps: Vec::new(),
            pwm_ids: Vec::new(),
            msm5205_smps: Vec::new(),
            msm5205_ids: Vec::new(),
            ga20_smps: Vec::new(),
            ga20_ids: Vec::new(),
            msm5232_ids: Vec::new(),
            msm5232_controls: Vec::new(),
            es5503_smps: Vec::new(),
            es5503_oscs: Vec::new(),
            es5503_ids: Vec::new(),
            n_es5503_ins: 0,
            psg_dac_smps: Vec::new(),
            psg_dac_voices: Vec::new(),
            psg_dac_rate: 0,
            n_psg_dac_ins: 0,
            n_upd_ins: 0,
            n_pwm_ins: 0,
            n_msm5205_ins: 0,
            n_ga20_ins: 0,
            n_msm5232_ins: 0,
            mikey_smps: Vec::new(),
            mikey_ids: Vec::new(),
            n_mikey_ins: 0,
            x1_smps: Vec::new(),
            x1_ids: Vec::new(),
            x1_waves: &[],
            x1_envs: &[],
            n_x1_ins: 0,
            dac_smps: Vec::new(),
            n_dac_ins: 0,
            ym2610_a_smps: Vec::new(),
            ym2610_b_smps: Vec::new(),
            n_ym2610_a_ins: 0,
            n_ym2610_b_ins: 0,
            ym2608_b_smps: Vec::new(),
            n_ym2608_b_ins: 0,
            n_ym2608_rhythm_ins: 0,
            y8950_b_smps: Vec::new(),
            n_y8950_b_ins: 0,
            opl4_wave_smps: Vec::new(),
            n_opl4_wave_ins: 0,
            ssg_pcm_smps: Vec::new(),
            n_ssg_pcm_ins: 0,
            oki_ids: Vec::new(),
            oki_groups: IndexMap::new(),
            adpcm_ids: Vec::new(),
            adpcm_groups: IndexMap::new(),
            c140_ids: Vec::new(),
            c352_ids: Vec::new(),
            k053260_ids: Vec::new(),
            scc_ids: Vec::new(),
            scc_waves: &[],
            scc_specs: &[],
            ay_ids: Vec::new(),
            ay_specs: &[],
            psg_ids: Vec::new(),
            psg_specs: &[],
            nes_ids: Vec::new(),
            nes_specs: &[],
            nes_dpcm_smps: Vec::new(),
            nes_pcm_smps: Vec::new(),
            huc6280_ids: Vec::new(),
            huc6280_waves: &[],
            huc6280_specs: &[],
            huc6280_pcm_smps: Vec::new(),
            gb_ids: Vec::new(),
            gb_waves: &[],
            gb_specs: &[],
            vsu_ids: Vec::new(),
            vsu_waves: &[],
            vsu_mod_waves: &[],
            vsu_specs: &[],
            saa_ids: Vec::new(),
            saa_specs: &[],
            pokey_ids: Vec::new(),
            pokey_specs: &[],
            mikey_specs: &[],
            x1_specs: &[],
            fds_ids: Vec::new(),
            fds_specs: &[],
            fds_waves: &[],
            fds_mod_waves: &[],
            wonderswan_ids: Vec::new(),
            wonderswan_specs: &[],
            wonderswan_waves: &[],
            wonderswan_pcm_smps: Vec::new(),
            qsound_ids: Vec::new(),
            qsound_specs: &[],
            qsound_smps: Vec::new(),
            scc_chip: CHIP_K051649,
            k005289_ids: Vec::new(),
            k005289_waves: &[],
            k005289_specs: &[],
            n_scc: 0,
            n_ay: 0,
            n_k005289: 0,
            n_psg: 0,
            n_nes: 0,
            n_huc6280: 0,
            n_gb: 0,
            n_vsu: 0,
            n_saa: 0,
            n_pokey: 0,
            n_mikey: 0,
            n_x1: 0,
            n_fds: 0,
            n_wonderswan: 0,
            n_qsound: 0,
            n_nes_dpcm_ins: 0,
            n_nes_pcm_ins: 0,
            n_huc6280_pcm_ins: 0,
            want_k007: false,
            n_fm: 0,
            n_ssg: 0,
            n_adpcm_a: 0,
            n_adpcm_b: 0,
            n_k007: 0,
            rf5c68_ids: Vec::new(),
            rf5c164_ids: Vec::new(),
            n_rf5c: 0,
            n_ch: 0,
            fx_cols: 8,
            k_clock: 0,
            spcm_clock: 0,
            rf5c68_clock: 0,
            rf5c164_clock: 0,
            c140_clock: 0,
            c219_clock: 0,
            k054539_clock: 0,
            c352_clock: 0,
            k053260_clock: 0,
            n_ins: 0,
            n_smp: 0,
            n_wav: 0,
            vsu_wave_base: 0,
            x1_wave_base: 0,
            x1_env_base: 0,
            fds_wave_base: 0,
            wonderswan_wave_base: 0,
            k005289_wave_base: 0,
            orders: Vec::new(),
            patterns: Vec::new(),
            effect_cols: Vec::new(),
            pat_len: 0,
            n_ord: 0,
            n_pat: 0,
            dac_smp_base: 0,
            ym2610_a_smp_base: 0,
            ym2610_b_smp_base: 0,
            ym2608_b_smp_base: 0,
            y8950_b_smp_base: 0,
            opl4_wave_smp_base: 0,
            ssg_pcm_ins_base: 0,
            ssg_pcm_smp_base: 0,
            nes_dpcm_smp_base: 0,
            nes_pcm_smp_base: 0,
            huc6280_pcm_smp_base: 0,
            ymz_smp_base: 0,
            multipcm_smp_base: 0,
            upd7759_smp_base: 0,
            pwm_smp_base: 0,
            msm5205_smp_base: 0,
            ga20_smp_base: 0,
            c219_smp_base: 0,
            k054539_smp_base: 0,
            es5503_smp_base: 0,
            psg_dac_smp_base: 0,
            mikey_smp_base: 0,
            wonderswan_pcm_smp_base: 0,
            qsound_smp_base: 0,
            x1_smp_base: 0,
            names_long: Vec::new(),
            names_short: Vec::new(),
            chip_label: String::new(),
            comment: String::new(),
            chips: Vec::new(),
            flag_texts: Vec::new(),
            ym2610_sys: IndexMap::new(),
            ym2608_sys: IndexMap::new(),
            opl_sys: IndexMap::new(),
            ymz_sys: IndexMap::new(),
            multipcm_sys: IndexMap::new(),
            pwm_sys: IndexMap::new(),
            msm5205_sys: IndexMap::new(),
            ga20_sys: IndexMap::new(),
            k054539_sys: IndexMap::new(),
            msm5232_sys: IndexMap::new(),
            es5503_sys: IndexMap::new(),
            psg_dac_sys: IndexMap::new(),
            n_chips: 0,
            opl_first: 0,
            chip_vols: Vec::new(),
            chip_pans: Vec::new(),
            master_gain: 1.0,
            master_out: 1.0,
            vols: vec![64; 32],
            render_masks: DEFAULT_RENDER_MASKS,
            flag_ptr_off: 0,
            adir_off: 0,
            ins_ptr_off: 0,
            wave_ptr_off: 0,
            smp_ptr_off: 0,
            pat_ptr_off: 0,
            ins_ptrs: Vec::new(),
            smp_ptrs: Vec::new(),
            wave_ptrs: Vec::new(),
            pat_ptrs: Vec::new(),
        }
    }

    /// `_ids_of`: the chip ids of one family, in module order.
    fn _ids_of(&self, kind: Option<&str>) -> Vec<i32> {
        match kind {
            Some(kind) => self
                .systems
                .iter()
                .filter(|s| s.kind == kind)
                .map(|s| s.chip_id as i32)
                .collect(),
            None => Vec::new(),
        }
    }

    /// Emit every block in the Python writer's order.
    pub fn run(self) -> Vec<u8> {
        let mut hooks = EmitHooks { sweep_hook: None };
        self.run_with(&mut hooks)
    }

    /// `run` with the emitter hooks the reference tests install by patching
    /// `furwrite._emit_sweep` (the plan's `EmitHooks` seam).
    pub fn run_with(mut self, hooks: &mut EmitHooks<'_>) -> Vec<u8> {
        self.plan_with(hooks);
        self.write_header();
        self.write_systems();
        self.write_info();
        self.write_adir_flags();
        self.write_instruments();
        self.write_samples();
        self.write_patterns();
        self.write_waves();
        self.patch_directories();
        let data = self.b.data;
        if self.compress {
            compress_fur(&data, 5)
        } else {
            data
        }
    }

    fn gd3(&self, key: &str) -> &str {
        self.song.vgm.gd3.get(key).map(|s| s.as_str()).unwrap_or("")
    }
}

/// Assemble a Furnace module from an analyzed song.
pub fn write_fur(song: &Song, opts: WriteOptions<'_>) -> Vec<u8> {
    FurWriter::new(song, opts).run()
}

/// `write_fur` with the reference tests' `_emit_sweep` patch installed
/// (`tests/_synth.py`'s `inject_rows`): the hook runs after the writer's own
/// sweep emitter for every note with a sweep.
pub fn write_fur_with_hooks(
    song: &Song,
    opts: WriteOptions<'_>,
    hooks: &mut EmitHooks<'_>,
) -> Vec<u8> {
    FurWriter::new(song, opts).run_with(hooks)
}

impl<'a> FurWriter<'a> {
    /// The Python prologue: systems, sample inventories, counts and patterns.
    fn plan_with(&mut self, hooks: &mut EmitHooks<'_>) {
        self.plan_systems();
        self.plan_counts();
        self.plan_layout(hooks);
        self.plan_names();
    }

    fn plan_systems(&mut self) {
        let song = self.song;
        let include_pcm = self.include_pcm;
        let include_fm = self.include_fm;
        self.systems = if song.fm_systems.is_empty() {
            _legacy_system(song)
        } else {
            song.fm_systems.clone()
        };
        let kinds: BTreeSet<String> = self.systems.iter().map(|s| s.kind.clone()).collect();
        self.kinds = kinds;

        self.ym2203 = self.kinds.contains("ym2203");
        self.ym2612 = self.kinds.contains("ym2612");
        self.ym2610 = self.kinds.contains("ym2610");
        self.ym2608 = self.kinds.contains("ym2608");
        // The YM2413 is its own system (0x89) with a separate drums layout.
        self.opll = self.kinds.contains("opll");
        self.opll_drums = self
            .systems
            .iter()
            .any(|s| s.kind == "opll" && s.drums);
        self.opll_ids = self._ids_of(Some("opll"));
        // The OPL family is one module system per chip.
        self.opl_kind = ["opl3", "opl2", "opl"].iter()
            .find(|k| self.kinds.contains(**k))
            .map(|k| k.to_string());
        self.opl = self.opl_kind.is_some();
        self.opl_drums = match self.opl_kind.as_deref() {
            Some(kind) => self.systems.iter().any(|s| s.kind == kind && s.drums),
            None => false,
        };
        self.opl_ids = self._ids_of(self.opl_kind.as_deref());
        self.ym_chip = self.ym2203 || self.ym2612 || self.ym2610 || self.ym2608;
        self.ym_kind = if self.ym2608 {
            Some("ym2608".to_string())
        } else if self.ym2610 {
            Some("ym2610".to_string())
        } else if self.ym2612 {
            Some("ym2612".to_string())
        } else if self.ym2203 {
            Some("ym2203".to_string())
        } else {
            None
        };
        self.ym_ids = self._ids_of(self.ym_kind.as_deref());
        self.ym_ext = match self.ym_kind.as_deref() {
            Some(kind) => self.systems.iter().any(|s| s.kind == kind && s.ext),
            None => false,
        };
        let fm_source = song.has_fm_source;
        let has_tone_chip = !song.scc_ids.is_empty()
            || !song.ay_ids.is_empty()
            || !song.nes_ids.is_empty()
            || !song.huc6280_ids.is_empty()
            || !song.gb_ids.is_empty()
            || !song.vsu_ids.is_empty()
            || !song.saa_ids.is_empty()
            || !song.psg_ids.is_empty()
            || !song.pokey_ids.is_empty()
            || !song.mikey_ids.is_empty()
            || !song.x1_010_ids.is_empty()
            || !song.fds_ids.is_empty()
            || !song.wonderswan_ids.is_empty()
            || !song.qsound_ids.is_empty()
            || !song.k005289_ids.is_empty()
            || !song.ymz280b_samples.is_empty()
            || !song.multipcm_samples.is_empty()
            || !song.upd7759_samples.is_empty()
            || !song.c219_ids.is_empty()
            || !song.k054539_ids.is_empty()
            || !song.ga20_ids.is_empty()
            || !song.msm5205_ids.is_empty()
            || !song.msm5232_ids.is_empty()
            || !song.es5503_ids.is_empty()
            || !song.psg_dac_voices.is_empty();
        let ym_ids_present = !self.ym_ids.is_empty();
        let opl_ids_present = !self.opl_ids.is_empty();
        self.fm_present = if self.ym2610 || self.ym2608 {
            (include_fm || include_pcm) && ym_ids_present
        } else if self.opl {
            include_fm && (opl_ids_present || ym_ids_present)
        } else if self.opll {
            include_fm && !self.opll_ids.is_empty()
        } else if self.ym_chip {
            include_fm && ym_ids_present
        } else {
            // The Y8950 owns a delta-T column. Its samples require the chip
            // entry even when the caller excluded FM notes.
            let has_y8950_pcm = include_pcm && !song.y8950_b_samples.is_empty();
            (include_fm || has_y8950_pcm) && (fm_source || !has_tone_chip)
        };
        self.fm_ins = if self.fm_present && include_fm {
            &song.fm_patches
        } else {
            &[]
        };

        self.pcm_smps = if include_pcm {
            song.pcm_samples.iter().filter(|s| s.length() >= 8).collect()
        } else {
            Vec::new()
        };
        self.pcm_ids = self
            .pcm_smps
            .iter()
            .map(|s| s.chip_id)
            .collect::<BTreeSet<u8>>()
            .into_iter()
            .collect();
        self.oki_smps = if include_pcm {
            song.oki_samples.iter().collect()
        } else {
            Vec::new()
        };
        self.adpcm_smps = if include_pcm {
            song.msm6258_samples.iter().collect()
        } else {
            Vec::new()
        };
        self.n_adpcm_ins = (self.adpcm_smps.len() as i32 + 119) / 120;
        self.spcm_smps = if include_pcm {
            song.segapcm_samples.iter().filter(|s| s.length() >= 8).collect()
        } else {
            Vec::new()
        };
        self.spcm_ids = self
            .spcm_smps
            .iter()
            .map(|s| s.chip_id)
            .collect::<BTreeSet<i32>>()
            .into_iter()
            .collect();
        self.rf5c68_smps = if include_pcm {
            song.rf5c68_samples.iter().filter(|s| s.length() >= 8).collect()
        } else {
            Vec::new()
        };
        self.rf5c164_smps = if include_pcm {
            song.rf5c164_samples.iter().filter(|s| s.length() >= 8).collect()
        } else {
            Vec::new()
        };
        self.c140_smps = if include_pcm {
            song.c140_samples.iter().filter(|s| s.length() >= 8).collect()
        } else {
            Vec::new()
        };
        self.c219_all = if include_pcm {
            song.c219_samples.iter().collect()
        } else {
            Vec::new()
        };
        self.c219_real = self
            .c219_all
            .iter()
            .enumerate()
            .filter(|(_i, s)| !s.hits.is_empty() && s.length() > 0)
            .map(|(i, &s)| (i, s))
            .collect();
        self.c219_ids = if include_pcm {
            song.c219_ids.clone()
        } else {
            Vec::new()
        };
        if self.c219_ids.is_empty() {
            self.c219_ids = self
                .c219_real
                .iter()
                .flat_map(|(_i, s)| s.hits.iter().map(|h| h.chip_id))
                .collect::<BTreeSet<i32>>()
                .into_iter()
                .collect();
        }
        self.c352_smps = if include_pcm {
            song.c352_samples.iter().filter(|s| s.length() >= 8).collect()
        } else {
            Vec::new()
        };
        self.k053260_smps = if include_pcm {
            song.k053260_samples.iter().filter(|s| s.length() >= 8).collect()
        } else {
            Vec::new()
        };
        self.k054539_smps = if include_pcm {
            song.k054539_samples
                .iter()
                .filter(|s| !s.hits.is_empty() && s.length() > 0)
                .collect()
        } else {
            Vec::new()
        };
        self.k054539_ids = if include_pcm {
            song.k054539_ids.clone()
        } else {
            Vec::new()
        };
        if self.k054539_ids.is_empty() {
            self.k054539_ids = self
                .k054539_smps
                .iter()
                .flat_map(|s| s.hits.iter().map(|h| h.chip_id))
                .collect::<BTreeSet<i32>>()
                .into_iter()
                .collect();
        }
        self.ymz_smps = if include_pcm {
            song.ymz280b_samples.iter().collect()
        } else {
            Vec::new()
        };
        self.ymz_ids = self
            .ymz_smps
            .iter()
            .map(|s| s.chip_id)
            .collect::<BTreeSet<i32>>()
            .into_iter()
            .collect();
        self.multipcm_smps = if include_pcm {
            song.multipcm_samples.iter().collect()
        } else {
            Vec::new()
        };
        self.multipcm_ids = if include_pcm {
            song.multipcm_ids.clone()
        } else {
            Vec::new()
        };
        if self.multipcm_ids.is_empty() {
            self.multipcm_ids = self
                .multipcm_smps
                .iter()
                .map(|s| s.chip_id)
                .collect::<BTreeSet<i32>>()
                .into_iter()
                .collect();
        }
        self.upd7759_smps = if include_pcm {
            song.upd7759_samples.iter().collect()
        } else {
            Vec::new()
        };
        self.upd7759_ids = if include_pcm {
            song.upd7759_ids.clone()
        } else {
            Vec::new()
        };
        if self.upd7759_ids.is_empty() {
            self.upd7759_ids = self
                .upd7759_smps
                .iter()
                .map(|s| s.chip_id)
                .collect::<BTreeSet<i32>>()
                .into_iter()
                .collect();
        }
        self.pwm_smps = if include_pcm {
            song.pwm_samples.iter().collect()
        } else {
            Vec::new()
        };
        self.pwm_ids = if include_pcm {
            song.pwm_ids.clone()
        } else {
            Vec::new()
        };
        if self.pwm_ids.is_empty() {
            self.pwm_ids = self
                .pwm_smps
                .iter()
                .map(|s| s.chip_id as i32)
                .collect::<BTreeSet<i32>>()
                .into_iter()
                .collect();
        }
        self.msm5205_smps = if include_pcm {
            song.msm5205_samples.iter().collect()
        } else {
            Vec::new()
        };
        self.msm5205_ids = if include_pcm {
            song.msm5205_ids.clone()
        } else {
            Vec::new()
        };
        if self.msm5205_ids.is_empty() {
            self.msm5205_ids = self
                .msm5205_smps
                .iter()
                .map(|s| s.chip_id)
                .collect::<BTreeSet<i32>>()
                .into_iter()
                .collect();
        }
        self.ga20_smps = if include_pcm {
            song.ga20_samples.iter().collect()
        } else {
            Vec::new()
        };
        self.ga20_ids = if include_pcm {
            song.ga20_ids.clone()
        } else {
            Vec::new()
        };
        if self.ga20_ids.is_empty() {
            self.ga20_ids = self
                .ga20_smps
                .iter()
                .map(|s| s.chip_id)
                .collect::<BTreeSet<i32>>()
                .into_iter()
                .collect();
        }
        self.msm5232_ids = song.msm5232_ids.clone();
        self.msm5232_controls = song.msm5232_controls.clone();
        if !self.msm5232_ids.is_empty() && self.msm5232_controls.is_empty() {
            self.msm5232_controls = vec![0x0F];
        }
        self.es5503_smps = if include_pcm {
            song.es5503_samples
                .iter()
                .filter(|s| !s.hits.is_empty() && s.length() > 0)
                .collect()
        } else {
            Vec::new()
        };
        self.es5503_oscs = if include_pcm && !self.es5503_smps.is_empty() {
            song.es5503_oscs.clone()
        } else {
            Vec::new()
        };
        self.es5503_ids = self
            .es5503_smps
            .iter()
            .map(|s| s.chip_id)
            .collect::<BTreeSet<i32>>()
            .into_iter()
            .collect();
        self.n_es5503_ins = self.es5503_smps.len() as i32;
        self.psg_dac_smps = if include_pcm {
            song.psg_dac_samples.iter().collect()
        } else {
            Vec::new()
        };
        self.psg_dac_voices = if !self.psg_dac_smps.is_empty() {
            song.psg_dac_voices.clone()
        } else {
            Vec::new()
        };
        self.psg_dac_rate = if song.psg_dac_rate == 0 {
            22_050
        } else {
            song.psg_dac_rate as u32
        };
        self.n_psg_dac_ins = (self.psg_dac_smps.len() as i32 + 119) / 120;
        self.n_upd_ins = (self.upd7759_smps.len() as i32 + 119) / 120;
        self.n_pwm_ins = (self.pwm_smps.len() as i32 + 119) / 120;
        self.n_msm5205_ins = (self.msm5205_smps.len() as i32 + 119) / 120;
        self.n_ga20_ins = self.ga20_smps.len() as i32;
        self.n_msm5232_ins = if self.msm5232_ids.is_empty() {
            0
        } else {
            self.msm5232_controls.len() as i32
        };
        self.mikey_smps = if include_pcm {
            song.mikey_samples.iter().collect()
        } else {
            Vec::new()
        };
        self.mikey_ids = song.mikey_ids.clone();
        self.n_mikey_ins = (self.mikey_smps.len() as i32 + 119) / 120;
        self.x1_smps = if include_pcm {
            song.x1_010_samples.iter().collect()
        } else {
            Vec::new()
        };
        self.x1_ids = song.x1_010_ids.clone();
        self.x1_waves = &song.x1_waves;
        self.x1_envs = &song.x1_envs;
        self.n_x1_ins = (self.x1_smps.len() as i32 + 119) / 120;
        self.dac_smps = if include_pcm {
            song.dac_samples.iter().collect()
        } else {
            Vec::new()
        };
        self.n_dac_ins = (self.dac_smps.len() as i32 + 119) / 120;
        let dac_smp_base = self.pcm_smps.len() as i32
            + self.oki_smps.len() as i32
            + self.adpcm_smps.len() as i32
            + self.spcm_smps.len() as i32
            + self.rf5c68_smps.len() as i32
            + self.rf5c164_smps.len() as i32
            + self.c140_smps.len() as i32
            + self.c352_smps.len() as i32
            + self.k053260_smps.len() as i32;
        self.dac_smp_base = dac_smp_base;
        self.ym2610_a_smps = if include_pcm {
            song.ym2610_a_samples.iter().collect()
        } else {
            Vec::new()
        };
        self.ym2610_b_smps = if include_pcm {
            song.ym2610_b_samples.iter().collect()
        } else {
            Vec::new()
        };
        self.n_ym2610_a_ins = (self.ym2610_a_smps.len() as i32 + 119) / 120;
        self.n_ym2610_b_ins = (self.ym2610_b_smps.len() as i32 + 119) / 120;
        self.ym2610_a_smp_base = dac_smp_base + self.dac_smps.len() as i32;
        self.ym2610_b_smp_base =
            self.ym2610_a_smp_base + self.ym2610_a_smps.len() as i32;
        self.ym2608_b_smps = if include_pcm {
            song.ym2608_b_samples.iter().collect()
        } else {
            Vec::new()
        };
        self.n_ym2608_b_ins = (self.ym2608_b_smps.len() as i32 + 119) / 120;
        self.n_ym2608_rhythm_ins =
            if include_pcm && song.ym2608_rhythm_notes != 0 { 1 } else { 0 };
        self.ym2608_b_smp_base =
            self.ym2610_b_smp_base + self.ym2610_b_smps.len() as i32;
        self.y8950_b_smps = if include_pcm {
            song.y8950_b_samples.iter().collect()
        } else {
            Vec::new()
        };
        self.n_y8950_b_ins = (self.y8950_b_smps.len() as i32 + 119) / 120;
        self.y8950_b_smp_base =
            self.ym2608_b_smp_base + self.ym2608_b_smps.len() as i32;
        self.opl4_wave_smps = if include_pcm {
            song.opl4_wave_samples.iter().collect()
        } else {
            Vec::new()
        };
        self.n_opl4_wave_ins = self.opl4_wave_smps.len() as i32;
        self.opl4_wave_smp_base =
            self.y8950_b_smp_base + self.y8950_b_smps.len() as i32;
        self.ssg_pcm_smps = if include_pcm {
            song.ssg_pcm_samples.iter().collect()
        } else {
            Vec::new()
        };
        self.n_ssg_pcm_ins = (self.ssg_pcm_smps.len() as i32 + 119) / 120;

        // Chip ids in first-seen order, with the samples that belong to each.
        self.oki_ids = Vec::new();
        self.oki_groups = IndexMap::new();
        for smp in self.oki_smps.iter().copied() {
            if !self.oki_groups.contains_key(&smp.chip_id) {
                self.oki_ids.push(smp.chip_id);
                self.oki_groups.insert(smp.chip_id, Vec::new());
            }
            self.oki_groups.get_mut(&smp.chip_id).unwrap().push(smp);
        }
        self.adpcm_ids = Vec::new();
        self.adpcm_groups = IndexMap::new();
        for smp in self.adpcm_smps.iter().copied() {
            if !self.adpcm_groups.contains_key(&smp.chip_id) {
                self.adpcm_ids.push(smp.chip_id);
                self.adpcm_groups.insert(smp.chip_id, Vec::new());
            }
            self.adpcm_groups.get_mut(&smp.chip_id).unwrap().push(smp);
        }
        self.c140_ids = Vec::new();
        for smp in self.c140_smps.iter() {
            for h in smp.hits.iter() {
                if !self.c140_ids.contains(&h.chip_id) {
                    self.c140_ids.push(h.chip_id);
                }
            }
        }
        self.c352_ids = Vec::new();
        for smp in self.c352_smps.iter() {
            for h in smp.hits.iter() {
                if !self.c352_ids.contains(&h.chip_id) {
                    self.c352_ids.push(h.chip_id);
                }
            }
        }
        self.k053260_ids = Vec::new();
        for smp in self.k053260_smps.iter() {
            for h in smp.hits.iter() {
                if !self.k053260_ids.contains(&h.chip_id) {
                    self.k053260_ids.push(h.chip_id);
                }
            }
        }

        // Pitched (non-sample) chips: SCC voices and the AY-8910.
        self.scc_ids = song.scc_ids.clone();
        self.scc_waves = &song.scc_waves;
        self.scc_specs = &song.scc_ins;
        self.ay_ids = song.ay_ids.clone();
        self.ay_specs = &song.ay_ins;
        self.psg_ids = song.psg_ids.clone();
        self.psg_specs = &song.psg_ins;
        self.nes_ids = song.nes_ids.clone();
        self.nes_specs = &song.nes_ins;
        self.nes_dpcm_smps = song.nes_dpcm_samples.iter().collect();
        self.nes_pcm_smps = song.nes_pcm_samples.iter().collect();
        self.huc6280_ids = song.huc6280_ids.clone();
        self.huc6280_waves = &song.huc6280_waves;
        self.huc6280_specs = &song.huc6280_ins;
        self.huc6280_pcm_smps = song.huc6280_pcm_samples.iter().collect();
        self.gb_ids = song.gb_ids.clone();
        self.gb_waves = &song.gb_waves;
        self.gb_specs = &song.gb_ins;
        self.vsu_ids = song.vsu_ids.clone();
        self.vsu_waves = &song.vsu_waves;
        self.vsu_mod_waves = &song.vsu_mod_waves;
        self.vsu_specs = &song.vsu_ins;
        self.saa_ids = song.saa_ids.clone();
        self.saa_specs = &song.saa_ins;
        self.pokey_ids = song.pokey_ids.clone();
        self.pokey_specs = &song.pokey_ins;
        self.mikey_specs = &song.mikey_ins;
        self.x1_specs = &song.x1_010_ins;
        self.fds_ids = song.fds_ids.clone();
        self.fds_specs = &song.fds_ins;
        self.fds_waves = &song.fds_waves;
        self.fds_mod_waves = &song.fds_mod_waves;
        self.wonderswan_ids = song.wonderswan_ids.clone();
        self.wonderswan_specs = &song.wonderswan_ins;
        self.wonderswan_waves = &song.wonderswan_waves;
        self.wonderswan_pcm_smps = song.wonderswan_pcm_samples.iter().collect();
        self.qsound_ids = song.qsound_ids.clone();
        self.qsound_specs = &song.qsound_ins;
        self.qsound_smps = song.qsound_samples.iter().collect();
        self.scc_chip = if song.vgm.k051649_plus {
            CHIP_K052539
        } else {
            CHIP_K051649
        };
        self.k005289_ids = song.k005289_ids.clone();
        self.k005289_waves = &song.k005289_waves;
        self.k005289_specs = &song.k005289_ins;

        self.n_scc = 5 * self.scc_ids.len() as i32;
        self.n_ay = 3 * self.ay_ids.len() as i32;
        self.n_k005289 = 2 * self.k005289_ids.len() as i32;
        self.n_psg = 4 * self.psg_ids.len() as i32;
        self.n_nes = 5 * self.nes_ids.len() as i32;
        self.n_huc6280 = 6 * self.huc6280_ids.len() as i32;
        self.n_gb = 4 * self.gb_ids.len() as i32;
        self.n_vsu = 6 * self.vsu_ids.len() as i32;
        self.n_saa = 6 * self.saa_ids.len() as i32;
        self.n_pokey = 4 * self.pokey_ids.len() as i32;
        self.n_mikey = 4 * self.mikey_ids.len() as i32;
        self.n_x1 = X1_CHANNELS as i32 * self.x1_ids.len() as i32;
        self.n_fds = self.fds_ids.len() as i32;
        self.n_wonderswan = 4 * self.wonderswan_ids.len() as i32;
        self.n_qsound = 19 * self.qsound_ids.len() as i32;
        self.n_nes_dpcm_ins = self.nes_dpcm_smps.len() as i32;
        self.n_nes_pcm_ins = (self.nes_pcm_smps.len() as i32 + 119) / 120;
        self.n_huc6280_pcm_ins = self.huc6280_pcm_smps.len() as i32;
    }
}

impl<'a> FurWriter<'a> {
    fn plan_counts(&mut self) {
        let song = self.song;
        // K007232 appears only when the source carried usable samples for it,
        // and never beside another sample chip.
        self.want_k007 = !self.pcm_ids.is_empty();

        // Columns per system, summed.
        let mut n_fm = 0;
        let mut n_ssg = 0;
        let mut n_adpcm_a = 0;
        let mut n_adpcm_b = 0;
        if self.fm_present {
            for system in self.systems.iter() {
                let kind = system.kind.as_str();
                if matches!(kind, "opl" | "opl2" | "opl3" | "opll" | "y8950" | "opl4") {
                    n_fm += _system_span(system);
                    continue;
                }
                if kind == "ym2610" || kind == "ym2608" {
                    n_fm += system.fm_channels;
                    n_ssg += 3;
                    n_adpcm_a += 6;
                    n_adpcm_b += 1;
                } else if kind == "ym2203" {
                    n_fm += system.fm_channels;
                    n_ssg += 3;
                } else {
                    n_fm += if system.fm_channels != 0 {
                        system.fm_channels
                    } else {
                        8
                    };
                }
            }
        }
        self.n_fm = n_fm;
        self.n_ssg = n_ssg;
        self.n_adpcm_a = n_adpcm_a;
        self.n_adpcm_b = n_adpcm_b;
        self.n_k007 = if self.want_k007 {
            2 * self.pcm_ids.len() as i32
        } else {
            0
        };
        self.rf5c68_ids = self
            .rf5c68_smps
            .iter()
            .map(|s| s.chip_id)
            .collect::<BTreeSet<i32>>()
            .into_iter()
            .collect();
        self.rf5c164_ids = self
            .rf5c164_smps
            .iter()
            .map(|s| s.chip_id)
            .collect::<BTreeSet<i32>>()
            .into_iter()
            .collect();
        self.n_rf5c = 8 * (self.rf5c68_ids.len() as i32 + self.rf5c164_ids.len() as i32);
        self.n_ch = self.n_fm
            + self.n_ssg
            + self.n_adpcm_a
            + self.n_adpcm_b
            + self.n_psg
            + self.n_k007
            + 4 * self.oki_ids.len() as i32
            + self.adpcm_ids.len() as i32
            + 16 * self.spcm_ids.len() as i32
            + self.n_rf5c
            + 24 * self.c140_ids.len() as i32
            + 32 * self.c352_ids.len() as i32
            + 4 * self.k053260_ids.len() as i32
            + 8 * self.ymz_ids.len() as i32
            + 28 * self.multipcm_ids.len() as i32
            + self.upd7759_ids.len() as i32
            + 2 * self.pwm_ids.len() as i32
            + self.msm5205_ids.len() as i32
            + 4 * self.ga20_ids.len() as i32
            + 8 * self.msm5232_ids.len() as i32
            + 16 * self.c219_ids.len() as i32
            + 8 * self.k054539_ids.len() as i32
            + self.es5503_oscs.len() as i32
            + self.psg_dac_voices.len() as i32
            + self.n_scc
            + self.n_ay
            + self.n_nes
            + self.n_huc6280
            + self.n_gb
            + self.n_vsu
            + self.n_saa
            + self.n_pokey
            + self.n_mikey
            + self.n_x1
            + self.n_fds
            + self.n_wonderswan
            + self.n_qsound
            + self.n_k005289;
        if self.n_ch == 0 {
            panic!("write_fur: need include_fm and/or include_pcm");
        }
        self.fx_cols = 8;
        let vgm = &song.vgm;
        self.k_clock = if vgm.k007232_clock != 0 { vgm.k007232_clock } else { 3_579_545 };
        self.spcm_clock = if vgm.segapcm_clock != 0 { vgm.segapcm_clock } else { SEGAPCM_CLOCK_X };
        self.rf5c68_clock = if vgm.rf5c68_clock != 0 { vgm.rf5c68_clock } else { 8_000_000 };
        self.rf5c164_clock = if vgm.rf5c164_clock != 0 { vgm.rf5c164_clock } else { 12_500_000 };
        self.c140_clock = if vgm.c140_clock != 0 { vgm.c140_clock } else { 12_288_000 };
        self.c219_clock = if vgm.c140_clock != 0 { vgm.c140_clock } else { C219_DEFAULT_CLOCK };
        self.k054539_clock = if vgm.k054539_clock != 0 { vgm.k054539_clock } else { 18_432_000 };
        self.c352_clock = if vgm.c352_clock != 0 { vgm.c352_clock } else { C352_DEFAULT_CLOCK };
        self.k053260_clock = if vgm.k053260_clock != 0 { vgm.k053260_clock } else { 3_579_545 };

        self.n_ins = self.fm_ins.len() as i32
            + self.n_dac_ins
            + self.pcm_smps.len() as i32
            + self.oki_smps.len() as i32
            + self.n_adpcm_ins
            + self.spcm_smps.len() as i32
            + self.rf5c68_smps.len() as i32
            + self.rf5c164_smps.len() as i32
            + self.c140_smps.len() as i32
            + self.c352_smps.len() as i32
            + self.k053260_smps.len() as i32
            + self.n_ym2610_a_ins
            + self.n_ym2610_b_ins
            + self.n_ym2608_rhythm_ins
            + self.n_ym2608_b_ins
            + self.n_y8950_b_ins
            + self.n_opl4_wave_ins
            + self.n_ssg_pcm_ins
            + self.n_nes_dpcm_ins
            + self.n_nes_pcm_ins
            + self.n_huc6280_pcm_ins
            + self.ymz_smps.len() as i32
            + self.multipcm_smps.len() as i32
            + self.n_upd_ins
            + self.n_pwm_ins
            + self.n_msm5205_ins
            + self.n_ga20_ins
            + self.n_msm5232_ins
            + self.n_es5503_ins
            + self.c219_real.len() as i32
            + self.k054539_smps.len() as i32
            + self.n_psg_dac_ins
            + self.n_mikey_ins
            + self.n_x1_ins
            + self.scc_specs.len() as i32
            + self.ay_specs.len() as i32
            + self.psg_specs.len() as i32
            + self.nes_specs.len() as i32
            + self.huc6280_specs.len() as i32
            + self.gb_specs.len() as i32
            + self.vsu_specs.len() as i32
            + self.saa_specs.len() as i32
            + self.mikey_specs.len() as i32
            + self.x1_specs.len() as i32
            + self.k005289_specs.len() as i32;
        self.n_smp = self.pcm_smps.len() as i32
            + self.oki_smps.len() as i32
            + self.adpcm_smps.len() as i32
            + self.spcm_smps.len() as i32
            + self.rf5c68_smps.len() as i32
            + self.rf5c164_smps.len() as i32
            + self.c140_smps.len() as i32
            + self.c352_smps.len() as i32
            + self.k053260_smps.len() as i32
            + self.dac_smps.len() as i32
            + self.ym2610_a_smps.len() as i32
            + self.ym2610_b_smps.len() as i32
            + self.ym2608_b_smps.len() as i32
            + self.y8950_b_smps.len() as i32
            + self.opl4_wave_smps.len() as i32
            + self.ssg_pcm_smps.len() as i32
            + self.nes_dpcm_smps.len() as i32
            + self.nes_pcm_smps.len() as i32
            + self.huc6280_pcm_smps.len() as i32
            + self.ymz_smps.len() as i32
            + self.multipcm_smps.len() as i32
            + self.upd7759_smps.len() as i32
            + self.pwm_smps.len() as i32
            + self.mikey_smps.len() as i32
            + self.x1_smps.len() as i32
            + self.msm5205_smps.len() as i32
            + self.ga20_smps.len() as i32
            + self.c219_all.len() as i32
            + self.k054539_smps.len() as i32
            + self.es5503_smps.len() as i32
            + self.psg_dac_smps.len() as i32
            + self.qsound_smps.len() as i32
            + self.wonderswan_pcm_smps.len() as i32;
        self.n_wav = self.scc_waves.len() as i32
            + self.huc6280_waves.len() as i32
            + self.gb_waves.len() as i32
            + self.vsu_waves.len() as i32
            + self.x1_waves.len() as i32
            + self.x1_envs.len() as i32
            + self.fds_waves.len() as i32
            + self.wonderswan_waves.len() as i32
            + self.k005289_waves.len() as i32;
        // The VSU's 10xx effects name a wavetable in the module-wide list.
        self.vsu_wave_base = self.scc_waves.len() as i32
            + self.huc6280_waves.len() as i32
            + self.gb_waves.len() as i32;
        self.x1_wave_base = self.vsu_wave_base + self.vsu_waves.len() as i32;
        self.x1_env_base = self.x1_wave_base + self.x1_waves.len() as i32;
        self.fds_wave_base = self.x1_env_base + self.x1_envs.len() as i32;
        self.wonderswan_wave_base = self.fds_wave_base + self.fds_waves.len() as i32;
        self.k005289_wave_base =
            self.wonderswan_wave_base + self.wonderswan_waves.len() as i32;
    }

    fn plan_layout(&mut self, hooks: &mut EmitHooks<'_>) {
        // Analysis numbers sample-chip channels after the FM block. With no FM
        // they shift down to stay inside n_ch.
        let fm_analysis_block: i32 = self.systems.iter().map(_system_span).sum();
        let mut warnings: Vec<String> = Vec::new();
        let (orders, patterns, _loop_ord, pat_len) = _build_patterns(
            self.song,
            self.n_ch,
            PAT_LEN_MAX,
            self.fx_cols,
            if self.fm_present { 0 } else { -fm_analysis_block },
            self.stats.as_deref_mut(),
            self.vsu_wave_base,
            &mut warnings,
            hooks,
        );
        self.orders = orders;
        self.patterns = patterns;
        self.pat_len = pat_len;
        self.effect_cols = _fit_fx_cols(&self.patterns, self.n_ch);

        // Drop sample-map slots no placed note ever selects.
        let dac_ins_base = self.fm_ins.len() as i32;
        if self.include_pcm && !self.dac_smps.is_empty() {
            let old_dac_ins = self.n_dac_ins;
            let old_dac_len = self.dac_smps.len() as i32;
            let (indices, n_ins) = _compact_indices(
                &mut self.patterns,
                dac_ins_base,
                self.n_dac_ins,
                self.dac_smps.len(),
            );
            self.dac_smps = indices.iter().map(|&i| self.dac_smps[i]).collect();
            self.n_dac_ins = n_ins;
            if self.n_dac_ins != old_dac_ins {
                _shift_sample_map_instruments(
                    &mut self.patterns,
                    dac_ins_base + old_dac_ins,
                    self.n_dac_ins - old_dac_ins,
                );
            }
            if let Some(stats) = self.stats.as_deref_mut() {
                stats.insert(
                    "dac_samples_unused".to_string(),
                    (old_dac_len - self.dac_smps.len() as i32) as i64,
                );
            }
        }

        let dac_smp_base = self.pcm_smps.len() as i32
            + self.oki_smps.len() as i32
            + self.adpcm_smps.len() as i32
            + self.spcm_smps.len() as i32
            + self.rf5c68_smps.len() as i32
            + self.rf5c164_smps.len() as i32
            + self.c140_smps.len() as i32
            + self.c352_smps.len() as i32
            + self.k053260_smps.len() as i32;
        self.dac_smp_base = dac_smp_base;
        self.ym2610_a_smp_base = dac_smp_base + self.dac_smps.len() as i32;
        self.ym2610_b_smp_base =
            self.ym2610_a_smp_base + self.ym2610_a_smps.len() as i32;
        self.ym2608_b_smp_base =
            self.ym2610_b_smp_base + self.ym2610_b_smps.len() as i32;
        self.y8950_b_smp_base =
            self.ym2608_b_smp_base + self.ym2608_b_smps.len() as i32;
        self.opl4_wave_smp_base =
            self.y8950_b_smp_base + self.y8950_b_smps.len() as i32;
        let ssg_pcm_ins_base = self.fm_ins.len() as i32
            + self.n_dac_ins
            + self.pcm_smps.len() as i32
            + self.oki_smps.len() as i32
            + self.n_adpcm_ins
            + self.spcm_smps.len() as i32
            + self.rf5c68_smps.len() as i32
            + self.rf5c164_smps.len() as i32
            + self.c140_smps.len() as i32
            + self.c352_smps.len() as i32
            + self.k053260_smps.len() as i32
            + self.n_ym2610_a_ins
            + self.n_ym2610_b_ins
            + self.n_ym2608_rhythm_ins
            + self.n_ym2608_b_ins
            + self.n_y8950_b_ins
            + self.n_opl4_wave_ins;
        self.ssg_pcm_ins_base = ssg_pcm_ins_base;
        if self.include_pcm && !self.ssg_pcm_smps.is_empty() {
            let old_ssg_ins = self.n_ssg_pcm_ins;
            let old_ssg_len = self.ssg_pcm_smps.len() as i32;
            let (indices, n_ins) = _compact_indices(
                &mut self.patterns,
                ssg_pcm_ins_base,
                self.n_ssg_pcm_ins,
                self.ssg_pcm_smps.len(),
            );
            self.ssg_pcm_smps =
                indices.iter().map(|&i| self.ssg_pcm_smps[i]).collect();
            self.n_ssg_pcm_ins = n_ins;
            if self.n_ssg_pcm_ins != old_ssg_ins {
                _shift_sample_map_instruments(
                    &mut self.patterns,
                    ssg_pcm_ins_base + old_ssg_ins,
                    self.n_ssg_pcm_ins - old_ssg_ins,
                );
            }
            if let Some(stats) = self.stats.as_deref_mut() {
                stats.insert(
                    "ssg_pcm_samples_unused".to_string(),
                    (old_ssg_len - self.ssg_pcm_smps.len() as i32) as i64,
                );
            }
        }

        self.ssg_pcm_smp_base =
            self.opl4_wave_smp_base + self.opl4_wave_smps.len() as i32;
        self.nes_dpcm_smp_base =
            self.ssg_pcm_smp_base + self.ssg_pcm_smps.len() as i32;
        self.nes_pcm_smp_base =
            self.nes_dpcm_smp_base + self.nes_dpcm_smps.len() as i32;
        self.huc6280_pcm_smp_base =
            self.nes_pcm_smp_base + self.nes_pcm_smps.len() as i32;
        self.ymz_smp_base =
            self.huc6280_pcm_smp_base + self.huc6280_pcm_smps.len() as i32;
        self.multipcm_smp_base = self.ymz_smp_base + self.ymz_smps.len() as i32;
        self.upd7759_smp_base =
            self.multipcm_smp_base + self.multipcm_smps.len() as i32;
        self.pwm_smp_base = self.upd7759_smp_base + self.upd7759_smps.len() as i32;
        self.msm5205_smp_base = self.pwm_smp_base + self.pwm_smps.len() as i32;
        self.ga20_smp_base = self.msm5205_smp_base + self.msm5205_smps.len() as i32;
        self.c219_smp_base = self.ga20_smp_base + self.ga20_smps.len() as i32;
        self.k054539_smp_base = self.c219_smp_base + self.c219_all.len() as i32;
        self.es5503_smp_base =
            self.k054539_smp_base + self.k054539_smps.len() as i32;
        self.psg_dac_smp_base = self.es5503_smp_base + self.es5503_smps.len() as i32;
        self.mikey_smp_base = self.psg_dac_smp_base + self.psg_dac_smps.len() as i32;
        self.wonderswan_pcm_smp_base =
            self.mikey_smp_base + self.mikey_smps.len() as i32;
        self.qsound_smp_base =
            self.wonderswan_pcm_smp_base + self.wonderswan_pcm_smps.len() as i32;
        self.x1_smp_base = self.qsound_smp_base + self.qsound_smps.len() as i32;
        self.n_ins = self.fm_ins.len() as i32
            + self.n_dac_ins
            + self.pcm_smps.len() as i32
            + self.oki_smps.len() as i32
            + self.n_adpcm_ins
            + self.spcm_smps.len() as i32
            + self.rf5c68_smps.len() as i32
            + self.rf5c164_smps.len() as i32
            + self.c140_smps.len() as i32
            + self.c352_smps.len() as i32
            + self.k053260_smps.len() as i32
            + self.n_ym2610_a_ins
            + self.n_ym2610_b_ins
            + self.n_ym2608_rhythm_ins
            + self.n_ym2608_b_ins
            + self.n_y8950_b_ins
            + self.n_opl4_wave_ins
            + self.n_ssg_pcm_ins
            + self.n_nes_dpcm_ins
            + self.n_nes_pcm_ins
            + self.n_huc6280_pcm_ins
            + self.ymz_smps.len() as i32
            + self.multipcm_smps.len() as i32
            + self.n_upd_ins
            + self.n_pwm_ins
            + self.n_msm5205_ins
            + self.n_ga20_ins
            + self.n_msm5232_ins
            + self.n_es5503_ins
            + self.c219_real.len() as i32
            + self.k054539_smps.len() as i32
            + self.n_psg_dac_ins
            + self.n_mikey_ins
            + self.wonderswan_pcm_smps.len() as i32
            + self.n_x1_ins
            + self.scc_specs.len() as i32
            + self.ay_specs.len() as i32
            + self.psg_specs.len() as i32
            + self.nes_specs.len() as i32
            + self.huc6280_specs.len() as i32
            + self.gb_specs.len() as i32
            + self.vsu_specs.len() as i32
            + self.saa_specs.len() as i32
            + self.pokey_specs.len() as i32
            + self.mikey_specs.len() as i32
            + self.x1_specs.len() as i32
            + self.fds_specs.len() as i32
            + self.wonderswan_specs.len() as i32
            + self.qsound_specs.len() as i32
            + self.k005289_specs.len() as i32;
        self.n_smp = self.pcm_smps.len() as i32
            + self.oki_smps.len() as i32
            + self.adpcm_smps.len() as i32
            + self.spcm_smps.len() as i32
            + self.rf5c68_smps.len() as i32
            + self.rf5c164_smps.len() as i32
            + self.c140_smps.len() as i32
            + self.c352_smps.len() as i32
            + self.k053260_smps.len() as i32
            + self.dac_smps.len() as i32
            + self.ym2610_a_smps.len() as i32
            + self.ym2610_b_smps.len() as i32
            + self.ym2608_b_smps.len() as i32
            + self.y8950_b_smps.len() as i32
            + self.opl4_wave_smps.len() as i32
            + self.ssg_pcm_smps.len() as i32
            + self.nes_dpcm_smps.len() as i32
            + self.nes_pcm_smps.len() as i32
            + self.huc6280_pcm_smps.len() as i32
            + self.ymz_smps.len() as i32
            + self.multipcm_smps.len() as i32
            + self.upd7759_smps.len() as i32
            + self.pwm_smps.len() as i32
            + self.mikey_smps.len() as i32
            + self.x1_smps.len() as i32
            + self.msm5205_smps.len() as i32
            + self.ga20_smps.len() as i32
            + self.c219_all.len() as i32
            + self.k054539_smps.len() as i32
            + self.es5503_smps.len() as i32
            + self.psg_dac_smps.len() as i32
            + self.qsound_smps.len() as i32
            + self.wonderswan_pcm_smps.len() as i32;
        self.n_ord = if self.orders.is_empty() {
            1
        } else {
            self.orders[0].len() as i32
        };
        self.n_pat = self.patterns.len() as i32;
    }
}

impl<'a> FurWriter<'a> {
    fn plan_names(&mut self) {
        let song = self.song;
        let mut names_long: Vec<String> = Vec::new();
        let mut names_short: Vec<String> = Vec::new();
        if self.fm_present {
            _append_system_names(
                song,
                &self.systems,
                &mut names_long,
                &mut names_short,
            );
        }
        if !self.psg_ids.is_empty() {
            if song.psg_t6w28 {
                names_long.extend(
                    ["Square 1", "Square 2", "Square 3", "Noise"]
                        .iter()
                        .map(|s| s.to_string()),
                );
                names_short.extend(
                    ["S1", "S2", "S3", "NO"].iter().map(|s| s.to_string()),
                );
            } else {
                for cid in 0..self.psg_ids.len() {
                    let suffix = if self.psg_ids.len() > 1 {
                        format!(" ({})", cid + 1)
                    } else {
                        String::new()
                    };
                    for voice in 0..3 {
                        names_long.push(format!("PSG {}{}", voice + 1, suffix));
                        names_short.push(format!("S{}-{}", cid + 1, voice + 1));
                    }
                    names_long.push(format!("PSG Noise{}", suffix));
                    names_short.push(format!("S{}-N", cid + 1));
                }
            }
        }
        for cid in self.pcm_ids.iter().copied() {
            let suffix = if self.pcm_ids.len() > 1 {
                format!(" ({})", cid + 1)
            } else {
                String::new()
            };
            for voice in 0..2 {
                names_long.push(format!("PCM {}{}", voice + 1, suffix));
                names_short.push(format!("P{}-{}", cid + 1, voice + 1));
            }
        }
        for cid in self.oki_ids.iter().copied() {
            for voice in 0..4 {
                names_long.push(format!("OKI{} {}", cid + 1, voice + 1));
                names_short.push(format!("O{}{}", cid + 1, voice + 1));
            }
        }
        for cid in self.adpcm_ids.iter().copied() {
            if self.adpcm_ids.len() == 1 && cid == 0 {
                names_long.push("ADPCM".to_string());
                names_short.push("AD".to_string());
            } else {
                names_long.push(format!("ADPCM {}", cid + 1));
                names_short.push(format!("A{}", cid + 1));
            }
        }
        for cid in self.spcm_ids.iter().copied() {
            let suffix = if self.spcm_ids.len() > 1 {
                format!(" ({})", cid + 1)
            } else {
                String::new()
            };
            for voice in 0..16 {
                names_long.push(format!("SEGA {}{}", voice + 1, suffix));
                names_short.push(format!("S{}-{}", cid + 1, voice + 1));
            }
        }
        // The RF5C pair share a label family.
        let mut rf5c_n = 0;
        for smps in [&self.rf5c68_smps, &self.rf5c164_smps] {
            let ids: Vec<i32> = smps
                .iter()
                .map(|s| s.chip_id)
                .collect::<BTreeSet<i32>>()
                .into_iter()
                .collect();
            for cid in ids.iter().copied() {
                let suffix = if ids.len() > 1 {
                    format!(" ({})", cid + 1)
                } else {
                    String::new()
                };
                for voice in 0..8 {
                    names_long.push(format!("PCM {}{}", rf5c_n + voice + 1, suffix));
                    names_short.push(format!("P{}", rf5c_n + voice + 1));
                }
                rf5c_n += 8;
            }
        }
        for cid in self.c140_ids.iter().copied() {
            for voice in 0..24 {
                names_long.push(format!("C140 {}-{}", cid + 1, voice + 1));
                names_short.push(format!("C{}-{}", cid + 1, voice + 1));
            }
        }
        for cid in self.c352_ids.iter().copied() {
            for voice in 0..32 {
                names_long.push(format!("C352 {}-{}", cid + 1, voice + 1));
                names_short.push(format!("Z{}-{}", cid + 1, voice + 1));
            }
        }
        for cid in self.k053260_ids.iter().copied() {
            for voice in 0..4 {
                names_long.push(format!("K053260 {}-{}", cid + 1, voice + 1));
                names_short.push(format!("Q{}-{}", cid + 1, voice + 1));
            }
        }
        for cid in self.ymz_ids.iter().copied() {
            for voice in 0..8 {
                names_long.push(format!("YMZ280B {}-{}", cid + 1, voice + 1));
                names_short.push(format!("Y{}-{}", cid + 1, voice + 1));
            }
        }
        for cid in self.multipcm_ids.iter().copied() {
            for voice in 0..28 {
                names_long.push(format!("MultiPCM {}-{}", cid + 1, voice + 1));
                names_short.push(format!("M{}-{}", cid + 1, voice + 1));
            }
        }
        for cid in self.upd7759_ids.iter().copied() {
            let suffix = if self.upd7759_ids.len() > 1 {
                format!(" ({})", cid + 1)
            } else {
                String::new()
            };
            names_long.push(format!("uPD7759{}", suffix));
            names_short.push(format!("U{}", cid + 1));
        }
        for cid in self.pwm_ids.iter().copied() {
            let suffix = if self.pwm_ids.len() > 1 {
                format!(" ({})", cid + 1)
            } else {
                String::new()
            };
            names_long.push(format!("PWM Left{}", suffix));
            names_short.push(format!("PW{}L", cid + 1));
            names_long.push(format!("PWM Right{}", suffix));
            names_short.push(format!("PW{}R", cid + 1));
        }
        for cid in self.msm5205_ids.iter().copied() {
            let suffix = if self.msm5205_ids.len() > 1 {
                format!(" ({})", cid + 1)
            } else {
                String::new()
            };
            names_long.push(format!("MSM5205{}", suffix));
            names_short.push(format!("M{}", cid + 1));
        }
        for cid in self.ga20_ids.iter().copied() {
            for voice in 0..4 {
                names_long.push(format!("GA20 {}-{}", cid + 1, voice + 1));
                names_short.push(format!("G{}-{}", cid + 1, voice + 1));
            }
        }
        for cid in self.c219_ids.iter().copied() {
            for voice in 0..16 {
                names_long.push(format!("C219 {}-{}", cid + 1, voice + 1));
                names_short.push(format!("X{}-{}", cid + 1, voice + 1));
            }
        }
        for cid in self.k054539_ids.iter().copied() {
            for voice in 0..8 {
                names_long.push(format!("K054539 {}-{}", cid + 1, voice + 1));
                names_short.push(format!("K{}-{}", cid + 1, voice + 1));
            }
        }
        for cid in self.msm5232_ids.iter().copied() {
            for voice in 0..8 {
                names_long.push(format!("MSM5232 {}-{}", cid + 1, voice + 1));
                names_short.push(format!("MSM{}-{}", cid + 1, voice + 1));
            }
        }
        for (cid, osc) in self.es5503_oscs.iter().copied() {
            names_long.push(format!("ES5503 {}-{}", cid + 1, osc + 1));
            names_short.push(format!("E{}-{}", cid + 1, osc + 1));
        }
        for (index, key) in self.psg_dac_voices.iter().copied().enumerate() {
            let suffix = if self.psg_dac_voices.len() > 1 {
                format!(" {}", key.1 + 1)
            } else {
                String::new()
            };
            names_long.push(format!("PSG DAC{}", suffix));
            names_short.push(format!("P{}", index + 1));
        }
        for cid in self.scc_ids.iter().copied() {
            for voice in 0..5 {
                names_long.push(format!("SCC {}-{}", cid + 1, voice + 1));
                names_short.push(format!("K{}-{}", cid + 1, voice + 1));
            }
        }
        for cid in self.ay_ids.iter().copied() {
            for voice in 0..3 {
                names_long.push(format!("PSG {}-{}", cid + 1, voice + 1));
                names_short.push(format!("P{}-{}", cid + 1, voice + 1));
            }
        }
        let nes_labels = ["Pulse 1", "Pulse 2", "Triangle", "Noise", "DPCM"];
        // The label follows the chip's position in the module.
        let nes_slot: IndexMap<i32, usize> = self
            .nes_ids
            .iter()
            .copied()
            .enumerate()
            .map(|(i, cid)| (cid, i))
            .collect();
        for cid in self.nes_ids.iter().copied() {
            for (voice, label) in nes_labels.iter().enumerate() {
                let n = nes_slot[&cid] + 1;
                names_long.push(if self.nes_ids.len() > 1 {
                    format!("{} ({})", label, n)
                } else {
                    label.to_string()
                });
                names_short.push(format!("N{}-{}", n, voice + 1));
            }
        }
        let pce_labels = [
            "Wave 1", "Wave 2", "Wave 3", "Wave 4", "Wave 5", "Wave 6",
        ];
        for cid in self.huc6280_ids.iter().copied() {
            for (voice, label) in pce_labels.iter().enumerate() {
                names_long.push(format!("{} ({})", label, cid + 1));
                names_short.push(format!("P{}-{}", cid + 1, voice + 1));
            }
        }
        let gb_labels = ["Pulse 1", "Pulse 2", "Wavetable", "Noise"];
        let gb_short = ["S1", "S2", "WA", "NO"];
        for cid in self.gb_ids.iter().copied() {
            for (label, short) in gb_labels.iter().zip(gb_short.iter()) {
                names_long.push(format!("{} ({})", label, cid + 1));
                names_short.push(if cid != 0 {
                    format!("{}{}", short, cid + 1)
                } else {
                    short.to_string()
                });
            }
        }
        let vsu_labels = [
            "Voice 1", "Voice 2", "Voice 3", "Voice 4", "Voice 5", "Noise",
        ];
        let vsu_short = ["W1", "W2", "W3", "W4", "W5", "NO"];
        for cid in self.vsu_ids.iter().copied() {
            for (label, short) in vsu_labels.iter().zip(vsu_short.iter()) {
                names_long.push(format!("VB {} ({})", label, cid + 1));
                names_short.push(if cid != 0 {
                    format!("{}{}", short, cid + 1)
                } else {
                    short.to_string()
                });
            }
        }
        for cid in self.saa_ids.iter().copied() {
            for voice in 0..6 {
                names_long.push(format!("SAA1099 {}-{}", cid + 1, voice + 1));
                names_short.push(format!("A{}-{}", cid + 1, voice + 1));
            }
        }
        for cid in self.pokey_ids.iter().copied() {
            for voice in 0..POKEY_VOICES {
                names_long.push(format!("POKEY {}-{}", cid + 1, voice + 1));
                names_short.push(format!("PK{}{}", cid + 1, voice + 1));
            }
        }
        for cid in self.mikey_ids.iter().copied() {
            for voice in 0..MIKEY_VOICES {
                names_long.push(format!("Mikey {}-{}", cid + 1, voice + 1));
                names_short.push(format!("MK{}{}", cid + 1, voice + 1));
            }
        }
        for cid in self.x1_ids.iter().copied() {
            for voice in 0..X1_CHANNELS {
                names_long.push(format!("X1-010 {}-{}", cid + 1, voice + 1));
                names_short.push(format!("X1{}-{}", cid + 1, voice + 1));
            }
        }
        for cid in self.fds_ids.iter().copied() {
            names_long.push(format!("FDS ({})", cid + 1));
            names_short.push(format!("FDS{}", cid + 1));
        }
        for cid in self.wonderswan_ids.iter().copied() {
            for voice in 0..4 {
                names_long.push(format!("WonderSwan {} ({})", voice + 1, cid + 1));
                names_short.push(format!("WS{}-{}", cid + 1, voice + 1));
            }
        }
        for cid in self.qsound_ids.iter().copied() {
            for voice in 0..16 {
                names_long.push(format!("QSound PCM {} ({})", voice + 1, cid + 1));
                names_short.push(format!("QS{}-{}", cid + 1, voice + 1));
            }
            for voice in 0..3 {
                names_long.push(format!("QSound ADPCM {} ({})", voice + 1, cid + 1));
                names_short.push(format!("QA{}-{}", cid + 1, voice + 1));
            }
        }
        for cid in self.k005289_ids.iter().copied() {
            for voice in 0..2 {
                names_long.push(format!("WSG {}-{}", cid + 1, voice + 1));
                names_short.push(format!("W{}-{}", cid + 1, voice + 1));
            }
        }

        let mut label_parts: Vec<String> = Vec::new();
        if self.fm_present {
            // One entry per family, in module order.
            let mut listed: Vec<String> = Vec::new();
            for system in self.systems.iter() {
                let kind = system.kind.as_str();
                if listed.iter().any(|k| k == kind) {
                    continue;
                }
                listed.push(kind.to_string());
                let n = self.systems.iter().filter(|s| s.kind == kind).count();
                let mut base = match kind {
                    "opl" => "OPL".to_string(),
                    "opl2" => "OPL2".to_string(),
                    "opl3" => "OPL3".to_string(),
                    "opll" => "OPLL".to_string(),
                    "ym2610" => {
                        if system.fm_channels > 4 {
                            "YM2610B".to_string()
                        } else {
                            "YM2610".to_string()
                        }
                    }
                    "ym2608" => "YM2608".to_string(),
                    "ym2612" => "YM2612".to_string(),
                    "ym2203" => "YM2203".to_string(),
                    _ => "YM2151".to_string(),
                };
                if matches!(kind, "opl" | "opl2" | "opl3") && system.drums {
                    base += " drums";
                } else if kind == "opll" && system.drums {
                    base += " drums";
                } else if kind == "ym2608" && system.ext {
                    base += " Ext";
                } else if kind == "ym2612" && system.ext {
                    base += " Ext";
                }
                label_parts.push(if n > 1 {
                    format!("{} x{}", base, n)
                } else {
                    base
                });
            }
        }
        if !self.psg_ids.is_empty() {
            label_parts.push(if song.psg_t6w28 {
                "T6W28".to_string()
            } else if self.psg_ids.len() > 1 {
                format!("SN76489 x{}", self.psg_ids.len())
            } else {
                "SN76489".to_string()
            });
        }
        if !self.pcm_ids.is_empty() {
            label_parts.push(if self.pcm_ids.len() > 1 {
                format!("K007232 x{}", self.pcm_ids.len())
            } else {
                "K007232".to_string()
            });
        }
        if !self.oki_ids.is_empty() {
            label_parts.push("MSM6295".to_string());
        }
        if !self.adpcm_ids.is_empty() {
            label_parts.push("MSM6258".to_string());
        }
        if !self.spcm_ids.is_empty() {
            label_parts.push(if self.spcm_ids.len() > 1 {
                format!("SegaPCM x{}", self.spcm_ids.len())
            } else {
                "SegaPCM".to_string()
            });
        }
        if !self.rf5c68_smps.is_empty() {
            label_parts.push("RF5C68".to_string());
        }
        if !self.rf5c164_smps.is_empty() {
            label_parts.push("RF5C164".to_string());
        }
        if self.fm_present && self.systems.iter().any(|s| s.kind == "y8950") {
            label_parts.push("Y8950".to_string());
        }
        if self.fm_present && self.systems.iter().any(|s| s.kind == "opl4") {
            label_parts.push("OPL4".to_string());
        }
        if !self.c140_ids.is_empty() {
            label_parts.push("C140".to_string());
        }
        if !self.c352_ids.is_empty() {
            label_parts.push("C352".to_string());
        }
        if !self.k053260_ids.is_empty() {
            label_parts.push("K053260".to_string());
        }
        if !self.ymz_ids.is_empty() {
            label_parts.push("YMZ280B".to_string());
        }
        if !self.multipcm_ids.is_empty() {
            label_parts.push(if self.multipcm_ids.len() > 1 {
                format!("MultiPCM x{}", self.multipcm_ids.len())
            } else {
                "MultiPCM".to_string()
            });
        }
        if !self.upd7759_ids.is_empty() {
            label_parts.push("uPD7759".to_string());
        }
        if !self.pwm_ids.is_empty() {
            label_parts.push("32X PWM".to_string());
        }
        if !self.c219_ids.is_empty() {
            label_parts.push("C219".to_string());
        }
        if !self.k054539_ids.is_empty() {
            label_parts.push(if self.k054539_ids.len() > 1 {
                format!("K054539 x{}", self.k054539_ids.len())
            } else {
                "K054539".to_string()
            });
        }
        if !self.scc_ids.is_empty() {
            label_parts.push(if song.vgm.k051649_plus {
                "SCC+".to_string()
            } else {
                "SCC".to_string()
            });
        }
        if !self.ay_ids.is_empty() {
            label_parts.push("AY-8910".to_string());
        }
        if !self.nes_ids.is_empty() {
            label_parts.push(if self.nes_ids.len() > 1 {
                format!("NES APU x{}", self.nes_ids.len())
            } else {
                "NES APU".to_string()
            });
        }
        if !self.huc6280_ids.is_empty() {
            label_parts.push("PC Engine".to_string());
        }
        if !self.gb_ids.is_empty() {
            label_parts.push("Game Boy".to_string());
        }
        if !self.vsu_ids.is_empty() {
            label_parts.push("Virtual Boy".to_string());
        }
        if !self.saa_ids.is_empty() {
            label_parts.push(if self.saa_ids.len() > 1 {
                "SAA1099 x2".to_string()
            } else {
                "SAA1099".to_string()
            });
        }
        if !self.mikey_ids.is_empty() {
            label_parts.push(if self.mikey_ids.len() > 1 {
                "Atari Lynx x2".to_string()
            } else {
                "Atari Lynx".to_string()
            });
        }
        if !self.x1_ids.is_empty() {
            label_parts.push(if self.x1_ids.len() > 1 {
                "Seta X1-010 x2".to_string()
            } else {
                "Seta X1-010".to_string()
            });
        }
        if !self.fds_ids.is_empty() {
            label_parts.push("Famicom Disk System".to_string());
        }
        if !self.wonderswan_ids.is_empty() {
            label_parts.push("WonderSwan".to_string());
        }
        if !self.qsound_ids.is_empty() {
            label_parts.push("Capcom QSound".to_string());
        }
        if !self.k005289_ids.is_empty() {
            label_parts.push("Konami Bubble System WSG".to_string());
        }
        let chip_label = if label_parts.is_empty() {
            "empty".to_string()
        } else {
            label_parts.join(" + ")
        };

        let source_name = song
            .vgm
            .path
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        let notes = self.gd3("notes").to_string();
        let mut comment = format!(
            "Transcribed from VGM by vgm2151fur {} ({}).\n\
             Check volumes, sample C-4 rates, and the loop jump.\n\
             Reconvert with the current vgm2151fur when this version is older.\n\
             source: {}\n\
             grid: {:.0} Hz × speed {}\n\
             {}",
            _version(),
            chip_label,
            source_name,
            song.hz,
            song.speed,
            notes
        );
        if !self.c352_ids.is_empty() {
            comment += "\nNamco C352 is chip 0xD0. Furnace 0.6.8.3 does not emulate it, \
                so these channels stay silent in that build.\n";
        }
        if !self.k053260_ids.is_empty() {
            comment += &format!(
                "\nKonami K053260 at {} Hz; KADPCM samples are stored as sample type 7 \
                 and loop the whole body.\n",
                self.k053260_clock
            );
        }
        if !self.scc_ids.is_empty() {
            comment += &format!(
                "\nKonami {} at {} Hz; each SCC instrument carries one 32-byte waveform.\n",
                if song.vgm.k051649_plus {
                    "SCC+ (K052539)"
                } else {
                    "SCC (K051649)"
                },
                if song.vgm.k051649_clock != 0 {
                    song.vgm.k051649_clock
                } else {
                    1_789_772
                }
            );
        }
        if !self.ay_ids.is_empty() {
            comment += &format!(
                "\nAY-8910 at {} Hz; noise, channel mode and the hardware envelope are \
                 pattern effects (20xx/21xx/22xy/23xx/24xx).\n",
                if song.vgm.ay8910_clock != 0 {
                    song.vgm.ay8910_clock
                } else {
                    1_789_772
                }
            );
        }
        if !self.saa_ids.is_empty() {
            let dual = if self.saa_ids.len() > 1 {
                " (Creative Music System / Game Blaster)"
            } else {
                ""
            };
            comment += &format!(
                "\nPhilips SAA1099 at {} Hz{}; each chip has six square/noise voices and two \
                 3-channel noise generators. 10xy selects noise/tone, 11xx the noise generator \
                 and 12xx the envelope. The raw amplitude nibbles ride the instrument's \
                 pan-left/pan-right macros with a volume column of 15, so soft pan and the \
                 volume envelope stay exact.\n",
                if song.vgm.saa1099_clock != 0 {
                    song.vgm.saa1099_clock
                } else {
                    SAA_DEFAULT_CLOCK
                },
                dual
            );
        }
        if self.ym2612 {
            comment += &format!(
                "\nYM2612 at {} Hz; each system has six FM voices{}. LFO enable/speed is \
                 effect 10xy and per-channel PMS/AMS are 62xx/63xx.\n",
                if song.vgm.ym2612_clock != 0 {
                    song.vgm.ym2612_clock
                } else {
                    7_670_453
                },
                if self.ym_ext {
                    " and channel 3 split into four operator columns"
                } else {
                    ""
                }
            );
            if !self.dac_smps.is_empty() && self.opl {
                // The combined shape: the OPN2 never keys an FM voice.
                let dac_channel = song
                    .events
                    .iter()
                    .find(|e| e.dac && e.on)
                    .map(|e| e.ch)
                    .unwrap_or(-1);
                comment += &format!(
                    "\nThis log's YM2612 has no FM notes: channel {} is the DAC, and the OPL2 \
                     beside it plays the music. The stream's PCM is cut into {} rate-exact \
                     chunks; one sample-map note starts each of them.\n",
                    dac_channel + 1,
                    self.dac_smps.len()
                );
            }
        }
        if !self.psg_ids.is_empty() {
            if song.psg_t6w28 {
                comment += &format!(
                    "\nT6W28 at {} Hz; the two SN76489 halves are one stereo chip. The volume \
                     column is the louder side's 4-bit attenuation (15 = loudest), 08xy splits \
                     the two sides, and 20xy selects the noise mode.\n",
                    if song.vgm.sn76489_clock != 0 {
                        song.vgm.sn76489_clock
                    } else {
                        T6W28_DEFAULT_CLOCK
                    }
                );
            } else {
                comment += &format!(
                    "\nSN76489 at {} Hz; three square voices then noise. The volume column is \
                     the 4-bit attenuation (15 = loudest) and 20xy selects the noise mode.\n",
                    if song.vgm.sn76489_clock != 0 {
                        song.vgm.sn76489_clock
                    } else {
                        3_579_545
                    }
                );
            }
        } else if self.ym2610 {
            comment += &format!(
                "\nYM2610{} at {} Hz; each system has the FM voices, three SSG voices, six \
                 ADPCM-A channels and one ADPCM-B channel. ADPCM-A plays at clock/432; \
                 ADPCM-B uses the source rate register.\n",
                if song.ym2610_b { "B" } else { "" },
                if song.vgm.ym2610_clock != 0 {
                    song.vgm.ym2610_clock
                } else {
                    8_000_000
                }
            );
        } else if self.ym2608 {
            comment += &format!(
                "\nYM2608 at {} Hz; each system has the FM voices{} three SSG voices, the \
                 chip's six rhythm voices and one ADPCM-B channel. A rhythm column IS the drum \
                 - kick, snare, top cymbal, hi-hat, tom and rim from the ROM Furnace compiles \
                 in - so its note carries no sample, and the source's global $11 level is \
                 folded into each hit's volume.\n",
                if song.vgm.ym2608_clock != 0 {
                    song.vgm.ym2608_clock
                } else {
                    YM2608_DEFAULT_CLOCK
                },
                if self.ym_ext {
                    ", channel 3 split into four operator columns,"
                } else {
                    ","
                }
            );
        } else if self.ym_chip {
            comment += &format!(
                "\nYM2203 at {} Hz; each system has three FM voices then three SSG voices. \
                 The SSG register effects (20xx/21xx/22xy/23xx/24xx) work on the last three \
                 channels of each six-channel block.\n",
                if song.vgm.ym2203_clock != 0 {
                    song.vgm.ym2203_clock
                } else {
                    3_579_545
                }
            );
        }
        if self.opll {
            comment += "\nYM2413: instrument 0 is the user patch in $00-$07; every other voice \
                picks a ROM preset (the wave column, 1-15). The volume column is the $3x level \
                nibble (0 loudest).";
            comment += if self.opll_drums {
                " Rhythm mode takes channels 7-9 as BD/SD/TM/TP/HH.\n"
            } else {
                "\n"
            };
        }

        self.names_long = names_long;
        self.names_short = names_short;
        self.chip_label = chip_label;
        self.comment = comment;
    }
}

impl<'a> FurWriter<'a> {
    fn write_header(&mut self) {
        let b = &mut self.b;
        b.raw(FUR_MAGIC);
        b.u16(FUR_VERSION as i64);
        b.u16(0);
        b.u32(32);
        b.pad(8, 0);
    }

    fn write_systems(&mut self) {
        let song = self.song;
        let vgm = &song.vgm;
        let mut chips: Vec<u8> = Vec::new();
        let mut flag_texts: Vec<String> = Vec::new();
        let mut ym2610_sys: IndexMap<i32, usize> = IndexMap::new();
        let mut ym2608_sys: IndexMap<i32, usize> = IndexMap::new();
        let mut opl_sys: IndexMap<i32, usize> = IndexMap::new();
        if self.fm_present {
            for system in self.systems.iter() {
                // One module system per chip, in the order the channels were
                // laid out. Each kind carries its own clock flag.
                let kind = system.kind.as_str();
                let cid = system.chip_id as i32;
                if matches!(kind, "opl" | "opl2" | "opl3" | "y8950" | "opl4") {
                    chips.push(OPL_CHIP[&(kind, system.drums)]);
                    opl_sys.insert(cid, chips.len() - 1);
                    // An OPL core defaults to COLOR_NTSC and an OPL3/OPL4 to
                    // four times it. Outside that tolerance the clock decides
                    // every f-num, so it has to travel with the module.
                    let fm_clock = if kind == "opl3" {
                        vgm.opl3_clock
                    } else if kind == "opl2" {
                        vgm.opl2_clock
                    } else if kind == "y8950" {
                        vgm.y8950_clock
                    } else if kind == "opl4" {
                        vgm.ymf278b_clock
                    } else {
                        vgm.opl_clock
                    };
                    let is_opl3_clock = matches!(kind, "opl3" | "opl4");
                    let default = if is_opl3_clock {
                        FURNACE_CLOCK_OPL3
                    } else {
                        FURNACE_CLOCK
                    };
                    let module = opl_module_clock(fm_clock as f64, is_opl3_clock);
                    flag_texts.push(if module == default {
                        String::new()
                    } else {
                        format!("customClock={}", fm_clock)
                    });
                    continue;
                }
                if kind == "opll" {
                    chips.push(if system.vrc7 {
                        CHIP_VRC7
                    } else if system.drums {
                        CHIP_OPLL_DRUMS
                    } else {
                        CHIP_OPLL
                    });
                    // DivPlatformOPLL's named clock selections cover the usual
                    // MSX and arcade clocks exactly; use those before a custom
                    // clock.
                    flag_texts.push(opll_clock_flag(vgm.ym2413_clock as f64));
                    continue;
                }
                let fm_clock;
                let fm_default;
                if kind == "ym2612" {
                    chips.push(if system.ext {
                        CHIP_YM2612_EXT
                    } else {
                        CHIP_YM2612
                    });
                    fm_clock = vgm.ym2612_clock;
                    // Furnace's Genesis clock list has a named 8 MHz FM Towns
                    // entry (clockSel=2).
                    if fm_clock == 8_000_000 {
                        flag_texts.push("clockSel=2".to_string());
                        continue;
                    }
                    // Furnace's Genesis default is COLOR_NTSC*15/7 truncated
                    // to 7670454, while the VGM header normally says 7670453.
                    fm_default = 7_670_454;
                } else if kind == "ym2203" {
                    chips.push(if system.ext {
                        CHIP_YM2203_EXT
                    } else {
                        CHIP_YM2203
                    });
                    fm_clock = vgm.ym2203_clock;
                    fm_default = 3_579_545;
                } else if kind == "ym2610" {
                    chips.push(if system.fm_channels > 4 {
                        CHIP_YM2610B
                    } else {
                        CHIP_YM2610
                    });
                    ym2610_sys.insert(cid, chips.len() - 1);
                    fm_clock = vgm.ym2610_clock;
                    fm_default = 8_000_000;
                } else if kind == "ym2608" {
                    chips.push(if system.ext {
                        CHIP_YM2608_EXT
                    } else {
                        CHIP_YM2608
                    });
                    ym2608_sys.insert(cid, chips.len() - 1);
                    fm_clock = if vgm.ym2608_clock != 0 {
                        vgm.ym2608_clock
                    } else {
                        YM2608_DEFAULT_CLOCK
                    };
                    // Furnace's OPNA clock list has exactly one named entry
                    // beyond the 8 MHz default: 38400*13*16.
                    if fm_clock == 38_400 * 13 * 16 {
                        flag_texts.push("clockSel=1".to_string());
                        continue;
                    }
                    fm_default = 8_000_000;
                } else {
                    chips.push(CHIP_YM2151);
                    fm_clock = vgm.ym2151_clock;
                    fm_default = 3_579_545;
                }
                // The cores default to their console clocks. At any other
                // clock the notes play at the wrong pitch unless the flag
                // carries it.
                let mut flags: Vec<String> = Vec::new();
                if fm_clock != 0 && fm_clock != fm_default {
                    flags.push(format!("customClock={}", fm_clock));
                }
                // The linked SSG volume in the VGM extra header is ignored.
                // Furnace's default 128 stays, and Chip Manager lets the user
                // set it; VGMPlayer's chip-volume units do not match Furnace's
                // SSG mix calibration.
                flag_texts.push(flags.join("\n"));
            }
        }
        // The dual-YM3812 balance marks the two OPL chips of that system.
        let opl_first = if self.fm_present && self.opl {
            chips.len()
                - match self.opl_kind.as_deref() {
                    Some(kind) => self.systems.iter().filter(|s| s.kind == kind).count(),
                    None => 0,
                }
        } else {
            0
        };
        let t6w28 = !self.psg_ids.is_empty() && song.psg_t6w28;
        for _cid in self.psg_ids.iter().copied() {
            if t6w28 {
                chips.push(CHIP_T6W28);
                let psg_clock = if vgm.sn76489_clock != 0 {
                    vgm.sn76489_clock
                } else {
                    T6W28_DEFAULT_CLOCK
                };
                if (psg_clock as i64 - T6W28_DEFAULT_CLOCK as i64).abs() <= 2 {
                    flag_texts.push(String::new());
                } else {
                    flag_texts.push(format!("customClock={}", psg_clock));
                }
                continue;
            }
            chips.push(CHIP_SMS);
            let mut psg_clock = if vgm.sn76489_clock != 0 {
                vgm.sn76489_clock
            } else {
                3_579_545
            };
            if _cid == 1 && vgm.sn76489_clock_2 != 0 {
                psg_clock = vgm.sn76489_clock_2;
            }
            // Game Gear selects Furnace's stereo SMPSG core (chipType 3).
            let mut flags = if song.psg_stereo {
                "chipType=3".to_string()
            } else {
                String::new()
            };
            // Furnace's SMS core has an "easy noise" shortcut; turn it off.
            flags = (if flags.is_empty() {
                String::new()
            } else {
                format!("{}\n", flags)
            }) + "noEasyNoise=true";
            let clock_flag = if (psg_clock as i64 - 3_579_545).abs() <= 2 {
                "clockSel=0".to_string()
            } else {
                format!("customClock={}", psg_clock)
            };
            let sep = if flags.is_empty() { "" } else { "\n" };
            flag_texts.push(flags + sep + &clock_flag);
        }
        for _cid in self.pcm_ids.iter().copied() {
            chips.push(CHIP_K007232);
            let mut flags = "stereo=true".to_string();
            let pcm_clock = vgm.k007232_clock;
            if pcm_clock != 0 && pcm_clock != 3_579_545 {
                flags += &format!("\ncustomClock={}", pcm_clock);
            }
            flag_texts.push(flags);
        }
        for cid in self.oki_ids.iter().copied() {
            chips.push(CHIP_MSM6295);
            let group = &self.oki_groups[&cid];
            let total: usize = group.iter().map(|s| s.data.len()).sum();
            let banked = group.len() > 120 || total > 250_000;
            flag_texts.push(oki_flag_text(group[0].clock, group[0].pin7, banked));
        }
        for cid in self.adpcm_ids.iter().copied() {
            chips.push(CHIP_MSM6258);
            flag_texts.push(msm6258_flag_text(self.adpcm_groups[&cid][0].clock));
        }
        for cid in self.spcm_ids.iter().copied() {
            chips.push(CHIP_SEGAPCM);
            let rom_size = vgm
                .roms
                .iter()
                .filter(|s| s.kind == "segapcm" && s.chip as i32 == cid)
                .map(|s| s.rom_size)
                .max()
                .unwrap_or(0);
            flag_texts.push(segapcm_flag_text(vgm.segapcm_clock, rom_size));
        }
        for _cid in self.rf5c68_ids.iter().copied() {
            chips.push(CHIP_RF5C68);
            flag_texts.push(rf5c68_flag_text(vgm.rf5c68_clock, 0));
        }
        for _cid in self.rf5c164_ids.iter().copied() {
            chips.push(CHIP_RF5C68);
            flag_texts.push(rf5c68_flag_text(vgm.rf5c164_clock, 1));
        }
        for _cid in self.c140_ids.iter().copied() {
            chips.push(CHIP_C140);
            flag_texts.push(c140_flag_text(vgm.c140_clock, vgm.c140_type));
        }
        for _cid in self.c352_ids.iter().copied() {
            chips.push(CHIP_C352);
            flag_texts.push(c352_flag_text(vgm.c352_clock, song.c352_quad));
        }
        for _cid in self.k053260_ids.iter().copied() {
            chips.push(CHIP_K053260);
            flag_texts.push(k053260_flag_text(vgm.k053260_clock));
        }
        let mut ymz_sys: IndexMap<i32, usize> = IndexMap::new();
        for cid in self.ymz_ids.iter().copied() {
            ymz_sys.insert(cid, chips.len());
            chips.push(CHIP_YMZ280B);
            flag_texts.push(ymz280b_flag_text(ymz280b_clock(vgm, cid)));
        }
        let mut multipcm_sys: IndexMap<i32, usize> = IndexMap::new();
        for cid in self.multipcm_ids.iter().copied() {
            multipcm_sys.insert(cid, chips.len());
            chips.push(CHIP_MULTIPCM);
            let mut clock = if vgm.multipcm_clock != 0 {
                vgm.multipcm_clock
            } else {
                8_053_975
            };
            if cid == 1 && vgm.multipcm_clock_2 != 0 {
                clock = vgm.multipcm_clock_2;
            }
            flag_texts.push(multipcm_flag_text(clock));
        }
        for _cid in self.upd7759_ids.iter().copied() {
            chips.push(CHIP_PCM_DAC);
            flag_texts.push(pcm_dac_flag_text(44100, false));
        }
        let mut pwm_sys: IndexMap<(i32, i32), usize> = IndexMap::new();
        for cid in self.pwm_ids.iter().copied() {
            for side in 0..2 {
                pwm_sys.insert((cid, side), chips.len());
                chips.push(CHIP_PCM_DAC);
                flag_texts.push(pcm_dac_flag_text(22020, true));
            }
        }
        let mut msm5205_sys: IndexMap<i32, usize> = IndexMap::new();
        for cid in self.msm5205_ids.iter().copied() {
            msm5205_sys.insert(cid, chips.len());
            chips.push(CHIP_PCM_DAC);
            flag_texts.push(pcm_dac_flag_text(44100, false));
        }
        let mut ga20_sys: IndexMap<i32, usize> = IndexMap::new();
        for cid in self.ga20_ids.iter().copied() {
            ga20_sys.insert(cid, chips.len());
            chips.push(CHIP_GA20);
            flag_texts.push(ga20_flag_text(if vgm.ga20_clock != 0 {
                vgm.ga20_clock
            } else {
                3_579_545
            }));
        }
        for _cid in self.c219_ids.iter().copied() {
            chips.push(CHIP_C219);
            flag_texts.push(c219_flag_text(self.c219_clock));
        }
        let k054539_out = 1000.max(py_int(py_round(self.k054539_clock as f64 / 384.0)));
        let mut k054539_sys: IndexMap<i32, Vec<usize>> = IndexMap::new();
        for cid in self.k054539_ids.iter().copied() {
            let mut systems_for_chip: Vec<usize> = Vec::new();
            for _voice in 0..8 {
                systems_for_chip.push(chips.len());
                chips.push(CHIP_PCM_DAC);
                flag_texts.push(pcm_dac_flag_text(k054539_out, true));
            }
            k054539_sys.insert(cid, systems_for_chip);
        }
        let mut msm5232_sys: IndexMap<i32, usize> = IndexMap::new();
        for cid in self.msm5232_ids.iter().copied() {
            msm5232_sys.insert(cid, chips.len());
            chips.push(CHIP_MSM5232);
            flag_texts.push(msm5232_flag_text(if vgm.msm5232_clock != 0 {
                vgm.msm5232_clock
            } else {
                2_119_040
            }));
        }
        let mut es5503_sys: IndexMap<(i32, i32), usize> = IndexMap::new();
        let es5503_rate = if song.es5503_rate != 0 {
            song.es5503_rate
        } else {
            44100
        };
        for (cid, osc) in self.es5503_oscs.iter().copied() {
            es5503_sys.insert((cid, osc), chips.len());
            chips.push(CHIP_PCM_DAC);
            flag_texts.push(pcm_dac_flag_text(es5503_rate as i64, true));
        }
        let mut psg_dac_sys: IndexMap<(i32, i32), usize> = IndexMap::new();
        for key in self.psg_dac_voices.iter().copied() {
            psg_dac_sys.insert(key, chips.len());
            chips.push(CHIP_PCM_DAC);
            flag_texts.push(pcm_dac_flag_text(self.psg_dac_rate as i64, false));
        }
        for _cid in self.scc_ids.iter().copied() {
            chips.push(self.scc_chip);
            flag_texts.push(k051649_flag_text(vgm.k051649_clock));
        }
        // A Famicom's AY is always a Sunsoft 5B (SEL tied low).
        let ay_sunsoft5b = !self.nes_ids.is_empty() || song.nes_present;
        for _cid in self.ay_ids.iter().copied() {
            chips.push(CHIP_AY8910);
            flag_texts.push(ay8910_flag_text(
                vgm.ay8910_clock,
                vgm.ay8910_type,
                ay_sunsoft5b,
            ));
        }
        for _cid in self.nes_ids.iter().copied() {
            chips.push(CHIP_NES);
            // NTSC vs PAL selects the CPU clock; a dump based on the FDS/NSF
            // mapper still runs the same 2A03.
            let clock = if vgm.nes_clock != 0 { vgm.nes_clock } else { 1_789_773 };
            flag_texts.push(if (clock as i64 - 1_789_773).abs() <= 2 {
                String::new()
            } else {
                format!("customClock={}", clock)
            });
        }
        for _cid in self.huc6280_ids.iter().copied() {
            chips.push(CHIP_PCE);
            flag_texts.push(pce_flag_text(vgm.huc6280_clock));
        }
        for _cid in self.gb_ids.iter().copied() {
            chips.push(CHIP_GB);
            // chipType 2 is the CGB-E core.
            let clock = if vgm.gb_dmg_clock != 0 {
                vgm.gb_dmg_clock
            } else {
                4_194_304
            };
            let mut flags = "chipType=2".to_string();
            if (clock as i64 - 4_194_304).abs() > 2 {
                flags += &format!("\ncustomClock={}", clock);
            }
            flag_texts.push(flags);
        }
        for _cid in self.vsu_ids.iter().copied() {
            chips.push(CHIP_VBOY);
            let clock = if vgm.vsu_clock != 0 { vgm.vsu_clock } else { 5_000_000 };
            let mut flags = if clock == 5_000_000 {
                String::new()
            } else {
                format!("customClock={}", clock)
            };
            if song.vsu_dynamic {
                // The source rewrites wave RAM while voices are keyed.
                flags = (if flags.is_empty() {
                    String::new()
                } else {
                    format!("{}\n", flags)
                }) + "screwThis=true";
            }
            flag_texts.push(flags);
        }
        for _cid in self.saa_ids.iter().copied() {
            chips.push(CHIP_SAA1099);
            flag_texts.push(saa1099_flag_text(vgm.saa1099_clock));
        }
        for cid in self.pokey_ids.iter().copied() {
            chips.push(CHIP_POKEY);
            let clock = if cid == 1 && vgm.pokey_clock_2 != 0 {
                vgm.pokey_clock_2
            } else if vgm.pokey_clock != 0 {
                vgm.pokey_clock
            } else {
                1_789_772
            };
            flag_texts.push(pokey_flag_text(clock));
        }
        for _cid in self.mikey_ids.iter().copied() {
            chips.push(CHIP_LYNX);
            let clock = if vgm.mikey_clock != 0 { vgm.mikey_clock } else { 16_000_000 };
            flag_texts.push(if (clock as i64 - 16_000_000).abs() <= 2 {
                String::new()
            } else {
                format!("customClock={}", clock)
            });
        }
        for _cid in self.x1_ids.iter().copied() {
            chips.push(CHIP_X1_010);
            let clock = if vgm.x1_010_clock != 0 { vgm.x1_010_clock } else { 16_000_000 };
            // The X1-010 output is stereo.
            let mut parts = vec!["stereo=true".to_string()];
            if (clock as i64 - 16_000_000).abs() <= 2 {
            } else if (clock as f64 - 50_000_000.0 / 3.0).abs() <= 2.0 {
                parts.push("clockSel=1".to_string());
            } else if (clock as i64 - 14_318_180).abs() <= 2 {
                parts.push("clockSel=2".to_string());
            } else {
                parts.push(format!("customClock={}", clock));
            }
            flag_texts.push(parts.join("\n"));
        }
        for _cid in self.fds_ids.iter().copied() {
            chips.push(CHIP_FDS);
            let clock = if vgm.nes_clock != 0 { vgm.nes_clock } else { 1_789_773 };
            let flags = if (clock as i64 - 1_789_773).abs() <= 2 {
                String::new()
            } else if (clock as i64 - 1_662_607).abs() <= 2 {
                "clockSel=1".to_string()
            } else if (clock as i64 - 1_773_447).abs() <= 2 {
                "clockSel=2".to_string()
            } else {
                format!("customClock={}", clock)
            };
            flag_texts.push(flags);
        }
        for _cid in self.wonderswan_ids.iter().copied() {
            chips.push(CHIP_SWAN);
            let clock = if vgm.wonderswan_clock != 0 {
                vgm.wonderswan_clock
            } else {
                3_072_000
            };
            let mut flags = "stereo=true".to_string();
            if (clock as i64 - 3_072_000).abs() > 2 {
                flags += &format!("\ncustomClock={}", clock);
            }
            flag_texts.push(flags);
        }
        for _cid in self.qsound_ids.iter().copied() {
            chips.push(CHIP_QSOUND);
            let mut feedback = 0;
            let mut delay = 0;
            for w in vgm.writes.iter() {
                if w.chip != "qsound" {
                    continue;
                }
                if w.reg == 0x93 {
                    feedback = (py_int(py_round(w.val as f64 / 64.0))).clamp(0, 255);
                } else if w.reg == 0xD9 {
                    delay = (4095 - w.val as i32).clamp(0, 2725);
                }
            }
            flag_texts.push(format!(
                "echoDelay={}\nechoFeedback={}",
                delay, feedback
            ));
        }
        for _cid in self.k005289_ids.iter().copied() {
            chips.push(CHIP_K005289);
            flag_texts.push(k005289_flag_text(vgm.k005289_clock));
        }
        self.n_chips = chips.len();
        // MultiPCM owns a 2 MB / 512-sample memory. Only the bodies written
        // with an explicit MultiPCM mask may enter.
        let mut render_masks = DEFAULT_RENDER_MASKS;
        if !self.multipcm_ids.is_empty() {
            let mut mask: u32 = 0xFFFF_FFFF;
            for &idx in multipcm_sys.values() {
                if idx < 32 {
                    mask &= !(1u32 << idx) & 0xFFFF_FFFF;
                }
            }
            render_masks = [mask, 0xFFFF_FFFF, 0xFFFF_FFFF, 0xFFFF_FFFF];
        }
        self.render_masks = render_masks;

        let mut vols: Vec<i64> = vec![64; 32];
        let mut chip_vols: Vec<f64> = Vec::new();
        let mut chip_pans: Vec<f64> = Vec::new();
        // Per-chip balance. Only a Sound Blaster Pro YM3812 pair sets it.
        let k_vol = song.k007232_volume.clamp(0.0, 1.0);
        let mut chip_counts: IndexMap<u8, i64> = IndexMap::new();
        for &chip in chips.iter() {
            *chip_counts.entry(chip).or_insert(0) += 1;
        }
        let mut pwm_pan: IndexMap<usize, f64> = IndexMap::new();
        for ((_cid, side), idx) in pwm_sys.iter() {
            pwm_pan.insert(*idx, if *side == 0 { -1.0 } else { 1.0 });
        }
        let k054539_set: BTreeSet<usize> = k054539_sys
            .values()
            .flat_map(|v| v.iter().copied())
            .collect();
        let es5503_set: BTreeSet<usize> = es5503_sys.values().copied().collect();
        let psg_dac_set: BTreeSet<usize> = psg_dac_sys.values().copied().collect();
        let msm5205_set: BTreeSet<usize> = msm5205_sys.values().copied().collect();
        for (i, &chip) in chips.iter().enumerate() {
            // Twin16 K007232 sits under the OPM; the rip's mix says how far.
            let cv = if chip == CHIP_K007232 && self.include_fm {
                k_vol
            } else if pwm_pan.contains_key(&i) {
                // VGMPlay's 32X PWM default is 0xE0 for the stereo chip.
                _player_chip_volume(vgm, 0x11, 1)
            } else if k054539_set.contains(&i) {
                // One Generic PCM DAC system per K054539 voice; unity.
                1.0
            } else if es5503_set.contains(&i) {
                // One PCM DAC system per oscillator; unity.
                1.0
            } else if psg_dac_set.contains(&i) {
                // 0.500 by default, divided by the four PSG voices.
                _player_chip_volume(vgm, 0x00, 1) / 4.0
            } else if msm5205_set.contains(&i) {
                // Generic PCM DAC is unity in Furnace; MSM5205's player
                // default is 0x100.
                _player_chip_volume(
                    vgm,
                    0x2C,
                    1i64.max(self.msm5205_ids.len() as i64),
                )
            } else if super::buf::CHIP_DEV.contains_key(&chip) {
                _sample_chip_volume(song, chip, chip_counts[&chip])
            } else {
                1.0
            };
            chip_vols.push(cv);
            let mut pan = pwm_pan.get(&i).copied().unwrap_or(0.0);
            if self.opl
                && self.opl_kind.as_deref() == Some("opl2")
                && self.opl_ids.len() > 1
                && vgm.opl2_pan
            {
                pan = if i == opl_first { -1.0 } else { 1.0 };
            }
            chip_pans.push(pan);
        }
        // A chip volume above 1.0 does not fit Furnace's 0-64 field. Raise the
        // song master instead and scale every chip down by the same factor.
        let master_gain = chip_vols
            .iter()
            .enumerate()
            .filter(|(i, _)| chips[*i] != CHIP_MULTIPCM)
            .map(|(_, &cv)| cv)
            .chain(std::iter::once(1.0))
            .fold(f64::NEG_INFINITY, f64::max);
        for (i, &cv) in chip_vols.iter().enumerate() {
            vols[i] =
                (py_round(cv / master_gain * 64.0) as i64).clamp(0, 64);
        }
        self.chips = chips;
        self.flag_texts = flag_texts;
        self.ym2610_sys = ym2610_sys;
        self.ym2608_sys = ym2608_sys;
        self.opl_sys = opl_sys;
        self.ymz_sys = ymz_sys;
        self.multipcm_sys = multipcm_sys;
        self.pwm_sys = pwm_sys;
        self.msm5205_sys = msm5205_sys;
        self.ga20_sys = ga20_sys;
        self.k054539_sys = k054539_sys;
        self.msm5232_sys = msm5232_sys;
        self.es5503_sys = es5503_sys;
        self.psg_dac_sys = psg_dac_sys;
        self.opl_first = opl_first;
        self.chip_vols = chip_vols;
        self.chip_pans = chip_pans;
        self.master_gain = master_gain;
        self.vols = vols;
    }
}

impl<'a> FurWriter<'a> {
    fn write_info(&mut self) {
        let song = self.song;
        let track = if self.gd3("track").is_empty() {
            song.vgm
                .path
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default()
        } else {
            self.gd3("track").to_string()
        };
        let author = self.gd3("author").to_string();
        let track_raw = self.gd3("track").to_string();
        let system = if self.gd3("system").is_empty() {
            format!("Arcade ({})", self.chip_label)
        } else {
            self.gd3("system").to_string()
        };
        let game = self.gd3("game").to_string();
        let track_jp = self.gd3("track_jp").to_string();
        let author_jp = self.gd3("author_jp").to_string();
        let system_jp = self.gd3("system_jp").to_string();
        let game_jp = self.gd3("game_jp").to_string();
        let mut master_out = self.master_gain;
        if !self.es5503_oscs.is_empty() {
            let vgm_gain = if song.vgm.main_volume == 0.0 {
                1.0
            } else {
                song.vgm.main_volume
            };
            if vgm_gain > 0.0 {
                master_out *= vgm_gain;
            }
        }
        let chip_ids = {
            let mut chip_ids = self.chips.clone();
            chip_ids.resize(32, 0);
            chip_ids
        };
        let vol_bytes: Vec<u8> = self.vols.iter().map(|&v| (v & 0xFF) as u8).collect();
        let b = &mut self.b;
        let (info_size_off, info_start) = b.begin_block(b"INFO");
        b.u8(0); // timebase stored as value-1; 0 -> timebase 1
        b.u8(song.speed as i64);
        b.u8(song.speed as i64);
        b.u8(1);
        b.f32(song.hz);
        b.u16(self.pat_len as i64);
        b.u16(self.n_ord as i64);
        b.u8(song.hilight_a.clamp(1, 255) as i64);
        b.u8(song.hilight_b.clamp(1, 255) as i64);
        b.u16(self.n_ins as i64);
        b.u16(self.n_wav as i64);
        b.u16(self.n_smp as i64);
        b.u32(self.n_pat as i64);

        b.raw(&chip_ids);
        b.raw(&vol_bytes);
        b.pad(32, 0);
        let flag_ptr_off = b.len() as i64;
        b.pad(128, 0);

        b.strz(&track);
        b.strz(&author);
        b.f32(440.0);
        _compat_phase1(b);

        let ins_ptr_off = b.len() as i64;
        for _ in 0..self.n_ins {
            b.u32(0);
        }
        let wave_ptr_off = b.len() as i64;
        for _ in 0..self.n_wav {
            b.u32(0);
        }
        let smp_ptr_off = b.len() as i64;
        for _ in 0..self.n_smp {
            b.u32(0);
        }
        let pat_ptr_off = b.len() as i64;
        for _ in 0..self.n_pat {
            b.u32(0);
        }

        for ch in 0..self.n_ch as usize {
            for oi in 0..self.n_ord as usize {
                b.u8(self.orders[ch][oi] as i64);
            }
        }
        for &cols in self.effect_cols.iter() {
            b.u8(cols as i64);
        }
        for _ in 0..self.n_ch {
            b.u8(1);
        }
        for _ in 0..self.n_ch {
            b.u8(0);
        }
        for name in self.names_long.iter() {
            b.strz(name);
        }
        for name in self.names_short.iter() {
            b.strz(name);
        }
        b.strz(&self.comment);
        b.f32(master_out);
        _compat_phase2(b);
        b.u16(150);
        b.u16(150);
        b.strz(&track_raw);
        b.strz("");
        b.u8(0);
        b.pad(3, 0);
        b.strz(&system);
        b.strz(&game);
        b.strz(&track_jp);
        b.strz(&author_jp);
        b.strz(&system_jp);
        b.strz(&game_jp);

        // extra chip output (>=135), one (vol, panL, panR) triple per chip.
        for (i, &cv) in self.chip_vols.iter().enumerate() {
            b.f32(cv / self.master_gain);
            let pan = self.chip_pans.get(i).copied().unwrap_or(0.0);
            b.f32(pan);
            b.f32(0.0);
        }
        b.u32(0); // patchbay connections
        b.u8(1); // automatic patchbay (>=136), required or the song is silent
        b.pad(8, 0);
        // speed pattern (>=139) overrides speed 1/2; multiplied by
        // (timeBase+1)=1
        b.u8(2);
        let mut speed_pat = vec![(song.speed & 0xFF) as u8, (song.speed & 0xFF) as u8];
        speed_pat.resize(16, 0);
        b.raw(&speed_pat);
        b.u8(0); // groove count
        let adir_off = b.len() as i64;
        b.u32(0);
        b.u32(0);
        b.u32(0);
        b.end_block(info_size_off as i64, info_start as i64);

        self.flag_ptr_off = flag_ptr_off;
        self.ins_ptr_off = ins_ptr_off;
        self.wave_ptr_off = wave_ptr_off;
        self.smp_ptr_off = smp_ptr_off;
        self.pat_ptr_off = pat_ptr_off;
        self.adir_off = adir_off;
        self.master_out = master_out;
    }

    fn write_adir_flags(&mut self) {
        let adir_ptrs: Vec<usize> = (0..3).map(|_| _write_empty_adir(&mut self.b)).collect();
        for (i, &p) in adir_ptrs.iter().enumerate() {
            self.b.patch_u32(self.adir_off + 4 * i as i64, p as i64);
        }
        for (i, text) in self.flag_texts.iter().enumerate() {
            if !text.is_empty() {
                let ptr = _write_flag(&mut self.b, text);
                self.b
                    .patch_u32(self.flag_ptr_off + 4 * i as i64, ptr as i64);
            }
        }
    }
}

/// Python's `waves[index]` with its negative index rule, or `b""`.
fn _wave_at(waves: &[Vec<u8>], index: i32) -> &[u8] {
    let n = waves.len() as i32;
    let i = if index < 0 { n + index } else { index };
    if i >= 0 && i < n {
        &waves[i as usize]
    } else {
        &[]
    }
}

fn _maps(base: i32, used: i32, total: i32) -> Vec<i32> {
    (0..120.min(total - used)).map(|slot| base + slot).collect()
}

impl<'a> FurWriter<'a> {
    fn write_instruments(&mut self) {
        let b = &mut self.b;
        let mut ins_ptrs: Vec<usize> = Vec::new();
        for (i, patch) in self.fm_ins.iter().enumerate() {
            ins_ptrs.push(b.len());
            let fam = if patch.opll {
                "OPLL"
            } else if patch.opl {
                if patch.four {
                    "OPL3"
                } else {
                    "OPL"
                }
            } else if patch.opn {
                "OPN"
            } else {
                "OPM"
            };
            let name = if patch.name.is_empty() {
                format!("{} {:02}", fam, i)
            } else {
                patch.name.clone()
            };
            _write_ins_opm(b, &name, patch);
        }

        for group in 0..self.n_dac_ins {
            ins_ptrs.push(b.len());
            let first = self.dac_smp_base + group * 120;
            let maps = _maps(
                first,
                group * 120,
                self.dac_smps.len() as i32,
            );
            // 0.6.8.3 reads a nonzero base as an embedded sample count.
            _write_ins_sample_map(b, &format!("DAC {}", group + 1), 0, &maps, INS_C352, 0x01);
        }

        for (i, smp) in self.pcm_smps.iter().enumerate() {
            ins_ptrs.push(b.len());
            _write_ins_pcm(
                b,
                &format!("PCM {}", _pcm_label(smp, i as i32)),
                i as i32,
                INS_K007232,
                None,
            );
        }
        for (i, smp) in self.oki_smps.iter().enumerate() {
            ins_ptrs.push(b.len());
            _write_ins_pcm(
                b,
                &smp.name,
                (self.pcm_smps.len() + i) as i32,
                INS_MSM6295,
                None,
            );
        }
        let adpcm_smp_base = (self.pcm_smps.len() + self.oki_smps.len()) as i32;
        for group in 0..self.n_adpcm_ins {
            ins_ptrs.push(b.len());
            let start = group * 120;
            let maps = _maps(
                adpcm_smp_base + start,
                start,
                self.adpcm_smps.len() as i32,
            );
            _write_ins_sample_map(
                b,
                &format!("ADPCM {}", group + 1),
                0,
                &maps,
                INS_MSM6258,
                0x01,
            );
        }
        let spcm_ins_base =
            (self.pcm_smps.len() + self.oki_smps.len() + self.adpcm_smps.len()) as i32;
        for (i, smp) in self.spcm_smps.iter().enumerate() {
            ins_ptrs.push(b.len());
            _write_ins_pcm(
                b,
                &format!("SEGA {:05x}", smp.start),
                spcm_ins_base + i as i32,
                INS_SEGAPCM,
                None,
            );
        }
        let rf5c68_ins_base = spcm_ins_base + self.spcm_smps.len() as i32;
        for (i, smp) in self.rf5c68_smps.iter().enumerate() {
            ins_ptrs.push(b.len());
            _write_ins_pcm(
                b,
                &format!("RF5C68 {:04x}", smp.start),
                rf5c68_ins_base + i as i32,
                INS_RF5C68,
                None,
            );
        }
        let rf5c164_ins_base = rf5c68_ins_base + self.rf5c68_smps.len() as i32;
        for (i, smp) in self.rf5c164_smps.iter().enumerate() {
            ins_ptrs.push(b.len());
            _write_ins_pcm(
                b,
                &format!("RF5C164 {:04x}", smp.start),
                rf5c164_ins_base + i as i32,
                INS_RF5C68,
                None,
            );
        }
        let c140_ins_base = rf5c164_ins_base + self.rf5c164_smps.len() as i32;
        for (i, smp) in self.c140_smps.iter().enumerate() {
            ins_ptrs.push(b.len());
            _write_ins_pcm(
                b,
                &format!("C140 {:04x}", smp.start),
                c140_ins_base + i as i32,
                INS_C140,
                None,
            );
        }
        let c352_ins_base = c140_ins_base + self.c140_smps.len() as i32;
        for (i, smp) in self.c352_smps.iter().enumerate() {
            ins_ptrs.push(b.len());
            _write_ins_pcm(
                b,
                &format!("C352 {:06x}", smp.start),
                c352_ins_base + i as i32,
                INS_C352,
                None,
            );
        }
        let k053260_ins_base = c352_ins_base + self.c352_smps.len() as i32;
        for (i, smp) in self.k053260_smps.iter().enumerate() {
            ins_ptrs.push(b.len());
            _write_ins_pcm(
                b,
                &format!("K053260 {:06x}", smp.start),
                k053260_ins_base + i as i32,
                INS_K053260,
                None,
            );
        }
        for group in 0..self.n_ym2610_a_ins {
            ins_ptrs.push(b.len());
            let first = self.ym2610_a_smp_base + group * 120;
            let maps = _maps(
                first,
                group * 120,
                self.ym2610_a_smps.len() as i32,
            );
            _write_ins_sample_map(
                b,
                &format!("ADPCM-A {}", group + 1),
                0,
                &maps,
                INS_ADPCMA,
                0x01,
            );
        }
        for group in 0..self.n_ym2610_b_ins {
            ins_ptrs.push(b.len());
            let first = self.ym2610_b_smp_base + group * 120;
            let maps = _maps(
                first,
                group * 120,
                self.ym2610_b_smps.len() as i32,
            );
            _write_ins_sample_map(
                b,
                &format!("ADPCM-B {}", group + 1),
                0,
                &maps,
                INS_ADPCMB,
                0x01,
            );
        }
        if self.n_ym2608_rhythm_ins != 0 {
            // One empty map serves all six kit columns.
            ins_ptrs.push(b.len());
            _write_ins_sample_map(b, "Rhythm", 0, &[], INS_ADPCMA, 0x01);
        }
        for group in 0..self.n_ym2608_b_ins {
            ins_ptrs.push(b.len());
            let first = self.ym2608_b_smp_base + group * 120;
            let maps = _maps(
                first,
                group * 120,
                self.ym2608_b_smps.len() as i32,
            );
            _write_ins_sample_map(
                b,
                &format!("ADPCM-B {}", group + 1),
                0,
                &maps,
                INS_ADPCMB,
                0x01,
            );
        }
        for group in 0..self.n_y8950_b_ins {
            ins_ptrs.push(b.len());
            let first = self.y8950_b_smp_base + group * 120;
            let maps = _maps(
                first,
                group * 120,
                self.y8950_b_smps.len() as i32,
            );
            _write_ins_sample_map(
                b,
                &format!("Y8950 ADPCM {}", group + 1),
                0,
                &maps,
                INS_ADPCMB,
                0x01,
            );
        }
        for (i, smp) in self.opl4_wave_smps.iter().enumerate() {
            ins_ptrs.push(b.len());
            _write_ins_pcm(
                b,
                &format!("OPL4 WAVE {:03}", smp.wave),
                self.opl4_wave_smp_base + i as i32,
                INS_MULTIPCM,
                None,
            );
        }
        // The AY sample path allows 120 bodies per instrument through a note
        // map; notes 60-179 select a slot and every slot plays at C-4.
        for group in 0..self.n_ssg_pcm_ins {
            ins_ptrs.push(b.len());
            let first = group * 120;
            let maps: Vec<i32> = (0..120.min(self.ssg_pcm_smps.len() as i32 - first))
                .map(|slot| self.ssg_pcm_smp_base + first + slot)
                .collect();
            _write_ins_sample_map(
                b,
                &format!("PSG PCM {}", group + 1),
                0,
                &maps,
                INS_AY,
                0x03,
            );
        }
        for (i, _smp) in self.nes_dpcm_smps.iter().enumerate() {
            ins_ptrs.push(b.len());
            _write_ins_pcm(
                b,
                &format!("NES DPCM {:02}", i),
                self.nes_dpcm_smp_base + i as i32,
                INS_NES,
                None,
            );
        }
        for group in 0..self.n_nes_pcm_ins {
            ins_ptrs.push(b.len());
            let first = self.nes_pcm_smp_base + group * 120;
            let maps = _maps(first, group * 120, self.nes_pcm_smps.len() as i32);
            _write_ins_sample_map(
                b,
                &format!("NES PCM {}", group + 1),
                0,
                &maps,
                INS_C352,
                0x03,
            );
        }
        for (i, _smp) in self.huc6280_pcm_smps.iter().enumerate() {
            ins_ptrs.push(b.len());
            _write_ins_pcm(
                b,
                &format!("HuC6280 PCM {:02}", i),
                self.huc6280_pcm_smp_base + i as i32,
                INS_C352,
                None,
            );
        }
        for (i, smp) in self.ymz_smps.iter().enumerate() {
            ins_ptrs.push(b.len());
            _write_ins_pcm(
                b,
                &format!("YMZ{} {:06x}", smp.chip_id + 1, smp.start),
                self.ymz_smp_base + i as i32,
                INS_YMZ280B,
                None,
            );
        }
        for (i, smp) in self.multipcm_smps.iter().enumerate() {
            ins_ptrs.push(b.len());
            let mp = [
                smp.ar,
                smp.d1r,
                smp.dl,
                smp.d2r,
                smp.rr,
                smp.rc,
                smp.lfo,
                smp.vib,
                smp.am,
                if smp.direct { 8 } else { 0 },
            ];
            _write_ins_pcm(
                b,
                &format!("MP{} {:03}", smp.chip_id + 1, smp.index),
                self.multipcm_smp_base + i as i32,
                INS_MULTIPCM,
                Some(&mp[..]),
            );
        }
        for group in 0..self.n_upd_ins {
            ins_ptrs.push(b.len());
            let first = group * 120;
            let maps: Vec<i32> = (0..120.min(self.upd7759_smps.len() as i32 - first))
                .map(|slot| self.upd7759_smp_base + first + slot)
                .collect();
            _write_ins_sample_map(
                b,
                &format!("uPD PCM {}", group + 1),
                0,
                &maps,
                INS_AMIGA,
                0x03,
            );
        }
        for group in 0..self.n_pwm_ins {
            ins_ptrs.push(b.len());
            let first = group * 120;
            let maps: Vec<i32> = (0..120.min(self.pwm_smps.len() as i32 - first))
                .map(|slot| self.pwm_smp_base + first + slot)
                .collect();
            _write_ins_sample_map(
                b,
                &format!("PWM PCM {}", group + 1),
                0,
                &maps,
                INS_AMIGA,
                0x03,
            );
        }
        for group in 0..self.n_msm5205_ins {
            ins_ptrs.push(b.len());
            let first = group * 120;
            let maps: Vec<i32> = (0..120.min(self.msm5205_smps.len() as i32 - first))
                .map(|slot| self.msm5205_smp_base + first + slot)
                .collect();
            _write_ins_sample_map(
                b,
                &format!("MSM5205 {}", group + 1),
                0,
                &maps,
                INS_AMIGA,
                0x03,
            );
        }
        for (i, smp) in self.ga20_smps.iter().enumerate() {
            ins_ptrs.push(b.len());
            _write_ins_pcm(
                b,
                &format!("GA20 {:05x}", smp.start),
                self.ga20_smp_base + i as i32,
                INS_GA20,
                None,
            );
        }
        for (pos, smp) in self.c219_real.iter().copied() {
            ins_ptrs.push(b.len());
            _write_ins_pcm(
                b,
                &format!("C219 {:04x}", smp.start),
                pos as i32,
                INS_C219,
                None,
            );
        }
        for (i, smp) in self.k054539_smps.iter().enumerate() {
            ins_ptrs.push(b.len());
            _write_ins_pcm(
                b,
                &format!("K054539 {:06x}", smp.start),
                self.k054539_smp_base + i as i32,
                INS_AMIGA,
                None,
            );
        }
        for (i, control) in self.msm5232_controls.iter().enumerate() {
            ins_ptrs.push(b.len());
            // A duty macro keeps the group control write on the note and lets
            // the platform add its internal-envelope bit (0x20).
            let ins = crate::model::ToneIns {
                duty: vec![control & 0x1F],
                ..Default::default()
            };
            _write_ins_psg(
                b,
                &format!("MSM5232 {}", i + 1),
                Some(&ins),
                INS_MSM5232,
            );
        }
        for (i, smp) in self.es5503_smps.iter().enumerate() {
            ins_ptrs.push(b.len());
            _write_ins_pcm(
                b,
                &format!("ES5503 {:05x}", smp.ptr),
                self.es5503_smp_base + i as i32,
                INS_AMIGA,
                None,
            );
        }
        for group in 0..self.n_psg_dac_ins {
            ins_ptrs.push(b.len());
            let first = group * 120;
            let maps: Vec<i32> = (0..120.min(self.psg_dac_smps.len() as i32 - first))
                .map(|slot| self.psg_dac_smp_base + first + slot)
                .collect();
            _write_ins_sample_map(
                b,
                &format!("PSG DAC {}", group + 1),
                0,
                &maps,
                INS_AMIGA,
                0x03,
            );
        }
        for group in 0..self.n_mikey_ins {
            ins_ptrs.push(b.len());
            let first = group * 120;
            let maps: Vec<i32> = (0..120.min(self.mikey_smps.len() as i32 - first))
                .map(|slot| self.mikey_smp_base + first + slot)
                .collect();
            _write_ins_sample_map(
                b,
                &format!("MIKEY PCM {}", group + 1),
                0,
                &maps,
                INS_AMIGA,
                0x03,
            );
        }
        for (i, _smp) in self.wonderswan_pcm_smps.iter().enumerate() {
            ins_ptrs.push(b.len());
            _write_ins_pcm(
                b,
                &format!("WS PCM {:02}", i),
                self.wonderswan_pcm_smp_base + i as i32,
                INS_SWAN,
                None,
            );
        }
        for group in 0..self.n_x1_ins {
            ins_ptrs.push(b.len());
            let first = group * 120;
            let maps: Vec<i32> = (0..120.min(self.x1_smps.len() as i32 - first))
                .map(|slot| self.x1_smp_base + first + slot)
                .collect();
            _write_ins_sample_map(
                b,
                &format!("X1 PCM {}", group + 1),
                0,
                &maps,
                INS_X1_010,
                0x03,
            );
        }
        for (i, spec) in self.scc_specs.iter().enumerate() {
            ins_ptrs.push(b.len());
            let wave = _wave_at(self.scc_waves, spec.wave);
            let mut name = format!("SCC {:02} {}", i, _wave_label(wave));
            if !spec.waves.is_empty() {
                name += "+morph";
            }
            if !spec.pitch.is_empty() {
                name += "+pitch";
            }
            _write_ins_scc(b, &name, spec, 0);
        }
        for spec in self.ay_specs.iter() {
            ins_ptrs.push(b.len());
            let name = "PSG".to_string()
                + if !spec.pitch.is_empty() { "+pitch" } else { "" };
            _write_ins_ay(b, &name, Some(spec));
        }
        let psg_ins_type = if self.song.psg_t6w28 {
            INS_T6W28
        } else {
            INS_STD
        };
        let psg_label = if self.song.psg_t6w28 { "T6W28" } else { "SN" };
        for spec in self.psg_specs.iter() {
            ins_ptrs.push(b.len());
            let name = psg_label.to_string()
                + if !spec.pitch.is_empty() { "+pitch" } else { "" };
            _write_ins_psg(b, &name, Some(spec), psg_ins_type);
        }
        for spec in self.nes_specs.iter() {
            ins_ptrs.push(b.len());
            _write_ins_nes(b, "NES", Some(spec));
        }
        for (i, spec) in self.huc6280_specs.iter().enumerate() {
            ins_ptrs.push(b.len());
            let wave = _wave_at(self.huc6280_waves, spec.wave);
            let mut name = format!("PCE {:02} {}", i, _wave_label(wave));
            if !spec.waves.is_empty() {
                name += "+morph";
            }
            if !spec.pitch.is_empty() {
                name += "+pitch";
            }
            _write_ins_pce(b, &name, spec);
        }
        for (i, spec) in self.gb_specs.iter().enumerate() {
            ins_ptrs.push(b.len());
            let mut name;
            if spec.gb_wave {
                let wave = _wave_at(self.gb_waves, spec.wave);
                name = format!("GB Wave {:02} {}", i, _wave_label(wave));
                if !spec.waves.is_empty() {
                    name += "+morph";
                }
            } else if spec.gb_soft_env {
                name = "GB Soft".to_string()
                    + if !spec.pitch.is_empty() { "+pitch" } else { "" };
            } else {
                name = format!(
                    "GB Env {:x}{}{}",
                    spec.gb_env_vol, spec.gb_env_dir, spec.gb_env_len
                ) + if !spec.pitch.is_empty() { "+pitch" } else { "" };
            }
            _write_ins_gb(b, &name, spec);
        }
        for (i, spec) in self.vsu_specs.iter().enumerate() {
            ins_ptrs.push(b.len());
            let mod_table: Vec<u8> = if 0 <= spec.vsu_mod
                && (spec.vsu_mod as usize) < self.vsu_mod_waves.len()
            {
                self.vsu_mod_waves[spec.vsu_mod as usize].clone()
            } else {
                vec![0u8; 32]
            };
            let mut name = format!("VSU {:02}", i);
            if !spec.pitch.is_empty() {
                name += "+pitch";
            }
            if !spec.vol.is_empty() {
                name += "+vol";
            }
            _write_ins_vsu(
                b,
                &name,
                spec,
                &mod_table,
                self.vsu_wave_base + spec.wave.max(0),
            );
        }
        for (i, spec) in self.saa_specs.iter().enumerate() {
            ins_ptrs.push(b.len());
            let mut name = format!("SAA {:02}", i);
            if !spec.pitch.is_empty() {
                name += "+pitch";
            }
            if !spec.panl.is_empty() || !spec.panr.is_empty() {
                name += "+pan";
            }
            if !spec.env.is_empty() {
                name += "+env";
            }
            if !spec.mode.is_empty() {
                name += "+mode";
            }
            if !spec.noise.is_empty() {
                name += "+noise";
            }
            _write_ins_saa(b, &name, Some(spec));
        }
        for (i, spec) in self.pokey_specs.iter().enumerate() {
            ins_ptrs.push(b.len());
            let mut name = format!("POKEY {:02}", i);
            if !spec.pitch.is_empty() {
                name += "+pitch";
            }
            if !spec.vol.is_empty() {
                name += "+vol";
            }
            _write_ins_pokey(b, &name, Some(spec));
        }
        for (i, spec) in self.mikey_specs.iter().enumerate() {
            ins_ptrs.push(b.len());
            let mut name = format!("MIKEY {:02}", i);
            if !spec.pitch.is_empty() {
                name += "+pitch";
            }
            if !spec.vol.is_empty() {
                name += "+vol";
            }
            if !spec.duty.is_empty() {
                name += "+duty";
            }
            if !spec.lfsr.is_empty() {
                name += "+lfsr";
            }
            _write_ins_mikey(b, &name, Some(spec));
        }
        for (i, spec) in self.x1_specs.iter().enumerate() {
            ins_ptrs.push(b.len());
            let mut name = format!("X1-010 {:02}", i);
            if !spec.waves.is_empty() {
                name += "+wave";
            }
            if !spec.pitch.is_empty() {
                name += "+pitch";
            }
            _write_ins_x1(
                b,
                &name,
                Some(spec),
                self.x1_wave_base,
                self.x1_env_base,
            );
        }
        for (i, spec) in self.fds_specs.iter().enumerate() {
            ins_ptrs.push(b.len());
            let mod_table: Vec<u8> = if 0 <= spec.fds_mod
                && (spec.fds_mod as usize) < self.fds_mod_waves.len()
            {
                self.fds_mod_waves[spec.fds_mod as usize].clone()
            } else {
                vec![0u8; 32]
            };
            let mut name = format!("FDS {:02}", i);
            if !spec.waves.is_empty() {
                name += "+wave";
            }
            if !spec.pitch.is_empty() {
                name += "+pitch";
            }
            _write_ins_fds(
                b,
                &name,
                spec,
                &mod_table,
                self.fds_wave_base + spec.wave.max(0),
            );
        }
        for (i, spec) in self.wonderswan_specs.iter().enumerate() {
            ins_ptrs.push(b.len());
            let mut name = format!("WS {:02}", i);
            if !spec.waves.is_empty() {
                name += "+wave";
            }
            if !spec.pitch.is_empty() {
                name += "+pitch";
            }
            if !spec.panl.is_empty() || !spec.panr.is_empty() {
                name += "+pan";
            }
            _write_ins_swan(
                b,
                &name,
                spec,
                self.wonderswan_wave_base + spec.wave.max(0),
            );
        }
        for (i, spec) in self.qsound_specs.iter().enumerate() {
            ins_ptrs.push(b.len());
            let mut name = if spec.sample_kind == "adpcm" {
                "QS ADPCM ".to_string()
            } else {
                "QS PCM ".to_string()
            } + &format!("{:02}", i);
            if !spec.pitch.is_empty() {
                name += "+pitch";
            }
            if !spec.vol.is_empty() {
                name += "+vol";
            }
            let mut spec2 = spec.clone();
            spec2.sample = self.qsound_smp_base + spec.sample.max(0);
            _write_ins_qsound(b, &name, &spec2);
        }
        for (i, spec) in self.k005289_specs.iter().enumerate() {
            ins_ptrs.push(b.len());
            let wave = _wave_at(self.k005289_waves, spec.wave);
            let mut name = format!("WSG {:02} {}", i, _wave_label(wave));
            if !spec.waves.is_empty() {
                name += "+morph";
            }
            if !spec.pitch.is_empty() {
                name += "+pitch";
            }
            _write_ins_scc(b, &name, spec, self.k005289_wave_base);
        }
        self.ins_ptrs = ins_ptrs;
    }
}

impl<'a> FurWriter<'a> {
    fn write_samples(&mut self) {
        let b = &mut self.b;
        let render_masks = self.render_masks;
        let mut smp_ptrs: Vec<usize> = Vec::new();
        for (i, smp) in self.pcm_smps.iter().enumerate() {
            smp_ptrs.push(b.len());
            _write_sample(
                b,
                &_pcm_label(smp, i as i32),
                &smp.pcm7,
                smp.c4_rate(self.k_clock) as i32,
                smp.volume_scale,
                Some(&render_masks),
            );
        }
        for smp in self.oki_smps.iter() {
            smp_ptrs.push(b.len());
            _write_vox(b, &smp.name, &smp.data, smp.rate, false, Some(&render_masks));
        }
        for smp in self.adpcm_smps.iter() {
            smp_ptrs.push(b.len());
            _write_vox(b, &smp.name, &smp.data, smp.rate, true, Some(&render_masks));
        }
        for smp in self.spcm_smps.iter() {
            smp_ptrs.push(b.len());
            _write_sample_s8(
                b,
                &format!("spcm_{:05x}", smp.start),
                &smp.data,
                smp.c4_rate(self.spcm_clock),
                (
                    smp.loop_start.map(|v| v as i32),
                    smp.loop_end.map(|v| v as i32),
                ),
                SMP_DEPTH_8BIT,
                0,
                Some(&render_masks),
            );
        }
        for smp in self.rf5c68_smps.iter() {
            smp_ptrs.push(b.len());
            _write_sample_s8(
                b,
                &format!("rf5c68_{:04x}", smp.start),
                &smp.data,
                smp.c4_rate(self.rf5c68_clock),
                (smp.loop_start, smp.loop_end),
                SMP_DEPTH_8BIT,
                0,
                Some(&render_masks),
            );
        }
        for smp in self.rf5c164_smps.iter() {
            smp_ptrs.push(b.len());
            _write_sample_s8(
                b,
                &format!("rf5c164_{:04x}", smp.start),
                &smp.data,
                smp.c4_rate(self.rf5c164_clock),
                (smp.loop_start, smp.loop_end),
                SMP_DEPTH_8BIT,
                0,
                Some(&render_masks),
            );
        }
        for smp in self.c140_smps.iter() {
            smp_ptrs.push(b.len());
            _write_sample_s8(
                b,
                &format!("c140_{:04x}", smp.start),
                &smp.data,
                smp.c4_rate(self.c140_clock),
                (smp.loop_start, smp.loop_end),
                smp.depth,
                0,
                Some(&render_masks),
            );
        }
        for smp in self.c352_smps.iter() {
            smp_ptrs.push(b.len());
            _write_sample_s8(
                b,
                &format!("c352_{:06x}", smp.start),
                &smp.data,
                smp.c4_rate(self.c352_clock) as i32,
                (
                    smp.loop_start.map(|v| v as i32),
                    smp.loop_end.map(|v| v as i32),
                ),
                smp.depth,
                smp.loop_mode,
                Some(&render_masks),
            );
        }
        for smp in self.k053260_smps.iter() {
            smp_ptrs.push(b.len());
            let name = format!("k053260_{:06x}", smp.start);
            let c4_rate = smp.c4_rate(self.k053260_clock);
            if smp.depth == SMP_DEPTH_ADPCM_K {
                _write_sample_k(
                    b,
                    &name,
                    &smp.data,
                    c4_rate,
                    smp.loop_start.is_some(),
                    Some(&render_masks),
                );
            } else {
                _write_sample_s8(
                    b,
                    &name,
                    &smp.data,
                    c4_rate,
                    (
                        smp.loop_start.map(|v| v as i32),
                        smp.loop_end.map(|v| v as i32),
                    ),
                    SMP_DEPTH_8BIT,
                    0,
                    None,
                );
            }
        }
        for (i, smp) in self.dac_smps.iter().enumerate() {
            smp_ptrs.push(b.len());
            let rate = 1i64.max(py_int(py_round(
                smp.c4_rate as f64 * DAC_RATE_COMPENSATION,
            )));
            _write_sample_s8(
                b,
                &format!("dac_{:03}", i),
                &smp.data,
                rate as i32,
                (None, None),
                SMP_DEPTH_8BIT,
                0,
                Some(&render_masks),
            );
        }
        let ym2610_clock = if self.song.vgm.ym2610_clock != 0 {
            self.song.vgm.ym2610_clock
        } else {
            8_000_000
        };
        let ym2610_a_rate = 1i64.max(py_int(py_round(ym2610_clock as f64 / 432.0)));
        for smp in self.ym2610_a_smps.iter() {
            let sys = self.ym2610_sys.get(&(smp.chip_id as i32)).copied().unwrap_or(0);
            smp_ptrs.push(b.len());
            let masks = [if sys < 32 { 1u32 << sys } else { 0 }, 0, 0, 0];
            _write_sample_adpcm(
                b,
                &format!("adpcmA_{:06x}", smp.start),
                &smp.data,
                ym2610_a_rate as i32,
                SMP_DEPTH_ADPCM_A,
                &masks,
                false,
            );
        }
        for smp in self.ym2610_b_smps.iter() {
            let sys = self.ym2610_sys.get(&(smp.chip_id as i32)).copied().unwrap_or(0);
            let rate = 1i64.max(py_int(py_round(
                ym2610_clock as f64 / 144.0 * (smp.rate & 0xFFFF) as f64 / 65535.0
                    * ADPCM_B_RATE_COMPENSATION,
            )));
            let loop_ = smp.hits.iter().any(|h| h.r#loop);
            smp_ptrs.push(b.len());
            let masks = [0u32, if sys < 32 { 1u32 << sys } else { 0 }, 0, 0];
            _write_sample_adpcm(
                b,
                &format!("adpcmB_{:06x}_{:04x}", smp.start, smp.rate),
                &smp.data,
                rate as i32,
                SMP_DEPTH_ADPCM_B,
                &masks,
                loop_,
            );
        }
        let ym2608_clock = if self.song.vgm.ym2608_clock != 0 {
            self.song.vgm.ym2608_clock
        } else {
            YM2608_DEFAULT_CLOCK
        };
        for smp in self.ym2608_b_smps.iter() {
            let sys = self.ym2608_sys.get(&(smp.chip_id as i32)).copied().unwrap_or(0);
            let rate = 1i64.max(py_int(py_round(
                ym2608_clock as f64 / 144.0 * (smp.rate & 0xFFFF) as f64 / 65535.0
                    * ADPCM_B_RATE_COMPENSATION,
            )));
            let loop_ = smp.hits.iter().any(|h| h.r#loop);
            let body = deltat_store_body(&smp.data);
            smp_ptrs.push(b.len());
            let masks = [if sys < 32 { 1u32 << sys } else { 0 }, 0, 0, 0];
            _write_sample_adpcm(
                b,
                &format!("deltaT_{:06x}_{:04x}", smp.start, smp.rate),
                &body,
                rate as i32,
                SMP_DEPTH_ADPCM_B,
                &masks,
                loop_,
            );
        }
        let y8950_clock = if self.song.vgm.y8950_clock != 0 {
            self.song.vgm.y8950_clock
        } else {
            3_579_545
        };
        for smp in self.y8950_b_smps.iter() {
            let sys = self.opl_sys.get(&(smp.chip_id as i32)).copied().unwrap_or(0);
            let rate = 1i64.max(py_int(py_round(
                y8950_clock as f64 / 144.0 * (smp.rate & 0xFFFF) as f64 / 65535.0
                    * ADPCM_B_RATE_COMPENSATION,
            )));
            let loop_ = smp.hits.iter().any(|h| h.r#loop);
            let body = deltat_store_body(&smp.data);
            smp_ptrs.push(b.len());
            let masks = [if sys < 32 { 1u32 << sys } else { 0 }, 0, 0, 0];
            _write_sample_adpcm(
                b,
                &format!("y8950_{:06x}_{:04x}", smp.start, smp.rate),
                &body,
                rate as i32,
                SMP_DEPTH_ADPCM_B,
                &masks,
                loop_,
            );
        }
        let opl4_clock = if self.song.vgm.ymf278b_clock != 0 {
            self.song.vgm.ymf278b_clock
        } else {
            33_868_800
        };
        for smp in self.opl4_wave_smps.iter() {
            smp_ptrs.push(b.len());
            _write_sample_s8(
                b,
                &format!("opl4_{:03}", smp.wave),
                &smp.data,
                smp.c4_rate(opl4_clock) as i32,
                (
                    smp.loop_start.map(|v| v as i32),
                    smp.loop_end.map(|v| v as i32),
                ),
                SMP_DEPTH_16BIT,
                0,
                Some(&render_masks),
            );
        }
        for smp in self.ssg_pcm_smps.iter() {
            smp_ptrs.push(b.len());
            _write_sample_s8(
                b,
                &smp.name,
                &smp.data,
                smp.rate,
                (None, None),
                SMP_DEPTH_8BIT,
                0,
                Some(&render_masks),
            );
        }
        let nes_clock = if self.song.vgm.nes_clock != 0 {
            self.song.vgm.nes_clock
        } else {
            1_789_773
        };
        for smp in self.nes_dpcm_smps.iter() {
            smp_ptrs.push(b.len());
            _write_sample_dpcm(
                b,
                &format!("nes_dpcm_{:04x}", smp.start),
                &smp.data,
                nes_dpcm_rate(nes_clock, smp.rate_index),
                smp.loop_,
                Some(&render_masks),
            );
        }
        for smp in self.nes_pcm_smps.iter() {
            smp_ptrs.push(b.len());
            // 8-bit signed bodies; compensate like the YM2612 DAC bodies.
            let rate = 1i64.max(py_int(py_round(
                smp.rate as f64 * DAC_RATE_COMPENSATION,
            )));
            _write_sample_s8(
                b,
                &smp.name,
                &smp.data,
                rate as i32,
                (None, None),
                SMP_DEPTH_8BIT,
                0,
                Some(&render_masks),
            );
        }
        for smp in self.huc6280_pcm_smps.iter() {
            smp_ptrs.push(b.len());
            let loop_ = if smp.loop_ {
                (Some(0), Some(smp.data.len() as i32))
            } else {
                (None, None)
            };
            _write_sample_s8(
                b,
                &smp.name,
                &smp.data,
                smp.rate,
                loop_,
                SMP_DEPTH_8BIT,
                0,
                Some(&render_masks),
            );
        }
        for smp in self.ymz_smps.iter() {
            smp_ptrs.push(b.len());
            let sys = self.ymz_sys.get(&smp.chip_id).copied().unwrap_or(0);
            // Only the owning YMZ system uploads this body.
            let masks = [if sys < 32 { 1u32 << sys } else { 0 }, 0, 0, 0];
            let loop_ = match (smp.loop_start, smp.loop_end) {
                (Some(lo), Some(hi)) => (Some(lo as i32), Some(hi as i32)),
                _ => (None, None),
            };
            let name = format!("ymz{}_{:06x}", smp.chip_id + 1, smp.start);
            if smp.depth == SMP_DEPTH_YMZ_ADPCM {
                _write_sample_ymz_adpcm(
                    b,
                    &name,
                    &smp.data,
                    smp.c4_rate() as i32,
                    loop_,
                    Some(&masks),
                );
            } else {
                _write_sample_s8(
                    b,
                    &name,
                    &smp.data,
                    smp.c4_rate() as i32,
                    loop_,
                    smp.depth,
                    0,
                    Some(&masks),
                );
            }
        }
        for smp in self.multipcm_smps.iter() {
            smp_ptrs.push(b.len());
            let sys = self.multipcm_sys.get(&smp.chip_id).copied().unwrap_or(0);
            let masks = [if sys < 32 { 1u32 << sys } else { 0 }, 0, 0, 0];
            let loop_ = match (smp.loop_start, smp.loop_end) {
                (Some(lo), Some(hi)) => (Some(lo as i32), Some(hi as i32)),
                _ => (None, None),
            };
            let name = format!("mp{}_{:03}", smp.chip_id + 1, smp.index);
            if smp.depth == 12 {
                _write_sample_s12(
                    b,
                    &name,
                    &smp.data,
                    smp.frames as i32,
                    smp.c4_rate() as i32,
                    loop_,
                    Some(&masks),
                );
            } else {
                _write_sample_s8(
                    b,
                    &name,
                    &smp.data,
                    smp.c4_rate() as i32,
                    loop_,
                    smp.depth,
                    0,
                    Some(&masks),
                );
            }
        }
        for smp in self.upd7759_smps.iter() {
            smp_ptrs.push(b.len());
            // The Generic PCM DAC reads the DivSample directly.
            let masks = [0u32, 0, 0, 0];
            _write_sample_s8(
                b,
                &smp.name,
                &smp.data,
                44100,
                (None, None),
                SMP_DEPTH_16BIT,
                0,
                Some(&masks),
            );
        }
        for smp in self.pwm_smps.iter() {
            smp_ptrs.push(b.len());
            let loop_ = match (smp.loop_start, smp.loop_end) {
                (Some(lo), Some(hi)) => (Some(lo as i32), Some(hi as i32)),
                _ => (None, None),
            };
            let masks = [0u32, 0, 0, 0];
            _write_sample_s8(
                b,
                &smp.name(),
                &smp.data,
                smp.rate,
                loop_,
                SMP_DEPTH_16BIT,
                0,
                Some(&masks),
            );
        }
        for smp in self.msm5205_smps.iter() {
            smp_ptrs.push(b.len());
            let masks = [0u32, 0, 0, 0];
            _write_sample_s8(
                b,
                &smp.name(),
                &smp.data,
                smp.rate,
                (None, None),
                SMP_DEPTH_16BIT,
                0,
                Some(&masks),
            );
        }
        for (i, smp) in self.ga20_smps.iter().enumerate() {
            smp_ptrs.push(b.len());
            let sys = self.ga20_sys.get(&smp.chip_id).copied().unwrap_or(0);
            let masks = [if sys < 32 { 1u32 << sys } else { 0 }, 0, 0, 0];
            let clock = if self.song.vgm.ga20_clock != 0 {
                self.song.vgm.ga20_clock
            } else {
                3_579_545
            };
            _write_sample_s8(
                b,
                &format!("GA20 {:03} {:05x}", i, smp.start),
                &smp.data,
                smp.c4_rate(clock) as i32,
                (None, None),
                SMP_DEPTH_8BIT,
                0,
                Some(&masks),
            );
        }
        for (i, smp) in self.c219_all.iter().enumerate() {
            smp_ptrs.push(b.len());
            let loop_ = match (smp.loop_start, smp.loop_end) {
                (Some(lo), Some(hi)) => (Some(lo), Some(hi)),
                _ => (None, None),
            };
            _write_sample_s8(
                b,
                &format!("c219_{:03}_{:04x}", i, smp.start),
                &smp.data,
                smp.c4_rate(self.c219_clock),
                loop_,
                smp.depth,
                0,
                Some(&render_masks),
            );
        }
        for (i, smp) in self.k054539_smps.iter().enumerate() {
            smp_ptrs.push(b.len());
            let loop_ = match (smp.loop_start, smp.loop_end) {
                (Some(lo), Some(hi)) => (Some(lo as i32), Some(hi as i32)),
                _ => (None, None),
            };
            let rate = 1i64.max(py_int(py_round(
                smp.c4_rate(self.k054539_clock) as f64 * DAC_RATE_COMPENSATION,
            )));
            let masks = [0u32, 0, 0, 0];
            _write_sample_s8(
                b,
                &format!("k054539_{:03}_{:06x}", i, smp.start),
                &smp.data,
                rate as i32,
                loop_,
                smp.depth,
                0,
                Some(&masks),
            );
        }
        for (i, smp) in self.es5503_smps.iter().enumerate() {
            smp_ptrs.push(b.len());
            let rate = 1i64.max(py_int(py_round(
                smp.c4_rate as f64 * DAC_RATE_COMPENSATION,
            )));
            let loop_ = if smp.loop_ {
                (Some(0), Some(smp.length() as i32))
            } else {
                (None, None)
            };
            let masks = [0u32, 0, 0, 0];
            _write_sample_s8(
                b,
                &format!("es5503_{:03}_{:05x}", i, smp.ptr),
                &smp.data,
                rate as i32,
                loop_,
                SMP_DEPTH_8BIT,
                0,
                Some(&masks),
            );
        }
        for (i, smp) in self.psg_dac_smps.iter().enumerate() {
            smp_ptrs.push(b.len());
            let masks = [0u32, 0, 0, 0];
            _write_sample_4bit(
                b,
                &format!("psg_dac_{:03}", i),
                &smp.data,
                smp.frames as i32,
                1i64.max(py_int(py_round(
                    smp.rate as f64 * DAC_RATE_COMPENSATION,
                ))) as i32,
                Some(&masks),
            );
        }
        for smp in self.mikey_smps.iter() {
            smp_ptrs.push(b.len());
            let masks = [0u32, 0, 0, 0];
            _write_sample_s8(
                b,
                &smp.name(),
                &smp.data,
                smp.rate,
                (None, None),
                SMP_DEPTH_8BIT,
                0,
                Some(&masks),
            );
        }
        for (i, smp) in self.wonderswan_pcm_smps.iter().enumerate() {
            smp_ptrs.push(b.len());
            let rate = 1i64.max(py_int(py_round(
                smp.rate as f64 * WS_PCM_RATE_COMPENSATION,
            )));
            let masks = [0u32, 0, 0, 0];
            _write_sample_s8(
                b,
                &format!("ws_pcm_{:02}", i),
                &smp.data,
                rate as i32,
                (None, None),
                SMP_DEPTH_8BIT,
                0,
                Some(&masks),
            );
        }
        for (i, smp) in self.qsound_smps.iter().enumerate() {
            smp_ptrs.push(b.len());
            if smp.kind == "adpcm" {
                _write_sample_qsound_adpcm(
                    b,
                    &format!("qs_adpcm_{:02}", i),
                    &smp.data,
                    py_int(py_round(QSOUND_ADPCM_NIBBLE_RATE)) as i32,
                );
            } else {
                let loop_ = if smp.loop_start >= 0 && smp.loop_end >= 0 {
                    (Some(smp.loop_start), Some(smp.loop_end))
                } else {
                    (None, None)
                };
                let masks = [0xFFFF_FFFFu32, 0, 0, 0];
                _write_sample_s8(
                    b,
                    &format!("qs_pcm_{:03}", i),
                    &smp.data,
                    8363,
                    loop_,
                    SMP_DEPTH_8BIT,
                    0,
                    Some(&masks),
                );
            }
        }
        for smp in self.x1_smps.iter() {
            smp_ptrs.push(b.len());
            _write_sample_s8(
                b,
                &format!("x1_{}_{:06x}", smp.chip_id + 1, smp.start),
                &smp.data,
                smp.rate(),
                (None, None),
                SMP_DEPTH_8BIT,
                0,
                Some(&render_masks),
            );
        }

        self.smp_ptrs = smp_ptrs;
    }
}

impl<'a> FurWriter<'a> {
    fn write_patterns(&mut self) {
        let b = &mut self.b;
        let mut pat_ptrs: Vec<usize> = Vec::new();
        for (ch, idx, rows) in self.patterns.iter() {
            pat_ptrs.push(b.len());
            _write_patn(b, *ch as i64, *idx as i64, rows, self.fx_cols, "");
        }
        self.pat_ptrs = pat_ptrs;
    }

    fn write_waves(&mut self) {
        let b = &mut self.b;
        let mut wave_ptrs: Vec<usize> = Vec::new();
        for wave in self.scc_waves.iter() {
            // The chip reads signed bytes; Furnace's wavetable is unsigned
            // with 128 as the center.
            let data: Vec<u8> = wave.iter().map(|&val| val.wrapping_add(128)).collect();
            wave_ptrs.push(_write_wave(
                b,
                &format!("scc_wave_{}", _wave_label(wave)),
                &data,
                255,
            ));
        }
        for wave in self.huc6280_waves.iter() {
            // 5-bit samples (0-31); the PCE wavetable height is 31.
            wave_ptrs.push(_write_wave(
                b,
                &format!("pce_wave_{}", _wave_label(wave)),
                wave,
                31,
            ));
        }
        for wave in self.gb_waves.iter() {
            // The DMG wave RAM is 32 4-bit samples; height 15.
            let data: Vec<u8> = wave.iter().map(|&v| 15 - (v & 0x0F)).collect();
            wave_ptrs.push(_write_wave(
                b,
                &format!("gb_wave_{}", _wave_label(wave)),
                &data,
                15,
            ));
        }
        for wave in self.vsu_waves.iter() {
            // The VSU wave RAM is 32 6-bit samples; height 63.
            wave_ptrs.push(_write_wave(
                b,
                &format!("vsu_wave_{}", _wave_label(wave)),
                wave,
                63,
            ));
        }
        for wave in self.x1_waves.iter() {
            // X1 waveform RAM is signed 8-bit.
            let data: Vec<u8> = wave.iter().map(|&val| val.wrapping_add(128)).collect();
            wave_ptrs.push(_write_wave(
                b,
                &format!("x1_wave_{}", _wave_label(wave)),
                &data,
                255,
            ));
        }
        for env in self.x1_envs.iter() {
            // Envelope RAM holds a left/right nibble pair.
            let data: Vec<u8> = env
                .iter()
                .map(|&val| (((val >> 4) & 0x0F) + (val & 0x0F) + 1) / 2)
                .collect();
            wave_ptrs.push(_write_wave(
                b,
                &format!("x1_env_{}", _wave_label(env)),
                &data,
                15,
            ));
        }
        for wave in self.fds_waves.iter() {
            // FDS wave RAM is 64 6-bit samples.
            wave_ptrs.push(_write_wave(
                b,
                &format!("fds_wave_{}", _wave_label(wave)),
                wave,
                63,
            ));
        }
        for wave in self.wonderswan_waves.iter() {
            // WonderSwan wave RAM is 32 4-bit samples per channel.
            wave_ptrs.push(_write_wave(
                b,
                &format!("ws_wave_{}", _wave_label(wave)),
                wave,
                15,
            ));
        }
        for wave in self.k005289_waves.iter() {
            // The WSG PROM holds 32 4-bit samples per waveform.
            wave_ptrs.push(_write_wave(
                b,
                &format!("wsg_wave_{}", _wave_label(wave)),
                wave,
                15,
            ));
        }
        self.wave_ptrs = wave_ptrs;
    }

    fn patch_directories(&mut self) {
        for (i, &p) in self.ins_ptrs.iter().enumerate() {
            self.b.patch_u32(self.ins_ptr_off + 4 * i as i64, p as i64);
        }
        for (i, &p) in self.wave_ptrs.iter().enumerate() {
            self.b.patch_u32(self.wave_ptr_off + 4 * i as i64, p as i64);
        }
        for (i, &p) in self.smp_ptrs.iter().enumerate() {
            self.b.patch_u32(self.smp_ptr_off + 4 * i as i64, p as i64);
        }
        for (i, &p) in self.pat_ptrs.iter().enumerate() {
            self.b.patch_u32(self.pat_ptr_off + 4 * i as i64, p as i64);
        }
    }
}

/// Walk a .fur we wrote and return header facts (for tests / sanity).
#[derive(Clone, Debug, PartialEq)]
pub struct FurSkeleton {
    pub version: u16,
    pub bytes: usize,
    pub info_end: usize,
    pub first_block: Vec<u8>,
    pub magics: Vec<String>,
    pub chips: Vec<u8>,
    pub notes: i64,
    pub offs: i64,
    pub bxx: i64,
    pub d00: i64,
    pub fx_cmds: IndexMap<i32, i64>,
}

pub fn parse_fur_skeleton(data: &[u8]) -> FurSkeleton {
    let data = decompress_fur(data);
    if data.len() < 16 || &data[..16] != FUR_MAGIC {
        panic!("not a Furnace module");
    }
    let u16_at = |off: usize| -> u16 {
        u16::from_le_bytes([data[off], data[off + 1]])
    };
    let u32_at = |off: usize| -> u32 {
        u32::from_le_bytes([data[off], data[off + 1], data[off + 2], data[off + 3]])
    };
    let ver = u16_at(16);
    let info_ptr = u32_at(20) as usize;
    if data.len() < info_ptr + 4 || &data[info_ptr..info_ptr + 4] != b"INFO" {
        panic!("INFO missing");
    }
    let info_size = u32_at(info_ptr + 4) as usize;
    let info_end = info_ptr + 8 + info_size;
    let mut magics: Vec<String> = Vec::new();
    let mut pos = info_end;
    let mut notes: i64 = 0;
    let mut offs: i64 = 0;
    let mut bxx: i64 = 0;
    let mut d00: i64 = 0;
    let mut fx_cmds: IndexMap<i32, i64> = IndexMap::new();
    while pos + 8 <= data.len() {
        let mag = &data[pos..pos + 4];
        let size = u32_at(pos + 4) as usize;
        magics.push(String::from_utf8_lossy(mag).into_owned());
        let block = &data[(pos + 8).min(data.len())..(pos + 8 + size).min(data.len())];
        if mag == &b"PATN"[..] && block.len() >= 4 {
            let mut i = 4;
            while i < block.len() && block[i] != 0 {
                i += 1;
            }
            i += 1;
            let mut row = 0;
            while i < block.len() && block[i] != 0xFF && row < 256 {
                let mask = block[i];
                i += 1;
                if mask == 0 {
                    row += 1;
                    continue;
                }
                let mut effect_mask: u32 = 0;
                if mask & 0x20 != 0 {
                    if i < block.len() {
                        effect_mask |= block[i] as u32;
                        i += 1;
                    }
                }
                if mask & 0x40 != 0 {
                    if i < block.len() {
                        effect_mask |= (block[i] as u32) << 8;
                        i += 1;
                    }
                }
                if mask & 0x08 != 0 {
                    effect_mask |= 1;
                }
                if mask & 0x10 != 0 {
                    effect_mask |= 2;
                }
                if mask & 1 != 0 {
                    if i < block.len() {
                        let n = block[i];
                        i += 1;
                        if n == 180 {
                            offs += 1;
                        } else if n < 180 {
                            notes += 1;
                        }
                    }
                }
                if mask & 2 != 0 {
                    i += 1;
                }
                if mask & 4 != 0 {
                    i += 1;
                }
                let mut fx_bytes: Vec<u8> = Vec::new();
                for bit in 0..16 {
                    if effect_mask & (1 << bit) != 0 {
                        if i < block.len() {
                            fx_bytes.push(block[i]);
                            i += 1;
                        }
                    }
                }
                let mut col = 0;
                while col + 1 < fx_bytes.len() {
                    let cmd = fx_bytes[col] as i32;
                    *fx_cmds.entry(cmd).or_insert(0) += 1;
                    if cmd == 0x0B {
                        bxx += 1;
                    } else if cmd == 0x0D {
                        d00 += 1;
                    }
                    col += 2;
                }
                row += 1;
            }
        }
        pos += 8 + size;
        if pos > data.len() {
            break;
        }
    }
    // INFO payload: 24 bytes of song params, then 32 chip IDs.
    let start = info_ptr + 8 + 24;
    let mut chips: Vec<u8> = if start < data.len() {
        data[start..(start + 32).min(data.len())].to_vec()
    } else {
        Vec::new()
    };
    chips.retain(|&c| c != 0);
    FurSkeleton {
        version: ver,
        bytes: data.len(),
        info_end,
        first_block: data[info_end.min(data.len())..(info_end + 4).min(data.len())].to_vec(),
        magics,
        chips,
        notes,
        offs,
        bxx,
        d00,
        fx_cmds,
    }
}
