//! YM2610/YM2610B support: parsing, pitch, ADPCM collection and .fur output.
//!
//! Port of `tests/test_vgm2151fur_ym2610.py` at vgm2151fur a8edb6e.

mod common;
#[allow(unused_imports)]
use common::*;

use std::collections::BTreeSet;
use std::path::PathBuf;

use vgm2fur::analyze::{_adpcm_b_motion, analyze, AnalyzeOptions};
use vgm2fur::audit::compare_ym2610_samples;
use vgm2fur::diag::{ym2610_keyed_intervals, ym2610_pitch_stream};
use vgm2fur::furio::{iter_blocks, parse_fur, parse_smp2_payload, CHIP_CHANNELS};
use vgm2fur::furwrite::{
    parse_fur_skeleton, write_fur, WriteOptions, CHIP_YM2610, CHIP_YM2610B, INS_ADPCMA,
    INS_ADPCMB, INS_FM, NOTE_OFF,
};
use vgm2fur::furnace::export_vgm;
use vgm2fur::vgm::{assemble_rom, chips_in_head, load_vgm, ChipWrite, RomSlice, VgmFile};
use vgm2fur::ym2610::{
    collect_ym2610_adpcm, ym2610_note_fnum, ym2610_note_for, AdpcmBHit, YM2610State,
    CHAN_OFF_4, CHAN_OFF_6, KON_4, KON_6, YM2610_DEFAULT_CLOCK,
};
use vgm2fur::ym2612::ym2612_pitch_units;

const CLOCK: u32 = YM2610_DEFAULT_CLOCK;

fn tmp_dir() -> PathBuf {
    let p = std::env::temp_dir().join("vgm2151fur_ym2610");
    std::fs::create_dir_all(&p).expect("create temp dir");
    p
}

fn tmp() -> PathBuf {
    tmp_dir()
}

fn _raw_vgm(body: &[u8], clock: u32, b: bool, dual: bool) -> Vec<u8> {
    let mut buf = vec![0u8; 0xC0];
    buf[0..4].copy_from_slice(b"Vgm ");
    buf[8..12].copy_from_slice(&0x161u32.to_le_bytes());
    buf[0x18..0x1C].copy_from_slice(&44100u32.to_le_bytes());
    buf[0x34..0x38].copy_from_slice(&(0xC0u32 - 0x34).to_le_bytes());
    let mut raw = clock & 0x3FFF_FFFF;
    if b {
        raw |= 0x8000_0000;
    }
    if dual {
        raw |= 0x4000_0000;
    }
    buf[0x4C..0x50].copy_from_slice(&raw.to_le_bytes());
    let mut out = buf;
    out.extend_from_slice(body);
    out.push(0x66);
    let len = (out.len() as u32 - 4).to_le_bytes();
    out[4..8].copy_from_slice(&len);
    out
}

fn _rom_block(data: &[u8], typ: u8, start: u32) -> Vec<u8> {
    let size = 8 + data.len() as u32;
    let mut out = vec![0x67, 0x66, typ];
    out.extend_from_slice(&size.to_le_bytes());
    out.extend_from_slice(&(data.len() as u32).to_le_bytes());
    out.extend_from_slice(&start.to_le_bytes());
    out.extend_from_slice(data);
    out
}

fn _vgm(
    writes: Vec<ChipWrite>,
    rom: Option<Vec<u8>>,
    b: bool,
    total_samples: i64,
) -> VgmFile {
    let roms = match rom {
        Some(rom) => vec![RomSlice {
            chip: 0,
            rom_size: rom.len() as i64,
            start: 0,
            data: rom,
            kind: "ym2610_adpcm",
            time: 0,
        }],
        None => Vec::new(),
    };
    let mut writes = writes;
    writes.sort_by_key(|w| w.sample);
    VgmFile {
        path: PathBuf::from("synthetic.vgm"),
        buf: Vec::new(),
        version: 0x161,
        total_samples,
        loop_samples: 0,
        loop_file_pos: None,
        loop_sample: None,
        ym2151_clock: 0,
        k007232_clock: 0,
        gd3: Default::default(),
        roms,
        writes,
        ym2610_clock: CLOCK,
        ym2610_b: b,
        ..Default::default()
    }
}

