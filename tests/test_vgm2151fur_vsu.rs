//! Nintendo Virtual Boy VSU: parsing, envelopes, waves and .fur emission.
//!
//! Port of `tests/test_vgm2151fur_vsu.py`.

#![allow(non_snake_case)]

mod common;
use common::*;

use std::collections::BTreeMap;
use std::path::PathBuf;

use vgm2fur::analyze::{_vsu_sw_effect, analyze, AnalyzeOptions};
use vgm2fur::edit::read_chips;
use vgm2fur::furio::{iter_blocks, master_volume, parse_fur, FurModule};
use vgm2fur::furwrite::{write_fur, WriteOptions, CHIP_VBOY, INS_VBOY};
use vgm2fur::model::Song;
use vgm2fur::parity::py_round_to;
use vgm2fur::vgm::{chips_in_head, load_vgm, ChipWrite, VgmFile, VSU_DEFAULT_CLOCK};
use vgm2fur::vsu::{collect_vsu, vsu_note};

const CLOCK: u32 = VSU_DEFAULT_CLOCK;

fn tmp_dir() -> PathBuf {
    let p = std::env::temp_dir().join("vgm2151fur_vsu");
    std::fs::create_dir_all(&p).expect("temp dir");
    p
}

fn build_vgm(writes: Vec<ChipWrite>) -> VgmFile {
    let mut v = vgm(writes);
    v.version = 0x171;
    v.vsu_clock = CLOCK;
    v
}

fn w(t: i64, reg: u32, val: u32, chip: u8) -> ChipWrite {
    ChipWrite::new(t, "vsu", chip, reg & 0x7FFF, val & 0xFF)
}

fn wave(table: u32, vals: Option<&[u32]>) -> Vec<ChipWrite> {
    let default_vals: Vec<u32> = (0..32).map(|i: u32| (i * 2 + 1) & 0x3F).collect();
    let vals = vals.unwrap_or(&default_vals);
    vals.iter()
        .enumerate()
        .map(|(i, &v)| w(0, table * 0x20 + i as u32, v, 0))
        .collect()
}

#[derive(Clone, Copy)]
struct NoteArgs {
    t: i64,
    ch: u32,
    fnum: u32,
    vol: u32,
    pan: u32,
    off: Option<i64>,
    intl: u32,
    ev1: u32,
    ram: u32,
    chip: u8,
}

impl Default for NoteArgs {
    fn default() -> Self {
        NoteArgs {
            t: 0,
            ch: 0,
            fnum: 0x100,
            vol: 15,
            pan: 0x88,
            off: None,
            intl: 0x80,
            ev1: 0,
            ram: 0,
            chip: 0,
        }
    }
}

fn note(a: NoteArgs) -> Vec<ChipWrite> {
    let base = 0x100 + a.ch * 0x10;
    let mut out = vec![
        w(a.t, base + 6, a.ram, a.chip),
        w(a.t, base + 1, a.pan, a.chip),
        w(a.t, base + 2, a.fnum & 0xFF, a.chip),
        w(a.t, base + 3, (a.fnum >> 8) & 0x07, a.chip),
        w(a.t, base + 4, (a.vol & 0x0F) << 4, a.chip),
        w(a.t, base + 5, a.ev1, a.chip),
        w(a.t, base + 0, a.intl, a.chip),
    ];
    if let Some(off) = a.off {
        out.push(w(off, base + 0, 0, a.chip));
    }
    out
}

fn raw_vgm_vsu(cmds: &[u8], clock: u32, dual: bool) -> Vec<u8> {
    let mut buf = vec![0u8; 0x100];
    buf[0..4].copy_from_slice(b"Vgm ");
    buf[8..12].copy_from_slice(&0x171u32.to_le_bytes());
    buf[0x18..0x1C].copy_from_slice(&44100u32.to_le_bytes());
    buf[0x34..0x38].copy_from_slice(&(0x100u32 - 0x34).to_le_bytes());
    let raw = clock | if dual { 0x4000_0000 } else { 0 };
    buf[0xC4..0xC8].copy_from_slice(&raw.to_le_bytes());
    let mut out = buf;
    out.extend_from_slice(cmds);
    out.push(0x66);
    out
}

fn c7(reg: u32, val: u32) -> Vec<u8> {
    vec![0xC7, ((reg >> 8) & 0xFF) as u8, (reg & 0xFF) as u8, (val & 0xFF) as u8]
}

fn write_song(song: &Song) -> Vec<u8> {
    write_fur(
        song,
        WriteOptions {
            include_pcm: true,
            ..Default::default()
        },
    )
}

