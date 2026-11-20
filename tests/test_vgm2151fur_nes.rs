//! NES APU tone/DPCM and Konami VRC7 detection/emission.
//!
//! Port of `tests/test_vgm2151fur_nes.py`.

mod common;
use common::*;

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use vgm2fur::analyze::{_absorb_detune, analyze, AnalyzeOptions};
use vgm2fur::furio::{iter_blocks, parse_fur, FurModule};
use vgm2fur::furwrite::{
    write_fur, WriteOptions, CHIP_NES, CHIP_VRC7, INS_C352, INS_NES, SMP_DEPTH_DPCM,
};
use vgm2fur::model::{NoteEvent, Song};
use vgm2fur::nes::{
    cluster_nes_pcm, collect_nes_dpcm, collect_nes_pcm, collect_nes_tones, nes_note, NesPcmHit,
    NesPcmSample,
};
use vgm2fur::vgm::{load_vgm, ChipWrite, RomSlice, VgmFile};

fn tmp_dir() -> PathBuf {
    let p = std::env::temp_dir().join("vgm2151fur_nes");
    std::fs::create_dir_all(&p).expect("temp dir");
    p
}

fn nes_vgm(writes: Vec<ChipWrite>) -> VgmFile {
    vgm(writes)
}

fn nes_vgm_roms(writes: Vec<ChipWrite>, roms: Vec<RomSlice>) -> VgmFile {
    let mut v = nes_vgm(writes);
    v.roms = roms;
    v
}

fn raw_vgm_nes(
    cmds: &[u8],
    rom_blocks: &[(u8, i64, Vec<u8>)],
    nes_clock: u32,
    ym2413: u32,
    extra_blocks: &[u8],
) -> Vec<u8> {
    let mut buf = vec![0u8; 0xC0];
    buf[0..4].copy_from_slice(b"Vgm ");
    buf[8..12].copy_from_slice(&0x161u32.to_le_bytes());
    buf[0x10..0x14].copy_from_slice(&(ym2413 & 0xFFFF_FFFF).to_le_bytes());
    buf[0x18..0x1C].copy_from_slice(&44100u32.to_le_bytes());
    buf[0x34..0x38].copy_from_slice(&(0xC0u32 - 0x34).to_le_bytes());
    // The NES clock lives at 0x84; 0x80 is the Game Boy DMG clock.
    buf[0x84..0x88].copy_from_slice(&(nes_clock & 0xFFFF_FFFF).to_le_bytes());
    let mut body: Vec<u8> = extra_blocks.to_vec();
    for (typ, start, data) in rom_blocks {
        // Database blocks (< 0x40) carry no start address; ROM/RAM dumps do.
        let mut payload: Vec<u8> = Vec::new();
        if *typ < 0x40 {
            payload.extend_from_slice(data);
        } else {
            payload.extend_from_slice(&((*start & 0xFFFF) as u16).to_le_bytes());
            payload.extend_from_slice(data);
        }
        body.extend_from_slice(&[0x67, 0x66, *typ]);
        body.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        body.extend_from_slice(&payload);
    }
    body.extend_from_slice(cmds);
    body.push(0x66);
    buf.extend_from_slice(&body);
    buf
}

fn nes(sample: i64, reg: u32, val: u32) -> ChipWrite {
    ChipWrite::new(sample, "nes", 0, reg, val)
}

fn nes_chip(sample: i64, reg: u32, val: u32, chip_id: u8) -> ChipWrite {
    ChipWrite::new(sample, "nes", chip_id, reg, val)
}

