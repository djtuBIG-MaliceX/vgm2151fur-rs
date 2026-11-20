//! FDS, WonderSwan and QSound parsing, analysis and conversion.
//!
//! Port of `tests/test_vgm2151fur_new_chips.py`.

mod common;

use std::path::PathBuf;

use vgm2fur::analyze::analyze;
use vgm2fur::fds::collect_fds;
use vgm2fur::furio::{decompress_fur, parse_fur};
use vgm2fur::furwrite::write_fur;
use vgm2fur::qsound::{collect_qsound, qsound_pan};
use vgm2fur::vgm::{load_vgm, ChipWrite, RomSlice, VgmFile};
use vgm2fur::wonderswan::collect_wonderswan;

fn tmp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("vgm2fur_newchips_{tag}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir
}

fn write(t: i64, chip: &'static str, reg: u32, val: u32, cid: u16) -> ChipWrite {
    ChipWrite::new(t, chip, cid as u8, reg, val)
}

fn vgm(writes: Vec<ChipWrite>, total: i64, roms: Vec<RomSlice>) -> VgmFile {
    let mut v = VgmFile::default();
    v.path = PathBuf::from("synthetic.vgm");
    v.version = 0x171;
    v.total_samples = total;
    v.ym2151_clock = 0;
    v.writes = writes;
    v.roms = roms;
    v
}

/// The sample count in INFO (`info + 8 + 18`).
fn info_sample_count(data: &[u8]) -> u16 {
    let raw = decompress_fur(data);
    let info = u32::from_le_bytes([raw[20], raw[21], raw[22], raw[23]]) as usize;
    let off = info + 8 + 18;
    u16::from_le_bytes([raw[off], raw[off + 1]])
}

/// `_raw_vgm`: a 0x120-byte header, the command body at 0x100.
fn raw_vgm(body: &[u8], clock_off: Option<usize>, clock: u32) -> Vec<u8> {
    let mut buf = vec![0u8; 0x120];
    buf[0..4].copy_from_slice(b"Vgm ");
    buf[8..12].copy_from_slice(&0x171u32.to_le_bytes());
    buf[0x18..0x1C].copy_from_slice(&44100u32.to_le_bytes());
    buf[0x34..0x38].copy_from_slice(&((0x100u32) - 0x34).to_le_bytes());
    if let Some(off) = clock_off {
        buf[off..off + 4].copy_from_slice(&clock.to_le_bytes());
    }
    buf[0x100..0x100 + body.len()].copy_from_slice(body);
    buf[0x100 + body.len()] = 0x66;
    let len = (buf.len() - 4) as u32;
    buf[4..8].copy_from_slice(&len.to_le_bytes());
    buf
}

mod TestParsing {
    use super::*;

    #[test]
    fn test_fds_write_split() {
        let body = [
            0xB4, 0x20, 0x80, // $4080 volume
            0xB4, 0x40, 0x12, // wave RAM
            0xB4, 0x00, 0x30, // NES APU pulse
            0xB4, 0x3F, 0x02, // FDS I/O enable
        ];
        let path = tmp_dir("parse_fds").join("fds.vgm");
        std::fs::write(&path, raw_vgm(&body, Some(0x84), 1_789_773)).expect("write");
        let v = load_vgm(&path).expect("load");
        assert_eq!(v.nes_clock, 1_789_773);
        let fds: Vec<u32> = v.writes.iter().filter(|w| w.chip == "fds").map(|w| w.reg).collect();
        let nes: Vec<u32> = v.writes.iter().filter(|w| w.chip == "nes").map(|w| w.reg).collect();
        assert_eq!(fds, vec![0x80, 0x40, 0x23]);
        assert_eq!(nes, vec![0x00]);
    }

