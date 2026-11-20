//! C219 and K054539 parsing, collection and .fur emission.
//!
//! Port of `tests/test_vgm2151fur_c219_k054539.py`.

mod common;
#[allow(unused_imports)]
use common::*;

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use vgm2fur::analyze::{analyze, AnalyzeOptions};
use vgm2fur::c219::{
    c219_furnace_length, c219_layout_warnings, c219_rate, collect_c219, layout_c219_samples,
    C219Hit, C219Sample,
};
use vgm2fur::es5503::collect_es5503;
use vgm2fur::furio::{decompress_fur, iter_blocks, parse_fur};
use vgm2fur::furwrite::{
    c219_flag_text, write_fur, WriteOptions, CHIP_C219, CHIP_GA20, CHIP_MSM5232, CHIP_PCM_DAC,
};
use vgm2fur::ga20::collect_ga20;
use vgm2fur::k054539::{
    collect_k054539, k054539_pan_fx, k054539_volume_fx, FORMAT_16BIT, FORMAT_8BIT,
    FORMAT_DPCM,
};
use vgm2fur::msm5205::collect_msm5205;
use vgm2fur::msm5232::{
    collect_msm5232, msm5232_attack_fx, msm5232_decay_fx, FURNACE_MSM5232_ATTACK_MAP,
    FURNACE_MSM5232_DECAY_MAP,
};
use vgm2fur::vgm::{load_vgm, ChipWrite, RomSlice, VgmFile};

fn _write(t: i64, chip: &'static str, reg: u32, val: u32, cid: u8) -> ChipWrite {
    ChipWrite::new(t, chip, cid, reg, val)
}

fn write(t: i64, chip: &'static str, reg: u32, val: u32) -> ChipWrite {
    _write(t, chip, reg, val, 0)
}

fn _vgm(writes: Vec<ChipWrite>, total: i64, roms: Vec<RomSlice>) -> VgmFile {
    let mut v = VgmFile::default();
    v.path = PathBuf::from("synthetic.vgm");
    v.buf = Vec::new();
    v.version = 0x171;
    v.total_samples = total;
    v.loop_samples = 0;
    v.loop_file_pos = None;
    v.loop_sample = None;
    v.ym2151_clock = 0;
    v.k007232_clock = 0;
    v.gd3 = Default::default();
    v.writes = writes;
    v.roms = roms;
    v
}

fn put_u32(buf: &mut [u8], off: usize, value: u32) {
    buf[off..off + 4].copy_from_slice(&value.to_le_bytes());
}

fn _raw_vgm(body: &[u8], clock_off: usize, clock: u32, version: u32, extra: &[u8]) -> Vec<u8> {
    let mut buf = vec![0u8; 0x100 + body.len() + 1];
    buf[..4].copy_from_slice(b"Vgm ");
    put_u32(&mut buf, 0x08, version);
    put_u32(&mut buf, 0x18, 44100);
    put_u32(&mut buf, 0x34, (0x100 - 0x34) as u32);
    put_u32(&mut buf, clock_off, clock);
    if !extra.is_empty() {
        buf[0x95] = extra[0];
    }
    buf[0x100..0x100 + body.len()].copy_from_slice(body);
    buf[0x100 + body.len()] = 0x66;
    let total = (buf.len() - 4) as u32;
    put_u32(&mut buf, 0x04, total);
    buf
}

fn _pair_swap(data: &[u8]) -> Vec<u8> {
    let mut out = vec![0u8; data.len() & !1];
    for i in (0..out.len()).step_by(2) {
        out[i] = data[i + 1];
        out[i + 1] = data[i];
    }
    out
}

fn _c219_writes(mode: u32, compressed: bool, noise: bool) -> (Vec<ChipWrite>, Vec<RomSlice>) {
    let body: Vec<u8> = if compressed {
        vec![0x00, 0x12, 0x34, 0x56]
    } else {
        vec![1, 2, 3, 4, 5, 6, 7, 8]
    };
    let rom = vec![RomSlice {
        chip: 0,
        rom_size: body.len() as i64,
        start: 0,
        data: _pair_swap(&body),
        kind: "c219",
        time: 0,
    }];
    let writes = vec![
        write(0, "c219", 0x1F7, 0), // bank 0 for voices 0-3
        write(10, "c219", 0x06, 0x00),
        write(10, "c219", 0x07, 0x00), // start word 0
        write(10, "c219", 0x08, 0x00),
        write(10, "c219", 0x09, if compressed { 0x02 } else { 0x04 }),
        write(10, "c219", 0x0A, 0x00),
        write(10, "c219", 0x0B, if compressed { 0x01 } else { 0x02 }),
        write(10, "c219", 0x02, 0xFF),
        write(10, "c219", 0x03, 0xFF), // freq 0xFFFF
        write(10, "c219", 0x00, 0xC8),
        write(10, "c219", 0x01, 0x64), // R 200, L 100
        write(
            20,
            "c219",
            0x05,
            mode | (if compressed { 0x01 } else { 0 }) | (if noise { 0x04 } else { 0 }),
        ),
        write(900, "c219", 0x05, 0x10), // key off
    ];
    (writes, rom)
}