#[allow(clippy::too_many_arguments)]
fn _fm_note(
    ch: usize,
    b: bool,
    block: i32,
    fnum: i32,
    t: i64,
    off_arg: i64,
    cid: u8,
) -> Vec<ChipWrite> {
    let _ = off_arg;
    let offsets: &[(usize, usize)] = if b { &CHAN_OFF_6 } else { &CHAN_OFF_4 };
    let kon_map: &[(i32, i32)] = if b { &KON_6 } else { &KON_4 };
    let (bank, off) = offsets[ch];
    let base: u32 = if bank != 0 { 0x100 } else { 0 };
    let mut out: Vec<ChipWrite> = Vec::new();
    for page in [0x30u32, 0x40, 0x50, 0x60, 0x70, 0x80, 0x90] {
        for op in 0..4u32 {
            let val = match page {
                0x30 => 0x01,
                0x50 => 0x1F,
                _ => 0,
            };
            out.push(ChipWrite::new(
                t,
                "ym2610",
                cid,
                base | page | (op << 2) | off as u32,
                val,
            ));
        }
    }
    out.push(ChipWrite::new(t, "ym2610", cid, base | 0xB0 | off as u32, 0x07));
    out.push(ChipWrite::new(t, "ym2610", cid, base | 0xB4 | off as u32, 0xC0));
    out.push(ChipWrite::new(
        t,
        "ym2610",
        cid,
        base | 0xA4 | off as u32,
        ((block << 3) | ((fnum >> 8) & 7)) as u32,
    ));
    out.push(ChipWrite::new(
        t,
        "ym2610",
        cid,
        base | 0xA0 | off as u32,
        (fnum & 0xFF) as u32,
    ));
    let kon = kon_map.iter().find(|(_, v)| *v == ch as i32).unwrap().0 as u32;
    out.push(ChipWrite::new(t, "ym2610", cid, 0x28, 0xF0 | kon));
    if off != 0 {
        out.push(ChipWrite::new(off as i64, "ym2610", cid, 0x28, kon));
    }
    out
}

fn _adpcm_writes(rom_len: i32, stop: i64, a_stop: i64) -> Vec<ChipWrite> {
    let _ = rom_len;
    vec![
        // ADPCM-A channel 1: region 0x000..0x1ff, gate level 0 then 8.
        ChipWrite::new(0, "ym2610", 0, 0x101, 0x3F),
        ChipWrite::new(100, "ym2610", 0, 0x110, 0x00),
        ChipWrite::new(100, "ym2610", 0, 0x118, 0x00),
        ChipWrite::new(100, "ym2610", 0, 0x120, 0x01),
        ChipWrite::new(100, "ym2610", 0, 0x128, 0x00),
        ChipWrite::new(100, "ym2610", 0, 0x108, 0xC0),
        ChipWrite::new(100, "ym2610", 0, 0x100, 0x01),
        ChipWrite::new(108, "ym2610", 0, 0x108, 0xC8),
        // Mid-note level and pan changes, then a restore.
        ChipWrite::new(900, "ym2610", 0, 0x108, 0x00),
        ChipWrite::new(1600, "ym2610", 0, 0x108, 0x28),
        ChipWrite::new(1700, "ym2610", 0, 0x108, 0xC8),
        ChipWrite::new(a_stop, "ym2610", 0, 0x100, 0x81),
        // ADPCM-B: region 0x200..0x3ff, 50% rate, pan set after start.
        ChipWrite::new(2000, "ym2610", 0, 0x12, 0x02),
        ChipWrite::new(2000, "ym2610", 0, 0x13, 0x00),
        ChipWrite::new(2000, "ym2610", 0, 0x14, 0x03),
        ChipWrite::new(2000, "ym2610", 0, 0x15, 0x00),
        ChipWrite::new(2000, "ym2610", 0, 0x11, 0x00),
        ChipWrite::new(2000, "ym2610", 0, 0x1B, 0x00),
        ChipWrite::new(2000, "ym2610", 0, 0x10, 0x80),
        ChipWrite::new(2002, "ym2610", 0, 0x11, 0xC0),
        ChipWrite::new(2002, "ym2610", 0, 0x1B, 0x80),
        ChipWrite::new(2013, "ym2610", 0, 0x19, 0x00),
        ChipWrite::new(2015, "ym2610", 0, 0x1A, 0x80),
        // Mid-note level and rate changes while the one-shot plays.
        ChipWrite::new(3000, "ym2610", 0, 0x1B, 0x40),
        ChipWrite::new(3300, "ym2610", 0, 0x19, 0x00),
        ChipWrite::new(3301, "ym2610", 0, 0x1A, 0x40),
        ChipWrite::new(stop, "ym2610", 0, 0x10, 0x00),
    ]
}