fn rom(data: Vec<u8>) -> RomSlice {
    RomSlice {
        chip: 0,
        rom_size: data.len() as i64,
        start: 0xC000,
        data,
        kind: "nes_dpcm",
        time: 0,
    }
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

fn contains_bytes(hay: &[u8], needle: &[u8]) -> bool {
    hay.windows(needle.len()).any(|w| w == needle)
}

fn all_fx(mod_: &FurModule) -> Vec<(u8, u8)> {
    mod_.patterns
        .values()
        .flat_map(|rows| rows.iter())
        .flat_map(|r| r.fx.iter().copied())
        .collect()
}

fn order4_notes(mod_: &FurModule) -> Vec<i32> {
    let mut notes = Vec::new();
    for &oi in &mod_.orders[4] {
        if let Some(rows) = mod_.patterns.get(&(4, oi)) {
            notes.extend(rows.iter().filter(|r| r.note >= 0).map(|r| r.note));
        }
    }
    notes
}

mod test_nes_parsing {
    use super::*;

    #[test]
    fn test_register_and_dpcm_ram() {
        let raw = raw_vgm_nes(
            &[0xB4, 0x00, 0x30, 0xB4, 0x15, 0x1F],
            &[(0xC2, 0xC000, (0..64).collect())],
            1789773,
            0,
            &[],
        );
        let p = temp_file(&tmp_dir(), "parse_nes.vgm", &raw);
        let v = load_vgm(&p).unwrap();
        assert_eq!(v.nes_clock, 1789773);
        assert_eq!(
            v.writes
                .iter()
                .filter(|w| w.chip == "nes")
                .map(|w| (w.reg, w.val))
                .collect::<Vec<_>>(),
            vec![(0x00, 0x30), (0x15, 0x1F)],
        );
        assert_eq!(
            v.roms.iter().map(|r| r.kind).collect::<Vec<_>>(),
            vec!["nes_dpcm"],
        );
        assert_eq!(v.roms[0].start, 0xC000);
        assert_eq!(v.roms[0].data.len(), 64);
    }

    #[test]
    fn test_vrc7_bit() {
        let raw = raw_vgm_nes(&[], &[], 1789773, 0x80000000 | 3579545, &[]);
        let p = temp_file(&tmp_dir(), "vrc7.vgm", &raw);
        let v = load_vgm(&p).unwrap();
        assert!(v.ym2413_vrc7);
        assert_eq!(v.ym2413_clock, 3579545);

        let raw = raw_vgm_nes(&[], &[], 1789773, 3579545, &[]);
        let p = temp_file(&tmp_dir(), "ym2413.vgm", &raw);
        let v = load_vgm(&p).unwrap();
        assert!(!v.ym2413_vrc7);
    }
}

mod test_dpcm_bank {
    use super::*;

    #[test]
    fn test_type07_bank_and_68_ram_write() {
        // A 4 KB window copied from the type-0x07 database into $C000.
        let raw = raw_vgm_nes(
            &[
                0x68, 0x00, 0x07, 0x00, 0x00, 0x00, // type 7, src 0
                0x00, 0xC0, 0x00, // dest $C000
                0x04, 0x00, 0x00, // 4 bytes
                0xB4, 0x10, 0x0E, 0xB4, 0x12, 0x00, 0xB4, 0x13, 0x00, 0xB4, 0x15, 0x1F,
            ],
            &[(0x07, 0x0000, vec![0xAA, 0x55, 0x01, 0x80])],
            1789773,
            0,
            &[],
        );
        let p = temp_file(&tmp_dir(), "dpcm_bank.vgm", &raw);
        let v = load_vgm(&p).unwrap();
        assert_eq!(
            &v.nes_dpcm_bank[..4],
            &[0xAA, 0x55, 0x01, 0x80],
        );
        assert_eq!(v.pcm_ram_writes.len(), 1);
        let (samples, hits, warn) = collect_nes_dpcm(&v, None);
        assert_eq!(samples.len(), 1);
        // $4013 = 0 is the minimum DMC length: one byte.
        assert_eq!(samples[0].data, b"\xAA".to_vec());
        assert_eq!(hits.len(), 1);
        assert_eq!(warn, Vec::<String>::new());
    }

    #[test]
    fn test_dpcm_ram_reload_follows_stream_time() {
        // Gimmick! re-dumps $C000/$D000 while the song plays. A hit must read
        // the window that was loaded at that point, not the last one in the
        // file (which made most DPCM notes play the wrong sample).
        fn dump_block(data: &[u8], addr: i64) -> Vec<u8> {
            let mut payload = ((addr & 0xFFFF) as u16).to_le_bytes().to_vec();
            payload.extend_from_slice(data);
            let mut out = vec![0x67, 0x66, 0xC2];
            out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
            out.extend_from_slice(&payload);
            out
        }

        let mut cmds: Vec<u8> = vec![
            0xB4, 0x10, 0x0E, 0xB4, 0x12, 0x00, 0xB4, 0x13, 0x00, 0xB4, 0x15,
            0x1F, // hit 1 at t=0
            0x61, 0x10, 0x00, // wait 16 samples
        ];
        cmds.extend_from_slice(&dump_block(&[0x55; 64], 0xC000)); // reload at t=16
        cmds.extend_from_slice(&[
            0xB4, 0x15, 0x0F, // stop
            0xB4, 0x13, 0x00, 0xB4, 0x15, 0x1F, // hit 2 at t=16
        ]);
        let raw = raw_vgm_nes(&cmds, &[(0xC2, 0xC000, vec![0xAA; 64])], 1789773, 0, &[]);
        let p = temp_file(&tmp_dir(), "dpcm_reload.vgm", &raw);
        let v = load_vgm(&p).unwrap();
        assert_eq!(v.roms.iter().map(|r| r.time).collect::<Vec<_>>(), vec![0, 16]);
        let (samples, hits, warn) = collect_nes_dpcm(&v, None);
        assert_eq!(hits.iter().map(|h| h.sample_time).collect::<Vec<_>>(), vec![0, 16]);
        assert_eq!(
            samples.iter().map(|s| s.data.clone()).collect::<Vec<_>>(),
            vec![b"\xAA".to_vec(), b"\x55".to_vec()],
        );
        assert_eq!(warn, Vec::<String>::new());
    }

    #[test]
    fn test_trigger_without_data_warns() {
        let raw = raw_vgm_nes(
            &[
                0xB4, 0x10, 0x0E, 0xB4, 0x12, 0x00, 0xB4, 0x13, 0x00, 0xB4, 0x15, 0x1F,
            ],
            &[],
            1789773,
            0,
            &[],
        );
        let p = temp_file(&tmp_dir(), "dpcm_nodata.vgm", &raw);
        let (_samples, hits, warn) = collect_nes_dpcm(&load_vgm(&p).unwrap(), None);
        assert_eq!(hits.len(), 1);
        assert!(warn.iter().any(|w| w.contains("no DPCM data block")));
    }
}

mod test_sunsoft5b {
    use super::*;

    fn song(nes: bool) -> Song {
        let period: u32 = 0x100;
        let mut writes = vec![
            ChipWrite::new(0, "ay8910", 0, 7, 0x38),
            ChipWrite::new(0, "ay8910", 0, 0, period & 0xFF),
            ChipWrite::new(0, "ay8910", 0, 1, (period >> 8) & 0x0F),
            ChipWrite::new(0, "ay8910", 0, 8, 0x0F),
        ];
        if nes {
            writes.push(ChipWrite::new(0, "nes", 0, 0x15, 0x00));
        }
        // No pin-26 flag: the Famicom pairing alone infers the 5B.
        let mut v = nes_vgm(writes);
        v.ay8910_clock = 1789773;
        v.ay8910_type = 0x10;
        v.ay8910_flags = 0;
        v.nes_clock = if nes { 1789773 } else { 0 };
        analyze(&v, AnalyzeOptions::default()).unwrap()
    }

    #[test]
    fn test_famicom_ay_is_sunsoft5b_and_one_octave_lower() {
        let plain = song(false);
        let fami = song(true);
        let pn = plain
            .events
            .iter()
            .find(|e| e.on && e.tone_chip == 2)
            .unwrap();
        let fn_ = fami
            .events
            .iter()
            .find(|e| e.on && e.tone_chip == 2)
            .unwrap();
        // The 5B halves the clock: its note is an octave below the same
        // register on a full-clock YM2149.
        assert_eq!(
            (pn.note * 64 + pn.e5) - (fn_.note * 64 + fn_.e5),
            12 * 64,
        );
        let data = write_song(&fami);
        assert!(contains_bytes(&data, b"chipType=2"));
        assert!(contains_bytes(&data, b"customClock=1789773"));
    }
}

mod test_nes_tones {
    use super::*;

    #[test]
    fn test_pulse_duty_and_volume() {
        // Key pulse 1: $4003 loads the length and frequency high byte, $4000
        // sets duty 2 and volume 9.
        let writes = vec![
            nes(1000, 0x15, 0x0F),
            nes(1000, 0x02, 0x40),
            nes(1000, 0x03, 0x09),
            nes(1000, 0x00, 0xB9), // duty 2, constant, halt, volume 9
            nes(2000, 0x00, 0xB4),
            nes(3000, 0x00, 0xB0),
        ];
        let (hits, offs, warn) = collect_nes_tones(&nes_vgm(writes), None);
        assert_eq!(hits.len(), 1);
        let hit = &hits[0];
        assert_eq!(hit.voice, 0);
        assert_eq!(hit.period, 0x140);
        assert_eq!(hit.duty, 2);
        assert_eq!(hit.vol, 9);
        assert_eq!(hit.vol_pts, vec![(2000i64, 4i32)]);
        assert_eq!(offs[0].sample_time, 3000);
        assert_eq!(warn, Vec::<String>::new());
    }

    #[test]
    fn test_triangle_linear_and_noise_mode() {
        let writes = vec![
            nes(500, 0x15, 0x0F),
            nes(500, 0x08, 0x81),
            nes(500, 0x0A, 0x34),
            nes(500, 0x0B, 0x01),
            nes(500, 0x0C, 0x30),
            nes(500, 0x0E, 0x87), // short mode, period 7
            nes(600, 0x0C, 0x3A), // noise volume 10
        ];
        let (hits, _offs, _warn) = collect_nes_tones(&nes_vgm(writes), None);
        let tri = hits.iter().find(|h| h.voice == 2).unwrap();
        assert_eq!(tri.period, 0x134);
        assert_eq!(tri.linear, 0x81);
        let noise = hits.iter().find(|h| h.voice == 3).unwrap();
        assert_eq!(noise.duty, 1);
        assert_eq!(noise.period, 7);
        assert_eq!(noise.vol, 10);
    }

    #[test]
    fn test_period_jump_is_a_new_note() {
        let mut writes = vec![
            nes(1000, 0x15, 0x0F),
            nes(1000, 0x00, 0xBF),
            nes(1000, 0x02, 0x00),
            nes(1000, 0x03, 0x01),
        ];
        writes.extend_from_slice(&[
            nes(5000, 0x02, 0x08),
            nes(5000, 0x03, 0x00), // octave up
        ]);
        let (hits, offs, _warn) = collect_nes_tones(&nes_vgm(writes), None);
        assert_eq!(hits.len(), 2);
        assert!(hits[0].period > hits[1].period);
    }

    #[test]
    fn test_parking_a_voice_below_the_audible_floor_is_silence() {
        // The console's output filter drops anything above ~16 kHz.  A driver
        // that parks an unused voice at timer 0 is not playing the top note of
        // Furnace's range, so the write ends the note instead of starting one.
        let writes = vec![
            nes(1000, 0x15, 0x0F),
            nes(1000, 0x00, 0xBF),
            nes(1000, 0x02, 0x00),
            nes(1000, 0x03, 0x01),
            nes(5000, 0x02, 0x00),
            nes(5000, 0x03, 0x00),
        ];
        let (hits, offs, _warn) = collect_nes_tones(&nes_vgm(writes), None);
        assert_eq!(
            hits.iter().map(|h| (h.voice, h.period)).collect::<Vec<_>>(),
            vec![(0, 0x100)],
        );
        assert_eq!(offs.iter().map(|o| o.sample_time).collect::<Vec<_>>(), vec![5000]);
    }

    #[test]
    fn test_zero_length_load_is_a_noise_hit() {
        // ``$400F = 0`` loads the table's index 0, which is 10 half-frames: a
        // driver that marks each drum with that write gets an 83 ms burst, not
        // silence.  The reference player gates the same way.
        let writes = vec![
            nes(1000, 0x0C, 0x0F),
            nes(1000, 0x0E, 0x00),
            nes(1000, 0x0F, 0x00),
        ];
        let (hits, offs, _warn) = collect_nes_tones(&nes_vgm(writes), None);
        assert_eq!(
            hits.iter().map(|h| (h.voice, h.period)).collect::<Vec<_>>(),
            vec![(3, 0)],
        );
        assert_eq!(
            offs.iter().map(|o| o.sample_time).collect::<Vec<_>>(),
            vec![1000 + 3675],
        );
    }

    #[test]
    fn test_noise_hits_retrigger_after_the_gate_ran_out() {
        // Each hit is marked by a ``$400F`` write; the previous 83 ms gate has
        // already ended, so the write starts a new one.
        let writes = vec![
            nes(1000, 0x0C, 0x0F),
            nes(1000, 0x0E, 0x00),
            nes(1000, 0x0F, 0x00),
            nes(21000, 0x0E, 0x00),
            nes(21000, 0x0F, 0x00),
        ];
        let (hits, offs, _warn) = collect_nes_tones(&nes_vgm(writes), None);
        assert_eq!(
            hits.iter().map(|h| h.sample_time).collect::<Vec<_>>(),
            vec![1000, 21000],
        );
        assert_eq!(
            offs.iter().map(|o| o.sample_time).collect::<Vec<_>>(),
            vec![4675, 24675],
        );
    }

    #[test]
    fn test_noise_length_reload_every_frame_stays_one_note() {
        // A driver that holds the noise alive by rewriting the length register
        // must not become one note per frame: the gate never runs out.  It ends
        // one gate after the last rewrite.
        let mut writes = vec![nes(1000, 0x0C, 0x0F), nes(1000, 0x0E, 0x00)];
        for i in 0..20 {
            writes.push(nes(1000 + i * 1000, 0x0F, 0xF8));
        }
        let (hits, offs, _warn) = collect_nes_tones(&nes_vgm(writes), None);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].sample_time, 1000);
        // Index 31 is 32 half-frames, so the last reload at 20000 ends at 31760.
        assert_eq!(
            offs.iter().map(|o| o.sample_time).collect::<Vec<_>>(),
            vec![31760],
        );
    }

    #[test]
    fn test_envelope_mode_becomes_a_constant_peak_with_a_decay() {
        // Furnace's NES envelope runs once and a later note-on does not restart
        // it, so a channel whose driver writes $4000 once would fall silent
        // after the first decay.  Keep the peak as a constant level and carry
        // the decay on the volume column at the hardware rate.
        let writes = vec![
            nes(1000, 0x00, 0x8F),
            nes(1000, 0x02, 0xCA),
            nes(1000, 0x03, 0xC0),
        ];
        let song = analyze(&nes_vgm(writes), AnalyzeOptions::default()).unwrap();
        let ev = song
            .events
            .iter()
            .find(|e| e.on && e.tone_chip == 4)
            .unwrap();
        assert_eq!(ev.vol, 15); // the envelope's peak, not its period
        let levels: Vec<i32> = ev.vol_pts.iter().map(|p| p.1).collect();
        assert!(levels.len() > 10);
        assert_eq!(levels[0], 15);
        assert_eq!(*levels.last().unwrap(), 1);
        let mut sorted = levels.clone();
        sorted.sort();
        sorted.reverse();
        assert_eq!(levels, sorted);
        // Period 15: one step per 16 quarter frames (240 Hz), so ~1 s in all.
        let delta = (ev.vol_pts.last().unwrap().0 - ev.sample) as f64;
        assert!(
            (delta - 14.0 * 16.0 * (44100.0 / 240.0)).abs() <= 3.0,
            "{delta} vs {}",
            14.0 * 16.0 * (44100.0 / 240.0),
        );
        let data = write_song(&song);
        let mod_ = parse_fur(&data).unwrap();
        let fx = all_fx(&mod_);
        assert!(fx.contains(&(0x15, 0x03))); // constant volume + length halt
        let vols: Vec<i32> = mod_
            .patterns
            .values()
            .flat_map(|rows| rows.iter())
            .filter(|r| r.vol >= 0)
            .map(|r| r.vol)
            .collect();
        assert!(vols.contains(&15));
        assert!(vols.iter().any(|&v| v < 15));
    }

    #[test]
    fn test_triangle_notes_end_at_the_linear_counter() {
        // The triangle is gated by its linear counter ((value/240) s), which the
        // reference player honours: a note the driver does not retrigger in time
        // ends there instead of ringing on.  $4008 = 0x2F is 47 quarter frames,
        // 8,636 samples.
        let writes = vec![
            nes(1000, 0x08, 0x2F),
            nes(1000, 0x0A, 0xFD),
            nes(1000, 0x0B, 0xC1),
            nes(22050, 0x0A, 0xA7),
            nes(22050, 0x0B, 0xC1),
        ];
        let (hits, offs, _warn) = collect_nes_tones(&nes_vgm(writes), None);
        assert_eq!(
            hits.iter().map(|h| h.sample_time).collect::<Vec<_>>(),
            vec![1000, 22050],
        );
        assert_eq!(hits[0].off_sample, 1000 + 8636);
        assert_eq!(hits[1].off_sample, 22050 + 8636);
        assert_eq!(
            offs.iter().map(|o| o.sample_time).collect::<Vec<_>>(),
            vec![1000 + 8636, 22050 + 8636],
        );
    }

    #[test]
    fn test_low_byte_first_does_not_split_a_parked_voice() {
        // 270 -> 0 is written as low 0x00 then high 0xC0.  Reading the low write
        // alone turns 270 into 256, and a "held step" split closed the note
        // without the note-off the parked write should give (Santa Claus' second
        // APU: its pulses lost exactly the offs the first chip had).
        let writes = vec![
            nes(1000, 0x00, 0xBF),
            nes(1000, 0x02, 0x0E),
            nes(1000, 0x03, 0xC1),
            nes(20000, 0x02, 0x00),
            nes(20000, 0x03, 0xC0),
        ];
        let (hits, offs, _warn) = collect_nes_tones(&nes_vgm(writes), None);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].period, 0x10E);
        assert_eq!(offs.iter().map(|o| o.sample_time).collect::<Vec<_>>(), vec![20000]);
    }

    #[test]
    fn test_module_halts_a_constant_volume_note() {
        // Furnace reloads the length counter on note-on, so an un-halted note
        // dies after a fraction of a second.  A constant-volume voice can take
        // the halt bit without side effects, so the note lasts until the
        // driver's next write.
        let writes = vec![
            nes(1000, 0x15, 0x0F),
            nes(1000, 0x00, 0xBF),
            nes(1000, 0x02, 0x00),
            nes(1000, 0x03, 0x01),
        ];
        let song = analyze(&nes_vgm(writes), AnalyzeOptions::default()).unwrap();
        let mod_ = parse_fur(&write_song(&song)).unwrap();
        let fx = all_fx(&mod_);
        assert!(fx.contains(&(0x15, 0x03)));
    }

    #[test]
    fn test_module_holds_the_triangle_linear_counter() {
        // Without the control bit the linear counter runs out after
        // (value/240) s and the note stops between the driver's writes.
        let writes = vec![
            nes(1000, 0x08, 0x3C),
            nes(1000, 0x0A, 0x5D),
            nes(1000, 0x0B, 0xC2),
        ];
        let song = analyze(&nes_vgm(writes), AnalyzeOptions::default()).unwrap();
        let mod_ = parse_fur(&write_song(&song)).unwrap();
        let fx = all_fx(&mod_);
        assert!(fx.contains(&(0x19, 0xBC)));
    }

    #[test]
    fn test_high_byte_before_low_completes_the_pitch() {
        // Gimmick! writes $400B (timer high) before $400A (timer low).  The
        // low write must re-read the period or the note keeps the stale low
        // byte from the previous note.
        let writes = vec![
            nes(1000, 0x15, 0x0F),
            nes(1000, 0x08, 0xFF),
            nes(1000, 0x0B, 0x01),
            nes(1000, 0x0A, 0xC9),
        ];
        let (hits, offs, _warn) = collect_nes_tones(&nes_vgm(writes), None);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].period, 0x1C9);
    }

    #[test]
    fn test_same_pitch_rewrites_stay_one_note() {
        let mut writes = vec![
            nes(1000, 0x15, 0x0F),
            nes(1000, 0x00, 0xBF),
            nes(1000, 0x02, 0x40),
            nes(1000, 0x03, 0x09),
        ];
        for t in (2000..8000).step_by(735) {
            writes.extend_from_slice(&[
                nes(t, 0x00, 0xB9),
                nes(t, 0x02, 0x40),
                nes(t, 0x03, 0x09),
            ]);
        }
        let (hits, offs, _warn) = collect_nes_tones(&nes_vgm(writes), None);
        assert_eq!(hits.len(), 1);
    }

    #[test]
    fn test_pulse_volume_reattack_starts_a_new_note() {
        let writes = vec![
            nes(0, 0x15, 0x0F),
            nes(1000, 0x02, 0x80),
            nes(1000, 0x03, 0x08),
            nes(1000, 0x00, 0xB9),
            nes(5000, 0x00, 0xB2), // gate the level down
            nes(6000, 0x00, 0xB9), // re-attack at the same pitch
            nes(9000, 0x00, 0xB0),
        ];
        let (hits, offs, _warn) = collect_nes_tones(&nes_vgm(writes), None);
        assert_eq!(
            hits.iter().map(|h| h.sample_time).collect::<Vec<_>>(),
            vec![1000, 6000],
        );
        assert_eq!(hits[1].vol, 9);
    }

    #[test]
    fn test_pulse_swell_from_a_sounding_level_stays_one_note() {
        let writes = vec![
            nes(0, 0x15, 0x0F),
            nes(1000, 0x02, 0x80),
            nes(1000, 0x03, 0x08),
            nes(1000, 0x00, 0xB6), // volume 6
            nes(2000, 0x00, 0xBA), // swell to 10, same pitch
            nes(9000, 0x00, 0xB0),
        ];
        let (hits, offs, _warn) = collect_nes_tones(&nes_vgm(writes), None);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].vol_pts, vec![(2000i64, 10i32)]);
    }

    #[test]
    fn test_pulse_held_pitch_step_is_a_new_note() {
        // Gimmick! changes the period by under a semitone between sustained
        // notes. Keeping that as a pitch macro made one instrument play
        // several source notes.
        let writes = vec![
            nes(0, 0x15, 0x0F),
            nes(1000, 0x02, 0x50),
            nes(1000, 0x03, 0x08),
            nes(1000, 0x00, 0xB9),
            nes(8000, 0x02, 0x54),
            nes(20000, 0x00, 0xB0),
        ];
        let (hits, offs, _warn) = collect_nes_tones(&nes_vgm(writes), None);
        assert_eq!(
            hits.iter().map(|h| h.period).collect::<Vec<_>>(),
            vec![0x50, 0x54],
        );
    }

    #[test]
    fn test_pulse_vibrato_does_not_split() {
        let mut writes = vec![
            nes(0, 0x15, 0x0F),
            nes(1000, 0x02, 0x50),
            nes(1000, 0x03, 0x08),
            nes(1000, 0x00, 0xB9),
        ];
        let mut t = 2000i64;
        for per in [0x54u32, 0x50, 0x54, 0x50, 0x54, 0x50] {
            writes.push(nes(t, 0x02, per));
            t += 700;
        }
        writes.push(nes(t, 0x00, 0xB0));
        let (hits, offs, _warn) = collect_nes_tones(&nes_vgm(writes), None);
        assert_eq!(hits.len(), 1);
        assert!(!hits[0].pitch_pts.is_empty());
        // The return to the base pitch must still be a pitch point, or the
        // macro would hold the bend.
        assert!(hits[0].pitch_pts.contains(&(2700, 0x50)));
    }

    #[test]
    fn test_note_mapping_round_trips() {
        for period in [0, 1, 0x100, 0x400, 0x7FF] {
            let (note, e5) = nes_note(1789773, period);
            assert!(note >= 0);
            assert!(note <= 179);
            assert!(e5 >= 0);
            assert!(e5 <= 0xFF);
        }
    }
}