/// Local copy of vgm2151fur.c219._c219_furnace_addresses, which the Python
/// test imports but the Rust module keeps private.
fn c219_furnace_addresses(samples: &[C219Sample]) -> (Vec<(i32, i32, usize, usize)>, usize) {
    let mut out: Vec<(i32, i32, usize, usize)> = Vec::new();
    let mut mem_pos = 0usize;
    for smp in samples {
        let length = c219_furnace_length(smp);
        if (mem_pos & 0xFE0000) != ((mem_pos + length) & 0xFE0000) {
            mem_pos = (mem_pos + 0x1FFFF) & 0xFE0000;
        }
        let sample_off = mem_pos >> 1;
        out.push((
            ((sample_off >> 16) & 3) as i32,
            (sample_off & 0xFFFF) as i32,
            mem_pos,
            length,
        ));
        mem_pos += length;
    }
    (out, mem_pos)
}

fn k054539_writes(fmt: i32, loop_: bool) -> (Vec<ChipWrite>, Vec<RomSlice>) {
    let body: Vec<u8> = if fmt == FORMAT_8BIT {
        vec![0x12, 0x34, 0x56, 0x78, 0x80, 0xAA]
    } else if fmt == FORMAT_16BIT {
        let mut b = Vec::new();
        for value in [0x1234i16, -0x1234, 0x2345, -0x2345] {
            b.extend_from_slice(&value.to_le_bytes());
        }
        b.extend_from_slice(b"\x00\x80");
        b
    } else {
        vec![0x12, 0x34, 0x88, 0xAA]
    };
    let rom = vec![RomSlice {
        chip: 0,
        rom_size: body.len() as i64,
        start: 0,
        data: body,
        kind: "k054539",
        time: 0,
    }];
    let base: u32 = 0;
    let writes = vec![
        write(0, "k054539", 0x22F, 0x01), // PCM enable
        write(
            0,
            "k054539",
            0x200,
            ((fmt as u32) << 2) | (if loop_ { 0x01 } else { 0 }),
        ),
        write(0, "k054539", 0x201, if loop_ { 0x01 } else { 0 }),
        write(0, "k054539", base + 0x00, 0x00),
        write(0, "k054539", base + 0x01, 0x40),
        write(0, "k054539", base + 0x02, 0x00), // delta 0x4000
        write(0, "k054539", base + 0x03, 0x00),
        write(0, "k054539", base + 0x04, 0x00),
        write(0, "k054539", base + 0x05, 0x18),
        write(0, "k054539", base + 0x08, 0x00),
        write(0, "k054539", base + 0x09, 0x00),
        write(0, "k054539", base + 0x0A, 0x00),
        write(0, "k054539", base + 0x0C, 0x00),
        write(0, "k054539", base + 0x0D, 0x00),
        write(0, "k054539", base + 0x0E, 0x00),
        write(10, "k054539", 0x214, 0x01),
        write(900, "k054539", 0x215, 0x01),
    ];
    (writes, rom)
}

fn tmp_dir() -> PathBuf {
    let p = std::env::temp_dir().join("vgm2151fur_c219_k054539");
    std::fs::create_dir_all(&p).expect("temp dir");
    p
}

fn parse_file(raw: &[u8], name: &str) -> VgmFile {
    let p = temp_file(&tmp_dir(), name, raw);
    parse_path(&p)
}

fn parse_path(p: &Path) -> VgmFile {
    load_vgm(p).expect("load vgm")
}

#[allow(non_snake_case)]
mod TestC219 {
    use super::*;