    #[test]
    fn test_wonderswan_commands_and_clock() {
        let body = [0xBC, 0x01, 0x40, 0xC6, 0x00, 0x80, 0xAB];
        let path = tmp_dir("parse_ws").join("ws.vgm");
        std::fs::write(&path, raw_vgm(&body, Some(0xC0), 3_072_000)).expect("write");
        let v = load_vgm(&path).expect("load");
        assert_eq!(v.wonderswan_clock, 3_072_000);
        assert_eq!(v.writes.len(), 2);
        assert_eq!(v.writes[0].reg, 0x81);
        assert_eq!(v.writes[1].reg, 0x10080);
    }

    #[test]
    fn test_qsound_command_is_big_endian_and_rom() {
        let mut payload: Vec<u8> = Vec::new();
        payload.extend_from_slice(&0x10000u32.to_le_bytes());
        payload.extend_from_slice(&0u32.to_le_bytes());
        payload.extend_from_slice(&[0x01, 0x02]);
        let mut body: Vec<u8> = vec![0x67, 0x66, 0x8F];
        body.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        body.extend_from_slice(&payload);
        body.extend_from_slice(&[0xC4, 0x12, 0x34, 0x56]);
        let path = tmp_dir("parse_qs").join("qs.vgm");
        std::fs::write(&path, raw_vgm(&body, Some(0xB4), 60_000_000)).expect("write");
        let v = load_vgm(&path).expect("load");
        assert_eq!(v.qsound_clock, 60_000_000);
        assert_eq!(v.writes.last().unwrap().reg, 0x56);
        assert_eq!(v.writes.last().unwrap().val, 0x1234);
        assert_eq!(v.roms[0].kind, "qsound");
    }
}

mod TestFds {
    use super::*;

    fn song() -> VgmFile {
        let mut writes: Vec<ChipWrite> = (0..64)
            .map(|i| write(0, "fds", 0x40 + i as u32, (i as u32) & 0x3F, 0))
            .collect();
        writes.extend([
            write(10, "fds", 0x80, 0x80 | 20, 0),
            write(20, "fds", 0x82, 0x65, 0),
            write(21, "fds", 0x83, 0x02, 0),
            write(100, "fds", 0x80, 0x80, 0),
            write(200, "fds", 0x82, 0x80, 0),
            write(201, "fds", 0x83, 0x03, 0),
            write(400, "fds", 0x80, 0x80, 0),
        ]);
        let mut v = vgm(writes, 500, Vec::new());
        v.nes_clock = 1_789_773;
        v
    }

    #[test]
    fn test_collector() {
        let (hits, _warn) = collect_fds(&song());
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].freq, 0x265);
        assert_eq!(hits[0].volume, 20);
        assert_eq!(hits[0].off_sample, 100);
        assert_eq!(hits[1].freq, 0x380);
        assert_eq!(hits[1].off_sample, 400);
    }

    #[test]
    fn test_high_byte_before_low_byte() {
        let mut writes: Vec<ChipWrite> = (0..64)
            .map(|i| write(0, "fds", 0x40 + i as u32, (i as u32) & 0x3F, 0))
            .collect();
        writes.extend([
            write(10, "fds", 0x80, 0x80 | 20, 0),
            write(20, "fds", 0x83, 0x04, 0),
            write(20, "fds", 0x82, 0x44, 0),
        ]);
        let mut v = vgm(writes, 100, Vec::new());
        v.nes_clock = 1_789_773;
        let (hits, _warn) = collect_fds(&v);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].freq, 0x444);
    }

    #[test]
    fn test_fast_tick_lattice() {
        let mut writes: Vec<ChipWrite> = (0..64)
            .map(|i| write(0, "fds", 0x40 + i as u32, ((i as u32) * 3) & 0x3F, 0))
            .collect();
        writes.push(write(0, "fds", 0x80, 0x80 | 20, 0));
        for i in 0..100i64 {
            let t = i * 110;
            let freq = 500 + (i % 16) * 37;
            writes.push(write(t, "fds", 0x82, (freq & 0xFF) as u32, 0));
            writes.push(write(t, "fds", 0x83, ((freq >> 8) & 0x0F) as u32, 0));
        }
        let mut v = vgm(writes, 100 * 110 + 1000, Vec::new());
        v.nes_clock = 1_789_773;
        let s = analyze(&v, Default::default()).expect("analyze");
        assert_eq!(s.speed, 1);
        assert!(s.hz > 350.0);
        assert_eq!(s.fds_count, 100);
        let module = parse_fur(&write_fur(&s, Default::default())).expect("parse");
        let notes = module
            .patterns
            .values()
            .flat_map(|rows| rows.iter())
            .filter(|r| r.note != -1 && r.note != 180)
            .count();
        assert_eq!(notes, 100);
    }

    #[test]
    fn test_conversion() {
        let s = analyze(&song(), Default::default()).expect("analyze");
        let module = parse_fur(&write_fur(&s, Default::default())).expect("parse");
        assert_eq!(module.chips, vec![0x8A]);
        assert_eq!(module.n_ch, 1);
        assert!(module
            .patterns
            .values()
            .flat_map(|rows| rows.iter())
            .any(|r| r.note == 180));
    }
}

