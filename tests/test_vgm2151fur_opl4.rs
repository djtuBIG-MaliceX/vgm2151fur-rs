//! Yamaha YMF278B (OPL4): OPL3-shaped FM plus 24 wave channels.
//!
//! Port of `tests/test_vgm2151fur_opl4.py`.

mod common;

use std::path::PathBuf;

use vgm2fur::analyze::analyze;
use vgm2fur::furio::{chip_channels, iter_blocks, parse_fur};
use vgm2fur::furwrite::{
    write_fur, WriteOptions, CHIP_OPL4, CHIP_OPL4_DRUMS, INS_MULTIPCM, INS_OPL,
};
use vgm2fur::opl::{OPLState, KIND_OPL3};
use vgm2fur::vgm::{chips_in_head, load_vgm, ChipWrite, RomSlice, VgmFile};
use vgm2fur::ymf278b::{collect_opl4_wave, opl4_pan_fx, opl4_rate, opl4_step};

const CLOCK: u32 = 33_868_800;

fn tmp_dir() -> PathBuf {
    let dir = std::env::temp_dir().join("vgm2fur_opl4");
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir
}

/// byte 3/4 = loop high/low, byte 5/6 = end high/low (ymf278.cpp).
fn descriptor(bits: u32, start: u32, loop_: u32, frames: u32) -> Vec<u8> {
    let end = (0x10000u32.wrapping_sub(frames)) & 0xFFFF;
    vec![
        ((bits << 6) | ((start >> 16) & 0x3F)) as u8,
        ((start >> 8) & 0xFF) as u8,
        (start & 0xFF) as u8,
        ((loop_ >> 8) & 0xFF) as u8,
        (loop_ & 0xFF) as u8,
        ((end >> 8) & 0xFF) as u8,
        (end & 0xFF) as u8,
        0,
        0,
        0,
        0,
        0,
    ]
}

/// A wave memory with one 8-bit descriptor for `wave`.
fn rom_for(wave: usize, sample: &[u8], start: usize) -> Vec<u8> {
    let mut mem = vec![0u8; start + sample.len() + 16];
    let off = wave * 12;
    mem[off..off + 12].copy_from_slice(&descriptor(0, start as u32, 0x10, sample.len() as u32));
    mem[start..start + sample.len()].copy_from_slice(sample);
    mem
}

fn vgm(writes: Vec<ChipWrite>, roms: Vec<RomSlice>, total: i64) -> VgmFile {
    let mut v = VgmFile::default();
    v.path = PathBuf::from("synthetic.vgm");
    v.version = 0x161;
    v.total_samples = total;
    v.ym2151_clock = 3_579_545;
    v.ymf278b_clock = CLOCK;
    v.writes = writes;
    v.roms = roms;
    v
}

fn wave_reg(slot: i32, sub: i32) -> u32 {
    ((2 << 8) | (0x08 + slot + sub * 24)) as u32
}

#[allow(clippy::too_many_arguments)]
fn wave_note(t: i64, wave: u32, slot: i32, fnum: u32, octave: i32, tl: u32, off: i64) -> Vec<ChipWrite> {
    vec![
        ChipWrite::new(t, "ymf278b", 0, wave_reg(slot, 0), wave & 0xFF),
        ChipWrite::new(
            t,
            "ymf278b",
            0,
            wave_reg(slot, 1),
            ((fnum & 0x7F) << 1) | ((wave >> 8) & 1),
        ),
        ChipWrite::new(
            t,
            "ymf278b",
            0,
            wave_reg(slot, 2),
            (((octave & 0xF) as u32) << 4) | ((fnum >> 7) & 7),
        ),
        ChipWrite::new(t, "ymf278b", 0, wave_reg(slot, 3), (tl << 1) & 0xFE),
        ChipWrite::new(t, "ymf278b", 0, wave_reg(slot, 4), 0x8F),
        ChipWrite::new(off, "ymf278b", 0, wave_reg(slot, 4), 0x0F),
    ]
}

