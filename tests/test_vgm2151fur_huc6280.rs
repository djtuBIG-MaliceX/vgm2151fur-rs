//! HuC6280 (PC Engine) wavetable, direct-D/A PCM and clock parsing.
//!
//! Port of `tests/test_vgm2151fur_huc6280.py`.

mod common;
#[allow(unused_imports)]
use common::*;

use std::collections::BTreeSet;
use std::path::PathBuf;

use vgm2fur::analyze::{analyze, AnalyzeOptions};
use vgm2fur::furio::{chip_channels, iter_blocks, parse_fur};
#[allow(unused_imports)]
use vgm2fur::furwrite::{write_fur, WriteOptions, CHIP_NES, CHIP_PCE, INS_PCE};
use vgm2fur::huc6280::{
    _dda_to_s8, collect_huc6280, collect_huc6280_pcm, huc6280_note, huc6280_period_hz,
    pce_effective, HUC6280_DEFAULT_CLOCK,
};
use vgm2fur::parity::py_round;
use vgm2fur::vgm::{load_vgm, ChipWrite, VgmFile};

fn tmp_dir() -> PathBuf {
    let p = std::env::temp_dir().join("vgm2151fur_huc6280");
    std::fs::create_dir_all(&p).expect("temp dir");
    p
}

fn _vgm(writes: Vec<ChipWrite>, huc6280_clock: u32) -> VgmFile {
    let mut v = VgmFile::default();
    v.path = PathBuf::from("synthetic.vgm");
    v.buf = Vec::new();
    v.version = 0x161;
    v.total_samples = 400_000;
    v.loop_samples = 0;
    v.loop_file_pos = None;
    v.loop_sample = None;
    v.ym2151_clock = 3_579_545;
    v.k007232_clock = 0;
    v.gd3 = Default::default();
    v.roms = Vec::new();
    v.writes = writes;
    v.huc6280_clock = huc6280_clock;
    v
}

fn _raw_vgm(body: &[u8], huc_clock: u32, nes_clock: u32, blocks: &[(u8, Vec<u8>)]) -> Vec<u8> {
    let mut buf = vec![0u8; 0xC0];
    buf[0..4].copy_from_slice(b"Vgm ");
    buf[8..12].copy_from_slice(&0x161u32.to_le_bytes());
    buf[0x18..0x1C].copy_from_slice(&44100u32.to_le_bytes());
    buf[0x34..0x38].copy_from_slice(&(0xC0u32 - 0x34).to_le_bytes());
    if huc_clock != 0 {
        buf[0xA4..0xA8].copy_from_slice(&(huc_clock & 0xFFFF_FFFF).to_le_bytes());
    }
    if nes_clock != 0 {
        buf[0x84..0x88].copy_from_slice(&(nes_clock & 0xFFFF_FFFF).to_le_bytes());
    }
    let mut body_parts: Vec<u8> = Vec::new();
    for (typ, payload) in blocks {
        body_parts.extend_from_slice(&[0x67, 0x66, *typ]);
        body_parts.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        body_parts.extend_from_slice(payload);
    }
    body_parts.extend_from_slice(body);
    body_parts.push(0x66);
    let mut out = buf;
    out.extend_from_slice(&body_parts);
    let len = (out.len() - 4) as u32;
    out[4..8].copy_from_slice(&len.to_le_bytes());
    out
}

fn _w(sample: i64, reg: u32, val: u32) -> ChipWrite {
    ChipWrite::new(sample, "huc6280", 0, reg, val)
}

fn _load_wave_opts(
    wave_vals: &[u8],
    period: u32,
    volume: u32,
    balance: u32,
    sample: i64,
    channel: u32,
) -> Vec<ChipWrite> {
    let mut writes = vec![
        _w(sample, 0x01, 0xFF),
        _w(sample, 0x00, channel),
        _w(sample, 0x04, 0x00), // clear DDA -> waveform index 0
    ];
    for &v in wave_vals {
        writes.push(_w(sample, 0x06, v as u32));
    }
    writes.extend_from_slice(&[
        _w(sample, 0x04, 0x40), // DDA mode: forces index reset on enable
        _w(sample, 0x02, period & 0xFF),
        _w(sample, 0x03, (period >> 8) & 0x0F),
        _w(sample, 0x05, balance),
        _w(sample, 0x04, 0x80 | (volume & 0x1F)),
    ]);
    writes
}