mod TestWonderSwan {
    use super::*;

    fn wave_song() -> VgmFile {
        let mut writes: Vec<ChipWrite> = (0..16)
            .map(|i| write(0, "wonderswan", 0x10000 + i as u32, (i as u32) << 4 | i as u32, 0))
            .collect();
        writes.extend([
            write(10, "wonderswan", 0x8F, 0, 0),
            write(20, "wonderswan", 0x80, 0x00, 0),
            write(21, "wonderswan", 0x81, 0x04, 0),
            write(30, "wonderswan", 0x88, 0xC0, 0),
            write(40, "wonderswan", 0x90, 0x01, 0),
            write(200, "wonderswan", 0x88, 0x00, 0),
        ]);
        let mut v = vgm(writes, 300, Vec::new());
        v.wonderswan_clock = 3_072_000;
        v
    }

    #[test]
    fn test_semitone_pitch_jump_splits_note() {
        let writes = vec![
            write(0, "wonderswan", 0x90, 0x01, 0),
            write(0, "wonderswan", 0x88, 0x88, 0),
            write(1, "wonderswan", 0x80, 0xE0, 0),
            write(2, "wonderswan", 0x81, 0x07, 0),
            write(500, "wonderswan", 0x80, 0xDE, 0),
            write(501, "wonderswan", 0x81, 0x07, 0),
        ];
        let mut v = vgm(writes, 900, Vec::new());
        v.wonderswan_clock = 3_072_000;
        let (hits, _pcm, _warn) = collect_wonderswan(&v);
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].period, 32);
        assert_eq!(hits[1].period, 34);
    }

    #[test]
    fn test_wave_note_and_cut() {
        let (hits, pcm, _warn) = collect_wonderswan(&wave_song());
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].period, 2048 - 0x400);
        assert_eq!((hits[0].vol_l, hits[0].vol_r), (12, 0));
        assert_eq!(hits[0].off_sample, 200);
        assert!(pcm.is_empty());
    }

    #[test]
    fn test_pcm_pan_decodes_low_right_bits() {
        let mut writes = vec![write(0, "wonderswan", 0x90, 0x22, 0)];
        writes.push(write(1, "wonderswan", 0x94, 0x0F, 0));
        for i in 0..20i64 {
            writes.push(write(2 + i * 4, "wonderswan", 0x89, (128 + i) as u32, 0));
        }
        writes.push(write(200, "wonderswan", 0x90, 0x00, 0));
        let mut v = vgm(writes, 300, Vec::new());
        v.wonderswan_clock = 3_072_000;
        let (_hits, pcm, _warn) = collect_wonderswan(&v);
        assert_eq!(pcm.len(), 1);
        assert_eq!(pcm[0].hits[0].pan_l, 15);
        assert_eq!(pcm[0].hits[0].pan_r, 15);
    }

    #[test]
    fn test_pcm_stream() {
        let mut writes = vec![write(0, "wonderswan", 0x90, 0x22, 0)];
        for i in 0..40i64 {
            writes.push(write(1 + i * 4, "wonderswan", 0x89, (128 + (i % 5)) as u32, 0));
        }
        writes.push(write(200, "wonderswan", 0x90, 0x00, 0));
        let mut v = vgm(writes, 300, Vec::new());
        v.wonderswan_clock = 3_072_000;
        let (_hits, pcm, _warn) = collect_wonderswan(&v);
        assert_eq!(pcm.len(), 1);
        assert!(pcm[0].data.len() > 10);
        assert!((pcm[0].rate as f64 - 44100.0 / 4.0).abs() <= 2.0);
    }

    #[test]
    fn test_conversion() {
        let s = analyze(&wave_song(), Default::default()).expect("analyze");
        let module = parse_fur(&write_fur(&s, Default::default())).expect("parse");
        assert_eq!(module.chips, vec![0x96]);
        assert_eq!(module.n_ch, 4);
    }

    #[test]
    fn test_pcm_conversion_loads_sample() {
        let mut writes = vec![write(0, "wonderswan", 0x90, 0x22, 0)];
        for i in 0..80i64 {
            writes.push(write(1 + i * 4, "wonderswan", 0x89, (128 + (i % 7)) as u32, 0));
        }
        writes.push(write(400, "wonderswan", 0x90, 0x00, 0));
        let mut v = vgm(writes, 500, Vec::new());
        v.wonderswan_clock = 3_072_000;
        let s = analyze(&v, Default::default()).expect("analyze");
        let data = write_fur(&s, Default::default());
        let module = parse_fur(&data).expect("parse");
        assert_eq!(module.chips, vec![0x96]);
        assert_eq!(module.samples.len(), 1);
        assert_eq!(info_sample_count(&data), 1);
        assert_eq!(module.samples[0].depth, 8);
    }
}

