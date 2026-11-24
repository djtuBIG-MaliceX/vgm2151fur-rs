//! Game Boy DMG: parsing, notes, envelopes, wavetables and module layout.
//!
//! Port of `tests/test_vgm2151fur_gb.py`.

mod common;
use common::*;

use std::path::PathBuf;

use vgm2fur::analyze::{analyze, AnalyzeOptions};
use vgm2fur::furio::{iter_blocks, parse_fur};
use vgm2fur::furwrite::{write_fur, WriteOptions, CHIP_GB, INS_GB};
use vgm2fur::gb::{
    collect_gb, gb_noise_note, gb_note, gb_period_delta, wave_nibbles,
    GB_SOUND_LEN_INFINITE,
};
use vgm2fur::vgm::{load_vgm, ChipWrite, VgmFile};

fn tmp_dir() -> PathBuf {
    let p = std::env::temp_dir().join("vgm2151fur_gb");
    std::fs::create_dir_all(&p).expect("temp dir");
    p
}

fn gb_vgm(writes: Vec<ChipWrite>) -> VgmFile {
    let mut v = vgm(writes);
    v.ym2151_clock = 0;
    v.k007232_clock = 0;
    v.gb_dmg_clock = 4_194_304;
    v
}

fn gb(sample: i64, reg: u32, val: u32) -> ChipWrite {
    ChipWrite::new(sample, "gb", 0, reg, val)
}

fn new_note(sample: i64, env: u32, _env_len: u32, duty: u32, period: u32) -> Vec<ChipWrite> {
    vec![
        gb(sample, 0x01, (duty << 6) | 0),
        gb(sample, 0x02, env),
        gb(sample, 0x03, period & 0xFF),
        gb(sample, 0x04, ((period >> 8) & 7) | 0x80),
    ]
}

fn note(sample: i64, env: u32) -> Vec<ChipWrite> {
    new_note(sample, env, 0, 2, 0x600)
}

fn noise_note(sample: i64, env: u32, nr43: u32, length: u32, length_en: bool) -> Vec<ChipWrite> {
    vec![
        gb(sample, 0x10, length & 0x3F),
        gb(sample, 0x11, env),
        gb(sample, 0x12, nr43),
        gb(sample, 0x13, (if length_en { 0x40 } else { 0 }) | 0x80),
    ]
}

fn noise(sample: i64, env: u32, nr43: u32) -> Vec<ChipWrite> {
    noise_note(sample, env, nr43, 0, false)
}

fn wave_cmds(sample: i64, image: &[u8]) -> Vec<ChipWrite> {
    image
        .iter()
        .enumerate()
        .map(|(i, &b)| gb(sample, 0x20 + i as u32, b as u32))
        .collect()
}

fn write_song(song: &vgm2fur::model::Song) -> Vec<u8> {
    write_fur(
        song,
        WriteOptions {
            include_pcm: true,
            ..Default::default()
        },
    )
}

fn u16le(data: &[u8], off: usize) -> u16 {
    u16::from_le_bytes([data[off], data[off + 1]])
}

fn contains_bytes(hay: &[u8], needle: &[u8]) -> bool {
    hay.windows(needle.len()).any(|w| w == needle)
}

fn img_10_20() -> Vec<u8> {
    (0x10u8..0x20).collect()
}

#[allow(non_snake_case)]
mod TestGbParsing {
    use super::*;

    #[test]
    fn test_command_and_clock() {
        let mut buf = vec![0u8; 0xC0];
        buf[0..4].copy_from_slice(b"Vgm ");
        buf[8..12].copy_from_slice(&0x161u32.to_le_bytes());
        buf[0x34..0x38].copy_from_slice(&((0xC0u32 - 0x34) as u32).to_le_bytes());
        buf[0x80..0x84].copy_from_slice(&4_194_304u32.to_le_bytes());
        let body: Vec<u8> = vec![
            0xB3, 0x02, 0x47, 0xB3, 0x14, 0x77, 0xB3, 0x15, 0xFF, 0xB3, 0x20, 0xAB, 0x66,
        ];
        let mut data = buf;
        data.extend_from_slice(&body);
        let p = temp_file(&tmp_dir(), "parse_gb.vgm", &data);
        let v = load_vgm(&p).expect("load");
        assert_eq!(v.gb_dmg_clock, 4_194_304);
        assert_eq!(
            v.writes
                .iter()
                .filter(|w| w.chip == "gb")
                .map(|w| (w.reg, w.val))
                .collect::<Vec<_>>(),
            vec![(0x02, 0x47), (0x14, 0x77), (0x15, 0xFF), (0x20, 0xAB)],
        );
        assert!(!v.warnings.join(" ").contains("unsupported VGM command"));
    }