fn _load_wave(wave_vals: &[u8]) -> Vec<ChipWrite> {
    _load_wave_opts(wave_vals, 0x100, 31, 0xFF, 0, 0)
}

fn _square() -> Vec<u8> {
    let mut out = vec![31u8; 16];
    out.extend_from_slice(&[0u8; 16]);
    out
}

fn _sine() -> Vec<u8> {
    (0..32)
        .map(|i| {
            let v = 15.5 + 15.5 * (2.0 * std::f64::consts::PI * i as f64 / 32.0).sin();
            py_round(v) as u8
        })
        .collect()
}

fn _wave_values(block: &[u8]) -> (u32, Vec<u32>) {
    let name_end = block.iter().position(|&b| b == 0).expect("name terminator");
    let mut pos = name_end + 1;
    let length = u32::from_le_bytes([
        block[pos],
        block[pos + 1],
        block[pos + 2],
        block[pos + 3],
    ]);
    pos += 4;
    pos += 4; // min
    let maximum = u32::from_le_bytes([
        block[pos],
        block[pos + 1],
        block[pos + 2],
        block[pos + 3],
    ]);
    pos += 4;
    let values = (0..length as usize)
        .map(|i| {
            let p = pos + 4 * i;
            u32::from_le_bytes([block[p], block[p + 1], block[p + 2], block[p + 3]])
        })
        .collect();
    (maximum, values)
}

#[allow(non_snake_case)]
mod TestHuc6280Parsing {
    use super::*;

    #[test]
    fn test_register_clock_and_pcm_block() {
        let body: Vec<u8> = vec![
            0xB9, 0x00, 0x02, // select channel 2
            0xB9, 0x04, 0x9F, // enable + volume 31
            0xB9, 0x06, 0x1F, // waveform byte
        ];
        let raw = _raw_vgm(
            &body,
            3_579_545,
            0,
            &[(0x05, (0u8..16).collect())],
        );
        let p = temp_file(&tmp_dir(), "parse_huc.vgm", &raw);
        let v = load_vgm(&p).expect("load");
        assert_eq!(v.huc6280_clock, HUC6280_DEFAULT_CLOCK);
        assert_eq!(
            v.writes
                .iter()
                .filter(|w| w.chip == "huc6280")
                .map(|w| (w.reg, w.val))
                .collect::<Vec<_>>(),
            vec![(0x00, 0x02), (0x04, 0x9F), (0x06, 0x1F)],
        );
        assert_eq!(v.huc6280_pcm, (0u8..16).collect::<Vec<_>>());
        assert_eq!(v.huc6280_pcm_blocks.len(), 1);
    }

    #[test]
    fn test_nes_clock_is_at_0x84() {
        // Regression: 0x80 is the Game Boy DMG clock; the NES moved to 0x84.
        let raw = _raw_vgm(&[], 0, 1_789_773, &[]);
        let p = temp_file(&tmp_dir(), "nes_clock.vgm", &raw);
        let v = load_vgm(&p).expect("load");
        assert_eq!(v.nes_clock, 1_789_773);
    }

    #[test]
    fn test_second_chip_bit() {
        let body: Vec<u8> = vec![0xB9, 0x80 | 0x04, 0x1F];
        let raw = _raw_vgm(&body, 3_579_545, 0, &[]);
        let p = temp_file(&tmp_dir(), "second.vgm", &raw);
        let v = load_vgm(&p).expect("load");
        let writes: Vec<_> = v.writes.iter().filter(|w| w.chip == "huc6280").collect();
        assert_eq!(writes[0].chip_id, 1);
        assert_eq!(writes[0].reg, 0x04);
    }
}