fn fx_counter(mod_: &FurModule) -> BTreeMap<u8, usize> {
    let mut fx: BTreeMap<u8, usize> = BTreeMap::new();
    for ch in 0..mod_.n_ch {
        for &oi in &mod_.orders[ch] {
            for row in mod_.patterns.get(&(ch as u16, oi)).into_iter().flatten() {
                for (cmd, _val) in &row.fx {
                    *fx.entry(*cmd).or_insert(0) += 1;
                }
            }
        }
    }
    fx
}

fn find_subslice(hay: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || hay.len() < needle.len() {
        return None;
    }
    hay.windows(needle.len()).position(|win| win == needle)
}

mod TestVsuParsing {
    use super::*;

    #[test]
    fn test_header_command_and_chip_kind() {
        let mut cmds = c7(0x100, 0x80);
        cmds.extend(c7(0x000, 0x3F));
        let raw = raw_vgm_vsu(&cmds, CLOCK, false);
        let p = tmp_dir().join("parse.vgm");
        std::fs::write(&p, &raw).expect("write");
        let v = load_vgm(&p).expect("load");
        assert_eq!(v.vsu_clock, CLOCK);
        assert!(!v.vsu_dual);
        let writes: Vec<(u32, u32)> = v
            .writes
            .iter()
            .filter(|wr| wr.chip == "vsu")
            .map(|wr| (wr.reg, wr.val))
            .collect();
        assert_eq!(writes, vec![(0x100, 0x80), (0x000, 0x3F)]);
        assert!(chips_in_head(&raw).contains("vsu"));
    }

    #[test]
    fn test_second_chip_bit() {
        let raw = raw_vgm_vsu(&c7(0x8100, 0x80), CLOCK, true);
        let p = tmp_dir().join("dual.vgm");
        std::fs::write(&p, &raw).expect("write");
        let v = load_vgm(&p).expect("load");
        assert!(v.vsu_dual);
        assert!(chips_in_head(&raw).contains("vsu_dual"));
        let wr = v.writes.iter().find(|wr| wr.chip == "vsu").expect("vsu write");
        assert_eq!(wr.chip_id, 1);
        assert_eq!(wr.reg, 0x100);
    }
}

mod TestVsuCollector {
    use super::*;

    #[test]
    fn test_key_note_pan_and_wave() {
        let mut writes = wave(0, None);
        writes.extend(note(NoteArgs {
            t: 1000,
            fnum: 0x123,
            vol: 12,
            pan: 0x13,
            off: Some(5000),
            ..Default::default()
        }));
        let (hits, _offs, waves, _mods, _dyn, warn) = collect_vsu(&build_vgm(writes));
        assert!(warn.is_empty());
        assert_eq!(hits.len(), 1);
        let h = &hits[0];
        assert_eq!(h.voice, 0);
        assert_eq!(h.period, 2048 - 0x123);
        assert_eq!(h.vol, 12);
        assert_eq!(h.pan, 0x13);
        assert_eq!(h.off_sample, 5000);
        assert_eq!(h.wave, 1); // zero image is 0, the written wave is 1
        let expected: Vec<u8> = (0..32).map(|i: u32| ((i * 2 + 1) & 0x3F) as u8).collect();
        assert_eq!(waves[h.wave as usize], expected);
        assert!(vsu_note(CLOCK, h.period).0 > 0);
    }

    #[test]
    fn test_volume_rise_starts_new_note() {
        let mut writes = wave(0, None);
        writes.extend(note(NoteArgs {
            t: 1000,
            vol: 15,
            off: None,
            ..Default::default()
        }));
        // A held driver: drop to 3, then re-attack at 15.
        writes.extend([w(3000, 0x104, 0x30, 0), w(5000, 0x104, 0xF0, 0)]);
        writes.extend([w(9000, 0x100, 0x00, 0)]);
        let (hits, offs, _waves, _mods, _dyn, _warn) = collect_vsu(&build_vgm(writes));
        assert_eq!(
            hits.iter().map(|h| h.sample_time).collect::<Vec<_>>(),
            vec![1000, 5000]
        );
        assert_eq!(hits[0].off_sample, 5000);
        assert_eq!(hits[1].off_sample, 9000);
        assert_eq!(hits[1].vol, 15);
        assert_eq!(offs.len(), 1);
    }