mod test_nes_pitch_motion {
    use super::*;

    fn event(e5: i32) -> NoteEvent {
        NoteEvent {
            sample: 0,
            ch: 13,
            on: true,
            note: 112,
            ins: 0,
            vol: 15,
            e5,
            pan: 0,
            ..Default::default()
        }
    }

    #[test]
    fn test_delayed_pitch_change_stays_a_bend() {
        // Gimmick!'s triangle holds one period and then moves once near the
        // end of the note. Folding that into E5 retunes the whole note.
        let mut ev = event(119);
        let pitch = _absorb_detune(&mut ev, &[0, 0, 0, 60, 60], 1);
        assert_eq!(pitch, vec![0, 0, 0, 60, 60]);
        assert_eq!((ev.note, ev.e5), (112, 119));
    }

    #[test]
    fn test_pitch_that_settles_on_the_first_tick_is_the_note() {
        let mut ev = event(119);
        let pitch = _absorb_detune(&mut ev, &[0, 60, 60], 1);
        assert_eq!(pitch, Vec::<i32>::new());
        assert_eq!(ev.e5, 119 + 60);
    }

    #[test]
    fn test_mid_row_channel_effect_does_not_delay_the_note() {
        // Gimmick!'s intro opens pulse 2 on row 0 and changes duty later in
        // the same row. The row's ED must not push the note to that later
        // tick; the duty change moves to the next row instead.
        let writes = vec![
            nes(0, 0x15, 0x0F),
            nes(44, 0x00, 0xB9),
            nes(44, 0x02, 0x1D),
            nes(44, 0x03, 0x09),
            nes(753, 0x00, 0x39), // duty 2 -> 0, still inside row 0
            nes(2000, 0x00, 0x30),
        ];
        let mut song = analyze(&nes_vgm(writes), AnalyzeOptions::default()).unwrap();
        // Force the grid the source uses: 9 ticks of 91.7 samples per row.
        song.hz = 44100.0 / 91.7;
        song.speed = 9;
        song.t0 = 0;
        let mod_ = parse_fur(&write_song(&song)).unwrap();
        // Pulse 1 is channel 0 in a NES-only module.
        let row0 = &mod_.patterns.get(&(0u16, 0u16)).unwrap()[0];
        let row1 = &mod_.patterns.get(&(0u16, 0u16)).unwrap()[1];
        assert!(row0.note >= 0);
        assert!(
            !row0.fx.iter().any(|f| f.0 == 0xED),
            "a mid-row duty change must not delay the note",
        );
        assert!(row0.fx.contains(&(0x12, 2)));
        assert!(row1.fx.contains(&(0x12, 0)));
    }

