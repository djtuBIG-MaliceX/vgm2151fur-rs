//! Yamaha YMW258-F (MultiPCM) parsing, collector and module emission.
//!
//! Port of `tests/test_vgm2151fur_multipcm.py` (v0.11.0).

mod common;

use std::path::PathBuf;

use vgm2fur::analyze::{analyze, AnalyzeOptions};
use vgm2fur::edit::read_chips_file;
use vgm2fur::furio::parse_fur;
use vgm2fur::furwrite::{write_fur, WriteOptions, CHIP_MULTIPCM};
use vgm2fur::multipcm::{collect_multipcm, played_rate, MULTIPCM_VOICES};
use vgm2fur::vgm::{ChipWrite, RomSlice, VgmFile};

/// One sample header at index 0 and a 256-frame body at 0x100.
fn rom(depth: i32) -> Vec<u8> {
    let mut rom = vec![0u8; 0x1000];
    let mut start: u32 = 0x100;
    if depth == 12 {
        start |= 1 << 22; // format bit 2 -> 12-bit
    }
    let start_bytes = start.to_be_bytes();
    rom[0..3].copy_from_slice(&start_bytes[1..4]);
    rom[3..5].copy_from_slice(&0x20u16.to_be_bytes());
    rom[5..7].copy_from_slice(&(0x10000u32 - 256).to_be_bytes()[2..4]);
    rom[7] = 0x00; // LFO / VIB
    rom[8] = 0xF0; // AR 15, D1R 0
    rom[9] = 0x00; // DL 0, D2R 0
    rom[10] = 0x0F; // RC 0, RR 15
    rom[11] = 0x00; // AM 0
    let mut body: Vec<u8> = (0..256).map(|i| (i & 0xFF) as u8).collect();
    if depth == 12 {
        // Pack 256 12-bit frames; the exact values do not matter here.
        let mut packed = Vec::new();
        for i in (0..256).step_by(2) {
            let a = ((i * 13) & 0xFFF) as u32;
            let b = (((i + 1) * 13) & 0xFFF) as u32;
            packed.push((a >> 4) as u8);
            packed.push((((a & 0xF) << 4) | (b >> 8)) as u8);
            packed.push((b & 0xFF) as u8);
        }
        body = packed;
    }
    rom[0x100..0x100 + body.len()].copy_from_slice(&body);
    rom
}

fn make_vgm(writes: Vec<ChipWrite>, roms: Vec<RomSlice>, multipcm_dual: bool) -> VgmFile {
    VgmFile {
        path: PathBuf::from("synthetic.vgm"),
        version: 0x161,
        total_samples: 44100,
        ym2151_clock: 0,
        k007232_clock: 0,
        writes,
        multipcm_clock: 8_000_000,
        roms,
        multipcm_dual,
        ..Default::default()
    }
}

fn write(sample: i64, reg: u32, val: u32) -> ChipWrite {
    ChipWrite::new(sample, "multipcm", 0, reg, val)
}

fn note(reg5: u32, pan: u32) -> Vec<ChipWrite> {
    // Select channel 0, then set registers 3,2,1,5,0,4.
    vec![
        write(0, 1, 0),
        write(0, 2, 3),
        write(0, 0, 0x10), // octave 1, fn high 0
        write(0, 2, 2),
        write(0, 0, 0x00), // fn low 0, sample high bit
        write(0, 2, 1),
        write(0, 0, 0x00), // sample index 0
        write(0, 2, 5),
        write(0, 0, reg5), // TL
        write(0, 2, 0),
        write(0, 0, pan), // pan
        write(0, 2, 4),
        write(0, 0, 0x80), // key on
    ]
}

fn rom_slice(rom: Vec<u8>) -> RomSlice {
    RomSlice {
        chip: 0,
        rom_size: rom.len() as i64,
        start: 0,
        data: rom,
        kind: "multipcm",
        time: 0,
    }
}

#[allow(non_snake_case)]
mod TestMultipcmCollector {
    use super::*;

    #[test]
    fn test_eight_bit_sample_and_note() {
        let rom = rom(8);
        let mut writes = note(0x00, 0x00);
        writes.push(write(1000, 2, 4)); // key off
        writes.push(write(1000, 0, 0x00));
        let v = make_vgm(writes, vec![rom_slice(rom.clone())], false);
        let (samples, warn) = collect_multipcm(&v);
        assert_eq!(warn, Vec::<String>::new());
        assert_eq!(samples.len(), 1);
        let s = &samples[0];
        assert_eq!((s.index, s.depth, s.frames), (0, 8, 256));
        assert_eq!(s.data, rom[0x100..0x200].to_vec());
        assert_eq!((s.loop_start, s.loop_end), (Some(0x20), Some(256)));
        assert_eq!(s.ar, 15);
        assert_eq!(s.rr, 15);
        assert_eq!(s.hits.len(), 1);
        let hit = &s.hits[0];
        assert_eq!((hit.octave, hit.fnum, hit.pan), (1, 0, 0));
        assert_eq!(hit.off_sample, 1000);
    }