    #[test]
    fn test_dual_bit_selects_second_instance() {
        let mut buf = vec![0u8; 0xC0];
        buf[0..4].copy_from_slice(b"Vgm ");
        buf[8..12].copy_from_slice(&0x161u32.to_le_bytes());
        buf[0x34..0x38].copy_from_slice(&((0xC0u32 - 0x34) as u32).to_le_bytes());
        buf[0x80..0x84].copy_from_slice(&(0x4000_0000u32 | 4_194_304).to_le_bytes());
        let mut data = buf;
        data.extend_from_slice(&[0xB3, 0x82, 0x47, 0x66]);
        let p = temp_file(&tmp_dir(), "parse_gb_dual.vgm", &data);
        let v = load_vgm(&p).expect("load");
        assert!(v.gb_dmg_dual);
        assert_eq!(
            v.writes.iter().map(|w| (w.chip_id, w.reg)).collect::<Vec<_>>(),
            vec![(1, 0x02)],
        );
    }
}

#[allow(non_snake_case)]
mod TestGbNotes {
    use super::*;

    #[test]
    fn test_pulse_note_and_pitch() {
        let v = gb_vgm(new_note(1000, 0xF0, 0, 2, 0x600));
        let (hits, offs, waves, warn) = collect_gb(&v, None);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].voice, 0);
        assert_eq!(hits[0].period, 0x600);
        assert_eq!(hits[0].duty, 2);
        let (note, e5) = gb_note(4_194_304, 0x600);
        let back = gb_note(4_194_304, 0x600);
        assert_eq!((note, e5), back);
        assert_eq!(gb_period_delta(0x600, 0x600), 0);
        assert!(gb_period_delta(0x600, 0x700) > 0); // higher pitch
        assert!(offs.is_empty());
        assert!(waves.is_empty());
        assert!(warn.is_empty());
    }

    #[test]
    fn test_trigger_before_its_setup_is_reordered() {
        // Merged logs can emit NRx4 before the period low byte, duty and
        // envelope at the same sample. Applying the trigger first would open
        // the note on the previous (here muted) state and lose it.
        let writes = vec![
            gb(1000, 0x04, 0x86), // trigger, period high 6
            gb(1000, 0x03, 0x00), // period low
            gb(1000, 0x02, 0x60), // volume 6
            gb(1000, 0x01, 0x80), // duty 2
            gb(2000, 0x02, 0x00), // DAC off
        ];
        let (hits, offs, _w, _warn) = collect_gb(&gb_vgm(writes), None);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].sample_time, 1000);
        assert_eq!(hits[0].period, 0x600);
        assert_eq!(hits[0].duty, 2);
        assert_eq!(hits[0].vol, 6);
        assert_eq!(offs.iter().map(|o| o.sample_time).collect::<Vec<_>>(), vec![2000]);
    }

    #[test]
    fn test_pitch_correction_after_trigger_is_the_note_base() {
        // "Start the voice, then set the real frequency": the trigger carries
        // one period and a low/high pair after it corrects the pitch at the
        // same sample. The correction must be the note's base; applying the
        // late trigger write over it left the note an octave or more high.
        let mut writes = new_note(1000, 0x80, 0, 2, 0x600);
        writes.extend([
            gb(1000, 0x0A, 0x80), // wave DAC on
            gb(1000, 0x0C, 0x20), // 50%
            gb(1000, 0x0D, 0xE0),
            gb(1000, 0x0E, 0x87), // trigger, period 0x7E0
            gb(1000, 0x0D, 0xB2),
            gb(1000, 0x0E, 0x06), // correction, period 0x6B2
        ]);
        let (hits, _o, _w, _warn) = collect_gb(&gb_vgm(writes), None);
        let pulse = hits.iter().find(|h| h.voice == 0).unwrap();
        let wave = hits.iter().find(|h| h.voice == 2).unwrap();
        assert_eq!(pulse.period, 0x600);
        assert_eq!(wave.period, 0x6B2);
    }

    #[test]
    fn test_pitch_correction_without_a_complete_pair_keeps_the_trigger() {
        // Trigger-before-setup still applies the setup's high byte when only
        // one half of the pair follows the trigger.
        let writes = vec![
            gb(1000, 0x04, 0x86), // trigger, period high 6
            gb(1000, 0x03, 0x00), // period low
            gb(1000, 0x02, 0x60), // volume 6
        ];
        let (hits, _o, _w, _warn) = collect_gb(&gb_vgm(writes), None);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].period, 0x600);
    }

    #[test]
    fn test_frequency_write_without_a_trigger_is_a_bend() {
        // Slow slides rewrite NRx3/NRx4 without a trigger, so the hardware
        // envelope keeps running. They stay in pitch_pts and become row pitch
        // steps, not a new note.
        let mut writes = note(1000, 0x61);
        writes.extend([gb(2000, 0x03, 0x10), gb(2000, 0x04, 0x06)]);
        let (hits, _o, _w, _warn) = collect_gb(&gb_vgm(writes), None);
        assert_eq!(hits.len(), 1);
        assert!(!hits[0].pitch_pts.is_empty());
    }

    #[test]
    fn test_large_pitch_jump_is_a_legato_note() {
        // A clear semitone-or-more jump without a trigger is a legato melody
        // move. It gets its own note cell with EAxx so the envelope continues.
        let mut writes = note(1000, 0x61);
        writes.extend([gb(2000, 0x03, 0x86), gb(2000, 0x04, 0x06)]);
        let (hits, _o, _w, _warn) = collect_gb(&gb_vgm(writes), None);
        assert_eq!(hits.len(), 2);
        assert!(hits[1].legato);
        assert_eq!(hits[1].sample_time, 2000);
        assert_eq!(hits[1].period, 0x686);
    }

    #[test]
    fn test_two_byte_pitch_jump_uses_the_final_period() {
        // The low byte arrives first; splitting there would keep the old high
        // byte and start the legato note on a lower pitch.
        let mut writes = note(1000, 0x61);
        writes.extend([gb(2000, 0x03, 0x20), gb(2000, 0x04, 0x07)]);
        let (hits, _o, _w, _warn) = collect_gb(&gb_vgm(writes), None);
        assert_eq!(hits.len(), 2);
        assert!(hits[1].legato);
        assert_eq!(hits[1].period, 0x720);
        assert_eq!(gb_note(4_194_304, hits[1].period), (122, 112));
    }

    #[test]
    fn test_mid_note_volume_attack_is_a_note() {
        // A mid-note NRx2 write that raises the level is a software attack,
        // not a zombie volume step inside the same note.
        let mut writes = note(1000, 0x60);
        writes.extend([gb(3000, 0x02, 0x90)]);
        let (hits, _o, _w, _warn) = collect_gb(&gb_vgm(writes), None);
        assert_eq!(hits.len(), 2);
        assert!(!hits[1].legato);
        assert_eq!(hits[1].sample_time, 3000);
        assert_eq!(hits[1].vol, 9);
    }

    #[test]
    fn test_note_off_on_dac_disable() {
        let mut writes = note(1000, 0xF0);
        writes.extend([gb(9000, 0x02, 0x00)]); // DAC off
        let (hits, offs, _w, _warn) = collect_gb(&gb_vgm(writes), None);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].off_sample, 9000);
        assert_eq!(
            offs.iter().map(|o| (o.sample_time, o.voice)).collect::<Vec<_>>(),
            vec![(9000, 0)],
        );
    }

    #[test]
    fn test_silent_retrigger_is_a_note_off() {
        // The Pokemon Card GB2 driver stops a pulse with NR12 = 0x08 (DAC on,
        // volume 0) followed by a trigger. That must emit an off, not a silent
        // replacement note that drops the previous note's end.
        let mut writes = note(1000, 0x90);
        writes.extend([gb(9000, 0x02, 0x08), gb(9000, 0x04, 0x80)]);
        let (hits, offs, _w, _warn) = collect_gb(&gb_vgm(writes), None);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].off_sample, 9000);
        assert_eq!(
            offs.iter().map(|o| (o.sample_time, o.voice)).collect::<Vec<_>>(),
            vec![(9000, 0)],
        );
    }

    #[test]
    fn test_noise_silent_retrigger_is_a_note_off() {
        let mut writes = noise(1000, 0xA1, 0x16);
        writes.extend([gb(9000, 0x11, 0x08), gb(9000, 0x13, 0x80)]);
        let (hits, offs, _w, _warn) = collect_gb(&gb_vgm(writes), None);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].off_sample, 9000);
        assert_eq!(
            offs.iter().map(|o| (o.sample_time, o.voice)).collect::<Vec<_>>(),
            vec![(9000, 3)],
        );
    }

    #[test]
    fn test_noise_nr43_change_stays_one_note() {
        // A drum body sweeps NR43 without re-triggering; it must remain one
        // note so the hardware envelope keeps decaying.
        let mut writes = noise(1000, 0xA1, 0x16);
        writes.extend([gb(3000, 0x12, 0x15), gb(5000, 0x12, 0x17)]);
        let (hits, _o, _w, _warn) = collect_gb(&gb_vgm(writes), None);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].period, 0x16);
        assert_eq!(hits[0].pitch_pts, vec![(3000, 0x15), (5000, 0x17)]);
    }

    #[test]
    fn test_noise_pitch_macro_is_the_table_offset() {
        let mut writes = noise(1000, 0xA1, 0x16);
        writes.extend([gb(3000, 0x12, 0x15)]); // one table step higher
        let song = analyze(&gb_vgm(writes), AnalyzeOptions::default()).expect("analyze");
        let ev = song
            .events
            .iter()
            .find(|e| e.tone_chip == 6 && e.tone_noise)
            .expect("noise event");
        // The binding subtracts the note position, so the event stores ntPos.
        assert_eq!(ev.tone_pitch_pts, vec![(3000, gb_noise_note(0x15) - 60)]);
        let ins = &song.gb_ins[ev.ins as usize];
        assert!(ins.pitch.iter().map(|v| v.abs()).max().unwrap() <= 2);
    }

    #[test]
    fn test_hardware_envelope_not_mistaken_for_zombie() {
        // Repeated NRx2 refreshes with a nonzero period must stay hardware.
        let mut writes = note(1000, 0xA1);
        writes.extend([gb(2000, 0x02, 0xA1), gb(3000, 0x02, 0xA1)]);
        let (hits, _o, _w, _warn) = collect_gb(&gb_vgm(writes), None);
        assert!(!hits[0].soft_env);
        assert_eq!(
            (hits[0].env_vol, hits[0].env_dir, hits[0].env_len),
            (10, 0, 1),
        );
    }

    #[test]
    fn test_hardware_envelope_rate_change_splits_note() {
        // A mid-note NRx2 rewrite that changes the period speeds up/slows down
        // the decay. The note splits there and the new segment starts at the
        // new high nibble (VGMPlay's behavior, and the driver's intent).
        let mut writes = noise(1000, 0xE7, 0x16); // vol 14, decay, period 7
        writes.extend([gb(4000, 0x11, 0x51)]); // vol 5, decay, period 1
        let (hits, _o, _w, _warn) = collect_gb(&gb_vgm(writes), None);
        assert_eq!(hits.len(), 2);
        assert_eq!((hits[0].env_vol, hits[0].env_len), (14, 7));
        assert_eq!(hits[0].off_sample, 4000);
        assert!(!hits[0].soft_env);
        assert_eq!(
            (hits[1].env_vol, hits[1].env_dir, hits[1].env_len),
            (5, 0, 1),
        );
        // A same-period refresh is not a split.
        let mut writes = noise(1000, 0xE7, 0x16);
        writes.extend([gb(4000, 0x11, 0xE7)]);
        let (hits, _o, _w, _warn) = collect_gb(&gb_vgm(writes), None);
        assert_eq!(hits.len(), 1);
    }

    #[test]
    fn test_pan_at_the_trigger_is_the_note_pan() {
        // NR51 is often written at (or one sample after) the trigger. That is
        // the note's pan, not a mid-note move.
        let mut writes = noise(1000, 0xA1, 0x16);
        writes.push(gb(1000, 0x15, 0xF7)); // noise left, same sample
        let (hits, _o, _w, _warn) = collect_gb(&gb_vgm(writes), None);
        assert_eq!(hits[0].pan, 0xF0);
        assert_eq!(hits[0].pan_pts, Vec::<(i64, i32)>::new());
    }

    #[test]
    fn test_next_note_setup_is_not_absorbed() {
        // A fast percussion driver re-triggers every ~700 samples. The next
        // note's envelope/pan must not be folded into the previous note.
        let mut writes = noise(1000, 0xC1, 0x50);
        writes.extend([gb(1000, 0x15, 0xF7)]);
        writes.extend(noise(1680, 0x72, 0x50));
        writes.extend([gb(1680, 0x15, 0xFF)]);
        let (hits, _o, _w, _warn) = collect_gb(&gb_vgm(writes), None);
        let noise_hits: Vec<_> = hits.iter().filter(|h| h.voice == 3).collect();
        assert_eq!(noise_hits.len(), 2);
        assert_eq!(
            (
                noise_hits[0].env_vol,
                noise_hits[0].env_dir,
                noise_hits[0].env_len
            ),
            (12, 0, 1),
        );
        assert_eq!(noise_hits[0].pan, 0xF0);
        assert_eq!(
            (
                noise_hits[1].env_vol,
                noise_hits[1].env_dir,
                noise_hits[1].env_len
            ),
            (7, 0, 2),
        );
        assert_eq!(noise_hits[1].pan, 0xFF);
    }

    #[test]
    fn test_noise_length_disabled_is_infinite() {
        // A source note that never enables the counter must carry the
        // instrument's infinite flag; the value alone would start the
        // hardware length counter and cut the note.
        let (hits, _o, _w, _warn) = collect_gb(&gb_vgm(noise(1000, 0xA1, 0x16)), None);
        assert_eq!(hits[0].sound_len, GB_SOUND_LEN_INFINITE);
        assert!(hits[0].sound_len_inf);
        let (hits, _o, _w, _warn) =
            collect_gb(&gb_vgm(noise_note(1000, 0xA1, 0x16, 36, true)), None);
        assert_eq!(hits[0].sound_len, 63 - 36);
        assert!(!hits[0].sound_len_inf);
    }

    #[test]
    fn test_hardware_envelope() {
        // One NRx2 write at the trigger: the platform's hardware envelope.
        let (hits, _o, _w, _warn) = collect_gb(&gb_vgm(note(1000, 0xF1)), None);
        assert!(!hits[0].soft_env);
        assert_eq!(hits[0].env_vol, 15);
        assert_eq!(hits[0].env_dir, 0); // bit 3 clear = decay
        assert_eq!(hits[0].env_len, 1);
    }

    #[test]
    fn test_software_envelope() {
        // Repeated NRx2 writes during the note are the zombie volume bug.
        let mut writes = note(1000, 0x28);
        for (i, val) in [0x18, 0x09, 0x11, 0x08].into_iter().enumerate() {
            writes.push(gb(1000 + (i as i64 + 1) * 700, 0x02, val));
        }
        let (hits, _o, _w, _warn) = collect_gb(&gb_vgm(writes), None);
        assert_eq!(hits.len(), 1);
        assert!(hits[0].soft_env);
        assert!(!hits[0].vol_pts.is_empty());
    }

    #[test]
    fn test_noise_note() {
        // Shift 4 / divider 6 is in the platform's table.
        assert_eq!(gb_noise_note(0x46), 60 + 46);
        // Shift 1 / divider 0 has no exact entry; the nearest is shift 0 / 1,
        // which has the same hardware period.
        assert_eq!(gb_noise_note(0x10) - 60, gb_noise_note(0x01) - 60);
    }

    #[test]
    fn test_fast_noise_notes_fold_to_c5() {
        // The platform's C-0 entry is the fastest rate (period 0.5), but
        // Furnace renders it as a near-DC rumble.  The converter keeps the
        // top of the noise range at C-5 (period 8), the fastest rate that
        // still sounds like noise.
        for nr43 in [0x00, 0x04, 0x08, 0x18, 0x22] {
            assert_eq!(gb_noise_note(nr43), 120, "{:x}", nr43);
        }
        // Slower rates keep their relative pitch below C-5.
        assert_eq!(gb_noise_note(0x5E), 102); // shift 5 / divider 6
        assert!(gb_noise_note(0x5E) < 120);
    }

    #[test]
    fn test_pan_from_nr51() {
        let mut writes = vec![gb(900, 0x15, 0x10)]; // ch1 left only, before the note
        writes.extend(note(1000, 0xF0));
        let (hits, _o, _w, _warn) = collect_gb(&gb_vgm(writes), None);
        assert_eq!(hits[0].pan, 0xF0);
        let mut writes = vec![gb(900, 0x15, 0x01)]; // ch1 right only
        writes.extend(note(1000, 0xF0));
        let (hits, _o, _w, _warn) = collect_gb(&gb_vgm(writes), None);
        assert_eq!(hits[0].pan, 0x0F);
        // A move while the note is held becomes a pan point.
        let mut writes = note(1000, 0xF0);
        writes.extend([gb(1100, 0x15, 0x10)]);
        let (hits, _o, _w, _warn) = collect_gb(&gb_vgm(writes), None);
        assert_eq!(hits[0].pan_pts, vec![(1100, 0xF0)]);
    }
}