    #[test]
    fn test_triangle_note_keeps_its_starting_pitch() {
        let writes = vec![
            nes(1000, 0x15, 0x0F),
            nes(1000, 0x08, 0xFF),
            nes(1000, 0x0A, 169),
            nes(1000, 0x0B, 0x00),
            nes(40000, 0x0A, 160), // one step down, long after the start
            nes(80000, 0x08, 0x00),
        ];
        let song = analyze(&nes_vgm(writes), AnalyzeOptions::default()).unwrap();
        let tri = song
            .events
            .iter()
            .find(|e| e.on && e.tone_chip == 4 && e.note < 150)
            .unwrap();
        assert_eq!((tri.note, tri.e5), nes_note(1789773, 169));
        assert_eq!(tri.tone_pitch_pts, vec![(40000i64, 160i32)]);
        assert!(!song.nes_ins.is_empty());
        assert!(!song.nes_ins[0].pitch.is_empty());
    }
}

mod test_nes_dpcm {
    use super::*;

    fn _rom(data: Vec<u8>) -> RomSlice {
        rom(data)
    }

    #[test]
    fn test_dpcm_sample_body() {
        let mut data = vec![0u8; 0x2000];
        let body: Vec<u8> = [0xAAu8, 0x55].repeat(8);
        data[0x440..0x440 + body.len()].copy_from_slice(&body);
        let writes = vec![
            nes(1000, 0x10, 0x0E),
            nes(1000, 0x12, 0x11), // $C000 + 0x11*64 = $C440
            nes(1000, 0x13, 0x00), // 1 byte? length 16*0+1
            nes(1000, 0x15, 0x1F),
            nes(2000, 0x15, 0x0F),
        ];
        let (samples, hits, warn) =
            collect_nes_dpcm(&nes_vgm_roms(writes, vec![_rom(data)]), None);
        assert_eq!(samples.len(), 1);
        assert_eq!(samples[0].data, body[..1].to_vec());
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].rate_index, 0x0E);
        assert_eq!(hits[0].end_time, 2000);
        assert_eq!(warn, Vec::<String>::new());
    }

    #[test]
    fn test_dpcm_loop_bit() {
        let data = vec![0u8; 0x100];
        let writes = vec![
            nes(1000, 0x10, 0x4F), // rate F, loop
            nes(1000, 0x12, 0x00),
            nes(1000, 0x13, 0x01),
            nes(1000, 0x15, 0x1F),
        ];
        let (samples, _hits, _warn) =
            collect_nes_dpcm(&nes_vgm_roms(writes, vec![_rom(data)]), None);
        assert!(samples[0].loop_);
    }

    #[test]
    fn test_undumped_dpcm_warns() {
        let writes = vec![
            nes(1000, 0x10, 0x0E),
            nes(1000, 0x12, 0xF0),
            nes(1000, 0x13, 0x01),
            nes(1000, 0x15, 0x1F),
        ];
        let (_s, hits, warn) =
            collect_nes_dpcm(&nes_vgm_roms(writes, vec![_rom(vec![0u8; 0x100])]), None);
        assert_eq!(hits.len(), 1);
        assert!(warn.iter().any(|w| w.contains("does not dump")));
    }

    #[test]
    fn test_dpcm_stop_becomes_a_note_off() {
        // The driver clears $4015 bit 4 to cut a drum short before its body
        // ends. That must become a PCM note-off, or the tracker plays the
        // whole sample.
        let data = vec![0u8; 0x1000];
        let writes = vec![
            nes(1000, 0x10, 0x0E),
            nes(1000, 0x12, 0x11),
            nes(1000, 0x13, 0x01),
            nes(1000, 0x15, 0x1F),
            nes(5000, 0x15, 0x0F),
        ];
        let song = analyze(
            &nes_vgm_roms(writes, vec![rom(data)]),
            AnalyzeOptions::default(),
        )
        .unwrap();
        let ons: Vec<&NoteEvent> = song.events.iter().filter(|e| e.on && e.pcm).collect();
        let offs: Vec<&NoteEvent> =
            song.events.iter().filter(|e| !e.on && e.pcm).collect();
        assert_eq!(ons.len(), 1);
        assert_eq!(ons[0].end, 5000);
        assert_eq!(offs.iter().map(|e| e.sample).collect::<Vec<_>>(), vec![5000]);
        let mod_ = parse_fur(&write_song(&song)).unwrap();
        let notes = order4_notes(&mod_);
        assert!(notes.contains(&180)); // NOTE_OFF
    }

    #[test]
    fn test_4011_spam_becomes_pcm() {
        // Two hundred 7-bit writes at 4-sample spacing = one 11025 Hz body.
        let writes = (0..200)
            .map(|i| nes(100 + i * 4, 0x11, ((i * 3) & 0x7F) as u32))
            .collect::<Vec<_>>();
        let (samples, hits, warn) = collect_nes_pcm(&nes_vgm(writes), None);
        assert_eq!(samples.len(), 1);
        assert_eq!(samples[0].rate, 11025);
        assert_eq!(hits.len(), 1);
        assert!(samples[0].data.len() > 180);
        assert_eq!(warn, Vec::<String>::new());
    }

    #[test]
    fn test_long_4011_stream_is_banked() {
        // 12000 writes at 4-sample spacing is about 50k frames. One body would
        // desync over a long track; slice it into sample-map chunks instead.
        let writes = (0..12000)
            .map(|i| nes(100 + i * 4, 0x11, ((i * 7) & 0x7F) as u32))
            .collect::<Vec<_>>();
        let (samples, hits, warn) = collect_nes_pcm(&nes_vgm(writes), None);
        assert!(samples.len() > 1);
        assert_eq!(samples.len(), hits.len());
        assert!(samples.iter().all(|s| s.data.len() <= 8192));
        assert_eq!(hits[0].sample_time, 100);
        assert!(hits.last().unwrap().end_time != 0);
        assert_eq!(warn, Vec::<String>::new());
    }

    #[test]
    fn test_long_4011_stream_uses_a_sample_map() {
        let writes = (0..12000)
            .map(|i| nes(100 + i * 4, 0x11, ((i * 7) & 0x7F) as u32))
            .collect::<Vec<_>>();
        let song = analyze(&nes_vgm(writes), AnalyzeOptions::default()).unwrap();
        let data = write_song(&song);
        let ons: Vec<&NoteEvent> = song.events.iter().filter(|e| e.on && e.dac).collect();
        assert_eq!(ons.len(), song.nes_pcm_samples.len());
        assert!(ons.iter().all(|e| 60 <= e.note && e.note < 180));
        let mod_ = parse_fur(&data).unwrap();
        let notes = order4_notes(&mod_);
        assert!(notes.iter().any(|&n| 60 <= n && n < 180));
        assert!(notes.contains(&180)); // the final chunk's OFF
        let blocks = iter_blocks(&data).unwrap();
        let types: Vec<i32> = blocks
            .iter()
            .filter(|b| &b.magic == b"INS2")
            .map(|b| u16::from_le_bytes([b.payload[2], b.payload[3]]) as i32)
            .collect();
        assert!(types.contains(&INS_C352));
    }

    #[test]
    fn test_long_silence_becomes_a_rest() {
        // A long zero run is not sample data: split there and let the channel
        // go OFF until the next sounding part.
        let mut writes = (0..1000)
            .map(|i| nes(100 + i * 4, 0x11, ((i * 7) & 0x7F) as u32))
            .collect::<Vec<_>>();
        let quiet_at = 100 + 1000 * 4;
        writes.extend(
            (0..1500).map(|i| nes(quiet_at + i * 4, 0x11, 0x40)),
        );
        let resume_at = quiet_at + 1500 * 4;
        writes.extend(
            (0..1000).map(|i| nes(resume_at + i * 4, 0x11, ((i * 5) & 0x7F) as u32)),
        );
        let (samples, hits, warn) = collect_nes_pcm(&nes_vgm(writes), None);
        assert_eq!(hits.len(), 2);
        assert_eq!(samples.len(), 2);
        assert_eq!(hits[0].sample_time, 100);
        assert_eq!(hits[1].sample_time, resume_at);
        assert!(hits[0].end_time < hits[1].sample_time);
        assert!(samples.iter().map(|s| s.data.len()).sum::<usize>() <= 2100);
        assert_eq!(warn, Vec::<String>::new());
    }
}