    #[test]
    fn test_c219_parse_and_rom() {
        let body = [0xD4, 0x00, 0x06, 0x80, 0xD4, 0x00, 0x05, 0x90];
        let mut payload = Vec::new();
        payload.extend_from_slice(&0x100u32.to_le_bytes());
        payload.extend_from_slice(&0u32.to_le_bytes());
        payload.extend_from_slice(b"\x01\x02");
        let mut vgm_body = vec![0x67, 0x66, 0x8D];
        vgm_body.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        vgm_body.extend_from_slice(&payload);
        vgm_body.extend_from_slice(&body);
        let mut raw = _raw_vgm(&vgm_body, 0xA8, 25_401_600, 0x171, b"");
        // Type byte 2 selects C219 on the shared 0xD4 command.
        raw[0x96] = 0x02;
        let v = parse_file(&raw, "c219.vgm");
        assert_eq!(v.c140_type, 2);
        assert_eq!(v.c140_clock, 25_401_600);
        assert_eq!(
            v.writes.iter().map(|w| w.chip).collect::<Vec<_>>(),
            vec!["c219", "c219"]
        );
        assert_eq!(
            v.roms.iter().map(|r| r.kind).collect::<Vec<_>>(),
            vec!["c219"]
        );
    }

    #[test]
    fn test_c219_sample_and_note() {
        let (writes, rom) = _c219_writes(0x90, false, false);
        let mut song = _vgm(writes, 1000, rom);
        song.c140_clock = 25_401_600;
        song.c140_type = 2;
        let (samples, offs, _warn) = collect_c219(&song);
        assert_eq!(samples.len(), 1);
        let smp = &samples[0];
        assert_eq!(smp.data, vec![1, 2, 3, 4, 5, 6, 7, 8]);
        assert_eq!(smp.loop_start, Some(4));
        assert_eq!(smp.loop_end, Some(8));
        assert_eq!(smp.source_bank, 0);
        assert_eq!(offs.len(), 1);
        let expected = c219_rate(25_401_600, 0xFFFF) * 0.998051;
        let actual = smp.c4_rate(25_401_600) as f64;
        assert!(
            (actual - expected).abs() <= 2.0,
            "assertAlmostEqual: {actual} != {expected} (delta 2)"
        );
    }

    #[test]
    fn test_c219_layout_keeps_real_bodies_only() {
        let first = C219Sample {
            start: 0,
            data: vec![0x01, 0x02],
            source_bank: 2,
            depth: 8,
            ..Default::default()
        };
        let second = C219Sample {
            start: 10,
            data: vec![0x03, 0x04],
            source_bank: 1,
            depth: 8,
            ..Default::default()
        };
        let laid = layout_c219_samples(&[first.clone(), second]);
        assert_eq!(
            laid.iter().map(|s| s.source_bank).collect::<Vec<_>>(),
            vec![1, 2]
        );
        assert_eq!(laid.len(), 2);
        // The first real body carries the trailing silence that pushes the
        // second source bank to Furnace bank 1; no extra sample is added.
        assert!(laid[0].data.len() > first.data.len());
        let (addresses, _mem) = c219_furnace_addresses(&laid);
        assert_eq!(
            addresses.iter().map(|a| a.0).collect::<Vec<_>>(),
            vec![0, 1]
        );
    }

    #[test]
    fn test_c219_odd_gap_does_not_split_source_bank() {
        // An odd extended donor used to end exactly on the 128 KB boundary,
        // so Furnace moved the donor itself to the next bank and split the
        // source group across two banks.
        let donor = C219Sample {
            start: 0,
            data: vec![0x01, 0x02],
            source_bank: 3,
            depth: 8,
            source_length: 2,
            ..Default::default()
        };
        let mut looping_data = Vec::new();
        for _ in 0..300 {
            looping_data.extend_from_slice(b"\x03\x04");
        }
        let looping = C219Sample {
            start: 10,
            data: looping_data,
            source_bank: 3,
            depth: 8,
            loop_start: Some(2),
            loop_end: Some(600),
            source_length: 600,
            ..Default::default()
        };
        let other = C219Sample {
            start: 20,
            data: vec![0x05, 0x06],
            source_bank: 2,
            depth: 8,
            source_length: 2,
            ..Default::default()
        };
        let laid = layout_c219_samples(&[donor, looping, other]);
        let (addresses, _mem) = c219_furnace_addresses(&laid);
        let mut by_source: BTreeMap<i32, BTreeSet<i32>> = BTreeMap::new();
        for (smp, address) in laid.iter().zip(addresses.iter()) {
            by_source
                .entry(smp.source_bank)
                .or_default()
                .insert(address.0);
        }
        assert_eq!(by_source.get(&3), Some(&BTreeSet::from([0])));
        assert_eq!(by_source.get(&2), Some(&BTreeSet::from([1])));
        assert!(!c219_layout_warnings(&laid)
            .iter()
            .any(|warning| warning.contains("spans Furnace banks")));
    }