    #[test]
    fn test_hardware_envelope_steps() {
        let mut writes = wave(0, None);
        writes.extend(note(NoteArgs {
            t: 1000,
            vol: 2,
            ev1: 0x01,
            off: Some(4000),
            ..Default::default()
        }));
        // EV0=0x21: volume 2, speed 1, down.
        for wr in writes.iter_mut() {
            if wr.reg == 0x104 {
                wr.val = 0x21;
            }
        }
        let (hits, _offs, _waves, _mods, _dyn, _warn) = collect_vsu(&build_vgm(writes));
        let h = &hits[0];
        assert_eq!(h.vol, 2);
        // 76,800 clocks/tick at 5 MHz = 677.376 samples; speed 1 = 2 ticks.
        assert_eq!(
            h.vol_pts.iter().map(|(_t, v)| *v).collect::<Vec<_>>(),
            vec![1, 0]
        );
        assert!((h.vol_pts[0].0 as f64 - 2355.0).abs() <= 3.0);
        assert!((h.vol_pts[1].0 as f64 - 3709.0).abs() <= 3.0);
    }

    #[test]
    fn test_interval_auto_stop() {
        // 0xAC = key-on + interval enable, length 12 -> 13 interval ticks.
        let mut writes = wave(0, None);
        writes.extend(note(NoteArgs {
            t: 100,
            intl: 0xAC,
            ..Default::default()
        }));
        let (hits, offs, _waves, _mods, _dyn, _warn) = collect_vsu(&build_vgm(writes));
        assert_eq!(hits.len(), 1);
        // 19,200 clocks / 5 MHz * 44100 = 169.344 samples per tick.
        assert!((hits[0].off_sample as f64 - 2301.0).abs() <= 3.0);
        assert_eq!(offs[0].sample_time, hits[0].off_sample);
    }

    #[test]
    fn test_noise_tap_and_mid_change() {
        let mut writes = wave(0, None);
        writes.extend(note(NoteArgs {
            t: 1000,
            ch: 5,
            fnum: 0x700,
            vol: 10,
            pan: 0x08,
            ev1: 0x10,
            ..Default::default()
        }));
        writes.extend([w(3000, 0x155, 0x70, 0), w(5000, 0x150, 0x00, 0)]);
        let (hits, _offs, _waves, _mods, _dyn, _warn) = collect_vsu(&build_vgm(writes));
        let h = &hits[0];
        assert_eq!(h.voice, 5);
        assert_eq!(h.tap, 1);
        assert_eq!(h.tap_pts, vec![(3000, 7)]);
    }

    #[test]
    fn test_sweep_and_mod_effect_mapping() {
        assert_eq!(_vsu_sw_effect(0x45, 0x41), (0x13, 0x45));
        assert_eq!(_vsu_sw_effect(0x74, 0x70), (0x14, 0x37));
        assert_eq!(_vsu_sw_effect(0x10, 0x40), (0x13, 0x10));
        assert_eq!(_vsu_sw_effect(0x45, 0x00), (0x13, 0x00));
    }

    #[test]
    fn test_dynamic_wave_change() {
        let mut writes = wave(0, None);
        writes.extend(note(NoteArgs {
            t: 1000,
            vol: 15,
            ..Default::default()
        }));
        // Rewrite table 0 while the voice is playing.
        writes.extend((0..32).map(|i: u32| w(2000, i, 0x3F, 0)));
        writes.extend([w(4000, 0x100, 0x00, 0)]);
        let (hits, _offs, waves, _mods, dyn_, _warn) = collect_vsu(&build_vgm(writes));
        assert!(dyn_);
        assert!(waves.len() >= 2);
        assert!(!hits[0].wave_pts.is_empty());
    }
}

mod TestVsuModule {
    use super::*;

    fn module(writes: Vec<ChipWrite>) -> (Song, Vec<u8>, FurModule) {
        let song = analyze(&build_vgm(writes), AnalyzeOptions::default()).expect("analyze");
        let data = write_song(&song);
        let mod_ = parse_fur(&data).expect("parse");
        (song, data, mod_)
    }