mod test_nes_module {
    use super::*;

    fn nes_song() -> Song {
        let writes = vec![
            nes(1000, 0x15, 0x0F),
            nes(1000, 0x00, 0xB9),
            nes(1000, 0x02, 0x40),
            nes(1000, 0x03, 0x09),
            nes(2000, 0x04, 0xB4),
            nes(2000, 0x06, 0x80),
            nes(2000, 0x07, 0x05),
            nes(2500, 0x08, 0x81),
            nes(2500, 0x0A, 0x34),
            nes(2500, 0x0B, 0x01),
            nes(3000, 0x0C, 0x3A),
            nes(3000, 0x0E, 0x03),
        ];
        let mut v = nes_vgm(writes);
        v.nes_clock = 1789773;
        analyze(&v, AnalyzeOptions::default()).unwrap()
    }

    #[test]
    fn test_nes_module_layout() {
        let song = nes_song();
        assert_eq!(song.nes_ids, vec![0]);
        let data = write_song(&song);
        let mod_ = parse_fur(&data).unwrap();
        // An NES-only rip has no FM write, so the empty YM2151 placeholder is
        // dropped and the APU owns all five columns.
        assert_eq!(mod_.chips, vec![CHIP_NES]);
        assert_eq!(mod_.n_ch, 5);
        let blocks = iter_blocks(&data).unwrap();
        let types: Vec<u16> = blocks
            .iter()
            .filter(|b| &b.magic == b"INS2")
            .map(|b| u16::from_le_bytes([b.payload[2], b.payload[3]]))
            .collect();
        assert!(types.contains(&(INS_NES as u16)));
    }