fn adpcm_writes_default() -> Vec<ChipWrite> {
    _adpcm_writes(1024, 3000, 1100)
}

fn analyze_song(vgm: &VgmFile) -> vgm2fur::model::Song {
    analyze(vgm, AnalyzeOptions::default()).expect("analyze")
}

fn fur_bytes(song: &vgm2fur::model::Song) -> Vec<u8> {
    write_fur(song, WriteOptions::default())
}

mod TestYm2610Parsing {
    use super::*;

    #[test]
    fn test_header_commands_and_ssg_split() {
        let body: &[u8] = &[
            0x58, 0x30, 0x01, // port 0 FM operator
            0x58, 0x08, 0x0F, // port 0 SSG volume
            0x59, 0x10, 0x02, // port 1 ADPCM-A start low
            0xA8, 0x40, 0x03, // second chip, port 0
            0xA9, 0x11, 0x04, // second chip, port 1
        ];
        let p = tmp().join("ym2610_dual.vgm");
        std::fs::write(&p, _raw_vgm(body, CLOCK, true, true)).unwrap();
        let vgm = load_vgm(&p).unwrap();
        assert_eq!(vgm.ym2610_clock, CLOCK);
        assert!(vgm.ym2610_b);
        assert!(vgm.ym2610_dual);
        let got: Vec<(&str, u8, u32, u32)> = vgm
            .writes
            .iter()
            .map(|w| (w.chip, w.chip_id, w.reg, w.val))
            .collect();
        assert_eq!(
            got,
            vec![
                ("ym2610", 0, 0x030, 0x01),
                ("ym2610_ssg", 0, 0x008, 0x0F),
                ("ym2610", 0, 0x110, 0x02),
                ("ym2610", 1, 0x040, 0x03),
                ("ym2610", 1, 0x111, 0x04),
            ]
        );
    }

    #[test]
    fn test_chips_in_head() {
        assert!(chips_in_head(&_raw_vgm(&[0x66], CLOCK, false, false)).contains("ym2610"));
        let kinds = chips_in_head(&_raw_vgm(&[0x66], CLOCK, true, false));
        assert!(kinds.contains("ym2610"));
        assert!(kinds.contains("ym2610_b"));
        assert!(!kinds.contains("ym2151"));
    }

    #[test]
    fn test_adpcm_rom_block() {
        let rom: Vec<u8> = (0..256u32).map(|i| i as u8).collect::<Vec<_>>().repeat(2);
        let p = tmp().join("ym2610_rom.vgm");
        std::fs::write(&p, _raw_vgm(&_rom_block(&rom, 0x82, 0), CLOCK, false, false)).unwrap();
        let vgm = load_vgm(&p).unwrap();
        let slices: Vec<&RomSlice> = vgm
            .roms
            .iter()
            .filter(|r| r.kind == "ym2610_adpcm")
            .collect();
        assert_eq!(slices.len(), 1);
        assert_eq!(slices[0].data, rom);
    }

    #[test]
    fn test_missing_clock_warning() {
        let p = tmp().join("ym2610_noclock.vgm");
        std::fs::write(&p, _raw_vgm(&[0x58, 0x30, 0x01], 0, false, false)).unwrap();
        let vgm = load_vgm(&p).unwrap();
        assert_eq!(vgm.ym2610_clock, 0);
        assert!(vgm.warnings.iter().any(|w| w.contains("YM2610 clock")));
    }
}

mod TestYm2610Pitch {
    use super::*;

    #[test]
    fn test_inverse_round_trip() {
        for note in [36, 48, 60, 72, 96, 120] {
            let (block, fnum) = ym2610_note_fnum(note, CLOCK, 0);
            let (note2, e5) = ym2610_note_for(block, fnum, CLOCK);
            let (block2, fnum2) = ym2610_note_fnum(note2, CLOCK, e5);
            assert!(
                (ym2612_pitch_units(block2, fnum2) - ym2612_pitch_units(block, fnum)).abs()
                    <= 1.0,
                "note {note}"
            );
        }
    }