mod TestQSound {
    use super::*;

    fn rom() -> Vec<u8> {
        (0..0x1000usize).map(|i| ((i * 3) & 0xFF) as u8).collect()
    }

    fn pcm_song() -> VgmFile {
        let roms = vec![RomSlice { chip: 0, rom_size: 0x1000, start: 0, data: rom(), kind: "qsound", time: 0 }];
        let writes = vec![
            write(0, "qsound", 0x78, 0x8000, 0),
            write(1, "qsound", 0x01, 0x0100, 0),
            write(2, "qsound", 0x05, 0x0200, 0),
            write(3, "qsound", 0x04, 0x40, 0),
            write(5, "qsound", 0x03, 0x8000, 0),
            write(6, "qsound", 0x02, 4096, 0),
            write(7, "qsound", 0x06, 1200, 0),
            write(8, "qsound", 0x80, 0x14D, 0),
            write(9, "qsound", 0xBA, 896, 0),
            write(100, "qsound", 0x06, 0, 0),
        ];
        let mut v = vgm(writes, 200, roms);
        v.qsound_clock = 60_000_000;
        v
    }

    #[test]
    fn test_pcm_hit_and_linear_pan() {
        let (pcm, adpcm, samples, _warn) = collect_qsound(&pcm_song());
        assert_eq!(pcm.len(), 1);
        assert!(adpcm.is_empty());
        let hit = &pcm[0];
        assert_eq!(hit.freq, 4096);
        assert_eq!(hit.start, 0x0100);
        assert_eq!(hit.end, 0x0200);
        assert_eq!(samples[0].loop_start, 0x100 - 0x40);
        let (linear, surround) = qsound_pan(0x14D);
        assert_eq!(linear, 13);
        assert_eq!(surround, 0);
        assert_eq!(hit.off_sample, 100);
    }