#[allow(non_snake_case)]
mod TestGbWave {
    use super::*;

    #[test]
    fn test_wave_large_pitch_jump_is_a_legato_note() {
        // The wave voice uses the same legato split as the pulses: a clear
        // period jump after the refit window changes pitch without a trigger.
        let writes = vec![
            gb(0, 0x0A, 0x80),
            gb(0, 0x0C, 0x40),
            gb(0, 0x0D, 0x00),
            gb(0, 0x0E, 0x86),
            gb(2000, 0x0D, 0x20),
            gb(2000, 0x0E, 0x07),
        ];
        let (hits, _o, _w, _warn) = collect_gb(&gb_vgm(writes), None);
        let wave: Vec<_> = hits.iter().filter(|h| h.voice == 2).collect();
        assert_eq!(wave.len(), 2);
        assert!(wave[1].legato);
        assert_eq!(wave[1].sample_time, 2000);
        assert_eq!(wave[1].period, 0x720);
    }

    #[test]
    fn test_wave_trigger_while_muted_starts_at_the_level_rise() {
        // Kirby's Dream Land 2 starts the wave voice with NR32 = 0 and raises
        // the output level a few samples later. The rise starts the note.
        let img = img_10_20();
        let mut writes = wave_cmds(0, &img);
        writes.extend([
            gb(0, 0x0A, 0x80),
            gb(0, 0x0C, 0x00),
            gb(0, 0x0D, 0x00),
            gb(0, 0x0E, 0x80),
        ]);
        writes.extend([gb(100, 0x0C, 0x40)]);
        let (hits, _o, _w, _warn) = collect_gb(&gb_vgm(writes), None);
        let wave: Vec<_> = hits.iter().filter(|h| h.voice == 2).collect();
        assert_eq!(wave.len(), 1);
        assert_eq!(wave[0].sample_time, 100);
        assert_eq!(wave[0].vol, 10);
    }