    #[test]
    fn test_c219_group_bank_switch_is_reported() {
        fn hit(voice: i32, when: i64) -> C219Hit {
            C219Hit {
                sample_time: when,
                chip_id: 0,
                voice,
                start: 0,
                end: 1,
                loop_: 0,
                loop_on: false,
                bank: 0,
                freq: 0x100,
                vol_l: 255,
                vol_r: 255,
                off_sample: when + 500,
                ..Default::default()
            }
        }

        let mut low = C219Sample {
            start: 0,
            data: vec![0x01, 0x02],
            source_bank: 1,
            depth: 8,
            source_length: 2,
            ..Default::default()
        };
        let mut high = C219Sample {
            start: 10,
            data: vec![0x03, 0x04],
            source_bank: 2,
            depth: 8,
            source_length: 2,
            ..Default::default()
        };
        low.hits.push(hit(0, 0));
        high.hits.push(hit(1, 100));
        let warnings = c219_layout_warnings(&layout_c219_samples(&[low, high]));
        assert!(
            warnings
                .iter()
                .any(|warning| warning.contains("voice group 0 switches")),
            "{warnings:?}"
        );
    }

    #[test]
    fn test_c219_compressed_and_noise() {
        let (writes, rom) = _c219_writes(0x80, true, true);
        let mut song = _vgm(writes, 1000, rom);
        song.c140_clock = 25_401_600;
        song.c140_type = 2;
        let (samples, _offs, _warn) = collect_c219(&song);
        assert_eq!(samples.len(), 1);
        assert_eq!(samples[0].depth, 8); // noise ignores the ROM body
        assert!(samples[0].noise);

        let (writes, rom) = _c219_writes(0x80, true, false);
        let mut song = _vgm(writes, 1000, rom);
        song.c140_clock = 25_401_600;
        song.c140_type = 2;
        let (samples, _offs, _warn) = collect_c219(&song);
        assert_eq!(samples[0].depth, 8);
        assert_eq!(samples[0].data, vec![0, 2, 20, 54]);
    }

    #[test]
    fn test_c219_fur() {
        let (writes, rom) = _c219_writes(0x80 | 0x40 | 0x08, false, false);
        let mut song = _vgm(writes, 1000, rom);
        song.c140_clock = 25_401_600;
        song.c140_type = 2;
        let song = analyze(&song, AnalyzeOptions::default()).expect("analyze");
        assert_eq!(song.c219_samples.len(), 1);
        assert_eq!(song.c219_count, 1);
        let mod_ = parse_fur(&write_fur(&song, WriteOptions::default())).expect("parse fur");
        assert_eq!(mod_.chips, vec![CHIP_C219]);
        assert_eq!(mod_.n_ch, 16);
        assert_eq!(mod_.master_vol, 1.0);
        assert_eq!(mod_.samples.len(), 1);
        assert_eq!(mod_.samples[0].depth, 8);
        assert_eq!(mod_.ins_sample[0], 0);
        let notes: Vec<_> = mod_
            .patterns
            .values()
            .flat_map(|rows| rows.iter())
            .filter(|row| row.note != -1 && row.note != 180)
            .collect();
        assert_eq!(notes.len(), 1);
        assert_eq!(
            notes[0]
                .fx
                .iter()
                .find(|(fx, _v)| *fx == 0x12)
                .map(|(_fx, v)| *v),
            Some(0x11)
        );
    }

    #[test]
    fn test_c219_flag_text() {
        let flags = c219_flag_text(25_401_600);
        assert!(flags.contains("customClock=100000000"), "{flags}");
    }
}

#[allow(non_snake_case)]
mod TestK054539 {
    use super::*;

    #[test]
    fn test_k054539_parse() {
        let mut payload = Vec::new();
        payload.extend_from_slice(&0x1000u32.to_le_bytes());
        payload.extend_from_slice(&0u32.to_le_bytes());
        payload.extend_from_slice(b"\x12\x34");
        let mut body = vec![0x67, 0x66, 0x8C];
        body.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        body.extend_from_slice(&payload);
        body.extend_from_slice(&[0xD3, 0x02, 0x14, 0x01]);
        let raw = _raw_vgm(&body, 0xA0, 18_432_000, 0x171, b"");
        let v = parse_file(&raw, "k054539.vgm");
        assert_eq!(v.k054539_clock, 18_432_000);
        assert_eq!(v.writes[0].chip, "k054539");
        assert_eq!(v.writes[0].reg, 0x214);
        assert_eq!(v.roms[0].kind, "k054539");
    }