fn note(t: i64) -> Vec<ChipWrite> {
    wave_note(t, 16, 0, 0x200, 2, 0x20, 8000)
}

/// A keyed OPL3-shaped FM voice on the OPL4's port 0.
fn fm_key(t: i64, ch: usize, block: u32, fnum: u32, off: Option<i64>) -> Vec<ChipWrite> {
    let st = OPLState::new(KIND_OPL3, false);
    let cm = st.chan_map[ch] as u32;
    let mut out = vec![
        ChipWrite::new(t, "ymf278b", 0, cm + 0xA0, fnum & 0xFF),
        ChipWrite::new(t, "ymf278b", 0, cm + 0xB0, (block << 2) | ((fnum >> 8) & 3) | 0x20),
    ];
    if let Some(off) = off {
        out.push(ChipWrite::new(
            off,
            "ymf278b",
            0,
            cm + 0xB0,
            (block << 2) | ((fnum >> 8) & 3),
        ));
    }
    out
}

/// A 0x100-byte header with the OPL4 clock at 0x60 and the given data blocks.
fn raw_vgm(cmds: &[u8], blocks: &[(u8, u32, &[u8])], clock: u32) -> Vec<u8> {
    let mut buf = vec![0u8; 0x100];
    buf[0..4].copy_from_slice(b"Vgm ");
    buf[8..12].copy_from_slice(&0x161u32.to_le_bytes());
    buf[0x18..0x1C].copy_from_slice(&44100u32.to_le_bytes());
    buf[0x30..0x34].copy_from_slice(&3_579_545u32.to_le_bytes());
    let hdr_len = buf.len() as u32;
    buf[0x34..0x38].copy_from_slice(&(hdr_len - 0x34).to_le_bytes());
    buf[0x60..0x64].copy_from_slice(&clock.to_le_bytes());
    let mut body: Vec<u8> = Vec::new();
    for (typ, start, data) in blocks {
        let mut payload: Vec<u8> = Vec::new();
        payload.extend_from_slice(&0x200000u32.to_le_bytes());
        payload.extend_from_slice(&start.to_le_bytes());
        payload.extend_from_slice(data);
        body.extend_from_slice(&[0x67, 0x66, *typ]);
        body.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        body.extend_from_slice(&payload);
    }
    body.extend_from_slice(cmds);
    body.push(0x66);
    let mut out = buf.clone();
    out.extend_from_slice(&body);
    let len = (out.len() - 4) as u32;
    out[4..8].copy_from_slice(&len.to_le_bytes());
    out
}

mod TestOpl4Parsing {
    use super::*;

    #[test]
    fn test_port_split_clock_and_blocks() {
        let cmds: &[u8] = &[
            0xD0, 0x00, 0x20, 0x01, 0xD0, 0x01, 0x20, 0x02, 0xD0, 0x02, 0x08, 0x10,
        ];
        let rom: Vec<u8> = (0..16u8).collect();
        let ram: Vec<u8> = (0..16u8).collect();
        let raw = raw_vgm(cmds, &[(0x84, 0, &rom), (0x87, 0, &ram)], CLOCK);
        let path = tmp_dir().join("parse.vgm");
        std::fs::write(&path, &raw).expect("write");
        let v = load_vgm(&path).expect("load");
        assert_eq!(v.ymf278b_clock, CLOCK);
        let got: Vec<(u32, u32)> = v
            .writes
            .iter()
            .filter(|w| w.chip == "ymf278b")
            .map(|w| (w.reg, w.val))
            .collect();
        assert_eq!(got, vec![(0x0020, 1), (0x0120, 2), (0x0208, 16)]);
        let kinds: Vec<&str> = v.roms.iter().map(|s| s.kind).collect();
        assert_eq!(kinds, vec!["ymf278b_rom", "ymf278b_ram"]);
        assert!(chips_in_head(&raw[..0x100]).contains("ymf278b"));
    }
}

mod TestOpl4Rhythm {
    use super::*;