    #[test]
    fn test_start_address_retriggers_while_enabled() {
        let roms = vec![RomSlice { chip: 0, rom_size: 0x1000, start: 0, data: rom(), kind: "qsound", time: 0 }];
        let writes = vec![
            write(0, "qsound", 0x78, 0x8000, 0),
            write(1, "qsound", 0x01, 0x0100, 0),
            write(2, "qsound", 0x05, 0x0200, 0),
            write(3, "qsound", 0x04, 0x40, 0),
            write(4, "qsound", 0x06, 1200, 0),
            write(5, "qsound", 0x02, 4096, 0),
            write(6, "qsound", 0x03, 0x8000, 0),
            write(1000, "qsound", 0x01, 0x0100, 0),
            write(1001, "qsound", 0x06, 900, 0),
            write(1002, "qsound", 0x02, 4096, 0),
        ];
        let mut v = vgm(writes, 1200, roms);
        v.qsound_clock = 60_000_000;
        let (pcm, _adpcm, _samples, _warn) = collect_qsound(&v);
        assert_eq!(pcm.len(), 2);
        assert_eq!(pcm[0].sample_time, 6);
        assert_eq!(pcm[1].sample_time, 1000);
    }

    #[test]
    fn test_adpcm_hit() {
        let roms = vec![RomSlice { chip: 0, rom_size: 0x10000, start: 0, data: rom(), kind: "qsound", time: 0 }];
        let writes = vec![
            write(0, "qsound", 0xCC, 0x8000, 0),
            write(1, "qsound", 0xCA, 0x0010, 0),
            write(2, "qsound", 0xCB, 0x0020, 0),
            write(3, "qsound", 0xD6, 0, 0),
            write(4, "qsound", 0xD6, 1, 0),
            write(5, "qsound", 0xCD, 16383, 0),
        ];
        let mut v = vgm(writes, 300, roms);
        v.qsound_clock = 60_000_000;
        let (pcm, adpcm, samples, _warn) = collect_qsound(&v);
        assert!(pcm.is_empty());
        assert_eq!(adpcm.len(), 1);
        assert_eq!(samples[0].kind, "adpcm");
        assert_eq!(samples[0].data.len(), 16);
        assert!(adpcm[0].off_sample > 0);
    }

    #[test]
    fn test_conversion_sample_depth_and_masks() {
        let base = pcm_song();
        let mut writes = base.writes.clone();
        let mut roms = base.roms.clone();
        roms[0] = RomSlice { chip: 0, rom_size: 0x10000, start: 0, data: rom(), kind: "qsound", time: 0 };
        writes.extend([
            write(0, "qsound", 0xCC, 0x8000, 0),
            write(1, "qsound", 0xCA, 0x0010, 0),
            write(2, "qsound", 0xCB, 0x0020, 0),
            write(3, "qsound", 0xD6, 0, 0),
            write(4, "qsound", 0xD6, 1, 0),
            write(5, "qsound", 0xCD, 16383, 0),
        ]);
        let mut v = vgm(writes, 200, roms);
        v.qsound_clock = 60_000_000;
        let s = analyze(&v, Default::default()).expect("analyze");
        let data = write_fur(&s, Default::default());
        let module = parse_fur(&data).expect("parse");
        assert_eq!(module.chips, vec![0xE0]);
        assert_eq!(module.n_ch, 19);
        assert_eq!(info_sample_count(&data) as usize, module.samples.len());
        let depths: std::collections::BTreeSet<u8> =
            module.samples.iter().map(|s| s.depth).collect();
        assert!(depths.contains(&8), "{depths:?}");
        assert!(depths.contains(&4), "{depths:?}");
        for smp in &module.samples {
            let masks: Vec<u32> = (0..16)
                .step_by(4)
                .map(|i| {
                    u32::from_le_bytes([
                        smp.presence[i],
                        smp.presence[i + 1],
                        smp.presence[i + 2],
                        smp.presence[i + 3],
                    ])
                })
                .collect();
            if smp.depth == 4 {
                assert_ne!(masks[1], 0);
                assert_eq!(masks[0], 0);
            } else {
                assert_ne!(masks[0], 0);
            }
        }
    }
}