    #[test]
    fn test_vrc7_module_uses_six_channels() {
        let raw = raw_vgm_nes(
            &[
                0x51, 0x30, 0x10, // ch 0: preset 1, volume 0
                0x51, 0x10, 0x40, // fnum low
                0x51, 0x20, 0x10, // key on, block 0
            ],
            &[],
            1789773,
            0x80000000 | 3579545,
            &[],
        );
        let p = temp_file(&tmp_dir(), "vrc7_song.vgm", &raw);
        let song = analyze(&load_vgm(&p).unwrap(), AnalyzeOptions::default()).unwrap();
        assert!(song.opll_vrc7);
        assert_eq!(song.fm_systems[0].fm_channels, 6);
        let data = write_song(&song);
        let mod_ = parse_fur(&data).unwrap();
        assert_eq!(mod_.chips, vec![CHIP_VRC7]);
        assert_eq!(mod_.n_ch, 6);
    }
}

mod test_dual_apu {
    use super::*;

    fn writes() -> Vec<ChipWrite> {
        vec![
            // APU 1's pulse 1, held from 1000 to 7000.
            nes(1000, 0x02, 0x40),
            nes(1000, 0x03, 0x89),
            nes(1000, 0x00, 0xB9),
            // APU 2's pulse 1 at another pitch, opened while APU 1 sounds.
            nes_chip(2000, 0x02, 0x20, 1),
            nes_chip(2000, 0x03, 0xC4, 1),
            nes_chip(2000, 0x00, 0xF5, 1),
            // APU 2 mutes its own pulse; APU 1 has to keep sounding.
            nes_chip(5000, 0x15, 0xFE, 1),
            nes(7000, 0x15, 0xFE),
        ]
    }