    #[test]
    fn test_bank_zero_bd_latches_the_kit() {
        let mut ws: Vec<ChipWrite> = Vec::new();
        for k in 0..8i64 {
            let t = 1000 + k * 5000;
            ws.extend(fm_key(t, 15, 4, 580, Some(t + 3000)));
        }
        ws.push(ChipWrite::new(2000, "ymf278b", 0, 0xBD, 0x20));
        ws.push(ChipWrite::new(4000, "ymf278b", 0, 0xBD, 0x3F));
        ws.push(ChipWrite::new(28000, "ymf278b", 0, 0xBD, 0x20));
        let song = analyze(&vgm(ws, Vec::new(), 400_000), Default::default()).expect("analyze");
        assert!(song.opl_drums);
        let used: std::collections::BTreeSet<i32> = song
            .events
            .iter()
            .filter(|e| e.on && e.ch >= 15)
            .map(|e| e.ch)
            .collect();
        assert_eq!(used, [15, 16, 17, 18, 19].into_iter().collect());
        assert!(song.drum_notes > 0);
        assert!(!song.warnings.iter().any(|w| w.contains("write no notes")));
    }
}

mod TestOpl4Wave {
    use super::*;

    fn sample() -> Vec<u8> {
        (1..=20u8).collect()
    }

    fn one_rom() -> Vec<u8> {
        rom_for(16, &sample(), 0x1000)
    }

    #[test]
    fn test_one_key_on_extracts_the_descriptor_sample() {
        let rom = one_rom();
        let v = vgm(
            note(100),
            vec![RomSlice { chip: 0, rom_size: 0, start: 0, data: rom.clone(), kind: "ymf278b_rom", time: 0 }],
            400_000,
        );
        let (samples, warn) = collect_opl4_wave(&v, &rom, &[]);
        assert!(warn.is_empty(), "{warn:?}");
        assert_eq!(samples.len(), 1);
        let smp = &samples[0];
        assert_eq!(smp.wave, 16);
        assert_eq!(smp.frames(), 20);
        assert_eq!((smp.loop_start, smp.loop_end), (Some(0x10), Some(20)));
        assert_eq!(smp.hits.len(), 1);
    }

    #[test]
    fn test_rate_matches_the_step_formula() {
        let step = opl4_step(2, 0x200);
        assert_eq!(step, (((0x200 + 1024) as i64) << 7));
        let want = CLOCK as f64 * step as f64 / (768.0 * 65536.0);
        assert!((opl4_rate(CLOCK, step) - want).abs() < 1e-9);
    }

    #[test]
    fn test_zero_loop_address_loops_the_whole_body() {
        let sample: Vec<u8> = (0..16u8).collect();
        let mut rom = vec![0u8; 0x1000 + sample.len() + 16];
        rom[16 * 12..16 * 12 + 12].copy_from_slice(&descriptor(2, 0x1000, 0, 8));
        rom[0x1000..0x1000 + sample.len()].copy_from_slice(&sample);
        let v = vgm(
            wave_note(100, 16, 0, 0x200, 2, 0x20, 8000),
            vec![RomSlice { chip: 0, rom_size: 0, start: 0, data: rom.clone(), kind: "ymf278b_rom", time: 0 }],
            400_000,
        );
        let (samples, _warn) = collect_opl4_wave(&v, &rom, &[]);
        let smp = &samples[0];
        assert_eq!((smp.loop_start, smp.loop_end), (Some(0), Some(8)));
    }

    #[test]
    fn test_negative_octave_shifts_right() {
        assert_eq!(opl4_step(-4, 0x200), ((0x200 + 1024) as i64) << 1);
        assert_eq!(opl4_step(-6, 0x200), ((0x200 + 1024) as i64) >> 1);
        assert_eq!(opl4_step(-7, 0x200), ((0x200 + 1024) as i64) >> 2);
        assert_eq!(opl4_step(-8, 0x200), 0);
    }

    #[test]
    fn test_negative_octave_note_collects() {
        let rom = one_rom();
        let v = vgm(
            wave_note(100, 16, 0, 0x200, -6, 0x20, 8000),
            vec![RomSlice { chip: 0, rom_size: 0, start: 0, data: rom.clone(), kind: "ymf278b_rom", time: 0 }],
            400_000,
        );
        let (samples, _warn) = collect_opl4_wave(&v, &rom, &[]);
        assert_eq!(samples[0].hits[0].step, opl4_step(-6, 0x200));
    }