    #[test]
    fn test_k054539_bodies() {
        for (fmt, depth, frames) in [
            (FORMAT_8BIT, 8, 4),
            (FORMAT_16BIT, 16, 4),
            (FORMAT_DPCM, 16, 4),
        ] {
            let (writes, rom) = k054539_writes(fmt, false);
            let mut song = _vgm(writes, 1000, rom);
            song.k054539_clock = 18_432_000;
            let (samples, offs, _warn) = collect_k054539(&song);
            assert_eq!(samples.len(), 1, "fmt {fmt}");
            let smp = &samples[0];
            assert_eq!(smp.depth, depth, "fmt {fmt}");
            assert_eq!(smp.length(), frames, "fmt {fmt}");
            assert_eq!(offs.len(), 1);
            if fmt == FORMAT_DPCM {
                let mut expected = Vec::new();
                for value in [0x200i16, 0x300, 0xB00, 0xF00] {
                    expected.extend_from_slice(&value.to_le_bytes());
                }
                assert_eq!(smp.data, expected);
            }
        }
    }

    #[test]
    fn test_k054539_immediate_keyoff_is_silent() {
        // Some logs begin with a full key-on/key-off mask pair to reset every
        // voice. Both writes land on the same VGM sample, so no audio plays.
        let (mut writes, rom) = k054539_writes(FORMAT_8BIT, false);
        let last = writes.len() - 1;
        writes[last] = write(10, "k054539", 0x215, 0x01);
        let mut song = _vgm(writes, 1000, rom);
        song.k054539_clock = 18_432_000;
        let (samples, _offs, _warn) = collect_k054539(&song);
        assert!(samples.is_empty());
        let analysed = analyze(&song, AnalyzeOptions::default()).expect("analyze");
        assert_eq!(analysed.k054539_count, 0);
        assert!(analysed.k054539_samples.is_empty());
    }

    #[test]
    fn test_k054539_loop_and_value_maps() {
        let (writes, rom) = k054539_writes(FORMAT_8BIT, true);
        let mut song = _vgm(writes, 1000, rom);
        song.k054539_clock = 18_432_000;
        let (samples, _offs, _warn) = collect_k054539(&song);
        let smp = &samples[0];
        assert_eq!(
            (smp.loop_start, smp.loop_end),
            (Some(0), Some(4))
        );
        assert_eq!(k054539_volume_fx(0), 255);
        assert_eq!(k054539_volume_fx(0x40), 4);
        assert_eq!(k054539_pan_fx(0x18), 0xBB);
        assert_eq!(k054539_pan_fx(0x11), 0x0F);
        assert_eq!(k054539_pan_fx(0x1F), 0xF0);
    }

    #[test]
    fn test_k054539_fur() {
        let (writes, rom) = k054539_writes(FORMAT_8BIT, false);
        let mut song = _vgm(writes, 1000, rom);
        song.k054539_clock = 18_432_000;
        let song = analyze(&song, AnalyzeOptions::default()).expect("analyze");
        assert_eq!(song.k054539_count, 1);
        let mod_ = parse_fur(&write_fur(&song, WriteOptions::default())).expect("parse fur");
        assert_eq!(mod_.chips.iter().filter(|&&c| c == CHIP_PCM_DAC).count(), 8);
        assert_eq!(mod_.n_ch, 8); // one DAC system per hardware voice
        assert_eq!(mod_.master_vol, 1.0);
        // Every one-voice DAC system stays at unity too. The decoded body
        // already carries the K054539 level; 0.25 per system was 4x too quiet.
        let raw = decompress_fur(&write_fur(&song, WriteOptions::default()));
        for block in iter_blocks(&raw).expect("iter blocks") {
            if block.magic == *b"INFO" || block.magic == *b"INF2" {
                let chips = &block.payload[24..56];
                let vols = &block.payload[56..88];
                assert!(
                    chips
                        .iter()
                        .enumerate()
                        .filter(|(_i, c)| **c == CHIP_PCM_DAC)
                        .all(|(i, _c)| vols[i] == 64)
                );
                break;
            }
        }
        assert_eq!(mod_.samples.len(), 1);
        assert_eq!(mod_.samples[0].depth, 8);
        let notes: Vec<_> = mod_
            .patterns
            .values()
            .flat_map(|rows| rows.iter())
            .filter(|row| row.note != -1 && row.note != 180)
            .collect();
        assert_eq!(notes.len(), 1);
        assert!(notes[0].fx.iter().any(|(fx, _v)| *fx == 0x08));
    }
}

#[allow(non_snake_case)]
mod TestGa20Msm5205Msm5232 {
    use super::*;

