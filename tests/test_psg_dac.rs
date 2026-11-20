//! SN76489 sample playback (4-bit DAC) -> Generic PCM DAC.
//!
//! Port of `tests/test_psg_dac.py`.

mod common;
#[allow(unused_imports)]
use common::*;

use std::path::PathBuf;

use vgm2fur::analyze::{analyze, AnalyzeOptions};
use vgm2fur::furio::{decompress_fur, iter_blocks, parse_fur};
use vgm2fur::furwrite::{write_fur, CHIP_PCM_DAC};
use vgm2fur::psg_dac::{collect_psg_dac, PSG_DAC_RATE};
use vgm2fur::vgm::{ChipWrite, VgmFile};

fn _vgm_full(
    writes: Vec<ChipWrite>,
    total_samples: i64,
    loop_sample: Option<i64>,
    clock: u32,
) -> VgmFile {
    let mut writes = writes;
    writes.sort_by_key(|w| w.sample);
    let mut v = VgmFile::default();
    v.path = PathBuf::from("psg_dac.vgm");
    v.buf = Vec::new();
    v.version = 0x161;
    v.total_samples = total_samples;
    v.loop_samples = 0;
    v.loop_file_pos = None;
    v.loop_sample = loop_sample;
    v.ym2151_clock = 0;
    v.k007232_clock = 0;
    v.gd3 = Default::default();
    v.roms = Vec::new();
    v.writes = writes;
    v.sn76489_clock = clock;
    v.ym2612_clock = 0;
    v
}

fn _vgm(writes: Vec<ChipWrite>) -> VgmFile {
    _vgm_full(writes, 4096, None, 3_579_545)
}

fn _vgm_loop(writes: Vec<ChipWrite>, loop_sample: i64) -> VgmFile {
    _vgm_full(writes, 4096, Some(loop_sample), 3_579_545)
}

fn _dac_writes(total: i64) -> Vec<ChipWrite> {
    let mut writes = vec![
        ChipWrite::new(0, "sn76489", 0, 0, 0x80), // voice 0 period low 0
        ChipWrite::new(0, "sn76489", 0, 0, 0x00), // period high 0
        ChipWrite::new(0, "sn76489", 0, 0, 0x90), // full volume
    ];
    let pattern: [u32; 16] = [5, 4, 6, 3, 7, 2, 8, 1, 9, 0, 10, 15, 5, 5, 5, 8];
    let mut i = 0usize;
    let mut t = 16i64;
    while t < total {
        writes.push(ChipWrite::new(
            t,
            "sn76489",
            0,
            0,
            0x90 | pattern[i % 16],
        ));
        i += 1;
        t += 8;
    }
    writes
}

#[allow(non_snake_case)]
mod TestPsgDacCollector {
    use super::*;

    #[test]
    fn test_detects_the_stream_and_splits_segments() {
        let got = collect_psg_dac(&_vgm_loop(_dac_writes(4096), 2048), PSG_DAC_RATE, false);
        assert_eq!(got.voices, vec![(0, 0)]);
        assert!(got.samples.len() >= 1);
        assert!(got.plays.len() > 1);
        assert_eq!(got.rate, 22050);
        assert!(got.samples.iter().any(|s| s.data.iter().any(|&v| v != 0)));
        // The loop point gets its own chunk boundary.
        assert!(got.plays.iter().any(|p| p.sample_time == 2048));
    }

    #[test]
    fn test_silent_chunks_are_filtered_out() {
        let mut writes = vec![
            ChipWrite::new(0, "sn76489", 0, 0, 0x80),
            ChipWrite::new(0, "sn76489", 0, 0, 0x00),
            ChipWrite::new(0, "sn76489", 0, 0, 0x90),
        ];
        let pattern: [u32; 8] = [5, 4, 6, 3, 7, 2, 8, 1];
        let mut i = 0usize;
        let mut t = 16i64;
        while t < 4096 {
            writes.push(ChipWrite::new(
                t,
                "sn76489",
                0,
                0,
                0x90 | pattern[i % 8],
            ));
            i += 1;
            t += 8;
        }
        // A long rest at the idle attenuation, then another burst.
        let mut t = 4096i64;
        while t < 30000 {
            writes.push(ChipWrite::new(t, "sn76489", 0, 0, 0x90 | 5));
            t += 8;
        }
        let mut i = 0usize;
        let mut t = 30000i64;
        while t < 34096 {
            writes.push(ChipWrite::new(
                t,
                "sn76489",
                0,
                0,
                0x90 | pattern[i % 8],
            ));
            i += 1;
            t += 8;
        }
        let got = collect_psg_dac(&_vgm_full(writes, 34096, None, 3_579_545), PSG_DAC_RATE, false);
        for smp in &got.samples {
            assert!(
                smp.data.iter().any(|&v| v != 8),
                "a silent body was kept",
            );
            assert_ne!(smp.data[0], 8, "a body starts in silence");
        }
    }

    #[test]
    fn test_non_dac_volume_writes_are_not_a_stream() {
        let mut writes = vec![
            ChipWrite::new(0, "sn76489", 0, 0, 0x80),
            ChipWrite::new(0, "sn76489", 0, 0, 0x10), // period 0x100, a tone
            ChipWrite::new(0, "sn76489", 0, 0, 0x90),
        ];
        let mut t = 500i64;
        while t < 4096 {
            writes.push(ChipWrite::new(
                t,
                "sn76489",
                0,
                0,
                0x90 | ((t / 64) % 12) as u32,
            ));
            t += 500;
        }
        let got = collect_psg_dac(&_vgm(writes), PSG_DAC_RATE, false);
        assert_eq!(got.voices, Vec::<(i32, i32)>::new());
        assert_eq!(got.samples, Vec::new());
    }
}

#[allow(non_snake_case)]
mod TestPsgDacModule {
    use super::*;

    #[test]
    fn test_module_uses_a_generic_pcm_dac_system() {
        let song = analyze(
            &_vgm_loop(_dac_writes(4096), 2048),
            AnalyzeOptions::default(),
        )
        .expect("analyze");
        assert_eq!(song.psg_dac_voices, vec![(0, 0)]);
        assert!(!song.psg_dac_samples.is_empty());
        assert!(!song.psg_dac_plays.is_empty());
        let data = write_fur(&song, Default::default());
        let mod_ = parse_fur(&data).expect("parse");
        assert!(mod_.chips.contains(&CHIP_PCM_DAC));
        assert_eq!(mod_.n_ch, 1);
        assert_eq!(mod_.samples.len(), song.psg_dac_samples.len());
        // Source and body are both 4-bit; Furnace's depth-15 nibbles.
        assert_eq!(mod_.samples[0].depth, 15);
        // One one-voice DAC system: the PSG chip level 0.5 divided by the
        // four-voice mixer share, i.e. 0.125 of unity.
        let raw = decompress_fur(&data);
        for block in iter_blocks(&raw).expect("blocks") {
            if block.magic == *b"INFO" || block.magic == *b"INF2" {
                let vols = &block.payload[56..88];
                assert_eq!(vols[0], 8);
                break;
            }
        }
        let notes: Vec<_> = mod_
            .patterns
            .values()
            .flat_map(|rows| rows.iter())
            .filter(|row| 0 <= row.note && row.note < 180)
            .collect();
        assert!(!notes.is_empty());
        assert!(notes.iter().all(|row| row.ins >= 0));
    }
}