    #[test]
    fn test_wave_mute_is_a_note_off() {
        // NR32 = 0 gates the voice; the next level starts a new note instead
        // of one long note with silent volume steps.
        let img = img_10_20();
        let mut writes = wave_cmds(0, &img);
        writes.extend([
            gb(0, 0x0A, 0x80),
            gb(0, 0x0C, 0x40),
            gb(0, 0x0D, 0x00),
            gb(0, 0x0E, 0x80),
            gb(500, 0x0C, 0x00),
            gb(900, 0x0C, 0x40),
        ]);
        let (hits, offs, _w, _warn) = collect_gb(&gb_vgm(writes), None);
        let wave: Vec<_> = hits.iter().filter(|h| h.voice == 2).collect();
        assert_eq!(wave.len(), 2);
        assert_eq!(wave[0].off_sample, 500);
        assert_eq!(wave[1].sample_time, 900);
        assert!(offs.iter().any(|o| (o.sample_time, o.voice) == (500, 2)));
    }

    #[test]
    fn test_wave_pitch_set_after_muted_trigger_is_the_base() {
        let img = img_10_20();
        let mut writes = wave_cmds(0, &img);
        writes.extend([
            gb(0, 0x0A, 0x80),
            gb(0, 0x0C, 0x00),
            gb(0, 0x0D, 0x00),
            gb(0, 0x0E, 0x80),
            gb(100, 0x0C, 0x40),
            gb(200, 0x0D, 0x51),
            gb(200, 0x0E, 0x04),
        ]);
        let (hits, _o, _w, _warn) = collect_gb(&gb_vgm(writes), None);
        let wave = hits.iter().find(|h| h.voice == 2).unwrap();
        assert_eq!(wave.period, 0x451);
        assert_eq!(wave.pitch_pts, Vec::<(i64, i32)>::new());
    }