#[allow(non_snake_case)]
mod TestHuc6280Tones {
    use super::*;

    fn song_of(v: &VgmFile) -> vgm2fur::model::Song {
        analyze(v, AnalyzeOptions::default()).expect("analyze")
    }

    #[test]
    fn test_wave_note() {
        let v = _vgm(_load_wave(&_square()), 3_579_545);
        let (notes, _offs, waves, warn) = collect_huc6280(&v);
        assert_eq!(notes.len(), 1);
        assert_eq!(warn, Vec::<String>::new());
        let hit = &notes[0].hit;
        assert_eq!(hit.voice, 0);
        assert_eq!(hit.period, 0x100);
        assert_eq!(hit.vol, 31);
        assert_eq!(hit.wave, _square());
        assert_eq!(waves, vec![_square()]);
    }

    #[test]
    fn test_wave_dedup() {
        // Two channels playing the same image share one wavetable.
        let mut writes = _load_wave(&_square());
        writes.extend(_load_wave_opts(&_square(), 0x100, 31, 0xFF, 0, 1));
        let (notes, _offs, waves, _warn) =
            collect_huc6280(&_vgm(writes, 3_579_545));
        assert_eq!(notes.len(), 2);
        assert_eq!(waves.len(), 1);
    }

    #[test]
    fn test_note_mapping_round_trip() {
        // Furnace writes the period register back unchanged.
        for period in [0x100, 0x3F9, 0x0FF] {
            let (note, e5) = huc6280_note(3_579_545, period);
            assert!(note >= 0);
            assert!(note <= 179);
            assert!(e5 >= 0);
            assert!(e5 <= 255);
        }
        assert_almost_eq!(huc6280_period_hz(3_579_545, 0x100), 437.0, 1);
    }

    #[test]
    fn test_gate_off_closes_note() {
        let mut writes = _load_wave_opts(&_square(), 0x100, 31, 0xFF, 0, 0);
        writes.push(_w(10000, 0x04, 0x00)); // disable the channel
        let (notes, offs, _waves, _warn) = collect_huc6280(&_vgm(writes, 3_579_545));
        assert_eq!(notes.len(), 1);
        assert_eq!(notes[0].hit.off_sample, 10000);
        assert_eq!(offs[0].sample_time, 10000);
    }

    #[test]
    fn test_module_output() {
        let song = song_of(&_vgm(_load_wave(&_sine()), 3_579_545));
        assert_eq!(song.huc6280_ids, vec![0]);
        assert_eq!(song.huc6280_count, 1);
        assert_eq!(song.huc6280_waves.len(), 1);
        assert_eq!(song.huc6280_ins.len(), 1);
        let data = write_fur(
            &song,
            WriteOptions {
                include_pcm: true,
                include_fm: false,
                ..Default::default()
            },
        );
        let mod_ = parse_fur(&data).expect("parse");
        assert_eq!(mod_.chips, vec![CHIP_PCE]);
        assert_eq!(chip_channels(CHIP_PCE), 6);
        assert_eq!(mod_.n_ch, 6);
        let ins_types: Vec<i32> = iter_blocks(&data)
            .expect("blocks")
            .into_iter()
            .filter(|b| b.magic == *b"INS2")
            .map(|b| u16::from_le_bytes([b.payload[2], b.payload[3]]) as i32)
            .collect();
        assert_eq!(ins_types, vec![INS_PCE]);
        let waves: Vec<&[u8]> = iter_blocks(&data)
            .expect("blocks")
            .into_iter()
            .filter(|b| b.magic == *b"WAVE")
            .map(|b| b.payload)
            .collect();
        assert_eq!(waves.len(), 1);
        let (maximum, values) = _wave_values(waves[0]);
        assert_eq!(maximum, 31);
        assert_eq!(values, _sine().iter().map(|&v| v as u32).collect::<Vec<_>>());
    }

