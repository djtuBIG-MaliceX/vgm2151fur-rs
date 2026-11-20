//! The `--report` text: one human-readable summary of an analyzed song.
//!
//! Ported from `analyze.format_report`. Every line and every number format is
//! the Python f-string's, because callers diff the report against the
//! reference converter.

use std::collections::BTreeSet;

use crate::model::{system_channel_ranges, FmSystem, Song};
use crate::parity::{fmt_g, format_f, py_mod};
use crate::vgm::{VGM_RATE, YM2203_DEFAULT_CLOCK, YM2612_DEFAULT_CLOCK};

const VERSION: &str = "0.11.0";

/// The two views of `Song::dac_stats` the port has used: the collector's
/// typed map and the plain integer counter map. Both answer the report's
/// integer reads; only the typed map can carry the text `dac_mode`.
trait DacStatsView {
    fn int(&self, key: &str) -> i64;
    fn text(&self, key: &str, default: &'static str) -> &'static str;
}

impl DacStatsView for crate::ym2612dac::DacStats {
    fn int(&self, key: &str) -> i64 {
        crate::ym2612dac::DacStats::int(self, key)
    }

    fn text(&self, key: &str, default: &'static str) -> &'static str {
        crate::ym2612dac::DacStats::text(self, key, default)
    }
}

impl DacStatsView for indexmap::IndexMap<String, i64> {
    fn int(&self, key: &str) -> i64 {
        self.get(key).copied().unwrap_or(0)
    }