    #[test]
    fn test_partial_wave_update_is_the_current_image() {
        // Drivers rewrite only the bytes that changed. A note must use the
        // current accumulator, not wait until all 16 bytes were touched.
        let mut img = img_10_20();
        let mut writes = wave_cmds(0, &img);
        writes.extend([
            gb(1000, 0x0A, 0x80),
            gb(1000, 0x0C, 0x40),
            gb(1000, 0x0D, 0x00),
            gb(1000, 0x0E, 0x85),
        ]);
        img[5] = 0xAB;
        writes.extend([
            gb(2000, 0x25, 0xAB),
            gb(2100, 0x0A, 0x00),
            gb(2101, 0x0A, 0x80),
            gb(2101, 0x0E, 0x85),
        ]);
        let (hits, _o, waves, _warn) = collect_gb(&gb_vgm(writes), None);
        let wave: Vec<_> = hits.iter().filter(|h| h.voice == 2).collect();
        assert_eq!(wave.len(), 2);
        assert_ne!(wave[0].wave, wave[1].wave);
        assert_eq!(waves[wave[1].wave], wave_nibbles(&img));
    }

    #[test]
    fn test_envelope_written_after_trigger_is_the_note_envelope() {
        // Blitz/Kuma noise: NR43, trigger, then NR42. The late NR42 is the
        // note's envelope, not a mid-note change.
        let mut writes = noise_note(1000, 0x11, 0x16, 0, false);
        writes.push(gb(1002, 0x11, 0x91));
        let (hits, _o, _w, _warn) = collect_gb(&gb_vgm(writes), None);
        assert_eq!(
            (hits[0].env_vol, hits[0].env_dir, hits[0].env_len),
            (9, 0, 1),
        );
    }