    #[test]
    fn test_vibrato_is_not_merged_away() {
        // Drivers vibrato by rewriting the period every frame. A one-frame
        // merge (used to collapse LO/HI pairs) would swallow the whole
        // oscillation and leave one arbitrary pitch.
        let mut writes = _load_wave_opts(&_square(), 0x200, 31, 0xFF, 0, 0);
        let vals = [0x204u32, 0x208, 0x204, 0x200];
        let vals: Vec<u32> = vals.iter().copied().cycle().take(16).collect();
        for (i, &p) in vals.iter().enumerate() {
            let t = 2000 + i as i64 * 730;
            writes.push(_w(t, 0x02, p & 0xFF));
            writes.push(_w(t, 0x03, (p >> 8) & 0x0F));
        }
        writes.push(_w(2000 + vals.len() as i64 * 730 + 2000, 0x04, 0x00));
        let song = song_of(&_vgm(writes, 3_579_545));
        let ev = song
            .events
            .iter()
            .find(|e| e.tone_chip == 5 && e.on)
            .expect("note");
        assert!(ev.tone_pitch_pts.len() >= 4);
        let ins = &song.huc6280_ins[ev.ins as usize];
        assert!(!ins.pitch.is_empty());
        let distinct: BTreeSet<i32> = ins.pitch.iter().copied().collect();
        assert!(distinct.len() > 1);
    }

    #[test]
    fn test_frame_rate_portamento_becomes_row_effects() {
        // A driver retuning every frame writes a stepped glide (2 semitones
        // per step here), each held just under a 60 Hz frame. That is not a
        // short blip to drop: it belongs on the pattern rows as E8/E9 plus E5,
        // not in a per-note instrument macro and not as a new note every step.
        let mut writes = _load_wave_opts(&_square(), 0x0D6, 31, 0xFF, 0, 0);
        let periods: [u32; 7] = [0x0BE, 0x0AA, 0x0A0, 0x08F, 0x07F, 0x078, 0x06B];
        for (i, &period) in periods.iter().enumerate() {
            let t = 1000 + i as i64 * 687;
            writes.push(_w(t, 0x02, period & 0xFF));
            writes.push(_w(t, 0x03, (period >> 8) & 0x0F));
        }
        writes.push(_w(1000 + periods.len() as i64 * 687, 0x04, 0x00));
        let song = song_of(&_vgm(writes, 3_579_545));
        let evs: Vec<_> = song
            .events
            .iter()
            .filter(|e| e.on && e.tone_chip == 5)
            .collect();
        assert_eq!(evs.len(), 1);
        let ev = evs[0];
        assert!(ev.tone_pitch_pts.is_empty());
        assert!(ev.pitch_steps.len() >= periods.len());
        assert!(song.huc6280_ins[ev.ins as usize].pitch.is_empty());
        let m = parse_fur(&write_fur(
            &song,
            WriteOptions {
                include_pcm: true,
                include_fm: false,
                ..Default::default()
            },
        ))
        .expect("parse");
        let fx: Vec<(u8, u8)> = m
            .patterns
            .values()
            .flat_map(|rows| rows.iter())
            .flat_map(|row| row.fx.iter().copied())
            .collect();
        assert!(fx.iter().any(|&(code, _v)| code == 0xE8), "{fx:?}");
        assert!(fx.iter().any(|&(code, _v)| code == 0xE5), "{fx:?}");
    }

    #[test]
    fn test_volume_and_balance() {
        let (notes, _offs, _waves, _warn) = collect_huc6280(&_vgm(
            _load_wave_opts(&_square(), 0x100, 20, 0xB9, 0, 0),
            3_579_545,
        ));
        let hit = &notes[0].hit;
        assert_eq!(hit.vol, 20);
        assert_eq!(hit.balance, 0xB9);
    }