    #[test]
    fn test_channel_offsets() {
        let st4 = YM2610State::new(false);
        assert_eq!(st4.channel_of(0x031), Some(0));
        assert_eq!(st4.channel_of(0x032), Some(1));
        assert_eq!(st4.channel_of(0x131), Some(2));
        assert_eq!(st4.channel_of(0x132), Some(3));
        assert_eq!(st4.channel_of(0x030), None);
        assert_eq!(st4.channel_of(0x133), None);
        let st6 = YM2610State::new(true);
        assert_eq!(st6.channel_of(0x030), Some(0));
        assert_eq!(st6.channel_of(0x032), Some(2));
        assert_eq!(st6.channel_of(0x130), Some(3));
        assert_eq!(st6.channel_of(0x132), Some(5));
    }
}

mod TestYm2610Adpcm {
    use super::*;

    #[test]
    fn test_collect_a_and_b() {
        let rom: Vec<u8> = (0..1024i32).map(|i| (i & 0xFF) as u8).collect();
        let vgm = _vgm(_adpcm_writes(1024, 4000, 2500), Some(rom.clone()), false, 8000);
        let (a, b, warnings) = collect_ym2610_adpcm(&vgm, &rom, None);
        assert!(warnings.is_empty());
        assert_eq!(a.len(), 1);
        assert_eq!((a[0].start, a[0].end), (0x000, 0x1FF));
        assert_eq!(a[0].data, rom[0..0x200].to_vec());
        let hit = &a[0].hits[0];
        assert_eq!(hit.vol, 8); // 0xC8 written after the gate
        assert_eq!(hit.pan, 0xFF);
        assert_eq!(hit.end_time, 2500);
        assert_eq!(hit.vol_pts, vec![(900, 0), (1600, 8)]);
        assert_eq!(hit.pan_pts, vec![(900, 0x00), (1700, 0xFF)]);
        assert_eq!(b.len(), 1);
        assert_eq!((b[0].start, b[0].end, b[0].rate), (0x200, 0x3FF, 0x8000));
        assert_eq!(b[0].data, rom[0x200..0x400].to_vec());
        let bhit = &b[0].hits[0];
        assert_eq!(bhit.vol, 0x80);
        assert_eq!(bhit.pan, 0xFF);
        assert_eq!(bhit.end_time, 4000);
        assert_eq!(bhit.vol_pts, vec![(3000, 0x40)]);
        assert_eq!(bhit.rate_pts, vec![(3301, 0x4000)]);
    }

    #[test]
    fn test_silent_pan_gate_skipped() {
        let rom = vec![0u8; 1024];
        let writes = vec![
            ChipWrite::new(0, "ym2610", 0, 0x101, 0x3F),
            ChipWrite::new(100, "ym2610", 0, 0x110, 0x00),
            ChipWrite::new(100, "ym2610", 0, 0x118, 0x00),
            ChipWrite::new(100, "ym2610", 0, 0x120, 0x01),
            ChipWrite::new(100, "ym2610", 0, 0x128, 0x00),
            ChipWrite::new(100, "ym2610", 0, 0x108, 0x00),
            ChipWrite::new(100, "ym2610", 0, 0x100, 0x01),
            ChipWrite::new(3000, "ym2610", 0, 0x100, 0x81),
        ];
        let song = analyze_song(&_vgm(writes, Some(rom), false, 3000));
        assert!(!song.events.iter().any(|e| e.on && e.pcm));
    }

