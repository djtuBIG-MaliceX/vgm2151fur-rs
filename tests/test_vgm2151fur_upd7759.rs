//! uPD7759 stand-alone/slave collection and Generic PCM DAC emission.
//!
//! Port of `tests/test_vgm2151fur_upd7759.py` at vgm2151fur a8edb6e.

mod common;
#[allow(unused_imports)]
use common::*;

use std::path::PathBuf;

use indexmap::IndexMap;

use vgm2fur::analyze::{analyze, AnalyzeOptions};
use vgm2fur::furio::parse_fur;
use vgm2fur::furwrite::{write_fur, WriteOptions, CHIP_PCM_DAC};
use vgm2fur::upd7759::{collect_upd7759, decode_stream, STANDARD_CLOCK};
use vgm2fur::vgm::{ChipWrite, RomSlice, VgmFile};

fn _rom() -> Vec<u8> {
    // One phrase: rate 1, four nibbles, end.
    let mut data = vec![0u8; 0x20];
    data[0..5].copy_from_slice(&[0x00, 0x5A, 0xA5, 0x69, 0x55]);
    data[5..7].copy_from_slice(&(6u16).to_be_bytes()); // phrase 0 -> 0x0D
    data[0x0D..0x12].copy_from_slice(&[0x80, 0x03, 0x12, 0x34, 0x00]);
    data
}

fn _vgm(writes: Vec<ChipWrite>, roms: Vec<RomSlice>, dual: bool, slave: bool) -> VgmFile {
    VgmFile {
        path: PathBuf::from("synthetic.vgm"),
        buf: Vec::new(),
        version: 0x161,
        total_samples: 44_100,
        loop_samples: 0,
        loop_file_pos: None,
        loop_sample: None,
        ym2151_clock: 0,
        k007232_clock: 0,
        gd3: IndexMap::new(),
        roms,
        writes,
        upd7759_clock: 640_000,
        upd7759_dual: dual,
        upd7759_slave: slave,
        ..Default::default()
    }
}

fn _start(sample: i64, port: u32) -> Vec<ChipWrite> {
    vec![
        ChipWrite::new(sample, "upd7759", 0, 2, port),
        ChipWrite::new(sample, "upd7759", 0, 1, 0),
        ChipWrite::new(sample, "upd7759", 0, 1, 1),
    ]
}

fn _rom_slice(rom: &[u8]) -> RomSlice {
    RomSlice {
        chip: 0,
        rom_size: rom.len() as i64,
        start: 0,
        data: rom.to_vec(),
        kind: "upd7759",
        time: 0,
    }
}

mod TestUpd7759Collector {
    use super::*;

    #[test]
    fn test_stand_alone_phrase() {
        let rom = _rom();
        let v = _vgm(_start(0, 0), vec![_rom_slice(&rom)], false, false);
        let (samples, warn) = collect_upd7759(&v, false).expect("collect");
        assert_eq!(warn, Vec::<String>::new());
        assert_eq!(samples.len(), 1);
        assert!(samples[0].frames() > 0);
        assert_eq!(samples[0].phrase, 0);
        assert_eq!(samples[0].hits.len(), 1);
        assert!(samples[0].hits[0].end > 0);
    }

    #[test]
    fn test_slave_stream_decodes_after_handshake() {
        // Five handshake bytes, then a rate-1 block with four nibbles.
        let burst: [u8; 10] = [
            0xFF, 0xFF, 0x00, 0x00, 0x00, 0x80, 0x03, 0x12, 0x34, 0x00,
        ];
        let mut writes = vec![ChipWrite::new(0, "upd7759", 0, 1, 0)];
        writes.push(ChipWrite::new(0, "upd7759", 0, 1, 1));
        for (i, &b) in burst.iter().enumerate() {
            writes.push(ChipWrite::new(i as i64, "upd7759", 0, 2, b as u32));
        }
        writes.push(ChipWrite::new(1000, "upd7759", 0, 1, 0));
        let v = _vgm(writes, Vec::new(), false, true);
        let (samples, warn) = collect_upd7759(&v, false).expect("collect");
        assert_eq!(warn, Vec::<String>::new());
        assert_eq!(samples.len(), 1);
        assert!(samples[0].frames() > 0);
    }

    #[test]
    fn test_decode_stream_repeat() {
        let phrase = decode_stream(&[0xC1, 0x80, 0x01, 0xAB, 0x00], 0, STANDARD_CLOCK);
        assert_eq!(phrase.blocks, 4);
        assert!(phrase.events.len() > 2);
    }
}

mod TestUpd7759Module {
    use super::*;

    fn analyze_song(v: &VgmFile) -> vgm2fur::model::Song {
        analyze(v, AnalyzeOptions::default()).expect("analyze")
    }

    #[test]
    fn test_pcm_dac_layout() {
        let rom = _rom();
        let v = _vgm(_start(0, 0), vec![_rom_slice(&rom)], false, false);
        let song = analyze_song(&v);
        assert_eq!(song.upd7759_ids, vec![0]);
        assert_eq!(song.upd7759_count, 1);
        let data = write_fur(
            &song,
            WriteOptions {
                include_fm: false,
                ..Default::default()
            },
        );
        let mod_ = parse_fur(&data).expect("parse");
        assert_eq!(mod_.chips, vec![CHIP_PCM_DAC]);
        assert_eq!(mod_.n_ch, 1);
    }

    #[test]
    fn test_dual_header_keeps_silent_second_chip() {
        let rom = _rom();
        let v = _vgm(_start(0, 0), vec![_rom_slice(&rom)], true, false);
        let song = analyze_song(&v);
        assert_eq!(song.upd7759_ids, vec![0, 1]);
        let data = write_fur(
            &song,
            WriteOptions {
                include_fm: false,
                ..Default::default()
            },
        );
        let mod_ = parse_fur(&data).expect("parse");
        assert_eq!(mod_.chips, vec![CHIP_PCM_DAC, CHIP_PCM_DAC]);
        assert_eq!(mod_.n_ch, 2);
    }

    #[test]
    fn test_identical_phrases_share_one_body() {
        let rom = _rom();
        // Two start edges for the same phrase.
        let mut writes = _start(0, 0);
        writes.push(ChipWrite::new(5000, "upd7759", 0, 1, 0));
        writes.push(ChipWrite::new(5000, "upd7759", 0, 1, 1));
        let v = _vgm(writes, vec![_rom_slice(&rom)], false, false);
        let (samples, _warn) = collect_upd7759(&v, false).expect("collect");
        assert_eq!(samples.len(), 1);
        assert_eq!(samples[0].hits.len(), 2);
    }
}