    fn text(&self, _key: &str, default: &'static str) -> &'static str {
        default
    }
}

/// Insert before the last element, like Python `lines.insert(-1, text)`.
fn insert_before_last(lines: &mut Vec<String>, text: String) {
    let at = lines.len().saturating_sub(1);
    lines.insert(at, text);
}

fn has_opl_family(families: &BTreeSet<&str>) -> bool {
    families
        .iter()
        .any(|kind| matches!(*kind, "opl" | "opl2" | "opl3" | "opll"))
}

fn system_label(kind: &str, ym2610_b: bool) -> String {
    match kind {
        "ym2151" => "YM2151".to_string(),
        "ym2203" => "YM2203".to_string(),
        "ym2612" => "YM2612".to_string(),
        "ym2610" => {
            if ym2610_b {
                "YM2610B".to_string()
            } else {
                "YM2610".to_string()
            }
        }
        "ym2608" => "YM2608".to_string(),
        "opl" => "YM3526".to_string(),
        "opl2" => "YM3812".to_string(),
        "opl3" => "YMF262".to_string(),
        "y8950" => "Y8950".to_string(),
        "opl4" => "YMF278B".to_string(),
        "opll" => "YM2413".to_string(),
        other => other.to_string(),
    }
}

fn clock_or(clock: u32, default: u32) -> u32 {
    if clock != 0 { clock } else { default }
}

/// Python `list(ints)` text.
fn list_text(values: &[i32]) -> String {
    let parts: Vec<String> = values.iter().map(|v| v.to_string()).collect();
    format!("[{}]", parts.join(", "))
}

/// The per-chip sample region fields the report prints.
trait RegionRow {
    fn r_start(&self) -> i64;
    fn r_length(&self) -> usize;
    fn r_hits(&self) -> usize;
    fn r_loop(&self) -> Option<(i64, i64)>;
    fn r_loop_mode(&self) -> i32 {
        0
    }
}

macro_rules! region_impl_i32 {
    ($ty:ty) => {
        impl RegionRow for $ty {
            fn r_start(&self) -> i64 {
                self.start as i64
            }
            fn r_length(&self) -> usize {
                self.length()
            }
            fn r_hits(&self) -> usize {
                self.hits.len()
            }
            fn r_loop(&self) -> Option<(i64, i64)> {
                self.loop_start.zip(self.loop_end).map(|(a, b)| (a as i64, b as i64))
            }
        }
    };
}

region_impl_i32!(crate::segapcm::SegapcmSample);
region_impl_i32!(crate::rf5c::Rf5cSample);
region_impl_i32!(crate::c140::C140Sample);
region_impl_i32!(crate::c219::C219Sample);
region_impl_i32!(crate::k053260::K053260Sample);
region_impl_i32!(crate::k054539::K054539Sample);

impl RegionRow for crate::c352::C352Sample {
    fn r_start(&self) -> i64 {
        self.start as i64
    }
    fn r_length(&self) -> usize {
        self.length()
    }
    fn r_hits(&self) -> usize {
        self.hits.len()
    }
    fn r_loop(&self) -> Option<(i64, i64)> {
        self.loop_start.zip(self.loop_end)
    }
    fn r_loop_mode(&self) -> i32 {
        self.loop_mode
    }
}

impl RegionRow for crate::ymz280b::YmzSample {
    fn r_start(&self) -> i64 {
        self.start as i64
    }
    fn r_length(&self) -> usize {
        self.length() as usize
    }
    fn r_hits(&self) -> usize {
        self.hits.len()
    }
    fn r_loop(&self) -> Option<(i64, i64)> {
        self.loop_start.zip(self.loop_end)
    }
}

fn push_region_lines<T: RegionRow>(lines: &mut Vec<String>, chip: &str, samples: &[T]) {
    if samples.is_empty() {
        return;
    }
    let hits: usize = samples.iter().map(|s| s.r_hits()).sum();
    lines.push(format!(
        "{:<11} {} regions  hits {}",
        chip,
        samples.len(),
        hits
    ));
    for (i, s) in samples.iter().enumerate() {
        let loop_text = match s.r_loop() {
            Some((start, end)) => {
                let mut text = format!("loop {}..{}", start, end);
                if s.r_loop_mode() == 2 {
                    text.push_str(" ping-pong");
                }
                text
            }
            None => "one-shot".to_string(),
        };
        lines.push(format!(
            "  {:02}  start {:05X}  len {:5}  hits {:3}  {}",
            i,
            s.r_start(),
            s.r_length(),
            s.r_hits(),
            loop_text
        ));
    }
}

pub fn format_report(song: &Song) -> String {
    let v = &song.vgm;
    let mut lines: Vec<String> = vec![
        format!("file        {}", v.path.display()),
        format!("track       {}", v.name()),
        format!("converter   vgm2151fur {}", VERSION),
        format!("game        {}", v.gd3.get("game").cloned().unwrap_or_default()),
        format!("author      {}", v.gd3.get("author").cloned().unwrap_or_default()),
        format!(
            "length      {}s  ({} samples)",
            format_f(v.total_samples as f64 / VGM_RATE as f64, 2),
            v.total_samples
        ),
        format!(
            "t0          {} ({}s) pattern origin",
            song.t0,
            format_f(song.t0 as f64 / VGM_RATE as f64, 3)
        ),
        match song.loop_sample {
            Some(loop_sample) => format!(
                "loop        {} ({}s)",
                loop_sample,
                format_f(loop_sample as f64 / VGM_RATE as f64, 3)
            ),
            None => "loop        none".to_string(),
        },
        format!(
            "grid        {} Hz, speed {} -> {} samples/row  ({} BPM 16ths{})",
            fmt_g(song.hz, 4),
            song.speed,
            format_f(song.samples_per_row(), 1),
            format_f(song.bpm(), 1),
            if song.grid_note.is_empty() {
                String::new()
            } else {
                format!("; {}", song.grid_note)
            }
        ),
        format!("FM patches  {}", song.fm_patches.len()),
    ];
    if song.legato_steps != 0 || song.legato_ramps != 0 {
        lines.push(format!(
            "pitch       {} legato steps, {} single-step ramps",
            song.legato_steps, song.legato_ramps
        ));
    }
    if song.fm_chip == "ym2203" {
        insert_before_last(
            &mut lines,
            format!(
                "FM chip     YM2203 x{} @ {} MHz, {} SSG notes",
                song.ym2203_ids.len(),
                fmt_g(clock_or(v.ym2203_clock, YM2203_DEFAULT_CLOCK) as f64 / 1e6, 6),
                song.ym2203_ssg_count
            ),
        );
    } else if song.fm_chip == "ym2612" {
        let layout = if song.ym2612_ext { " Ext" } else { "" };
        let csm = if song.ym2612_csm { " + CSM" } else { "" };
        insert_before_last(
            &mut lines,
            format!(
                "FM chip     YM2612{}{} x{} @ {} MHz",
                layout,
                csm,
                song.ym2612_ids.len(),
                fmt_g(clock_or(v.ym2612_clock, YM2612_DEFAULT_CLOCK) as f64 / 1e6, 6)
            ),
        );
    } else if matches!(song.fm_chip.as_str(), "opl" | "opl2" | "opl3") {
        let chip_name = match song.fm_chip.as_str() {
            "opl" => "YM3526",
            "opl2" => "YM3812",
            _ => "YMF262",
        };
        let default = match song.fm_chip.as_str() {
            "opl" => crate::opl::OPL_DEFAULT_CLOCK,
            "opl2" => crate::opl::OPL2_DEFAULT_CLOCK,
            _ => crate::opl::OPL3_DEFAULT_CLOCK,
        };
        let clock = if song.fm_chip == "opl3" {
            v.opl3_clock
        } else if song.fm_chip == "opl2" {
            v.opl2_clock
        } else {
            v.opl_clock
        };
        let mode = if song.opl_drums { " + rhythm" } else { "" };
        let pan = if song.fm_chip == "opl2" && v.opl2_pan {
            " (hard panned)"
        } else {
            ""
        };
        let four = song.fm_patches.iter().filter(|p| p.four).count();
        insert_before_last(
            &mut lines,
            format!(
                "FM chip     {}{} x{} @ {} MHz{}",
                chip_name,
                mode,
                song.opl_ids.len(),
                fmt_g(clock_or(clock, default) as f64 / 1e6, 6),
                pan
            ),
        );
        if song.fm_chip == "opl3" || four != 0 {
            insert_before_last(
                &mut lines,
                format!(
                    "FM voices   {} columns, {} four-operator instrument(s)",
                    crate::opl::channel_count(&song.fm_chip, song.opl_drums),
                    four
                ),
            );
        }
    } else if song.fm_chip == "ym2610" {
        let variant = if song.ym2610_b { "YM2610B" } else { "YM2610" };
        insert_before_last(
            &mut lines,
            format!(
                "FM chip     {} x{} @ {} MHz, {} SSG notes, {} ADPCM-A, {} ADPCM-B",
                variant,
                song.ym2610_ids.len(),
                fmt_g(
                    clock_or(v.ym2610_clock, crate::ym2610::YM2610_DEFAULT_CLOCK) as f64 / 1e6,
                    6
                ),
                song.ym2610_ssg_count,
                song.ym2610_a_count,
                song.ym2610_b_count
            ),
        );
    } else if song.fm_chip == "ym2608" {
        let layout = if song.ym2608_ext { " Ext" } else { "" };
        insert_before_last(
            &mut lines,
            format!(
                "FM chip     YM2608{} x{} @ {} MHz, {} SSG notes, {} rhythm, {} ADPCM-B",
                layout,
                song.ym2608_ids.len(),
                fmt_g(
                    clock_or(v.ym2608_clock, crate::ym2608::YM2608_DEFAULT_CLOCK) as f64 / 1e6,
                    6
                ),
                song.ym2608_ssg_count,
                song.ym2608_rhythm_count,
                song.ym2608_b_count
            ),
        );
    } else if song.fm_chip == "ym2151" {
        // The OPM was the tool's first target and needs no header to explain
        // itself, but a module that also carries an OPL family does.
        let kinds: BTreeSet<&str> = song.fm_systems.iter().map(|s| s.kind.as_str()).collect();
        if kinds.len() > 1 {
            insert_before_last(
                &mut lines,
                format!(
                    "FM chip     YM2151 @ {} MHz, eight voices",
                    fmt_g(clock_or(v.ym2151_clock, 3_579_545) as f64 / 1e6, 6)
                ),
            );
        }
    }
    let families: BTreeSet<&str> = song.fm_systems.iter().map(|s| s.kind.as_str()).collect();
    if families.len() > 1 {
        // A merged log converts more than one family. Name each with its own
        // clock and note count, so the report says who played what.
        let ranges = system_channel_ranges(&song.fm_systems);
        let mut kind_order: Vec<&str> = Vec::new();
        for system in &song.fm_systems {
            if !kind_order.contains(&system.kind.as_str()) {
                kind_order.push(system.kind.as_str());
            }
        }
        let mut parts: Vec<String> = Vec::new();
        for kind in kind_order {
            let group: Vec<(&FmSystem, i32)> = ranges
                .iter()
                .filter(|(s, _)| s.kind == kind)
                .map(|(s, base)| (s, *base))
                .collect();
            let first = group[0].0;
            let mut name = system_label(kind, song.ym2610_b);
            if first.ext {
                name.push_str(" Ext");
            }
            if first.drums {
                name.push_str(" kit");
            }
            let mut notes: usize = 0;
            for (system, base) in &group {
                let end = *base + system.fm_channels;
                notes += song
                    .events
                    .iter()
                    .filter(|e| {
                        e.on
                            && !e.pcm
                            && !e.dac
                            && e.tone_chip == 0
                            && *base <= e.ch
                            && e.ch < end
                    })
                    .count();
            }
            let clock = first.clock;
            parts.push(format!(
                "{} x{} @ {} MHz, {} notes",
                name,
                group.len(),
                fmt_g(clock as f64 / 1e6, 6),
                notes
            ));
        }
        insert_before_last(&mut lines, format!("FM systems  {}", parts.join("; ")));
    }

    for (i, p) in song.fm_patches.iter().enumerate() {
        let lfo = if !p.fms_macro.is_empty() {
            format!("  FMS {} @{}t", list_text(&p.fms_macro), p.macro_speed)
        } else {
            String::new()
        };
        // An operator level the driver scripted while notes of this voice sounded.
        let ops = if p.op_tl_macro.is_empty() {
            String::new()
        } else {
            let entries: Vec<String> = p
                .op_tl_macro
                .iter()
                .map(|(oi, vals, spd)| format!("op{} $4n x{}@{}t", oi + 1, vals.len(), spd))
                .collect();
            format!("  {}", entries.join(", "))
        };
        lines.push(format!(
            "  {:02}  alg {} fb {}  uses {:4}  carrierTL {:02X}  {}{}{}",
            i,
            p.alg,
            p.fb,
            p.uses,
            p.carrier_tl(),
            p.name,
            lfo,
            ops
        ));
    }
    if song.fm_chip == "ym2612" && !song.fm_patches.is_empty() {
        lines.push(
            "  (carrier TL is relative; the key-on level rides the volume column)".to_string(),
        );
    } else if has_opl_family(&families) && !song.fm_patches.is_empty() {
        // In a combined module this says what the OPL columns do; the arcade
        // family beside them keeps its carrier levels relative instead.
        lines.push(
            "  (carrier TL rides the volume column; a modulator level the log moved mid-note \
travels as an operator macro; a kit column shares one voice)"
                .to_string(),
        );
    }
    if !song.chip_fx.is_empty() {
        let codes = [0x10, 0x17, 0x18, 0x1E, 0x1F];
        let lfo_n = song
            .chip_fx
            .iter()
            .filter(|entry| codes.contains(&entry.1))
            .count();
        if matches!(song.fm_chip.as_str(), "ym2612" | "ym2610") {
            lines.push(format!("LFO fx      {} chip writes (10xx)", lfo_n));
        } else if matches!(song.fm_chip.as_str(), "opl" | "opl2" | "opl3") {
            lines.push(format!("chip fx     {} writes (0xBD depth and rhythm bits)", lfo_n));
        } else {
            lines.push(format!("LFO fx      {} chip writes (17/18/1E/1F)", lfo_n));
        }
    }
    let fm_ons: usize;
    if families.len() > 1 {
        // Two families share the module, so count every pitch column at its own
        // base instead of assuming the FM block starts at channel 0. The PSG,
        // SCC and sample-chip notes carry their own markers.
        fm_ons = song
            .events
            .iter()
            .filter(|e| e.on && !e.pcm && !e.dac && e.tone_chip == 0)
            .count();
    } else if song.fm_chip == "ym2203" {
        fm_ons = song
            .events
            .iter()
            .filter(|e| e.on && !e.pcm && py_mod(i64::from(e.ch), 6) < 3)
            .count();
    } else if song.fm_chip == "ym2612" {
        let fm_end = (if song.ym2612_ext { 9 } else { 6 }) * song.ym2612_ids.len() as i32;
        fm_ons = song
            .events
            .iter()
            .filter(|e| e.on && !e.pcm && !e.dac && 0 <= e.ch && e.ch < fm_end)
            .count();
    } else if matches!(song.fm_chip.as_str(), "opl" | "opl2" | "opl3") {
        // The analyzer has already told the kit from the voices: rhythm columns
        // share instruments and frequency registers, and they are counted apart.
        fm_ons = song.fm_notes as usize;
    } else if song.fm_chip == "ym2610" {
        let fm_end = (if song.ym2610_b { 6 } else { 4 }) * song.ym2610_ids.len() as i32;
        fm_ons = song
            .events
            .iter()
            .filter(|e| e.on && !e.pcm && !e.dac && 0 <= e.ch && e.ch < fm_end)
            .count();
    } else {
        let fm_end = 8 * std::cmp::max(
            1,
            if song.ym2151_ids.is_empty() {
                1
            } else {
                song.ym2151_ids.len()
            },
        ) as i32;
        fm_ons = song
            .events
            .iter()
            .filter(|e| e.on && !e.pcm && !e.dac && 0 <= e.ch && e.ch < fm_end)
            .count();
    }
    let pcm_ons = song.events.iter().filter(|e| e.on && e.pcm).count();
    let kit = if has_opl_family(&families) && song.drum_notes != 0 {
        format!("  (+{} kit hits)", song.drum_notes)
    } else {
        String::new()
    };
    lines.push(format!("FM notes    {}{}", fm_ons, kit));
    if !song.psg_ids.is_empty() {
        let label = if song.psg_t6w28 { "T6W28" } else { "SN76489" };
        lines.push(format!(
            "{:<12}{} notes  ({} instruments, {} Hz)",
            label,
            song.psg_count,
            song.psg_ins.len(),
            clock_or(song.vgm.sn76489_clock, crate::sn76489::PSG_DEFAULT_CLOCK as u32)
        ));
    }
    if song.dac_count != 0 {
        let data: usize = song.dac_samples.iter().map(|s| s.data.len()).sum();
        let maps = (song.dac_samples.len() + 119) / 120;
        let source: i64 = song.dac_stats.int("dac_source_bytes");
        let mode = song.dac_stats.text("dac_mode", "slices");
        let stat = |key: &str| -> i64 { song.dac_stats.int(key) };
        let (shape, extra): (String, String) = if mode == "source" {
            (
                format!("{} bank samples", song.dac_samples.len()),
                format!(
                    ", {} bank runs, {} reused",
                    stat("dac_source_runs"),
                    stat("dac_source_runs_reused")
                ),
            )
        } else if mode == "attacks" {
            (
                format!("{} PCM hits", song.dac_samples.len()),
                format!(
                    ", {} attacks, {} prefix-shared",
                    stat("dac_attack_segments"),
                    stat("dac_attack_prefix_reused")
                ),
            )
        } else if mode == "chunks" {
            (
                format!("{} PCM chunks", song.dac_samples.len()),
                format!(
                    ", {} pan chunks, {} silence bytes skipped",
                    stat("dac_pan_chunks"),
                    stat("dac_trimmed_silence_bytes")
                ),
            )
        } else if mode == "packed" {
            (
                format!("{} PCM phrases", song.dac_samples.len()),
                format!(
                    ", {} silence bytes skipped",
                    stat("dac_trimmed_silence_bytes")
                ),
            )
        } else if mode == "timeline" {
            ("1 timeline sample".to_string(), String::new())
        } else {
            let silent = stat("dac_silent_groups");
            (
                format!("{} slices", song.dac_samples.len()),
                if silent != 0 {
                    format!(", {} silent rows skipped", silent)
                } else {
                    String::new()
                },
            )
        };
        lines.push(format!(
            "DAC         {} notes  {}  {} maps  {} bytes  ({} source bytes{})",
            song.dac_count, shape, maps, data, source, extra
        ));
    }
    let n_regions = song.pcm_samples.len()
        + song.segapcm_samples.len()
        + song.rf5c68_samples.len()
        + song.rf5c164_samples.len()
        + song.c140_samples.len()
        + song.c219_samples.len()
        + song.c352_samples.len()
        + song.k053260_samples.len()
        + song.k054539_samples.len()
        + song.ym2610_a_samples.len()
        + song.ym2610_b_samples.len()
        + song.ymz280b_samples.len();
    lines.push(format!(
        "PCM notes   {}  ({} sample regions)",
        pcm_ons, n_regions
    ));
    if !song.scc_ids.is_empty() {
        lines.push(format!(
            "SCC notes   {}  ({} chip{}, {} waveforms, {} Hz{})",
            song.scc_count,
            song.scc_ids.len(),
            if song.scc_ids.len() > 1 { "s" } else { "" },
            song.scc_waves.len(),
            song.vgm.k051649_clock,
            if song.vgm.k051649_plus { ", SCC+" } else { "" }
        ));
    }
    if !song.ay_ids.is_empty() {
        lines.push(format!(
            "AY notes    {}  ({} chip{}, {} Hz)",
            song.ay_count,
            song.ay_ids.len(),
            if song.ay_ids.len() > 1 { "s" } else { "" },
            song.vgm.ay8910_clock
        ));
    }
    if !song.k005289_ids.is_empty() {
        lines.push(format!(
            "WSG notes   {}  ({} chip{}, {} waveforms, {} Hz)",
            song.k005289_count,
            song.k005289_ids.len(),
            if song.k005289_ids.len() > 1 { "s" } else { "" },
            song.k005289_waves.len(),
            clock_or(
                song.vgm.k005289_clock,
                crate::k005289::K005289_DEFAULT_CLOCK
            )
        ));
    }
    if !song.saa_ids.is_empty() {
        lines.push(format!(
            "SAA notes   {}  ({} chip{}, {} Hz, {} instruments)",
            song.saa_count,
            song.saa_ids.len(),
            if song.saa_ids.len() > 1 { "s" } else { "" },
            clock_or(song.vgm.saa1099_clock, crate::saa1099::SAA_DEFAULT_CLOCK),
            song.saa_ins.len()
        ));
    }
    if !song.pokey_ids.is_empty() {
        lines.push(format!(
            "POKEY notes {}  ({} chip{}, {} Hz, {} instruments)",
            song.pokey_count,
            song.pokey_ids.len(),
            if song.pokey_ids.len() > 1 { "s" } else { "" },
            clock_or(song.vgm.pokey_clock, crate::pokey::POKEY_DEFAULT_CLOCK),
            song.pokey_ins.len()
        ));
    }
    if !song.huc6280_ids.is_empty() || !song.huc6280_pcm_samples.is_empty() {
        lines.push(format!(
            "PCE notes   {}  ({} chip{}, {} waveforms, {} Hz)",
            song.huc6280_count,
            song.huc6280_ids.len(),
            if song.huc6280_ids.len() > 1 { "s" } else { "" },
            song.huc6280_waves.len(),
            clock_or(song.vgm.huc6280_clock, 3_579_545)
        ));
    }
    if !song.huc6280_pcm_samples.is_empty() {
        lines.push(format!(
            "PCE PCM     {} note(s) in {} 8-bit sample(s)",
            song.huc6280_pcm_count,
            song.huc6280_pcm_samples.len()
        ));
    }
    if !song.gb_ids.is_empty() {
        let sw = song.gb_ins.iter().filter(|i| i.gb_soft_env).count();
        lines.push(format!(
            "GB notes    {}  ({} chip{}, {} waveforms, {} instruments{}, {} Hz)",
            song.gb_count,
            song.gb_ids.len(),
            if song.gb_ids.len() > 1 { "s" } else { "" },
            song.gb_waves.len(),
            song.gb_ins.len(),
            if sw != 0 {
                format!(", {} soft-envelope", sw)
            } else {
                String::new()
            },
            clock_or(song.vgm.gb_dmg_clock, 4_194_304)
        ));
    }
    if !song.vsu_ids.is_empty() {
        lines.push(format!(
            "VSU notes   {}  ({} chip{}, {} waveforms, {} instruments{}, {} Hz)",
            song.vsu_count,
            song.vsu_ids.len(),
            if song.vsu_ids.len() > 1 { "s" } else { "" },
            song.vsu_waves.len(),
            song.vsu_ins.len(),
            if song.vsu_dynamic { ", dynamic waves" } else { "" },
            clock_or(song.vgm.vsu_clock, crate::vgm::VSU_DEFAULT_CLOCK)
        ));
    }
    if !song.nes_ids.is_empty() {
        lines.push(format!(
            "NES notes   {}  ({} APU{}, {} DPCM body(ies), {} Hz)",
            song.nes_count,
            song.nes_ids.len(),
            if song.nes_ids.len() > 1 { "s" } else { "" },
            song.nes_dpcm_samples.len(),
            clock_or(song.vgm.nes_clock, crate::nes::NES_DEFAULT_CLOCK)
        ));
    }
    if !song.ssg_pcm_samples.is_empty() {
        lines.push(format!(
            "SSG PCM     {} note(s) in {} 8-bit sample(s)",
            song.ssg_pcm_count,
            song.ssg_pcm_samples.len()
        ));
    }
    if !song.nes_pcm_samples.is_empty() {
        lines.push(format!(
            "NES PCM     {} note(s) in {} 8-bit sample(s)",
            song.nes_pcm_count,
            song.nes_pcm_samples.len()
        ));
    }
    if !song.pcm_samples.is_empty() {
        lines.push(format!(
            "PCM level   {}x of the YM2151 (rip mix)",
            fmt_g(song.k007232_volume, 4)
        ));
    }
    for (i, s) in song.pcm_samples.iter().enumerate() {
        if s.length() < 8 {
            continue;
        }
        lines.push(format!(
            "  {:02}  start {:05X}  len {:5}  hits {:3}  pitch {}  C-4 {} Hz",
            i,
            s.start,
            s.length(),
            s.hits.len(),
            s.mode_pitch(),
            s.c4_rate(clock_or(v.k007232_clock, 3_579_545))
        ));
    }
    if !song.oki_samples.is_empty() {
        let hits: usize = song.oki_samples.iter().map(|s| s.hits.len()).sum();
        let first = &song.oki_samples[0];
        let clock = first.clock;
        let pin = if first.pin7 { "pin7 high" } else { "pin7 low" };
        lines.push(format!(
            "OKI phrases {}  hits {}  {} Hz {}  {} Hz",
            song.oki_samples.len(),
            hits,
            clock,
            pin,
            first.rate
        ));
        for (i, s) in song.oki_samples.iter().enumerate() {
            lines.push(format!(
                "  {:02}  phrase {:02X}  len {:5}  hits {:3}  {}",
                i,
                s.phrase,
                s.length(),
                s.hits.len(),
                s.name
            ));
        }
    }
    if !song.msm6258_samples.is_empty() {
        lines.push(format!(
            "MSM6258     {} bursts",
            song.msm6258_samples.len()
        ));
    }
    push_region_lines(&mut lines, "SegaPCM", &song.segapcm_samples);
    push_region_lines(&mut lines, "RF5C68", &song.rf5c68_samples);
    push_region_lines(&mut lines, "RF5C164", &song.rf5c164_samples);
    push_region_lines(&mut lines, "C140", &song.c140_samples);
    push_region_lines(&mut lines, "C219", &song.c219_samples);
    push_region_lines(&mut lines, "C352", &song.c352_samples);
    push_region_lines(&mut lines, "K053260", &song.k053260_samples);
    push_region_lines(&mut lines, "K054539", &song.k054539_samples);
    push_region_lines(&mut lines, "YMZ280B", &song.ymz280b_samples);
    if !song.multipcm_samples.is_empty() {
        let hits: usize = song.multipcm_samples.iter().map(|s| s.hits.len()).sum();
        lines.push(format!(
            "MultiPCM    {} regions  hits {}",
            song.multipcm_samples.len(),
            hits
        ));
        for (i, s) in song.multipcm_samples.iter().enumerate() {
            let loop_text = match s.loop_start.zip(s.loop_end) {
                Some((start, end)) => format!("loop {}..{}", start, end),
                None => "one-shot".to_string(),
            };
            lines.push(format!(
                "  {:02}  idx {:3}  start {:05X}  len {:5}  {}-bit  hits {:3}  {}",
                i,
                s.index,
                s.start,
                s.length(),
                s.depth,
                s.hits.len(),
                loop_text
            ));
        }
    }
    if !song.upd7759_samples.is_empty() {
        let hits: usize = song.upd7759_samples.iter().map(|s| s.hits.len()).sum();
        lines.push(format!(
            "uPD7759     {} phrases  hits {}",
            song.upd7759_samples.len(),
            hits
        ));
        for (i, s) in song.upd7759_samples.iter().enumerate() {
            lines.push(format!(
                "  {:02}  {:<16}  frames {:6}  hits {:3}",
                i,
                s.name,
                s.frames(),
                s.hits.len()
            ));
        }
    }
    if !song.pwm_samples.is_empty() {
        lines.push(format!(
            "32X PWM     {} chunks  {} Hz",
            song.pwm_samples.len(),
            crate::pwm::PWM_RATE
        ));
        for s in &song.pwm_samples {
            lines.push(format!(
                "  {:<10}  frames {:7}  {}",
                s.name(),
                s.frames(),
                if s.loop_start.is_some() {
                    "loop"
                } else {
                    "one-shot"
                }
            ));
        }
    }
    if !song.msm5205_samples.is_empty() {
        lines.push(format!(
            "MSM5205     {} PCM chunks  {} note(s)",
            song.msm5205_samples.len(),
            song.msm5205_count
        ));
    }
    if !song.ga20_ids.is_empty() || !song.ga20_samples.is_empty() {
        lines.push(format!(
            "Irem GA20   {} chip(s)  {} note(s)  {} sample(s)",
            song.ga20_ids.len(),
            song.ga20_count,
            song.ga20_samples.len()
        ));
    }
    if !song.msm5232_ids.is_empty() {
        lines.push(format!(
            "OKI MSM5232 {} chip(s)  {} note(s)",
            song.msm5232_ids.len(),
            song.msm5232_count
        ));
    }
    if !song.mikey_ids.is_empty() || !song.mikey_ins.is_empty() || !song.mikey_samples.is_empty()
    {
        lines.push(format!(
            "Atari Lynx  {} chip(s)  {} note(s)  {} tone ins  {} DAC bodies",
            song.mikey_ids.len(),
            song.mikey_count,
            song.mikey_ins.len(),
            song.mikey_samples.len()
        ));
    }
    if !song.x1_010_ids.is_empty()
        || !song.x1_010_ins.is_empty()
        || !song.x1_010_samples.is_empty()
    {
        lines.push(format!(
            "Seta X1-010 {} chip(s)  {} note(s)  {} tone ins  {} PCM bodies  {} waves  {} envelopes",
            song.x1_010_ids.len(),
            song.x1_010_count,
            song.x1_010_ins.len(),
            song.x1_010_samples.len(),
            song.x1_waves.len(),
            song.x1_envs.len()
        ));
    }
    let last = match song.events.last() {
        Some(event) => event.sample,
        None => song.t0,
    };
    let rows = song.row_of(last) + 1;
    lines.push(format!(
        "rows        {}  (~{} orders of 256)",
        rows,
        (rows as f64 / 256.0).ceil() as i64
    ));
    for w in &song.warnings {
        lines.push(format!("warning     {}", w));
    }
    let mut out = lines.join("\n");
    out.push('\n');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fm::FMPatch;
    use crate::model::NoteEvent;
    use crate::vgm::VgmFile;
    use std::path::PathBuf;

    fn demo_vgm() -> VgmFile {
        let mut vgm = VgmFile::default();
        vgm.path = PathBuf::from("demo.vgm");
        vgm
    }

    fn base_song(vgm: VgmFile) -> Song {
        Song {
            vgm: vgm,
            t0: 0,
            loop_sample: None,
            speed: 1,
            hz: 60.0,
            fm_patches: Vec::new(),
            pcm_samples: Vec::new(),
            events: Vec::new(),
            warnings: Vec::new(),
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
            fm_chip: "ym2151".to_string(),
            fm_systems: Vec::new(),
            has_fm_source: true,
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
            fm_vol_max: 127,
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
            k007232_volume: 0.20,
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
            psg_dac_rate: crate::psg_dac::PSG_DAC_RATE,
            psg_dac_count: 0,
            psg_dac_stats: indexmap::IndexMap::new(),
            dac_samples: Vec::new(),
            dac_count: 0,
            dac_stats: Default::default(),
        }
    }

    #[test]
    fn empty_song_matches_reference_report() {
        let song = base_song(demo_vgm());
        let expected = concat!(
            "file        demo.vgm\n",
            "track       demo\n",
            "converter   vgm2151fur 0.11.0\n",
            "game        \n",
            "author      \n",
            "length      0.00s  (0 samples)\n",
            "t0          0 (0.000s) pattern origin\n",
            "loop        none\n",
            "grid        60 Hz, speed 1 -> 735.0 samples/row  (900.0 BPM 16ths)\n",
            "FM patches  0\n",
            "FM notes    0\n",
            "PCM notes   0  (0 sample regions)\n",
            "rows        1  (~1 orders of 256)\n",
        );
        assert_eq!(format_report(&song), expected);
    }

    #[test]
    fn fm_note_and_warning_match_reference_report() {
        let mut song = base_song(demo_vgm());
        let mut event = NoteEvent::default();
        event.sample = 735;
        event.ch = 2;
        event.on = true;
        event.note = 108;
        event.ins = 0;
        event.vol = 100;
        event.e5 = 0x80;
        event.pan = 0;
        song.events.push(event);
        song.warnings.push("test warning".to_string());
        let report = format_report(&song);
        assert!(report.contains("FM notes    1\n"));
        assert!(report.contains("rows        2  (~1 orders of 256)\n"));
        assert!(report.ends_with("warning     test warning\n"));
    }

    #[test]
    fn patch_line_prints_macros_like_python() {
        let mut song = base_song(demo_vgm());
        let mut patch = FMPatch::default();
        patch.alg = 4;
        patch.fb = 7;
        patch.uses = 12;
        patch.name = "Lead".to_string();
        patch.fms_macro = vec![1, 2];
        patch.macro_speed = 3;
        patch.op_tl_macro = vec![(0, vec![5, 6, 7], 4)];
        song.fm_patches.push(patch);
        let report = format_report(&song);
        assert!(
            report.contains(
                "  00  alg 4 fb 7  uses   12  carrierTL 7F  Lead  FMS [1, 2] @3t  op1 $4n x3@4t\n"
            ),
            "{}",
            report
        );
    }

    #[test]
    fn empty_report_uses_supplied_path_and_track() {
        let mut vgm = VgmFile::default();
        vgm.path = PathBuf::from("dir/other.vgm");
        vgm.gd3.insert("track".to_string(), "My Track".to_string());
        let song = base_song(vgm);
        let report = format_report(&song);
        assert!(report.starts_with("file        dir/other.vgm\ntrack       My Track\n"));
    }
}
