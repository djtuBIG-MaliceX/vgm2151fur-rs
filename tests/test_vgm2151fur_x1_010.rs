//! Seta/Allumer X1-010 VGM parsing, analysis and Furnace conversion.
//!
//! Port of `tests/test_vgm2151fur_x1_010.py`.

#![allow(non_snake_case)]

mod common;
use common::*;

use std::path::PathBuf;

use vgm2fur::analyze::{analyze, AnalyzeOptions};
use vgm2fur::furio::{decompress_fur, iter_blocks, master_volume, parse_fur};
use vgm2fur::furwrite::{write_fur, WriteOptions};
use vgm2fur::pitch::{frequency_pitch_table, furnace_frequency_note};
use vgm2fur::vgm::{load_vgm, ChipWrite, RomSlice, VgmFile};
use vgm2fur::x1_010::{collect_x1_010, x1_pcm_rate, X1_FREQ_DIV, X1_MAX_FREQ};

fn tmp_dir() -> PathBuf {
    let p = std::env::temp_dir().join("vgm2151fur_x1_010");
    std::fs::create_dir_all(&p).expect("temp dir");
    p
}

fn _write(sample: i64, reg: u32, value: u32) -> ChipWrite {
    ChipWrite::new(sample, "x1_010", 0, reg, value & 0xFF)
}

fn _vgm(writes: Vec<ChipWrite>, total: i64, clock: u32, rom: &[u8]) -> VgmFile {
    let roms = if rom.is_empty() {
        Vec::new()
    } else {
        vec![RomSlice {
            chip: 0,
            rom_size: i64::max(0x100000, rom.len() as i64),
            start: 0,
            data: rom.to_vec(),
            kind: "x1_010",
            time: 0,
        }]
    };
    VgmFile {
        path: PathBuf::from("synthetic.vgm"),
        buf: Vec::new(),
        version: 0x171,
        total_samples: total,
        loop_samples: 0,
        loop_file_pos: None,
        loop_sample: None,
        ym2151_clock: 0,
        k007232_clock: 0,
        gd3: Default::default(),
        writes,
        x1_010_clock: clock,
        roms,
        ..Default::default()
    }
}

fn _wave_setup(shape: i32, wave: i32, freq: i32, _clock: u32) -> Vec<ChipWrite> {
    let mut writes: Vec<ChipWrite> = Vec::new();
    // Envelope shape 1 lives at 0x80 on the chip; shape 0 aliases the channel
    // register block and is not used by the examples.
    for i in 0..128 {
        writes.push(_write(0, 0x80 + i, 0xFF));
    }
    for i in 0..128 {
        let v = (96.0
            * (2.0 * std::f64::consts::PI * i as f64 / 128.0).sin()) as i64;
        writes.push(_write(
            0,
            0x1000 + wave as u32 * 128 + i,
            (v & 0xFF) as u32,
        ));
    }
    for (reg, value) in [
        (0, 0),
        (1, wave),
        (2, freq & 0xFF),
        (3, (freq >> 8) & 0xFF),
        (4, 10),
        (5, shape),
    ] {
        writes.push(_write(0, reg as u32, value as u32));
    }
    writes.push(_write(0, 0, 3)); // key on, wavetable mode
    writes.push(_write(9_000, 0, 0));
    writes
}

fn default_wave_setup() -> Vec<ChipWrite> {
    _wave_setup(1, 0, 1845, 16_000_000)
}

fn write_song(song: &vgm2fur::model::Song) -> Vec<u8> {
    write_fur(
        song,
        WriteOptions { include_fm: false, ..Default::default() },
    )
}

fn contains_bytes(hay: &[u8], needle: &[u8]) -> bool {
    hay.windows(needle.len()).any(|w| w == needle)
}

fn put_u32(buf: &mut [u8], off: usize, value: u32) {
    buf[off..off + 4].copy_from_slice(&value.to_le_bytes());
}

mod TestX1Header {
    use super::*;