    #[test]
    fn test_ga20_body_and_fur() {
        let rom = vec![RomSlice {
            chip: 0,
            rom_size: 4,
            start: 0,
            data: vec![0x80, 0x90, 0xA0, 0x00],
            kind: "ga20",
            time: 0,
        }];
        let writes = vec![
            write(0, "ga20", 0x06, 0x00),
            write(0, "ga20", 0x07, 0x00),
            write(0, "ga20", 0x02, 0x00),
            write(0, "ga20", 0x03, 0x00),
            write(0, "ga20", 0x04, 0x80),
            write(0, "ga20", 0x05, 0xC0),
            write(10, "ga20", 0x06, 0x02),  // key on
            write(900, "ga20", 0x06, 0x00), // key off
        ];
        let mut song = _vgm(writes, 1000, rom);
        song.ga20_clock = 3_579_545;
        let (samples, offs, _warn) = collect_ga20(&song);
        assert_eq!(samples.len(), 1);
        // ROM 0x80/0x90/0xA0 becomes signed 0x00/0x10/0x20; zero terminates.
        assert_eq!(samples[0].data, vec![0x00, 0x10, 0x20]);
        assert_eq!(offs.len(), 1);

        let analysed = analyze(&song, AnalyzeOptions::default()).expect("analyze");
        assert_eq!(analysed.ga20_count, 1);
        let mod_ = parse_fur(&write_fur(&analysed, WriteOptions::default())).expect("parse fur");
        assert!(mod_.chips.contains(&CHIP_GA20));
        assert_eq!(mod_.samples.len(), 1);
        assert_eq!(mod_.samples[0].depth, 8);
        let notes: Vec<_> = mod_
            .patterns
            .values()
            .flat_map(|rows| rows.iter())
            .filter(|row| row.note != -1 && row.note != 180)
            .collect();
        assert_eq!(notes.len(), 1);
    }

    #[test]
    fn test_msm5232_note_and_group_fx() {
        let mut writes = vec![
            write(0, "msm5232", 0x0C, 0x0F),
            write(0, "msm5232", 0x08, 0x03),
            write(0, "msm5232", 0x0A, 0x05),
            write(10, "msm5232", 0x00, 0xA1), // key on pitch 0x21
            write(900, "msm5232", 0x00, 0x00), // key off
        ];
        writes.push(write(500, "msm5232", 0x1E, 0x08));
        let mut song = _vgm(writes, 1000, Vec::new());
        song.msm5232_clock = 2_000_000;
        let (hits, offs, fx, vols, _warn) = collect_msm5232(&song.writes);
        assert_eq!(hits.len(), 1);
        assert_eq!(vols.len(), 1);
        assert_eq!(vols[0].value, 8);
        assert_eq!(hits[0].pitch, 0x21);
        assert!(!hits[0].noise);
        assert_eq!(offs.len(), 1);
        assert!(fx.iter().any(|e| e.code == 0x12 && e.value == 3));
        // Furnace maps effect 8 back to hardware decay register 5.
        assert!(fx.iter().any(|e| e.code == 0x13 && e.value == 8));

        let analysed = analyze(&song, AnalyzeOptions::default()).expect("analyze");
        assert_eq!(analysed.msm5232_count, 1);
        let mod_ = parse_fur(&write_fur(&analysed, WriteOptions::default())).expect("parse fur");
        assert!(mod_.chips.contains(&CHIP_MSM5232));
        assert_eq!(mod_.n_ch, 8);
        let notes: Vec<_> = mod_
            .patterns
            .values()
            .flat_map(|rows| rows.iter())
            .filter(|row| row.note != -1 && row.note != 180)
            .collect();
        assert_eq!(notes.len(), 1);
        assert_eq!(notes[0].note, 72 + 0x21);
    }