    #[test]
    fn test_final_pan_off_stops_hit() {
        let rom = vec![0u8; 1024];
        let writes = vec![
            ChipWrite::new(0, "ym2610", 0, 0x101, 0x3F),
            ChipWrite::new(100, "ym2610", 0, 0x110, 0x00),
            ChipWrite::new(100, "ym2610", 0, 0x118, 0x00),
            ChipWrite::new(100, "ym2610", 0, 0x120, 0x01),
            ChipWrite::new(100, "ym2610", 0, 0x128, 0x00),
            ChipWrite::new(100, "ym2610", 0, 0x108, 0xC8),
            ChipWrite::new(100, "ym2610", 0, 0x100, 0x01),
            ChipWrite::new(900, "ym2610", 0, 0x108, 0x00),
            ChipWrite::new(3000, "ym2610", 0, 0x100, 0x81),
        ];
        let (a, _b, _w) = collect_ym2610_adpcm(&_vgm(writes, Some(rom.clone()), false, 8000), &rom, None);
        let hit = &a[0].hits[0];
        assert_eq!(hit.end_time, 900);
        assert_eq!(hit.pan_pts, Vec::<(i64, i32)>::new());
        assert_eq!(hit.vol_pts, Vec::<(i64, i32)>::new());
    }

    #[test]
    fn test_b_late_motion() {
        let mut hit = AdpcmBHit::new(100, 0, 0, 0x3FF, 0x8000, 0x80, 0xFF, false);
        hit.rate_pts = vec![(500, 0x4000)];
        hit.vol_pts = vec![(300, 0x40)];
        hit.pan_pts = vec![(400, 0x00)];
        let (steps, levels) = _adpcm_b_motion(&hit);
        assert_eq!(steps, vec![(500, -768)]);
        assert_eq!(levels, vec![(300, 0x40, Some(0xFF)), (400, 0, Some(0))]);
    }

    #[test]
    fn test_bank_b_falls_back_to_83() {
        let rom_a = vec![0u8; 1024];
        let rom_b: Vec<u8> = (0..1024i32).map(|i| (i & 0xFF) as u8).collect();
        let vgm = _vgm(adpcm_writes_default(), None, false, 8000);
        let (_a, b, _w) = collect_ym2610_adpcm(&vgm, &rom_a, Some(&rom_b));
        assert_eq!(b.len(), 1);
        assert_eq!(b[0].data, rom_b[0x200..0x400].to_vec());
        assert_eq!(b[0].rate, 0x8000);
    }

    #[test]
    fn test_sample_audit_reads_b_from_83() {
        let rom_a = vec![0u8; 1024];
        let rom_b: Vec<u8> = (0..1024i32).map(|i| ((i * 7) & 0xFF) as u8).collect();
        let mut src = _vgm(adpcm_writes_default(), Some(rom_a), false, 8000);
        src.roms.push(RomSlice {
            chip: 0,
            rom_size: rom_b.len() as i64,
            start: 0,
            data: rom_b.clone(),
            kind: "ym2610_deltat",
            time: 0,
        });
        let mut exp = _vgm(adpcm_writes_default(), None, false, 8000);
        exp.roms = vec![RomSlice {
            chip: 0,
            rom_size: rom_b.len() as i64,
            start: 0,
            data: rom_b,
            kind: "ym2610_deltat",
            time: 0,
        }];
        let samples = compare_ym2610_samples(&src, &exp);
        assert_eq!(samples["b"]["matched"].as_i64(), Some(1));
        assert_eq!(samples["b"]["data"].as_i64(), Some(0));
    }
}

mod TestYm2610Writer {
    use super::*;

    fn _song(b: bool) -> vgm2fur::model::Song {
        let mut writes = _fm_note(1, b, 4, 0x400, 100, 7000, 0);
        writes.extend([
            ChipWrite::new(200, "ym2610_ssg", 0, 0x00, 0x00),
            ChipWrite::new(200, "ym2610_ssg", 0, 0x01, 0x02),
            ChipWrite::new(200, "ym2610_ssg", 0, 0x08, 0x0F),
            ChipWrite::new(200, "ym2610_ssg", 0, 0x07, 0x3E),
        ]);
        writes.extend(adpcm_writes_default());
        let rom: Vec<u8> = (0..1024i32).map(|i| (i & 0xFF) as u8).collect();
        analyze_song(&_vgm(writes, Some(rom), b, 8000))
    }

    fn chip_channels(chip: u8) -> usize {
        CHIP_CHANNELS
            .iter()
            .find(|(c, _)| *c == chip)
            .map(|(_, n)| *n)
            .expect("chip in CHIP_CHANNELS")
    }