    #[test]
    fn test_hardware_envelope_needs_no_hwseq() {
        // Furnace applies the instrument's NRx2 fields at the note-on, so the
        // note carries no volume column (which would overwrite the start
        // level) and the instrument needs no hardware sequence: the sequence
        // could only write the same envelope a second time.
        let writes = noise(1000, 0x21, 0x16);
        let song = analyze(&gb_vgm(writes), AnalyzeOptions::default()).expect("analyze");
        let ev = song
            .events
            .iter()
            .find(|e| e.tone_chip == 6 && e.on && !e.tone_gb_wave && !e.gb_soft_env)
            .expect("gb event");
        assert_eq!(ev.vol, -1);
        let data = write_song(&song);
        let features: Vec<&[u8]> = iter_blocks(&data)
            .expect("blocks")
            .into_iter()
            .filter(|b| b.magic == *b"INS2")
            .map(|b| b.payload)
            .collect();
        let mut found: Option<&[u8]> = None;
        for block in features {
            let mut p = 4usize;
            if block.len() >= p + 2 && &block[p..p + 2] == b"NA" {
                let ln = u16le(block, p + 2) as usize;
                p += 4 + ln;
            }
            while p + 4 <= block.len() {
                let code = &block[p..p + 2];
                let ln = u16le(block, p + 2) as usize;
                if code == b"GB" {
                    found = Some(&block[p + 4..p + 4 + ln]);
                    break;
                }
                if code == b"EN" {
                    break;
                }
                p += 4 + ln;
            }
            if found.is_some() {
                break;
            }
        }
        let found = found.expect("GB feature");
        assert_eq!(found[0] & 15, 2); // source start volume
        assert_eq!(found[0] >> 5, 1); // envelope period
        assert_eq!(found[3], 0); // no hardware-sequence entry
        assert_eq!(found[2] & 8, 8); // soundLenInfinite
        assert_eq!(found[1], 255);
    }