    #[test]
    fn test_msm5232_envelope_maps_reach_the_source_registers() {
        // Furnace runs 12xx/13xx through its own attack/decay tables before
        // writing the chip, so the effect values must be the inverses.
        let writes = vec![
            write(0, "msm5232", 0x08, 0x08), // attack 8 -> hardware 0
            write(0, "msm5232", 0x09, 0x0F), // attack 7 -> 5 (fastest)
            write(0, "msm5232", 0x0A, 0x0A), // decay 10
            write(0, "msm5232", 0x0B, 0x05), // decay 5
            write(10, "msm5232", 0x00, 0xA1),
            write(20, "msm5232", 0x0B, 0x05), // same value: no new effect
        ];
        let mut song = _vgm(writes, 1000, Vec::new());
        song.msm5232_clock = 2_000_000;
        let (_hits, _offs, fx, _vols, _warn) = collect_msm5232(&song.writes);
        let attacks: Vec<(i32, i32)> = fx
            .iter()
            .filter(|e| e.code == 0x12)
            .map(|e| (e.group, e.value))
            .collect();
        let decays: Vec<(i32, i32)> = fx
            .iter()
            .filter(|e| e.code == 0x13)
            .map(|e| (e.group, e.value))
            .collect();
        assert!(attacks.contains(&(0, 0)));
        assert!(attacks.contains(&(1, 5)));
        assert!(decays.contains(&(0, 7))); // decayMap[7] == 10
        assert!(decays.contains(&(1, 8))); // decayMap[8] == 5
        assert_eq!(decays.iter().filter(|&&d| d == (1, 8)).count(), 1);

        for register in 0..16 {
            let effect = msm5232_decay_fx(register);
            if FURNACE_MSM5232_DECAY_MAP.contains(&register) {
                assert_eq!(
                    FURNACE_MSM5232_DECAY_MAP[effect as usize], register
                );
            }
        }
        for register in 0..16 {
            let effect = msm5232_attack_fx(register);
            assert_eq!(
                FURNACE_MSM5232_ATTACK_MAP[effect as usize],
                (register & 7).min(5)
            );
        }
    }

    #[test]
    fn test_msm5205_streams_to_pcm_dac() {
        let mut writes = vec![
            write(0, "msm5205", 0, 1), // reset
            write(0, "msm5205", 4, 2), // S2 prescaler
            write(0, "msm5205", 5, 1), // 4-bit
        ];
        for i in 0..40 {
            writes.push(write(i + 1, "msm5205", 1, (i & 0x0F) as u32));
        }
        writes.push(write(100, "msm5205", 0, 0)); // release
        let mut song = _vgm(writes, 2000, Vec::new());
        song.msm5205_clock = 384_000;
        song.msm5205_flags = 0x06;
        let (samples, _warn) = collect_msm5205(&song, false);
        assert!(!samples.is_empty());
        assert!(samples.iter().all(|s| s.rate == 44100));
        let mod_ = parse_fur(&write_fur(
            &analyze(&song, AnalyzeOptions::default()).expect("analyze"),
            WriteOptions::default(),
        ))
        .expect("parse fur");
        assert!(mod_.chips.contains(&CHIP_PCM_DAC));
        assert_eq!(mod_.n_ch, 1);
        assert!(mod_.samples.iter().all(|x| x.depth == 16));
    }
}

#[allow(non_snake_case)]
mod TestEs5503 {
    use super::*;

    fn _rom(body: &[u8]) -> Vec<RomSlice> {
        vec![RomSlice {
            chip: 0,
            rom_size: 0x20000,
            start: 0,
            data: body.to_vec(),
            kind: "es5503",
            time: 0,
        }]
    }

    fn _writes(control: u32) -> Vec<ChipWrite> {
        vec![
            write(0, "es5503", 0xE1, 62),
            write(0, "es5503", 0x00, 0x00),
            write(0, "es5503", 0x20, 0x04), // freq 0x400
            write(0, "es5503", 0x40, 0xFF), // volume 255
            write(0, "es5503", 0x80, 0x00), // wave pointer 0
            write(0, "es5503", 0xC0, 0x00), // 256 bytes, resolution 0
            write(10, "es5503", 0xA0, control),
            write(900, "es5503", 0xA0, 0x03),
        ]
    }