    #[test]
    fn test_the_two_apus_keep_separate_gates() {
        let mut v = nes_vgm(writes());
        v.nes_clock = 1789773;
        let (hits, offs, _warn) = collect_nes_tones(&v, None);
        assert_eq!(
            hits.iter()
                .map(|h| (h.chip_id, h.voice, h.off_sample))
                .collect::<Vec<_>>(),
            vec![(0, 0, 7000), (1, 0, 5000)],
        );
        assert_eq!(
            offs.iter().map(|o| o.chip_id).collect::<BTreeSet<_>>()
                .into_iter()
                .collect::<Vec<_>>(),
            vec![0, 1],
        );
    }

    #[test]
    fn test_module_gets_two_nes_chips_and_ten_columns() {
        let mut v = nes_vgm(writes());
        v.nes_clock = 1789773;
        let song = analyze(&v, AnalyzeOptions::default()).unwrap();
        assert_eq!(song.nes_ids, vec![0, 1]);
        let data = write_song(&song);
        let mod_ = parse_fur(&data).unwrap();
        assert_eq!(mod_.chips, vec![CHIP_NES, CHIP_NES]);
        assert_eq!(mod_.n_ch, 10);
        let mut notes: BTreeMap<u16, i32> = BTreeMap::new();
        for ((ch, _idx), rows) in mod_.patterns.iter() {
            *notes.entry(*ch).or_insert(0) +=
                rows.iter().filter(|r| 0 <= r.note && r.note < 180).count() as i32;
        }
        // Each pulse lands on its own chip's first column.
        assert_eq!(notes.get(&0), Some(&1));
        assert_eq!(notes.get(&5), Some(&1));
    }

    #[test]
    fn test_dual_header_keeps_apu_that_never_plays() {
        let mut w = writes()[..3].to_vec();
        w.push(nes_chip(1500, 0x15, 0x00, 1));
        let mut v = nes_vgm(w);
        v.nes_clock = 1789773;
        v.nes_dual = true;
        let song = analyze(&v, AnalyzeOptions::default()).unwrap();
        assert_eq!(song.nes_ids, vec![0, 1]);
        assert!(song.nes_present);
        let mod_ = parse_fur(&write_song(&song)).unwrap();
        assert_eq!(mod_.chips, vec![CHIP_NES, CHIP_NES]);
        assert_eq!(mod_.n_ch, 10);
    }

    #[test]
    fn test_dpcm_ram_is_read_per_apu() {
        // Both APUs hold the same address with different data: each 2A03 has its
        // own $C000 window, and clearing one chip's $4015 must not cut the
        // other's sample.
        let roms = vec![
            RomSlice {
                chip: 0,
                rom_size: 0x40,
                start: 0xC000,
                data: vec![0xAA; 0x40],
                kind: "nes_dpcm",
                time: 0,
            },
            RomSlice {
                chip: 1,
                rom_size: 0x40,
                start: 0xC000,
                data: vec![0x55; 0x40],
                kind: "nes_dpcm",
                time: 0,
            },
        ];
        let w = vec![
            nes(1000, 0x10, 0x0E),
            nes(1000, 0x12, 0x00),
            nes(1000, 0x13, 0x00),
            nes(1000, 0x15, 0x1F),
            nes_chip(2000, 0x10, 0x0E, 1),
            nes_chip(2000, 0x12, 0x00, 1),
            nes_chip(2000, 0x13, 0x00, 1),
            nes_chip(2000, 0x15, 0x1F, 1),
            nes(3000, 0x15, 0x0F),
        ];
        let (samples, hits, warn) = collect_nes_dpcm(&nes_vgm_roms(w.clone(), roms.clone()), None);
        assert_eq!(
            samples.iter().map(|s| s.data.clone()).collect::<Vec<_>>(),
            vec![b"\xAA".to_vec(), b"\x55".to_vec()],
        );
        assert_eq!(
            samples.iter().map(|s| (s.chip_id, s.start)).collect::<Vec<_>>(),
            vec![(0, 0xC000), (1, 0xC000)],
        );
        assert_eq!(
            hits.iter().map(|h| (h.chip_id, h.end_time)).collect::<Vec<_>>(),
            vec![(0, 3000), (1, 0)],
        );
        assert_eq!(warn, Vec::<String>::new());

        // Both DMC voices reach the module: one column and one instrument each.
        let mut v = nes_vgm_roms(w, roms);
        v.nes_clock = 1789773;
        let song = analyze(&v, AnalyzeOptions::default()).unwrap();
        assert_eq!(song.nes_ids, vec![0, 1]);
        let mod_ = parse_fur(&write_song(&song)).unwrap();
        let mut drum: BTreeMap<u16, Vec<(i32, i32)>> = BTreeMap::new();
        for ((ch, _idx), rows) in mod_.patterns.iter() {
            let got: Vec<(i32, i32)> = rows
                .iter()
                .filter(|r| 0 <= r.note && r.note < 180)
                .map(|r| (r.note, r.ins))
                .collect();
            if !got.is_empty() {
                drum.insert(*ch, got);
            }
        }
        assert_eq!(drum.keys().copied().collect::<Vec<_>>(), vec![4, 9]);
        assert_ne!(drum[&4][0].1, drum[&9][0].1);
        assert_eq!(
            (0..mod_.samples.len())
                .map(|i| mod_.samples[i].data[..1].to_vec())
                .collect::<BTreeSet<_>>(),
            BTreeSet::from([b"\xAA".to_vec(), b"\x55".to_vec()]),
        );
    }