    #[test]
    fn test_vsu_only_layout_and_effects() {
        let mut writes = wave(0, None);
        writes.extend(note(NoteArgs {
            t: 1000,
            vol: 15,
            pan: 0x13,
            off: Some(5000),
            ..Default::default()
        }));
        writes.extend(note(NoteArgs {
            t: 6000,
            ch: 5,
            fnum: 0x700,
            vol: 10,
            pan: 0x08,
            ev1: 0x10,
            off: Some(9000),
            ..Default::default()
        }));
        let (_song, data, mod_) = module(writes);
        assert_eq!(mod_.chips, vec![CHIP_VBOY]);
        assert_eq!(mod_.n_ch, 6);
        let types: Vec<i32> = iter_blocks(&data)
            .expect("blocks")
            .into_iter()
            .filter(|b| b.magic == *b"INS2")
            .map(|b| u16::from_le_bytes([b.payload[2], b.payload[3]]) as i32)
            .collect();
        assert!(!types.is_empty());
        assert!(types.iter().all(|t| *t == INS_VBOY));
        let wave_blocks = iter_blocks(&data)
            .expect("blocks")
            .into_iter()
            .filter(|b| b.magic == *b"WAVE")
            .count();
        // zero image + the written wave
        assert_eq!(wave_blocks, 2);
        let fx = fx_counter(&mod_);
        assert!(*fx.get(&0x11).unwrap_or(&0) > 0); // noise tap
        assert!(*fx.get(&0x08).unwrap_or(&0) > 0); // pan
    }

    #[test]
    fn test_short_noise_gate_keeps_a_safe_cut_beside_its_delay() {
        // A noise hit shorter than a pattern row must keep its cut. The VSU
        // counts ECxx from the delayed key-on, so a short gate is ECxx much
        // smaller than the note's EDxx. (The SN76489/T6W28 path is different
        // and requires ECxx > EDxx, covered in test_vgm2151fur_t6w28.)
        let mut writes = wave(0, None);
        writes.extend(note(NoteArgs {
            t: 66330,
            ch: 5,
            fnum: 0x700,
            vol: 15,
            pan: 0x88,
            ev1: 0,
            off: Some(67065),
            ..Default::default()
        }));
        let song = analyze(
            &build_vgm(writes),
            AnalyzeOptions {
                speed: Some(12),
                tick_samples: Some(44100.0 / 480.0),
                ..Default::default()
            },
        )
        .expect("analyze");
        let mod_ = parse_fur(&write_song(&song)).expect("parse");
        assert_eq!(song.vsu_ids, vec![0]);
        let mut found: Option<BTreeMap<u8, u8>> = None;
        for &oi in &mod_.orders[5] {
            if let Some(rows) = mod_.patterns.get(&(5u16, oi)) {
                if let Some(at) = rows.iter().position(|row| (0..180).contains(&row.note)) {
                    found = Some(rows[at].fx.iter().copied().collect());
                    break;
                }
            }
        }
        let fx = found.expect("the noise note vanished");
        assert!(fx.contains_key(&0xED));
        assert!(fx.contains_key(&0xEC));
        assert!(*fx.get(&0xEC).unwrap_or(&0) >= 1);
        assert!(*fx.get(&0xEC).unwrap_or(&0) < *fx.get(&0xED).unwrap_or(&0));
    }

    #[test]
    fn test_static_module_layout_and_master_volume() {
        let mut writes = wave(0, None);
        writes.extend(note(NoteArgs {
            t: 1000,
            vol: 15,
            off: Some(5000),
            ..Default::default()
        }));
        writes.extend(note(NoteArgs {
            t: 6000,
            ch: 5,
            fnum: 0x700,
            vol: 10,
            pan: 0x08,
            ev1: 0x10,
            off: Some(9000),
            ..Default::default()
        }));
        let song = analyze(&build_vgm(writes), AnalyzeOptions::default()).expect("analyze");
        assert_eq!(song.vsu_ids, vec![0]);
        assert!(!song.vsu_dynamic);
        assert!(song.vsu_waves.len() >= 2);
        assert!(song.vsu_ins.len() <= 250);
        let data = write_song(&song);
        let mod_ = parse_fur(&data).expect("parse");
        assert_eq!(mod_.chips, vec![CHIP_VBOY]);
        assert_eq!(mod_.n_ch, 6);
        assert_almost_eq!(master_volume(&data).expect("master"), 2.2, 2);
        let vols: Vec<f64> = read_chips(&data)
            .expect("chips")
            .iter()
            .map(|c| py_round_to(c.volume, 3))
            .collect();
        assert_eq!(vols, vec![1.0]);
    }

    #[test]
    fn test_channel4_sweep_effect_is_emitted() {
        let mut writes = wave(0, None);
        writes.push(w(1000, 0x147, 0x45, 0));
        writes.extend(note(NoteArgs {
            t: 1000,
            ch: 4,
            fnum: 0x200,
            vol: 12,
            ev1: 0x40,
            off: Some(5000),
            ..Default::default()
        }));
        let (_song, _data, mod_) = module(writes);
        let fx = fx_counter(&mod_);
        assert!(*fx.get(&0x13).unwrap_or(&0) > 0); // channel-4 sweep
        assert_eq!(*fx.get(&0x14).unwrap_or(&0), 0);
    }