    #[test]
    fn test_header_clock_and_command() {
        let mut buf = vec![0u8; 0x120];
        buf[0..4].copy_from_slice(b"Vgm ");
        put_u32(&mut buf, 0x04, 0x100 + 4 + 4);
        put_u32(&mut buf, 0x08, 0x171);
        put_u32(&mut buf, 0x18, 44_100);
        put_u32(&mut buf, 0x34, 0xCC);
        put_u32(&mut buf, 0xD8, 16_666_666);
        buf[0x100..0x104].copy_from_slice(&[0xC8, 0x00, 0x03, 0x5A]);
        let path = temp_file(&tmp_dir(), "x1.vgm", &buf);
        let vgm = load_vgm(&path).expect("load");
        assert_eq!(vgm.x1_010_clock, 16_666_666);
        assert_eq!(vgm.writes.len(), 1);
        assert_eq!(vgm.writes[0].chip, "x1_010");
        assert_eq!(vgm.writes[0].reg, 3);
        assert_eq!(vgm.writes[0].val, 0x5A);
    }

    #[test]
    fn test_rom_block() {
        let data: Vec<u8> = (0..32u8).collect();
        let mut payload = Vec::new();
        payload.extend_from_slice(&0x100000u32.to_le_bytes());
        payload.extend_from_slice(&0x1000u32.to_le_bytes());
        payload.extend_from_slice(&data);
        let mut buf = vec![0u8; 0x200];
        buf[0..4].copy_from_slice(b"Vgm ");
        put_u32(&mut buf, 0x04, (0x100 + 7 + payload.len() + 1) as u32);
        put_u32(&mut buf, 0x08, 0x171);
        put_u32(&mut buf, 0x18, 44_100);
        put_u32(&mut buf, 0x34, 0xCC);
        put_u32(&mut buf, 0xD8, 16_000_000);
        buf[0x100] = 0x67;
        buf[0x101] = 0x66;
        buf[0x102] = 0x91;
        put_u32(&mut buf, 0x103, payload.len() as u32);
        buf[0x107..0x107 + payload.len()].copy_from_slice(&payload);
        buf[0x107 + payload.len()] = 0x66;
        let path = temp_file(&tmp_dir(), "x1rom.vgm", &buf);
        let vgm = load_vgm(&path).expect("load");
        assert_eq!(vgm.roms.len(), 1);
        assert_eq!(vgm.roms[0].kind, "x1_010");
        assert_eq!(vgm.roms[0].start, 0x1000);
        assert_eq!(vgm.roms[0].data, data);
    }
}

mod TestX1Pitch {
    use super::*;

    #[test]
    fn test_frequency_inverse_round_trips() {
        let tbl = frequency_pitch_table(
            16_000_000.0,
            X1_FREQ_DIV as f64,
            X1_MAX_FREQ,
        );
        for value in [4, 16, 163, 1845, 2765, 0xFFFF] {
            let (note, e5) = furnace_frequency_note(
                16_000_000.0,
                X1_FREQ_DIV as f64,
                value,
                X1_MAX_FREQ,
            );
            assert_eq!(tbl.value(note, e5 - 0x80), value);
        }
    }
}

mod TestX1Collector {
    use super::*;

    #[test]
    fn test_wave_note() {
        let (hits, pcm, _offs, warnings) =
            collect_x1_010(&_vgm(default_wave_setup(), 44_100, 16_000_000, &[]));
        assert_eq!(hits.len(), 1);
        assert_eq!(pcm.len(), 0);
        assert_eq!(warnings, Vec::<String>::new());
        assert_eq!(hits[0].wave, 0);
        assert_eq!(hits[0].env_shape, 1);
        assert_eq!(hits[0].freq, 1845);
        assert_eq!(hits[0].wave_data.len(), 128);
        assert_eq!(hits[0].env_data.len(), 128);
    }

    #[test]
    fn test_pcm_note_body_and_rate() {
        let body: Vec<u8> = (0..0x2000u32).map(|i| (i * 7) as u8).collect();
        let writes = vec![
            _write(0, 1, 0xEE),
            _write(0, 2, 16),
            _write(0, 4, 0),
            _write(0, 5, 0xFE),
            _write(0, 0, 1),
            _write(8000, 0, 0),
        ];
        let (hits, pcm, offs, _warnings) =
            collect_x1_010(&_vgm(writes, 9000, 16_000_000, &body));
        assert_eq!(hits, Vec::new());
        assert_eq!(pcm.len(), 1);
        assert_eq!(pcm[0].start, 0);
        assert_eq!(pcm[0].end, 0x2000);
        assert_eq!(pcm[0].volume, 14);
        assert_eq!(pcm[0].rate(), x1_pcm_rate(16_000_000, 16));
        assert_eq!(offs.len(), 1);
    }