    #[test]
    fn test_dpcm_bank_offsets_are_per_apu() {
        // Two type-0x07 databases (bit 31 of the block length names the chip)
        // and one 0x68 copy each from offset zero. Without a per-chip base the
        // second APU reads the first cart's samples.
        fn block(chip: u32, data: &[u8]) -> Vec<u8> {
            let size = (data.len() as u32) | ((chip & 1) << 31);
            let mut out = vec![0x67, 0x66, 0x07];
            out.extend_from_slice(&size.to_le_bytes());
            out.extend_from_slice(data);
            out
        }

        fn copy(chip: u32) -> Vec<u8> {
            vec![
                0x68,
                0x00,
                (0x07 | ((chip & 1) << 7)) as u8,
                0x00,
                0x00,
                0x00, // src 0 of this chip's database
                0x00,
                0xC0,
                0x00, // dest $C000
                0x01,
                0x00,
                0x00, // one byte
            ]
        }

        let mut cmds: Vec<u8> = Vec::new();
        for chip in [0u32, 1] {
            cmds.extend_from_slice(&copy(chip));
            cmds.extend_from_slice(&[
                0xB4,
                0x10 | ((chip as u8) << 7),
                0x0E,
                0xB4,
                0x12 | ((chip as u8) << 7),
                0x00,
                0xB4,
                0x13 | ((chip as u8) << 7),
                0x00,
                0xB4,
                0x15 | ((chip as u8) << 7),
                0x1F,
            ]);
        }
        let mut extra = block(0, &[0xAA; 4]);
        extra.extend_from_slice(&block(1, &[0x55; 4]));
        let raw = raw_vgm_nes(&cmds, &[], 1789773 | 0x40000000, 0, &extra);
        let p = temp_file(&tmp_dir(), "dual_dpcm_bank.vgm", &raw);
        let v = load_vgm(&p).unwrap();
        assert!(v.nes_dual);
        assert_eq!(v.nes_dpcm_bank_base.get(&0), Some(&0));
        assert_eq!(v.nes_dpcm_bank_base.get(&1), Some(&4));
        let (samples, hits, warn) = collect_nes_dpcm(&v, None);
        assert_eq!(
            samples.iter().map(|s| s.data.clone()).collect::<Vec<_>>(),
            vec![b"\xAA".to_vec(), b"\x55".to_vec()],
        );
        assert_eq!(hits.iter().map(|h| h.chip_id).collect::<Vec<_>>(), vec![0, 1]);
        assert_eq!(warn, Vec::<String>::new());
    }

    #[test]
    fn test_interleaved_4011_streams_stay_one_per_apu() {
        // Chip 1 holds one DAC level, chip 2 another. A single burst detection
        // over the interleaved writes yields one body with both levels in it.
        let mut writes = Vec::new();
        for i in 0..400 {
            writes.push(nes(100 + i * 8, 0x11, 0x10));
            writes.push(nes_chip(104 + i * 8, 0x11, 0x60, 1));
        }
        let mut v = nes_vgm(writes);
        v.nes_clock = 1789773;
        let (samples, hits, _warn) = collect_nes_pcm(&v, None);
        assert_eq!(
            hits.iter().map(|h| h.chip_id).collect::<BTreeSet<_>>()
                .into_iter()
                .collect::<Vec<_>>(),
            vec![0, 1],
        );
        assert_eq!(samples.len(), hits.len());
        for smp in &samples {
            let held = ((smp.chip_id * 0x50 + 0x10) << 1) - 0x80;
            assert_eq!(
                smp.data.iter().copied().collect::<BTreeSet<_>>(),
                BTreeSet::from([(held & 0xFF) as u8]),
            );
        }
    }
}

mod test_nes_pcm_banking {
    use super::*;

    #[test]
    fn test_long_capture_is_banked_and_tail_silence_dropped() {
        // A long $4011 capture is banked across 120-slot sample maps.  Each
        // sounding burst has a different length so the bodies stay distinct;
        // the trailing zero run is a rest and is not stored.
        let mut writes: Vec<ChipWrite> = Vec::new();
        let mut t = 100i64;
        let pieces = 121;
        let mut tail_silence_start = 0i64;
        for j in 0..pieces {
            let length = 64 + 4 * j;
            for i in 0..length {
                writes.push(nes(t, 0x11, (0x20 + ((i + j) % 16)) as u32));
                t += 4;
            }
            if j == pieces - 1 {
                tail_silence_start = t;
            }
            // A zero run long enough to be a rest after resampling.
            for _i in 0..188 {
                writes.push(nes(t, 0x11, 0x40));
                t += 16;
            }
            t += 4;
        }
        for _i in 0..256 {
            writes.push(nes(t, 0x11, 0x40));
            t += 16;
        }

        let song = analyze(&nes_vgm(writes), AnalyzeOptions::default()).unwrap();
        assert!(song.nes_pcm_samples.len() > 120);
        assert!(song.nes_pcm_samples.len() <= 240);
        assert_eq!(song.nes_pcm_count, song.nes_pcm_samples.len() as i32);
        assert_eq!((song.nes_pcm_samples.len() + 119) / 120, 2);
        assert!(song
            .nes_pcm_samples
            .iter()
            .map(|s| s.data.len())
            .max()
            .unwrap()
            < 8193);
        let last_end = song
            .events
            .iter()
            .filter(|e| e.on && e.dac)
            .map(|e| e.end)
            .max()
            .unwrap();
        assert!(last_end < tail_silence_start + 64);
    }
}

mod test_nes_dpcm_writer {
    use super::*;

    #[test]
    fn test_dpcm_sample_depth() {
        let mut data = vec![0u8; 0x1000];
        let body: Vec<u8> = [0xAAu8, 0x55, 0x01, 0x80].repeat(8);
        data[0x440..0x440 + body.len()].copy_from_slice(&body);
        let writes = vec![
            nes(1000, 0x10, 0x0E),
            nes(1000, 0x12, 0x11),
            nes(1000, 0x13, 0x01),
            nes(1000, 0x15, 0x1F),
        ];
        let mut v = nes_vgm_roms(
            writes,
            vec![RomSlice {
                chip: 0,
                rom_size: data.len() as i64,
                start: 0xC000,
                data: data.clone(),
                kind: "nes_dpcm",
                time: 0,
            }],
        );
        v.nes_clock = 1789773;
        let song = analyze(&v, AnalyzeOptions::default()).unwrap();
        assert_eq!(song.nes_dpcm_samples.len(), 1);
        let fur = write_song(&song);
        let mod_ = parse_fur(&fur).unwrap();
        assert_eq!(mod_.samples.len(), 1);
        assert_eq!(mod_.samples[0].depth, SMP_DEPTH_DPCM as u8);
        // $4013 = 0x01 is 17 bytes (16*1+1), i.e. 136 DPCM bits.
        assert_eq!(mod_.samples[0].n_frames, 17 * 8);
    }
}

mod test_nes_pcm_clustering {
    use super::*;

    #[test]
    fn test_cluster_folds_fragments_and_remaps_hits() {
        let samples: Vec<NesPcmSample> = (0..40)
            .map(|i| NesPcmSample {
                name: format!("pcm{i}"),
                data: (0..32)
                    .map(|j| ((i * 53 + j * 11) & 0xFF) as u8)
                    .collect(),
                rate: 11025,
                start: i as i64 * 1000,
                end: i as i64 * 1000 + 32,
                chip_id: 0,
            })
            .collect();
        let hits: Vec<NesPcmHit> = (0..40)
            .map(|i| NesPcmHit {
                sample_time: i as i64 * 4410,
                end_time: 0,
                sample_index: i as i32,
                chip_id: 0,
            })
            .collect();
        let before = samples.len();
        let (out, out_hits) = cluster_nes_pcm(samples, hits, 32, 16);
        assert!(out.len() <= 32);
        assert_eq!(out_hits.len(), before);
        assert!(out_hits
            .iter()
            .all(|h| 0 <= h.sample_index && (h.sample_index as usize) < out.len()));
    }
}