    #[test]
    fn test_attack_balance_after_the_gate_is_the_note_level() {
        // The driver writes the attack balance a couple of samples after the
        // gate. It belongs to the note's start level, not a same-row volume
        // step (which the writer would drop). The later decay stays a step.
        let mut writes = _load_wave_opts(&_square(), 0x100, 31, 0x99, 0, 0);
        writes.push(_w(2, 0x05, 0xCC)); // attack, just after the gate
        writes.push(_w(1000, 0x05, 0x99)); // decay, a later row
        let song = song_of(&_vgm(writes, 3_579_545));
        let evs: Vec<_> = song
            .events
            .iter()
            .filter(|e| e.on && e.ch >= 8)
            .collect();
        let (attack, _pan) = pce_effective(31, 0xCC);
        let (decay, _dpan) = pce_effective(31, 0x99);
        assert_eq!(
            evs.iter().map(|e| e.sample).take(2).collect::<Vec<_>>(),
            vec![0, 1000],
        );
        assert_eq!(evs[0].vol, attack);
        assert_eq!(evs[1].vol, decay);
    }

    #[test]
    fn test_balance_step_becomes_note_on() {
        // LOX2-style drivers fade with the balance register while the control
        // volume stays at 31. Each balance step is a new 16th-note level.
        let mut writes = _load_wave_opts(&_square(), 0x100, 31, 0xFF, 0, 0);
        writes.extend([_w(5000, 0x00, 0), _w(5000, 0x05, 0x87)]);
        let song = song_of(&_vgm(writes, 3_579_545));
        let evs: Vec<_> = song
            .events
            .iter()
            .filter(|e| e.on && e.ch >= 8)
            .collect();
        assert_eq!(
            evs.iter().map(|e| e.sample).take(2).collect::<Vec<_>>(),
            vec![0, 5000],
        );
        assert_eq!(
            evs.iter().map(|e| e.vol).take(2).collect::<Vec<_>>(),
            vec![31, 16],
        );
        // And the module still writes (it would fail if the step were
        // dropped and the empty macro left the note without a level).
        let data = write_fur(
            &song,
            WriteOptions {
                include_pcm: true,
                include_fm: false,
                ..Default::default()
            },
        );
        assert_eq!(parse_fur(&data).expect("parse").chips, vec![CHIP_PCE]);
    }

    #[test]
    fn test_volume_steps_become_note_ons() {
        // A long PCE gate can carry many balance steps. In the percussion
        // drivers each step is the next 16th-note level (hi-hats and a louder
        // snare accent), so every step becomes a note with its own column.
        let mut writes = _load_wave_opts(&_square(), 0x100, 31, 0xCC, 0, 0);
        writes.extend([
            _w(2000, 0x05, 0x99),
            _w(4000, 0x05, 0x88),
            _w(6000, 0x05, 0xCC),
            _w(8000, 0x05, 0x99),
        ]);
        let song = song_of(&_vgm(writes, 3_579_545));
        let evs: Vec<_> = song
            .events
            .iter()
            .filter(|e| e.on && e.ch >= 8)
            .collect();
        assert_eq!(
            evs.iter().map(|e| e.sample).take(5).collect::<Vec<_>>(),
            vec![0, 2000, 4000, 6000, 8000],
        );
        assert_eq!(
            evs.iter().map(|e| e.vol).take(5).collect::<Vec<_>>(),
            vec![25, 19, 16, 25, 19],
        );
        assert_eq!(evs[0].end, 2000);
        for e in evs.iter().take(5) {
            assert!(song.huc6280_ins[e.ins as usize].vol.len() <= 60);
        }
    }

    #[test]
    fn test_noise_pitch_returns_to_the_first_value() {
        // 0x9F -> 0x9B -> 0x9F is a G2/D#2/G2 snare pattern. The noise
        // pitch list used to compare each change with the note's *start*
        // value, so the return to 0x9F was dropped and the voice stayed on
        // D#2 for the rest of the gate.
        let writes = vec![
            _w(0, 0x01, 0xFF),
            _w(0, 0x00, 4),
            _w(0, 0x04, 0x00),
            _w(0, 0x07, 0x9F),
            _w(0, 0x02, 0x00),
            _w(0, 0x03, 0x04),
            _w(0, 0x05, 0xFF),
            _w(0, 0x04, 0x9F),
            _w(2000, 0x07, 0x9B),
            _w(4000, 0x07, 0x9F),
            _w(6000, 0x04, 0x00),
        ];
        let song = song_of(&_vgm(writes, 3_579_545));
        let ev = song
            .events
            .iter()
            .find(|e| e.on && e.ch >= 8)
            .expect("note");
        assert!(ev.tone_noise);
        assert_eq!(ev.note, 91);
        assert_eq!(
            ev.tone_pitch_pts.iter().map(|&(_t, n)| n).collect::<Vec<_>>(),
            vec![31, 27, 31],
        );
    }