    #[test]
    fn test_twelve_bit_sample() {
        let rom = rom(12);
        let v = make_vgm(note(0x00, 0x00), vec![rom_slice(rom)], false);
        let (samples, _warn) = collect_multipcm(&v);
        assert_eq!(samples[0].depth, 12);
        assert_eq!(samples[0].frames, 256);
        assert_eq!(samples[0].data.len(), (256 * 3 + 1) / 2);
    }

    #[test]
    fn test_sega_bank_mapping() {
        let rom = rom(8);
        // Sample start 0x100000 selects the high bank; a 1 MB bank 1 maps it
        // back to 0x100000, which is inside a 4 MB image.
        let mut hdr = vec![0u8; 0x101000];
        hdr[0..rom.len()].copy_from_slice(&rom);
        let start_bytes = 0x100100u32.to_be_bytes();
        hdr[0..3].copy_from_slice(&start_bytes[1..4]);
        let body = &rom[0x100..0x200];
        hdr[0x100100..0x100200].copy_from_slice(body);
        let mut writes = vec![ChipWrite::new(0, "multipcm_bank", 0, 3, 16)];
        writes.extend(note(0x00, 0x00));
        let v = make_vgm(writes, vec![rom_slice(hdr)], false);
        let (samples, _warn) = collect_multipcm(&v);
        assert_eq!(samples[0].start, 0x100100);
    }

    #[test]
    fn test_rate_pitch() {
        // octave 1, fnum 0 is the chip's output rate (clock / 180).
        assert_almost_eq!(played_rate(8_000_000, 1, 0), 8_000_000.0 / 180.0);
        assert_almost_eq!(played_rate(8_000_000, 2, 0), 2.0 * 8_000_000.0 / 180.0);
    }
}

#[allow(non_snake_case)]
mod TestMultipcmModule {
    use super::*;

    #[test]
    fn test_layout_and_instrument_type() {
        let rom = rom(8);
        let v = make_vgm(note(0x00, 0x00), vec![rom_slice(rom)], false);
        let song = analyze(&v, AnalyzeOptions::default()).expect("analyze");
        assert_eq!(song.multipcm_ids, vec![0]);
        assert_eq!(song.multipcm_count, 1);
        assert!(song.events.iter().any(|e| e.ch >= 0));
        let data = write_fur(&song, WriteOptions {
            include_fm: false,
            ..Default::default()
        });
        let mod_ = parse_fur(&data).expect("parse_fur");
        assert_eq!(mod_.chips, vec![CHIP_MULTIPCM]);
        assert_eq!(mod_.n_ch, MULTIPCM_VOICES);
    }

    #[test]
    fn test_dual_header_keeps_silent_second_chip() {
        let rom = rom(8);
        let v = make_vgm(note(0x00, 0x00), vec![rom_slice(rom)], true);
        let song = analyze(&v, AnalyzeOptions::default()).expect("analyze");
        assert_eq!(song.multipcm_ids, vec![0, 1]);
        let data = write_fur(&song, WriteOptions {
            include_fm: false,
            ..Default::default()
        });
        let mod_ = parse_fur(&data).expect("parse_fur");
        assert_eq!(mod_.chips, vec![CHIP_MULTIPCM, CHIP_MULTIPCM]);
        assert_eq!(mod_.n_ch, 2 * MULTIPCM_VOICES);
    }

    #[test]
    fn test_key_off_stops_early() {
        let rom = rom(8);
        let mut writes = note(0x00, 0x00);
        writes.push(write(1000, 2, 4));
        writes.push(write(1000, 0, 0x00));
        let v = make_vgm(writes, vec![rom_slice(rom)], false);
        let song = analyze(&v, AnalyzeOptions::default()).expect("analyze");
        let offs: Vec<i64> = song
            .events
            .iter()
            .filter(|e| !e.on && e.pcm)
            .map(|e| e.sample)
            .collect();
        assert_eq!(offs, vec![1000]);
        let data = write_fur(&song, WriteOptions {
            include_fm: false,
            ..Default::default()
        });
        let mod_ = parse_fur(&data).expect("parse_fur");
        let notes: Vec<i32> = mod_
            .patterns
            .values()
            .flat_map(|rows| rows.iter())
            .map(|row| row.note)
            .collect();
        assert!(notes.contains(&180));
    }

    #[test]
    fn test_pan_volume_and_instrument_headers() {
        let rom = rom(8);
        // TL 0 -> volume 127; pan 0 -> both channels full.
        let v = make_vgm(note(0x00, 0x00), vec![rom_slice(rom)], false);
        let song = analyze(&v, AnalyzeOptions::default()).expect("analyze");
        let ev = song.events.iter().find(|e| e.on).expect("a key-on event");
        assert_eq!(ev.vol, 127);
        assert_eq!(ev.pan, 0xFF);
        let data = write_fur(&song, WriteOptions {
            include_fm: false,
            ..Default::default()
        });
        // The MultiPCM instrument carries its ADSR in an MP feature.
        assert!(data.windows(2).any(|w| w == b"MP"));
        let tmp = tempfile::tempdir().expect("tempdir");
        let p = tmp.path().join("m.fur");
        std::fs::write(&p, &data).expect("write fur");
        let chips = read_chips_file(&p).expect("read_chips_file");
        assert!(chips.iter().all(|c| c.volume <= 2.0));
        assert!(parse_fur(&data).expect("parse_fur").master_vol <= 3.0);
    }
}