    #[test]
    fn test_missing_rom_warns() {
        let v = vgm(note(100), Vec::new(), 400_000);
        let (samples, warn) = collect_opl4_wave(&v, &[], &[]);
        assert!(samples.is_empty());
        assert!(warn.iter().any(|w| w.contains("YRW801")), "{warn:?}");
    }

    #[test]
    fn test_ram_samples_read_the_ram_table() {
        let mut ram = vec![0u8; 0x400];
        ram[0..12].copy_from_slice(&descriptor(0, 0x200200, 0, 8));
        let data: Vec<u8> = (1..=8u8).collect();
        ram[0x200..0x208].copy_from_slice(&data);
        let v = vgm(
            wave_note(100, 384, 0, 0x200, 2, 0x20, 8000),
            Vec::new(),
            400_000,
        );
        let (samples, _warn) = collect_opl4_wave(&v, &[], &ram);
        assert_eq!(samples.len(), 1);
        assert_eq!(samples[0].wave, 384);
        assert_eq!(samples[0].frames(), 8);
    }

    #[test]
    fn test_full_64k_sample_is_one_shot() {
        let mut ram = vec![0u8; 0x200 + 0x10000];
        ram[0..12].copy_from_slice(&descriptor(0, 0x200200, 50_000, 0x10000));
        let v = vgm(
            wave_note(100, 384, 0, 0x200, 2, 0x20, 8000),
            Vec::new(),
            400_000,
        );
        let (samples, _warn) = collect_opl4_wave(&v, &[], &ram);
        assert_eq!(samples.len(), 1);
        let smp = &samples[0];
        assert_eq!(smp.frames(), 0x10000);
        assert!(smp.loop_start.is_none());
        assert!(smp.loop_end.is_none());
    }

    #[test]
    fn test_key_off_ends_the_hit() {
        let rom = one_rom();
        let v = vgm(
            wave_note(100, 16, 0, 0x200, 2, 0x20, 900),
            vec![RomSlice { chip: 0, rom_size: 0, start: 0, data: rom.clone(), kind: "ymf278b_rom", time: 0 }],
            400_000,
        );
        let (samples, _warn) = collect_opl4_wave(&v, &rom, &[]);
        assert_eq!(samples[0].hits[0].end_time, 900);
    }

    #[test]
    fn test_level_write_while_keyed_is_a_volume_point() {
        let rom = one_rom();
        let mut writes = note(100);
        writes.insert(5, ChipWrite::new(500, "ymf278b", 0, wave_reg(0, 3), 0x20));
        let v = vgm(
            writes,
            vec![RomSlice { chip: 0, rom_size: 0, start: 0, data: rom.clone(), kind: "ymf278b_rom", time: 0 }],
            400_000,
        );
        let (samples, _warn) = collect_opl4_wave(&v, &rom, &[]);
        assert_eq!(samples[0].hits[0].vol_pts, vec![(500, 16)]);
    }