    #[test]
    fn test_noise_transient_then_tone_tail() {
        // A percussion hit: noise on at the gate, then 0x07=0 mid-gate
        // switches back to the waveform.  It stays one note; the switch is the
        // instrument's noise macro so it lands on the tick, not the row.
        let mut writes = vec![
            _w(0, 0x01, 0xFF),
            _w(0, 0x00, 4),
            _w(0, 0x04, 0x00),
        ];
        for v in _square() {
            writes.push(_w(0, 0x06, v as u32));
        }
        writes.extend([
            _w(0, 0x04, 0x40),
            _w(0, 0x02, 0x00),
            _w(0, 0x03, 0x04), // period 0x400
            _w(0, 0x07, 0x9D), // noise on, n=29
            _w(0, 0x05, 0xFF),
            _w(0, 0x04, 0x9F), // gate
            _w(500, 0x07, 0x00), // noise off
            _w(500, 0x02, 0x00),
            _w(500, 0x03, 0x02), // tone-tail pitch
            _w(5000, 0x04, 0x00), // gate off
        ]);
        let song = song_of(&_vgm(writes, 3_579_545));
        let ch4: Vec<_> = song
            .events
            .iter()
            .filter(|e| e.on && e.ch == 12)
            .collect();
        // Split at the transition so each segment's note boundary is placed
        // with EDxx (tick-accurate); its instrument states the mode.
        assert_eq!(ch4.len(), 2);
        assert!(ch4[0].tone_noise);
        assert_eq!(ch4[0].end, 500);
        assert!(!ch4[1].tone_noise);
        let macros: Vec<Vec<i32>> = song
            .huc6280_ins
            .iter()
            .filter(|i| !i.noise.is_empty())
            .map(|i| i.noise.clone())
            .collect();
        assert!(macros.contains(&vec![1]));
        assert!(macros.contains(&vec![0]));
    }

    #[test]
    fn test_noise_note_before_waveform_load() {
        // A snare can fire before the channel has loaded any waveform.  Noise
        // does not use the wave, so the note must survive the all-zero check.
        let writes = vec![
            _w(0, 0x01, 0xFF),
            _w(0, 0x00, 5),
            _w(0, 0x07, 0x9F),
            _w(0, 0x05, 0xFF),
            _w(0, 0x04, 0x9F),
            _w(5000, 0x04, 0x00),
        ];
        let song = song_of(&_vgm(writes, 3_579_545));
        let ch5: Vec<_> = song
            .events
            .iter()
            .filter(|e| e.on && e.ch == 13)
            .collect();
        assert_eq!(ch5.len(), 1);
        assert!(ch5[0].tone_noise);
    }

    #[test]
    fn test_every_pce_instrument_has_a_noise_macro() {
        // Every converted PCE instrument states its noise mode explicitly so a
        // following tone note always clears a preceding noise hit.
        let song = song_of(&_vgm(_load_wave(&_square()), 3_579_545));
        assert!(!song.huc6280_ins.is_empty());
        assert!(song.huc6280_ins.iter().all(|i| !i.noise.is_empty()));
        assert!(song.huc6280_ins.iter().all(|i| i.noise == vec![0]));
    }