    #[test]
    fn test_layout_and_instruments() {
        let song = _song(true);
        let data = fur_bytes(&song);
        let sk = parse_fur_skeleton(&data);
        assert_eq!(sk.chips, vec![CHIP_YM2610B]);
        assert!(sk.notes >= 3);
        assert!(sk.offs >= 2);
        assert_eq!(chip_channels(CHIP_YM2610B), 16);
        let mut types: Vec<i32> = Vec::new();
        let mut depths: Vec<u8> = Vec::new();
        let mut presence: Vec<Vec<u8>> = Vec::new();
        for block in iter_blocks(&data).unwrap() {
            if &block.magic == b"INS2" && block.payload.len() >= 4 {
                types.push(
                    u16::from_le_bytes([block.payload[2], block.payload[3]]) as i32,
                );
            } else if &block.magic == b"SMP2" {
                let smp = parse_smp2_payload(block.payload).unwrap();
                depths.push(smp.depth);
                presence.push(smp.presence);
            }
        }
        assert!(types.contains(&INS_FM));
        assert!(types.contains(&INS_ADPCMA));
        assert!(types.contains(&INS_ADPCMB));
        depths.sort();
        assert_eq!(depths, vec![5, 6]);
        let masks: Vec<[u32; 4]> = presence
            .iter()
            .map(|p| {
                let mut m = [0u32; 4];
                for (i, slot) in m.iter_mut().enumerate() {
                    *slot = u32::from_le_bytes([
                        p[i * 4],
                        p[i * 4 + 1],
                        p[i * 4 + 2],
                        p[i * 4 + 3],
                    ]);
                }
                m
            })
            .collect();
        assert!(masks.iter().any(|m| m[0] & 1 != 0 && m[1] == 0));
        assert!(masks.iter().any(|m| m[1] & 1 != 0 && m[0] == 0));
    }

    #[test]
    fn test_plain_variant_layout() {
        let song = _song(false);
        let sk = parse_fur_skeleton(&fur_bytes(&song));
        assert_eq!(sk.chips, vec![CHIP_YM2610]);
        assert_eq!(chip_channels(CHIP_YM2610), 14);
    }

    #[test]
    fn test_adpcm_offs_written() {
        let song = analyze_song(&_vgm(
            adpcm_writes_default(),
            Some(vec![0u8; 1024]),
            false,
            3000,
        ));
        let m = parse_fur(&fur_bytes(&song)).unwrap();
        let mut offs: BTreeSet<u16> = BTreeSet::new();
        for ((ch, _idx), rows) in &m.patterns {
            for row in rows {
                if row.note == NOTE_OFF {
                    offs.insert(*ch);
                }
            }
        }
        assert!(offs.contains(&7)); // ADPCM-A channel 0
        assert!(offs.contains(&13)); // ADPCM-B
    }

    #[test]
    fn test_adpcm_loop_end_does_not_stop_a_sounding_sample() {
        // The B sample starts inside the loop and its source stop is past
        // the wrap. The player would never reach that stop before looping.
        // The module must not write an off at the seam either.
        let mut song = analyze_song(&_vgm(
            adpcm_writes_default(),
            Some(vec![0u8; 1024]),
            false,
            3000,
        ));
        song.loop_sample = Some(2000);
        song.loop_end_sample = Some(2900);
        song.loop_cut_sample = Some(2950);
        let m = parse_fur(&fur_bytes(&song)).unwrap();
        let offs: Vec<&vgm2fur::furio::FurRow> = m
            .patterns
            .iter()
            .filter(|((ch, _idx), _rows)| *ch == 13)
            .flat_map(|(_key, rows)| rows.iter())
            .filter(|row| row.note == NOTE_OFF)
            .collect();
        assert!(offs.is_empty());
    }

    #[test]
    fn test_adjusted_loop_cuts_the_source_tail() {
        // A trigger past the wrap belongs to the next iteration. Keeping it
        // would play it before the jump and then again after the jump.
        let mut song = analyze_song(&_vgm(
            adpcm_writes_default(),
            Some(vec![0u8; 1024]),
            false,
            3000,
        ));
        song.loop_sample = Some(1000);
        song.loop_end_sample = Some(1500);
        song.loop_cut_sample = Some(1800);
        let m = parse_fur(&fur_bytes(&song)).unwrap();
        let notes: Vec<i32> = m
            .patterns
            .iter()
            .filter(|((ch, _idx), _rows)| *ch == 13)
            .flat_map(|(_key, rows)| rows.iter())
            .map(|row| row.note)
            .filter(|note| (0..180).contains(note))
            .collect();
        assert!(!notes.contains(&60)); // B trigger at 2000, past the loop end
    }