    #[test]
    fn test_nr51_mute_gates_the_wave_with_nr32() {
        // The Pokemon drivers gate the wave voice by clearing its NR51 bits.
        // Furnace reads a 08 00 effect as "both on", so the gate must become
        // an NR32 level change instead: 0 while muted, the register's level
        // back when NR51 returns.
        let img = img_10_20();
        let mut writes = wave_cmds(0, &img);
        writes.extend([
            gb(1000, 0x0A, 0x80), // wave DAC on
            gb(1000, 0x0C, 0x20), // 100%
            gb(1000, 0x0D, 0x00),
            gb(1000, 0x0E, 0x86), // trigger
            gb(1000, 0x15, 0xFF), // every voice both sides
            gb(5000, 0x15, 0xBB), // wave off, everyone else stays
            gb(9000, 0x15, 0xFF), // wave back
        ]);
        let (hits, _o, _w, _warn) = collect_gb(&gb_vgm(writes), None);
        let wave: Vec<_> = hits.iter().filter(|h| h.voice == 2).collect();
        assert_eq!(wave.len(), 1);
        assert_eq!(wave[0].vol_pts, vec![(5000, 0), (9000, 15)]);
        assert_eq!(wave[0].pan, 0xFF);
        assert!(!wave[0].pan_pts.iter().any(|p| p.1 == 0));

        let writes = {
            let mut w = wave_cmds(0, &img);
            w.extend([
                gb(1000, 0x0A, 0x80),
                gb(1000, 0x0C, 0x20),
                gb(1000, 0x0D, 0x00),
                gb(1000, 0x0E, 0x86),
                gb(1000, 0x15, 0xFF),
                gb(5000, 0x15, 0xBB),
                gb(9000, 0x15, 0xFF),
            ]);
            w
        };
        let song = analyze(&gb_vgm(writes), AnalyzeOptions::default()).expect("analyze");
        let mod_ = parse_fur(&write_song(&song)).expect("parse");
        let cells = mod_.patterns.get(&(2, 0)).expect("wave pattern");
        assert!(
            cells.iter().any(|r| r.vol == 0),
            "the gate must write NR32 = 0"
        );
        assert!(
            !cells
                .iter()
                .any(|r| r.fx.iter().any(|&(c, v)| c == 0x08 && v == 0)),
            "08 00 is 'both on' on the GB platform"
        );
    }

    #[test]
    fn test_wave_trigger_while_nr51_muted_starts_silent() {
        // A wave note triggered while NR51 gates the voice must start at
        // level 0 and come back when the output is restored.
        let img = img_10_20();
        let mut writes = wave_cmds(0, &img);
        writes.extend([
            gb(0, 0x15, 0x00), // every voice off
            gb(1000, 0x0A, 0x80),
            gb(1000, 0x0C, 0x20), // 100%
            gb(1000, 0x0D, 0x00),
            gb(1000, 0x0E, 0x86),
            gb(5000, 0x15, 0xFF),
        ]);
        let (hits, _o, _w, _warn) = collect_gb(&gb_vgm(writes), None);
        let wave: Vec<_> = hits.iter().filter(|h| h.voice == 2).collect();
        assert_eq!(wave.len(), 1);
        assert_eq!(wave[0].vol, 0);
        assert_eq!(wave[0].pan, 0xFF);
        assert_eq!(wave[0].vol_pts, vec![(5000, 15)]);
    }