    #[test]
    fn test_pcm_key_on_after_natural_end_is_a_new_note() {
        // The voice keys itself off when it reaches the ROM end, so a later
        // key-on write restarts the sample even though the status byte was
        // never rewritten to 0.
        let body: Vec<u8> = (0..0x1000u32).map(|i| (i * 7) as u8).collect();
        let writes = vec![
            _write(0, 1, 0xEE),
            _write(0, 2, 16),
            _write(0, 4, 0),
            _write(0, 5, 0xFF),
            _write(0, 0, 1),
            _write(10_000, 0, 1), // after the first natural end
            _write(10_001, 0, 1), // duplicate start burst, same note
            _write(20_000, 0, 0),
        ];
        let (hits, pcm, _offs, _warnings) =
            collect_x1_010(&_vgm(writes, 21_000, 16_000_000, &body));
        assert_eq!(hits, Vec::new());
        assert_eq!(pcm.len(), 2);
        assert_eq!(pcm[0].sample_time, 0);
        assert_eq!(pcm[1].sample_time, 10_000);
        assert!(pcm[0].natural_end() < pcm[1].sample_time);
        assert_eq!(pcm[1].natural_end(), pcm[1].sample_time + 5_780);
    }
}

mod TestX1Module {
    use super::*;

    #[test]
    fn test_wave_module_and_instrument() {
        let song = analyze(
            &_vgm(default_wave_setup(), 44_100, 16_000_000, &[]),
            AnalyzeOptions::default(),
        )
        .expect("analyze");
        assert_eq!(song.x1_010_ids, vec![0]);
        assert_eq!(song.x1_010_count, 1);
        assert_eq!(song.x1_waves.len(), 1);
        assert_eq!(song.x1_envs.len(), 1);
        assert_eq!(song.events[0].note, 105);
        let data = write_song(&song);
        assert_eq!(master_volume(&decompress_fur(&data)).expect("master volume"), 1.0);
        let mod_ = parse_fur(&data).expect("parse fur");
        assert_eq!(mod_.chips, vec![0xB0]);
        assert_eq!(mod_.n_ch, 16);
        assert_eq!(mod_.samples, Vec::new());
        // The first INS2 is the X1 wavetable instrument, type 25.
        let data = decompress_fur(&write_song(&song));
        let types: Vec<i32> = iter_blocks(&data)
            .expect("blocks")
            .into_iter()
            .filter(|b| &b.magic == b"INS2")
            .map(|b| u16::from_le_bytes([b.payload[2], b.payload[3]]) as i32)
            .collect();
        assert!(types.contains(&25));
    }

    #[test]
    fn test_pcm_module_sample_and_frequency_effect() {
        let body: Vec<u8> = (0..0x2000u32).map(|i| (i * 5) as u8).collect();
        let writes = vec![
            _write(0, 1, 0xEE),
            _write(0, 2, 16),
            _write(0, 4, 0),
            _write(0, 5, 0xFE),
            _write(0, 0, 1),
            _write(8000, 0, 0),
        ];
        let song = analyze(
            &_vgm(writes, 9000, 16_000_000, &body),
            AnalyzeOptions::default(),
        )
        .expect("analyze");
        let mod_ = parse_fur(&write_song(&song)).expect("parse fur");
        assert_eq!(mod_.chips, vec![0xB0]);
        assert_eq!(mod_.samples.len(), 1);
        assert_eq!(mod_.samples[0].depth, 8);
        assert_eq!(mod_.ins_sample, vec![0]);
        let mut rows = Vec::new();
        for ((ch, pat), pat_rows) in &mod_.patterns {
            if *ch == 0 {
                for (i, row) in pat_rows.iter().enumerate() {
                    if row.note >= 0 {
                        rows.push((*pat, i, row.note, row.fx.clone()));
                    }
                }
            }
        }
        assert!(!rows.is_empty());
        assert_eq!(rows[0].2, 60);
        assert!(rows[0].3.iter().any(|&(cmd, val)| cmd == 0x20 && val == 16));
    }