    #[test]
    fn test_attack_level_and_pan_after_key_on_are_folded() {
        let rom = one_rom();
        let writes = vec![
            ChipWrite::new(100, "ymf278b", 0, wave_reg(0, 0), 16),
            ChipWrite::new(100, "ymf278b", 0, wave_reg(0, 1), 0x00),
            ChipWrite::new(100, "ymf278b", 0, wave_reg(0, 2), 0x20),
            ChipWrite::new(100, "ymf278b", 0, wave_reg(0, 3), 0xFE),
            ChipWrite::new(100, "ymf278b", 0, wave_reg(0, 4), 0x88),
            ChipWrite::new(140, "ymf278b", 0, wave_reg(0, 3), 0x42),
            ChipWrite::new(140, "ymf278b", 0, wave_reg(0, 4), 0x82),
            ChipWrite::new(9000, "ymf278b", 0, wave_reg(0, 4), 0x02),
        ];
        let v = vgm(
            writes,
            vec![RomSlice { chip: 0, rom_size: 0, start: 0, data: rom.clone(), kind: "ymf278b_rom", time: 0 }],
            400_000,
        );
        let (samples, _warn) = collect_opl4_wave(&v, &rom, &[]);
        let hit = &samples[0].hits[0];
        assert_eq!((hit.level, hit.pan), (0x21, 2));
        assert!(hit.vol_pts.is_empty());
        assert!(hit.pan_pts.is_empty());
        let song = analyze(&v, Default::default()).expect("analyze");
        let ev = song
            .events
            .iter()
            .find(|e| e.on && e.ch == 18)
            .expect("the wave note");
        assert_eq!(ev.vol, 0x7F - 0x21);
        assert_eq!(ev.pan, opl4_pan_fx(2));
    }

    #[test]
    fn test_late_pan_write_is_a_pan_point() {
        let rom = one_rom();
        let mut writes = note(100);
        writes.insert(5, ChipWrite::new(4000, "ymf278b", 0, wave_reg(0, 4), 0x82));
        let v = vgm(
            writes,
            vec![RomSlice { chip: 0, rom_size: 0, start: 0, data: rom.clone(), kind: "ymf278b_rom", time: 0 }],
            400_000,
        );
        let (samples, _warn) = collect_opl4_wave(&v, &rom, &[]);
        let hit = &samples[0].hits[0];
        assert_eq!(hit.pan_pts, vec![(4000, 2)]);
        let song = analyze(&v, Default::default()).expect("analyze");
        let ev = song
            .events
            .iter()
            .find(|e| e.on && e.ch == 18)
            .expect("the wave note");
        assert!(ev.vol_pts.contains(&(4000, ev.vol, Some(opl4_pan_fx(2)))));
    }

    #[test]
    fn test_held_fnum_write_is_a_pitch_step() {
        let rom = one_rom();
        let mut writes = note(100);
        writes.insert(
            5,
            ChipWrite::new(
                4000,
                "ymf278b",
                0,
                wave_reg(0, 2),
                ((3u32 << 4) | ((0x200 >> 7) & 7)) as u32,
            ),
        );
        let v = vgm(
            writes,
            vec![RomSlice { chip: 0, rom_size: 0, start: 0, data: rom.clone(), kind: "ymf278b_rom", time: 0 }],
            400_000,
        );
        let (samples, _warn) = collect_opl4_wave(&v, &rom, &[]);
        let hit = &samples[0].hits[0];
        assert_eq!(hit.pitch_pts, vec![(4000, opl4_step(3, 0x200))]);
        let song = analyze(&v, Default::default()).expect("analyze");
        let ev = song
            .events
            .iter()
            .find(|e| e.on && e.ch == 18)
            .expect("the wave note");
        assert_eq!(ev.pitch_steps, vec![(4000, 768)]);
    }

    #[test]
    fn test_trailing_silence_step_is_not_a_volume_zero_row() {
        let rom = one_rom();
        let mut writes = wave_note(100, 16, 0, 0x200, 2, 0x20, 9000);
        writes.insert(5, ChipWrite::new(8800, "ymf278b", 0, wave_reg(0, 3), 0xFE));
        let v = vgm(
            writes.clone(),
            vec![RomSlice { chip: 0, rom_size: 0, start: 0, data: rom.clone(), kind: "ymf278b_rom", time: 0 }],
            400_000,
        );
        let song = analyze(&v, Default::default()).expect("analyze");
        let ev = song
            .events
            .iter()
            .find(|e| e.on && e.ch == 18)
            .expect("the wave note");
        assert!(!ev.vol_pts.iter().any(|(_t, value, _pan)| *value == 0));
        // A later rise must keep the silent step: it is real, not a tail.
        writes.insert(6, ChipWrite::new(8900, "ymf278b", 0, wave_reg(0, 3), 0x42));
        let v = vgm(
            writes,
            vec![RomSlice { chip: 0, rom_size: 0, start: 0, data: rom.clone(), kind: "ymf278b_rom", time: 0 }],
            400_000,
        );
        let song = analyze(&v, Default::default()).expect("analyze");
        let ev = song
            .events
            .iter()
            .find(|e| e.on && e.ch == 18)
            .expect("the wave note");
        assert!(ev.vol_pts.iter().any(|(_t, value, _pan)| *value == 0));
    }