    #[test]
    fn test_no_empty_opm_for_non_fm_source() {
        // A PCE-only rip must not gain the analyzer's YM2151 placeholder: the
        // extra device rescales the mix and shows eight silent columns.
        let song = song_of(&_vgm(_load_wave(&_square()), 3_579_545));
        let data = write_fur(
            &song,
            WriteOptions {
                include_pcm: true, // include_fm defaults to True
                ..Default::default()
            },
        );
        let mod_ = parse_fur(&data).expect("parse");
        assert_eq!(mod_.chips, vec![CHIP_PCE]);
        assert_eq!(mod_.n_ch, 6);
    }
}

#[allow(non_snake_case)]
mod TestHuc6280Pcm {
    use super::*;

    #[test]
    fn test_play_block_and_offset() {
        let pcm: Vec<u8> = vec![
            16, 20, 24, 28, 24, 20, 16, 12, 8, 4, 0, 4, 8, 12, 16, 16, 16, 16, 16, 16, 16,
            16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 16,
        ];
        // Setup one stream on channel 2, register 6 (direct D/A).
        let body: Vec<u8> = vec![
            0x90, 0x00, 27, 0x02, 0x06, // setup: stream 0, ch 2, reg 6
            0x91, 0x00, 0x05, 0x01, 0x00, // bank 5, step 1, base 0
            0x92, 0x00, 0x40, 0x1F, 0x00, 0x00, // 8000 Hz
            0x95, 0x00, 0x00, 0x00, 0x00, // play block 0
            0x94, 0x00, // stop
        ];
        let raw = _raw_vgm(&body, 3_579_545, 0, &[(0x05, pcm.clone())]);
        let p = temp_file(&tmp_dir(), "pcm.vgm", &raw);
        let v = load_vgm(&p).expect("load");
        let (samples, hits, warn) = collect_huc6280_pcm(&v);
        assert_eq!(warn, Vec::<String>::new());
        assert_eq!(samples.len(), 1);
        assert_eq!(hits.len(), 1);
        assert_eq!(samples[0].rate, 8000);
        assert_eq!(hits[0].voice, 2);
        // Every DDA level maps back to the register value.
        assert_eq!(samples[0].data, _dda_to_s8(&pcm));
    }

    #[test]
    fn test_dda_conversion_round_trip() {
        let raw: Vec<u8> = (0u8..32).collect();
        let converted = _dda_to_s8(&raw);
        for (&v, &b) in raw.iter().zip(converted.iter()) {
            let signed = (b ^ 0x80) as i8;
            assert_eq!(((signed >> 3) & 0x1F) as u8, v);
        }
    }

    #[test]
    fn test_module_has_sample_instruments() {
        let mut pcm = Vec::new();
        for _ in 0..64 {
            pcm.extend_from_slice(&[16u8, 20, 24, 28, 24, 20, 16, 12]);
        }
        let body: Vec<u8> = vec![
            0x90, 0x00, 27, 0x00, 0x06, //
            0x91, 0x00, 0x05, 0x01, 0x00, //
            0x92, 0x00, 40, 0x1F, 0x00, 0x00, //
            0x95, 0x00, 0x00, 0x00, 0x00, //
        ];
        let raw = _raw_vgm(&body, 3_579_545, 0, &[(0x05, pcm)]);
        let p = temp_file(&tmp_dir(), "pcm_mod.vgm", &raw);
        let v = load_vgm(&p).expect("load");
        let song = analyze(&v, AnalyzeOptions::default()).expect("analyze");
        assert_eq!(song.huc6280_pcm_samples.len(), 1);
        assert_eq!(song.huc6280_pcm_count, 1);
        let data = write_fur(
            &song,
            WriteOptions {
                include_pcm: true,
                include_fm: false,
                ..Default::default()
            },
        );
        let mod_ = parse_fur(&data).expect("parse");
        assert_eq!(mod_.chips, vec![CHIP_PCE]);
        assert_eq!(mod_.samples.len(), 1);
        let pcm_notes = song
            .events
            .iter()
            .filter(|e| e.on && e.pcm && e.note == 108)
            .count();
        assert_eq!(pcm_notes, 1);
    }
}