    #[test]
    fn test_repeated_wave_and_envelope_share_wavetables() {
        let mut writes = default_wave_setup();
        // Second note reuses the same wave/envelope RAM.
        writes.push(_write(12_000, 1, 0));
        writes.push(_write(12_000, 5, 1));
        writes.push(_write(12_000, 0, 3));
        writes.push(_write(20_000, 0, 0));
        let song = analyze(
            &_vgm(writes, 20_000, 16_000_000, &[]),
            AnalyzeOptions::default(),
        )
        .expect("analyze");
        assert_eq!(song.x1_010_count, 2);
        assert_eq!(song.x1_waves.len(), 1);
        assert_eq!(song.x1_envs.len(), 1);
    }

    #[test]
    fn test_stereo_envelope_levels_become_pan() {
        // Wave envelope byte 0 is a left/right nibble pair. The loud side is
        // the note level and the ratio is the 08xy pan.
        let mut writes: Vec<ChipWrite> =
            (0..128).map(|i| _write(0, 0x80 + i, 0x0F)).collect(); // L0 R15
        writes.extend_from_slice(&default_wave_setup()[128..]);
        let n = writes.len();
        writes.insert(n - 2, _write(0, 4, 0));
        let song = analyze(&_vgm(writes, 44_100, 16_000_000, &[]), AnalyzeOptions::default())
            .expect("analyze");
        let ev = song
            .events
            .iter()
            .find(|e| e.on && e.tone_chip == 11)
            .expect("x1 event");
        assert_eq!(ev.vol, 15);
        assert_eq!(ev.pan, 0x0F);
        let data = write_song(&song);
        assert!(contains_bytes(&decompress_fur(&data), b"stereo=true"));
        let mod_ = parse_fur(&data).expect("parse fur");
        let mut rows = Vec::new();
        for ((ch, _pat), pat) in &mod_.patterns {
            if *ch == 0 {
                rows.extend(pat.iter());
            }
        }
        assert!(rows.into_iter().any(|row| {
            row.note >= 0 && row.fx.iter().any(|&(cmd, val)| cmd == 0x08 && val == 0x0F)
        }));
    }

    #[test]
    fn test_pcm_register_nibbles_become_pan() {
        let body: Vec<u8> = (0..0x2000u32).map(|i| (i * 7) as u8).collect();
        let writes = vec![
            _write(0, 1, 0xE0), // PCM L14 R0
            _write(0, 2, 16),
            _write(0, 4, 0),
            _write(0, 5, 0xFE),
            _write(0, 0, 1),
            _write(8000, 0, 0),
        ];
        let song = analyze(
            &_vgm(writes, 9000, 16_000_000, &body),
            AnalyzeOptions::default(),
        )
        .expect("analyze");
        let ev = song
            .events
            .iter()
            .find(|e| e.on && e.pcm)
            .expect("pcm event");
        assert_eq!(ev.vol, 14);
        assert_eq!(ev.pan, 0xF0);
    }

    #[test]
    fn test_pitch_point_becomes_a_row_step() {
        let mut writes = default_wave_setup();
        // A mid-note retune. The collector and NoteEvent keep the raw 16-bit
        // register; when the write has its own row the builder emits an
        // E8/E9/E5 step instead of spending an instrument on the macro.
        let last = writes.split_off(writes.len() - 1);
        writes.push(_write(3000, 2, 1900u32 & 0xFF));
        writes.push(_write(3000, 3, (1900u32 >> 8) & 0xFF));
        writes.extend(last);
        let song = analyze(&_vgm(writes, 44_100, 16_000_000, &[]), AnalyzeOptions::default())
            .expect("analyze");
        let ev = song
            .events
            .iter()
            .find(|e| e.on && e.tone_chip == 11)
            .expect("x1 event");
        assert_eq!(ev.tone_pitch_pts, Vec::<(i64, i32)>::new());
        assert_eq!(ev.pitch_steps, vec![(3000, 32)]);
        assert_eq!(ev.sweep_div, 2.0);
        assert!(!song.x1_010_ins.iter().any(|ins| !ins.pitch.is_empty()));
    }