    #[test]
    fn test_mute_far_before_the_key_off_is_kept() {
        let rom = one_rom();
        let mut writes = note(100);
        writes.insert(5, ChipWrite::new(4000, "ymf278b", 0, wave_reg(0, 3), 0xFE));
        writes.insert(6, ChipWrite::new(4000, "ymf278b", 0, wave_reg(0, 4), 0x88));
        let v = vgm(
            writes,
            vec![RomSlice { chip: 0, rom_size: 0, start: 0, data: rom.clone(), kind: "ymf278b_rom", time: 0 }],
            400_000,
        );
        let song = analyze(&v, Default::default()).expect("analyze");
        let ev = song
            .events
            .iter()
            .find(|e| e.on && e.ch == 18)
            .expect("the wave note");
        assert!(ev.vol_pts.iter().any(|(_t, value, _pan)| *value == 0));
    }

    #[test]
    fn test_pan_inverts_furnace_conversion() {
        let furnace_pan = |fx: i32| -> i32 {
            let left = (fx >> 4) & 15;
            let right = fx & 15;
            let linear = if left == right {
                7
            } else {
                ((1.0 + (right - left) as f64 / left.max(right) as f64) * 0.5 * 15.0) as i32
            };
            8 ^ (linear + 1).min(15)
        };
        assert_eq!(opl4_pan_fx(0), 0xFF);
        for pan in [0, 1, 2, 3, 4, 5, 6, 7, 9, 10, 11, 12, 13, 14, 15] {
            assert_eq!(furnace_pan(opl4_pan_fx(pan)), pan, "pan {pan}");
        }
        assert_eq!(opl4_pan_fx(8), 0);
    }
}

mod TestOpl4Writer {
    use super::*;

    fn song(drums: bool) -> vgm2fur::model::Song {
        let mut writes: Vec<ChipWrite> = Vec::new();
        for page in [0x20u32, 0x23, 0x40, 0x43, 0x60, 0x63, 0x80, 0x83, 0xC0] {
            writes.push(ChipWrite::new(100, "ymf278b", 0, page, 0x01));
        }
        writes.push(ChipWrite::new(200, "ymf278b", 0, 0xA0, 0x80));
        writes.push(ChipWrite::new(200, "ymf278b", 0, 0xB0, 0x20));
        writes.push(ChipWrite::new(3000, "ymf278b", 0, 0xB0, 0x00));
        if drums {
            writes.push(ChipWrite::new(4000, "ymf278b", 0, 0xBD, 0x20));
            writes.push(ChipWrite::new(4100, "ymf278b", 0, 0xBD, 0x30));
        }
        writes.extend(note(1000));
        let rom = rom_for(16, &(1..=20u8).collect::<Vec<u8>>(), 0x1000);
        analyze(
            &vgm(
                writes,
                vec![RomSlice { chip: 0, rom_size: 0, start: 0, data: rom, kind: "ymf278b_rom", time: 0 }],
                400_000,
            ),
            Default::default(),
        )
        .expect("analyze")
    }

    #[test]
    fn test_chip_and_channel_count() {
        let m = parse_fur(&write_fur(&song(false), WriteOptions::default())).expect("parse");
        assert_eq!(m.chips, vec![CHIP_OPL4]);
        assert_eq!(m.n_ch, 42);
        let total: usize = m.chips.iter().map(|c| chip_channels(*c)).sum();
        assert_eq!(total, m.n_ch);
    }

    #[test]
    fn test_drums_selects_the_bigger_system() {
        let m = parse_fur(&write_fur(&song(true), WriteOptions::default())).expect("parse");
        assert_eq!(m.chips, vec![CHIP_OPL4_DRUMS]);
        assert_eq!(m.n_ch, 44);
    }