    #[test]
    fn test_es5503_body_and_note() {
        // Unsigned 0xFF/0x01 becomes signed +127/-127; no 0x00 to stop it.
        let mut rom = vec![0xFF; 128];
        rom.extend(vec![0x01; 128]);
        let mut song = _vgm(_writes(0x02), 1000, _rom(&rom));
        song.es5503_clock = 7_159_090;
        song.es5503_channels = 2;
        let (samples, offs, _warn) = collect_es5503(&song);
        assert_eq!(samples.len(), 1);
        assert_eq!(samples[0].length(), 256);
        assert_eq!(&samples[0].data[..2], b"\x7f\x7f");
        assert_eq!(&samples[0].data[samples[0].data.len() - 2..], b"\x81\x81");
        assert_eq!(samples[0].c4_rate, 52_640);
        assert_eq!(offs.len(), 1);

        let analysed = analyze(&song, AnalyzeOptions::default()).expect("analyze");
        assert_eq!(analysed.es5503_count, 1);
        let notes: Vec<_> = analysed
            .events
            .iter()
            .filter(|e| e.on && e.pcm)
            .collect();
        assert_eq!(notes.len(), 1);
        assert_eq!(notes[0].note, 108);
        assert_eq!(notes[0].vol, 255);
        assert_eq!(notes[0].pan, 0xF0);
        assert_eq!(notes[0].ins, 0);
        assert_eq!(notes[0].ch, 8);

        let mod_ = parse_fur(&write_fur(&analysed, WriteOptions::default())).expect("parse fur");
        assert_eq!(mod_.chips.iter().filter(|&&c| c == CHIP_PCM_DAC).count(), 1);
        assert_eq!(mod_.n_ch, 1);
        assert_eq!(mod_.samples.len(), 1);
        assert_eq!(mod_.samples[0].depth, 8);
        assert_eq!(mod_.samples[0].n_frames, 256);
        let rows: Vec<_> = mod_
            .patterns
            .values()
            .flat_map(|rs| rs.iter())
            .filter(|row| row.note != -1 && row.note != 180)
            .collect();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].note, 108);
        assert_eq!(rows[0].vol, 255);
        assert!(rows[0]
            .fx
            .iter()
            .any(|&(fx, value)| fx == 0x08 && value == 0xF0));
    }

    #[test]
    fn test_es5503_pan_uses_second_output() {
        let mut rom = vec![0xFF; 128];
        rom.extend(vec![0x01; 128]);
        let mut song = _vgm(_writes(0x12), 1000, _rom(&rom));
        song.es5503_clock = 7_159_090;
        song.es5503_channels = 2;
        let analysed = analyze(&song, AnalyzeOptions::default()).expect("analyze");
        let note = analysed
            .events
            .iter()
            .find(|e| e.on && e.pcm)
            .expect("pcm note");
        assert_eq!(note.pan, 0x0F);
        let mod_ = parse_fur(&write_fur(&analysed, WriteOptions::default())).expect("parse fur");
        let row = mod_
            .patterns
            .values()
            .flat_map(|rs| rs.iter())
            .find(|row| row.note != -1 && row.note != 180)
            .expect("note row");
        assert!(row
            .fx
            .iter()
            .any(|&(fx, value)| fx == 0x08 && value == 0x0F));
    }

    #[test]
    fn test_es5503_body_stops_at_zero_and_empty_drops() {
        let mut song = _vgm(
            _writes(0x02),
            1000,
            _rom(&[0x90, 0x91, 0x00, 0x92]),
        );
        song.es5503_clock = 7_159_090;
        song.es5503_channels = 2;
        let (samples, _offs, _warn) = collect_es5503(&song);
        assert_eq!(samples.len(), 1);
        assert_eq!(samples[0].data, vec![0x10, 0x11]);

        let mut silent = _vgm(
            _writes(0x02),
            1000,
            _rom(&[0x00, 0x90, 0x91]),
        );
        silent.es5503_clock = 7_159_090;
        silent.es5503_channels = 2;
        let (samples, _offs, warn) = collect_es5503(&silent);
        assert!(samples.is_empty());
        assert!(warn.iter().any(|text| text.contains("empty")));
    }

    #[test]
    fn test_es5503_raw_vgm_header_and_ram_block() {
        let rom = [0x80, 0x81, 0x82];
        let mut payload = Vec::new();
        payload.extend_from_slice(&0u32.to_le_bytes());
        payload.extend_from_slice(&rom);
        let mut body = vec![0x67, 0x66, 0xE1];
        body.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        body.extend_from_slice(&payload);
        body.extend_from_slice(&[0xD5, 0x00, 0xE1, 62, 0xD5, 0x00, 0x40, 0x20]);
        let mut raw = _raw_vgm(&body, 0xCC, 7_159_090, 0x171, b"");
        raw[0xD4] = 0x02;
        let v = parse_file(&raw, "es5503.vgm");
        assert_eq!(v.es5503_clock, 7_159_090);
        assert_eq!(v.es5503_channels, 2);
        assert_eq!(
            v.writes.iter().map(|w| w.chip).collect::<Vec<_>>(),
            vec!["es5503", "es5503"]
        );
        assert_eq!(
            v.writes.iter().map(|w| (w.reg, w.val)).collect::<Vec<_>>(),
            vec![(0xE1, 62), (0x40, 0x20)]
        );
        assert_eq!(
            v.roms.iter().map(|r| r.kind).collect::<Vec<_>>(),
            vec!["es5503"]
        );
        assert_eq!(v.roms[0].start, 0);
        assert_eq!(v.roms[0].data, rom.to_vec());
        assert_eq!(v.roms[0].rom_size, 0x20000);
    }
}