    #[test]
    fn test_row_slides_do_not_spend_the_instrument_budget() {
        // Each note writes a unique pitch trajectory. Keeping those as
        // instrument macros fills the 250-slot budget and a later note on a
        // different waveform falls back to the first instrument (the wrong
        // wave). Row steps leave one instrument per waveform.
        let mut writes: Vec<ChipWrite> = Vec::new();
        for i in 0..128 {
            writes.push(_write(0, 0x80 + i, 0xFF));
        }
        let wave0: Vec<u8> = (0..128i32)
            .map(|i| {
                ((96.0 * (2.0 * std::f64::consts::PI * i as f64 / 128.0).sin()) as i64
                    & 0xFF) as u8
            })
            .collect();
        let wave1: Vec<u8> = (0..128i32).map(|i| ((i * 2 - 128) & 0xFF) as u8).collect();
        for (wave, body) in [(0i32, &wave0), (1i32, &wave1)] {
            for (i, value) in body.iter().enumerate() {
                writes.push(_write(0, 0x1000 + wave as u32 * 128 + i as u32, *value as u32));
            }
        }
        let count = 260i64;
        for i in 0..count {
            let start = 4000 + i * 4000;
            let wave = if i == count - 1 { 1 } else { 0 };
            let base = 1800;
            let a = (i % 7) + 1;
            let b = ((i / 7) % 7) + 1;
            let c = ((i / 49) % 7) + 1;
            writes.extend([
                _write(start, 1, wave as u32),
                _write(start, 2, (base & 0xFF) as u32),
                _write(start, 3, ((base >> 8) & 0xFF) as u32),
                _write(start, 4, 10),
                _write(start, 5, 1),
                _write(start, 0, 3),
            ]);
            for (j, offset) in [a, a + b, a + b + c].iter().enumerate() {
                let freq = base + offset * 5;
                writes.push(_write(start + 800 * (j as i64 + 1), 2, (freq & 0xFF) as u32));
                writes.push(_write(
                    start + 800 * (j as i64 + 1),
                    3,
                    ((freq >> 8) & 0xFF) as u32,
                ));
            }
            writes.push(_write(start + 3600, 0, 0));
        }
        let song = analyze(
            &_vgm(writes, 4000 + count * 4000 + 1000, 16_000_000, &[]),
            AnalyzeOptions::default(),
        )
        .expect("analyze");
        let notes: Vec<_> = song
            .events
            .iter()
            .filter(|e| e.on && e.tone_chip == 11)
            .collect();
        assert_eq!(notes.len(), count as usize);
        assert!(song.x1_010_ins.len() <= 2);
        assert!(!song.x1_010_ins.iter().any(|ins| !ins.pitch.is_empty()));
        assert!(notes.iter().all(|e| !e.pitch_steps.is_empty()));
        for e in &notes {
            assert_eq!(song.x1_010_ins[e.ins as usize].wave, e.tone_wave);
        }
        assert_eq!(notes[notes.len() - 1].tone_wave, 1);
    }

    #[test]
    fn test_period_zero_envelope_shape_changes_become_volume_steps() {
        // With a zero envelope period the hardware holds the first byte of the
        // selected shape. Drivers ramp the level by switching to another
        // constant shape; that is a volume column step, not an instrument
        // macro that would be stuck at the note's starting level.
        let mut writes: Vec<ChipWrite> = Vec::new();
        for i in 0..128 {
            writes.push(_write(0, 0x80 + i, 0xFF)); // shape 1: level 15
        }
        for i in 0..128 {
            writes.push(_write(0, 0x100 + i, 0x99)); // shape 2: level 9
        }
        for i in 0..128 {
            let v = (80.0 * (2.0 * std::f64::consts::PI * i as f64 / 128.0).sin()) as i64;
            writes.push(_write(0, 0x1000 + i, (v & 0xFF) as u32));
        }
        writes.extend([
            _write(0, 1, 0),
            _write(0, 2, 1845 & 0xFF),
            _write(0, 3, (1845 >> 8) & 0xFF),
            _write(0, 4, 0),
            _write(0, 5, 1),
            _write(0, 0, 3),
            _write(3000, 5, 2),
            _write(9000, 0, 0),
        ]);
        let song = analyze(&_vgm(writes, 44_100, 16_000_000, &[]), AnalyzeOptions::default())
            .expect("analyze");
        let ev = song
            .events
            .iter()
            .find(|e| e.on && e.tone_chip == 11)
            .expect("x1 event");
        assert_eq!(ev.vol, 15);
        assert_eq!(ev.vol_pts, vec![(3000, 9, Some(0xFF))]);
        assert!(!song.x1_010_ins.iter().any(|ins| !ins.vol.is_empty()));
    }