    #[test]
    fn test_instrument_types() {
        let raw = write_fur(&song(false), WriteOptions::default());
        let plain = vgm2fur::furwrite::decompress_fur(&raw);
        let types: Vec<u16> = iter_blocks(&plain)
            .expect("blocks")
            .iter()
            .filter(|b| b.magic == *b"INS2")
            .map(|b| u16::from_le_bytes([b.payload[2], b.payload[3]]))
            .collect();
        assert!(types.contains(&(INS_OPL as u16)), "{types:?}");
        assert!(types.contains(&(INS_MULTIPCM as u16)), "{types:?}");
    }

    #[test]
    fn test_wave_only_module_keeps_one_based_wave_instruments() {
        let rom = rom_for(16, &(1..=20u8).collect::<Vec<u8>>(), 0x1000);
        let s = analyze(
            &vgm(
                note(100),
                vec![RomSlice { chip: 0, rom_size: 0, start: 0, data: rom, kind: "ymf278b_rom", time: 0 }],
                400_000,
            ),
            Default::default(),
        )
        .expect("analyze");
        let m = parse_fur(&write_fur(&s, WriteOptions::default())).expect("parse");
        assert_eq!(m.ins_sample, vec![-1, 0]);
        let rows = m
            .patterns
            .get(&(18u16, m.orders[18][0]))
            .cloned()
            .unwrap_or_default();
        let notes: Vec<(i32, i32)> = rows
            .iter()
            .filter(|r| r.note != -1)
            .map(|r| (r.note, r.ins))
            .collect();
        assert!(!notes.is_empty());
        assert_eq!(notes[0].1, 1);
    }
}

/// Every rising wave-slot key bit in a register stream.
fn wave_keys(v: &VgmFile) -> Vec<(i64, i32)> {
    let mut out: Vec<(i64, i32)> = Vec::new();
    let mut prev: std::collections::BTreeMap<i32, bool> = std::collections::BTreeMap::new();
    for w in &v.writes {
        if w.chip != "ymf278b" || (w.reg >> 8) != 2 {
            continue;
        }
        let r = (w.reg & 0xFF) as i32;
        if (0x08..=0xF7).contains(&r) && (r - 8) / 24 == 4 {
            let slot = (r - 8) % 24;
            let on = (w.val & 0x80) != 0;
            let was = prev.get(&slot).copied().unwrap_or(false);
            prev.insert(slot, on);
            if on && !was {
                out.push((w.sample, slot));
            }
        }
    }
    out
}

mod TestOpl4WaveRoundTrip {
    use super::*;

    #[test]
    fn test_wave_keys_and_clock_survive_the_module() {
        skip_without_furnace!();
        let rom = rom_for(16, &(1..=20u8).collect::<Vec<u8>>(), 0x1000);
        let src = vgm(
            note(100),
            vec![RomSlice { chip: 0, rom_size: 0, start: 0, data: rom, kind: "ymf278b_rom", time: 0 }],
            10_000,
        );
        let s = analyze(&src, Default::default()).expect("analyze");
        let kinds: Vec<&str> = s.fm_systems.iter().map(|x| x.kind.as_str()).collect();
        assert_eq!(kinds, vec!["opl4"]);
        assert!(!s.opl4_wave_samples.is_empty());

        let dir = tmp_dir();
        let fur = dir.join("wave_roundtrip.fur");
        std::fs::write(
            &fur,
            write_fur(&s, WriteOptions { compress: true, ..Default::default() }),
        )
        .expect("write fur");
        let out = dir.join("wave_roundtrip.vgm");
        vgm2fur::furnace::export_vgm(&fur, &out).expect("export");
        let exp = load_vgm(&out).expect("load export");

        assert_eq!(exp.ymf278b_clock, src.ymf278b_clock);
        let lim = src.total_samples;
        let want = wave_keys(&src).len();
        let got = wave_keys(&exp).iter().filter(|k| k.0 < lim).count();
        assert_eq!(got, want);
    }
}