    #[test]
    fn test_wave_images_become_wavetables() {
        let wave_a: Vec<u8> = (0x10u8..0x20).collect();
        let wave_b: Vec<u8> = (0x90u8..0xA0).collect();
        let mut writes = wave_cmds(0, &wave_a);
        writes.extend([
            gb(1000, 0x0A, 0x80), // wave DAC on
            gb(1000, 0x0C, 0x20), // 100%
            gb(1000, 0x0D, 0x00),
            gb(1000, 0x0E, 0x86), // trigger, period 0x600
        ]);
        writes.extend(wave_cmds(3000, &wave_b)); // mid-note morph
        let (hits, _o, waves, _warn) = collect_gb(&gb_vgm(writes), None);
        assert_eq!(waves.len(), 2);
        assert_eq!(waves[0], wave_nibbles(&wave_a));
        assert_eq!(waves[1], wave_nibbles(&wave_b));
        assert_eq!(hits[0].voice, 2);
        assert_eq!(hits[0].wave, 0);
        assert_eq!(hits[0].wave_pts, vec![(3000, 1)]);
        assert_eq!(hits[0].vol, 15);
        assert_eq!(hits[0].sound_len, GB_SOUND_LEN_INFINITE);
        assert!(hits[0].sound_len_inf);
    }
}

#[allow(non_snake_case)]
mod TestGbModule {
    use super::*;

    #[test]
    fn test_gb_only_module() {
        let mut writes = note(1000, 0xF1);
        writes.extend((0..16).map(|i| gb(2000, 0x20 + i, 0x00)));
        writes.extend([
            gb(2000, 0x0A, 0x80),
            gb(2000, 0x0C, 0x40), // 50%
            gb(2000, 0x0D, 0x00),
            gb(2000, 0x0E, 0x86),
        ]);
        writes.extend([
            gb(3000, 0x10, 0x00),
            gb(3000, 0x11, 0xF2),
            gb(3000, 0x12, 0x46),
            gb(3000, 0x13, 0x80),
        ]);
        let song = analyze(&gb_vgm(writes), AnalyzeOptions::default()).expect("analyze");
        assert_eq!(song.gb_ids, vec![0]);
        assert!(song.gb_count >= 3);
        let data = write_song(&song);
        let mod_ = parse_fur(&data).expect("parse");
        assert_eq!(mod_.chips, vec![CHIP_GB]);
        assert_eq!(mod_.n_ch, 4);
        let types: Vec<u16> = iter_blocks(&data)
            .expect("blocks")
            .into_iter()
            .filter(|b| b.magic == *b"INS2")
            .map(|b| u16le(b.payload, 2))
            .collect();
        assert!(types.contains(&(INS_GB as u16)));
        // The GB chip flag picks the CGB-E core.
        assert!(contains_bytes(&data, b"chipType=2"));
    }

    #[test]
    fn test_finite_length_writes_the_counter() {
        // A source that does enable length counting keeps it: the instrument
        // clears soundLenInfinite and stores NRx1's encoding, so Furnace
        // writes NRx4 bit 6 with the same length the driver asked for.
        let writes = noise_note(1000, 0xA1, 0x16, 36, true);
        let song = analyze(&gb_vgm(writes), AnalyzeOptions::default()).expect("analyze");
        let data = write_song(&song);
        let mut found: Option<&[u8]> = None;
        for blk in iter_blocks(&data).expect("blocks") {
            if blk.magic != *b"INS2" {
                continue;
            }
            if let Some(i) = blk
                .payload
                .windows(4)
                .position(|w| w == b"GB\x04\x00")
            {
                found = Some(&blk.payload[i + 4..i + 8]);
                break;
            }
        }
        let found = found.expect("GB feature");
        assert_eq!(found[1], 63 - 36); // NRx1 low bits = 36
        assert_eq!(found[2] & 8, 0); // length counter enabled
        assert_eq!(found[3], 0); // still no hardware sequence
    }

    #[test]
    fn test_gb_scc_combo_keeps_both() {
        // Header-only SCC declaration plus a GB write: the analyzer must keep
        // both systems rather than treating SCC as a competing family.
        let writes = note(1000, 0xF1);
        let mut v = gb_vgm(writes);
        v.k051649_clock = 1_789_772;
        v.k051649_plus = true;
        let song = analyze(&v, AnalyzeOptions::default()).expect("analyze");
        // No SCC register writes -> no SCC voices, but GB still converts.
        assert_eq!(song.gb_ids, vec![0]);
        let mod_ = parse_fur(&write_song(&song)).expect("parse");
        assert!(mod_.chips.contains(&CHIP_GB));
    }
}