    #[test]
    fn test_period_zero_envelope_rewrites_become_volume_steps() {
        // Replace the constant envelope with a low starting level, then
        // rewrite envelope byte 0 while the note is sounding.
        let mut writes: Vec<ChipWrite> =
            (0..128).map(|i| _write(0, 0x80 + i, 0x11)).collect();
        writes.extend_from_slice(&default_wave_setup()[128..]);
        let n = writes.len();
        writes.splice(n - 2..n - 2, [_write(0, 4, 0)]);
        let n = writes.len();
        writes.insert(n - 1, _write(3_000, 0x80, 0x99));
        let song = analyze(&_vgm(writes, 44_100, 16_000_000, &[]), AnalyzeOptions::default())
            .expect("analyze");
        let ev = song
            .events
            .iter()
            .find(|e| e.on && e.tone_chip == 11)
            .expect("x1 event");
        assert_eq!(ev.vol, 1);
        assert_eq!(ev.vol_pts, vec![(3000, 9, Some(0xFF))]);
        assert!(!song.x1_010_ins.iter().any(|ins| !ins.vol.is_empty()));
    }

    #[test]
    fn test_pitch_macros_are_not_lfo_looped() {
        let mut writes = default_wave_setup();
        let mut extra = Vec::new();
        for i in 0..20i64 {
            let value = if i & 1 != 0 { 1900 } else { 1800 };
            extra.push(_write(2_500 + i * 300, 2, (value & 0xFF) as u32));
            extra.push(_write(2_500 + i * 300, 3, ((value >> 8) & 0xFF) as u32));
        }
        let n = writes.len();
        writes.splice(n - 1..n - 1, extra);
        let song = analyze(&_vgm(writes, 44_100, 16_000_000, &[]), AnalyzeOptions::default())
            .expect("analyze");
        assert!(song.x1_010_ins.iter().all(|ins| ins.pitch_loop == 255));
    }

    #[test]
    fn test_pcm_only_has_no_wave_assets() {
        let body: Vec<u8> = (0..0x2000u32).map(|i| (i * 3) as u8).collect();
        let writes = vec![
            _write(0, 1, 0xEE),
            _write(0, 2, 16),
            _write(0, 4, 0),
            _write(0, 5, 0xFE),
            _write(0, 0, 1),
            _write(7000, 0, 0),
        ];
        let song = analyze(
            &_vgm(writes, 8000, 16_000_000, &body),
            AnalyzeOptions::default(),
        )
        .expect("analyze");
        assert_eq!(song.x1_010_ins, Vec::new());
        assert_eq!(song.x1_waves, Vec::<Vec<u8>>::new());
        assert_eq!(song.x1_envs, Vec::<Vec<u8>>::new());
        let mod_ = parse_fur(&write_song(&song)).expect("parse fur");
        assert_eq!(mod_.samples.len(), 1);
        let data = decompress_fur(&write_song(&song));
        assert!(!contains_bytes(&data, b"WAVE"));
    }

    #[test]
    fn test_clock_select_flag() {
        let song = analyze(
            &_vgm(_wave_setup(1, 0, 1845, 16_666_666), 44_100, 16_666_666, &[]),
            AnalyzeOptions::default(),
        )
        .expect("analyze");
        let data = decompress_fur(&write_song(&song));
        assert!(contains_bytes(&data, b"clockSel=1"));
    }
}