    #[test]
    fn test_furnace_round_trip_motion() {
        skip_without_furnace!();
        let mut writes = _fm_note(1, true, 4, 0x400, 100, 7000, 0);
        writes.extend([
            ChipWrite::new(200, "ym2610_ssg", 0, 0x00, 0x00),
            ChipWrite::new(200, "ym2610_ssg", 0, 0x01, 0x02),
            ChipWrite::new(200, "ym2610_ssg", 0, 0x08, 0x0F),
            ChipWrite::new(200, "ym2610_ssg", 0, 0x07, 0x3E),
        ]);
        writes.extend(_adpcm_writes(1024, 4000, 2500));
        let rom: Vec<u8> = (0..1024i32).map(|i| (i & 0xFF) as u8).collect();
        let song = analyze_song(&_vgm(writes, Some(rom), true, 4000));
        let fur = tmp().join("ym2610_motion.fur");
        std::fs::write(
            &fur,
            write_fur(
                &song,
                WriteOptions {
                    compress: true,
                    ..Default::default()
                },
            ),
        )
        .unwrap();
        let out = tmp().join("ym2610_motion.vgm");
        export_vgm(&fur, &out).unwrap();
        let vgm = load_vgm(&out).unwrap();
        let (exp_a, exp_b, _w) = collect_ym2610_adpcm(
            &vgm,
            &assemble_rom(&vgm, "ym2610_adpcm", 0, 0, false, true),
            Some(&assemble_rom(&vgm, "ym2610_deltat", 0, 0, false, true)),
        );
        let ahit = exp_a.iter().flat_map(|s| s.hits.iter()).next().unwrap();
        assert!(ahit.vol_pts.iter().any(|(_t, v)| *v == 0));
        assert!(ahit.vol_pts.iter().any(|(_t, v)| *v == 8));
        let bhit = exp_b.iter().flat_map(|s| s.hits.iter()).next().unwrap();
        assert!(bhit.rate_pts.iter().any(|(_t, r)| *r == 0x4000));
        assert!(bhit.vol_pts.iter().any(|(_t, v)| *v == 0x40));
    }

    #[test]
    fn test_furnace_round_trip() {
        skip_without_furnace!();
        let song = _song(true);
        let fur = tmp().join("ym2610_roundtrip.fur");
        std::fs::write(
            &fur,
            write_fur(
                &song,
                WriteOptions {
                    compress: true,
                    ..Default::default()
                },
            ),
        )
        .unwrap();
        let out = tmp().join("ym2610_roundtrip.vgm");
        export_vgm(&fur, &out).unwrap();
        let vgm = load_vgm(&out).unwrap();
        assert!(vgm.ym2610_b);
        assert_eq!(vgm.ym2610_clock, CLOCK);
        // The FM voice and the SSG voice survive the export.
        assert!(!ym2610_pitch_stream(&vgm, 1).is_empty());
        assert!(!ym2610_keyed_intervals(&vgm, 1).is_empty());
        assert!(!ym2610_pitch_stream(&vgm, 6).is_empty());
        let samples = compare_ym2610_samples(&song.vgm, &vgm);
        assert_eq!(samples["a"]["matched"].as_i64(), Some(1));
        assert_eq!(samples["b"]["matched"].as_i64(), Some(1));
        assert_eq!(
            samples["a"]["data"].as_i64().unwrap()
                + samples["b"]["data"].as_i64().unwrap()
                + samples["a"]["vol"].as_i64().unwrap()
                + samples["b"]["vol"].as_i64().unwrap()
                + samples["b"]["rate"].as_i64().unwrap(),
            0
        );
        assert!(vgm.writes.iter().any(|w| {
            w.chip == "ym2610" && (w.reg & 0x100) == 0 && w.reg == 0x10 && (w.val & 1) != 0
        }));
        assert!(vgm.writes.iter().any(|w| {
            w.chip == "ym2610" && (w.reg & 0x1FF) == 0x100 && (w.val & 0x80) != 0
        }));
    }
}