    #[test]
    fn test_channel4_modulation_effect_and_fd_table() {
        let mut table: Vec<u8> = vec![0x00, 0xA7, 0x1D];
        table.extend(3u8..32);
        let mut writes: Vec<ChipWrite> = table
            .iter()
            .enumerate()
            .map(|(i, &b)| w(0, 0xA0 + i as u32, b as u32, 0))
            .collect();
        writes.extend(wave(0, None));
        writes.push(w(1000, 0x147, 0x74, 0));
        writes.extend(note(NoteArgs {
            t: 1000,
            ch: 4,
            fnum: 0x200,
            vol: 12,
            ev1: 0x50,
            off: Some(5000),
            ..Default::default()
        }));
        let (_song, data, mod_) = module(writes);
        let fx = fx_counter(&mod_);
        assert!(*fx.get(&0x14).unwrap_or(&0) > 0); // modulation
        // The FD feature carries the source's 32-byte modulation table.
        let mut tables: Vec<Vec<u8>> = Vec::new();
        for blk in iter_blocks(&data).expect("blocks") {
            if blk.magic != *b"INS2" {
                continue;
            }
            let blk = blk.payload;
            let mut p = 4usize;
            while p + 4 <= blk.len() {
                let code = &blk[p..p + 2];
                p += 2;
                if code == b"EN" {
                    break;
                }
                let size = u16::from_le_bytes([blk[p], blk[p + 1]]) as usize;
                p += 2;
                if code == b"FD" && size >= 41 {
                    if p + 41 <= blk.len() {
                        tables.push(blk[p + 9..p + 41].to_vec());
                    }
                }
                p += size;
            }
        }
        assert!(tables.iter().any(|t| t[..3] == table[..3]));
    }

    #[test]
    fn test_dynamic_waves_emit_live_effects_and_screwthis() {
        let mut writes = wave(0, None);
        writes.extend(note(NoteArgs {
            t: 1000,
            vol: 15,
            ..Default::default()
        }));
        // Rewrite table 0 while the voice is playing.
        writes.extend((0..32).map(|i: u32| w(2000, i, 0x3F, 0)));
        writes.extend([w(4000, 0x100, 0x00, 0)]);
        let song = analyze(&build_vgm(writes), AnalyzeOptions::default()).expect("analyze");
        assert!(song.vsu_dynamic);
        assert!(song.vsu_ins.len() <= 250);
        assert!(song.vsu_waves.len() <= 256);
        let data = write_song(&song);
        let mod_ = parse_fur(&data).expect("parse");
        assert_eq!(mod_.chips, vec![CHIP_VBOY]);
        assert_eq!(mod_.n_ch, 6);
        let mut wave_fx = 0usize;
        for ch in 0..mod_.n_ch {
            for &oi in &mod_.orders[ch] {
                for row in mod_.patterns.get(&(ch as u16, oi)).into_iter().flatten() {
                    wave_fx += row.fx.iter().filter(|(cmd, _v)| *cmd == 0x10).count();
                }
            }
        }
        assert!(wave_fx > 0);
        assert!(find_subslice(&data, b"screwThis=true").is_some());
    }
}

#[test]
fn zz_debug_dump() {
    let mut writes = wave(0, None);
    writes.extend(note(NoteArgs { t: 1000, vol: 15, pan: 0x13, off: Some(5000), ..Default::default() }));
    writes.extend(note(NoteArgs { t: 6000, ch: 5, fnum: 0x700, vol: 10, pan: 0x08, ev1: 0x10, off: Some(9000), ..Default::default() }));
    let song = analyze(&build_vgm(writes), AnalyzeOptions::default()).expect("analyze");
    eprintln!("vsu_ids={:?} dyn={} ins={} waves={} events={}", song.vsu_ids, song.vsu_dynamic, song.vsu_ins.len(), song.vsu_waves.len(), song.events.len());
    for e in &song.events {
        eprintln!("ev ch={} on={} note={} ins={} vol={} pan={} tone_chip={} fx={:?}", e.ch, e.on, e.note, e.ins, e.vol, e.pan, e.tone_chip, e.tone_fx_pts);
    }
    let data = write_song(&song);
    let mod_ = parse_fur(&data).expect("parse");
    for (k, rows) in &mod_.patterns {
        eprintln!("pat {:?} rows {} first {:?}", k, rows.len(), rows.iter().find(|r| r.note >= 0));
    }
    eprintln!("fx={:?}", fx_counter(&mod_));
}
