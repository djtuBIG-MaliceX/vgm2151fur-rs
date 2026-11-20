//! K051649/K052539 (SCC) and AY-8910 support: parsing, collection, .fur emission.
//!
//! Port of `tests/test_vgm2151fur_scc_ay.py`.

mod common;
#[allow(unused_imports)]
use common::*;

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use vgm2fur::analyze::{_fold_saw, _saw_anchors, _tone_pitch_tick_scale, _unit_saw, analyze, AnalyzeOptions};
use vgm2fur::ay8910::{
    ay_min_period, ay_note, ay_rate, collect_ay8910, FX_ENV_SET, FX_MODE, FX_NOISE,
};
use vgm2fur::furio::{chip_channels, iter_blocks, parse_fur};
use vgm2fur::furwrite::{
    write_fur, WriteOptions, CHIP_AY8910, CHIP_K051649, CHIP_K052539, INS_AY, INS_SCC,
};
use vgm2fur::model::Song;
use vgm2fur::parity::py_round;
use vgm2fur::pitch::PeriodPitchTable;
use vgm2fur::scc::{collect_scc, scc_note, scc_rate, SCC_VOICES};
use vgm2fur::vgm::{chips_in_head, load_vgm, ChipWrite, VgmFile};

fn tmp_dir() -> PathBuf {
    let p = std::env::temp_dir().join("vgm2151fur_scc_ay");
    std::fs::create_dir_all(&p).expect("temp dir");
    p
}

fn scc_vgm(writes: Vec<ChipWrite>) -> VgmFile {
    vgm(writes)
}

fn raw_vgm_scc_ay(body: &[u8], scc_clock: u32, ay_clock: u32, plus: bool) -> Vec<u8> {
    let mut buf = vec![0u8; 0xC0];
    buf[0..4].copy_from_slice(b"Vgm ");
    buf[8..12].copy_from_slice(&0x161u32.to_le_bytes());
    buf[0x18..0x1C].copy_from_slice(&44100u32.to_le_bytes());
    buf[0x34..0x38].copy_from_slice(&(0xC0u32 - 0x34).to_le_bytes());
    if ay_clock != 0 {
        buf[0x74..0x78].copy_from_slice(&ay_clock.to_le_bytes());
        buf[0x78] = 0;
    }
    if scc_clock != 0 {
        let raw = scc_clock | if plus { 0x8000_0000 } else { 0 };
        buf[0x9C..0xA0].copy_from_slice(&raw.to_le_bytes());
    }
    let mut payload = buf;
    payload.extend_from_slice(body);
    payload.push(0x66);
    let len = (payload.len() - 4) as u32;
    payload[4..8].copy_from_slice(&len.to_le_bytes());
    payload
}

fn scc_wave(offset: u32, data: &[u8]) -> Vec<ChipWrite> {
    data.iter()
        .enumerate()
        .map(|(i, &b)| ChipWrite::new(0, "k051649", 0, offset + i as u32, b as u32))
        .collect()
}

fn scc_wave_at(sample: i64, offset: u32, data: &[u8]) -> Vec<ChipWrite> {
    data.iter()
        .enumerate()
        .map(|(i, &b)| ChipWrite::new(sample, "k051649", 0, offset + i as u32, b as u32))
        .collect()
}

fn sine() -> Vec<u8> {
    (0..32)
        .map(|i: i32| {
            let v = 127.0 * (2.0 * std::f64::consts::PI * i as f64 / 32.0).sin();
            (py_round(v) as i32 & 0xFF) as u8
        })
        .collect()
}

fn ay_vgm(writes: Vec<ChipWrite>, clock: u32) -> VgmFile {
    let mut v = scc_vgm(writes);
    v.ay8910_clock = clock;
    v
}

fn scc_song_vgm(writes: Vec<ChipWrite>, clock: u32, plus: bool) -> VgmFile {
    let mut v = scc_vgm(writes);
    v.k051649_clock = clock;
    v.k051649_plus = plus;
    v
}

fn find_subslice(hay: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || hay.len() < needle.len() {
        return None;
    }
    hay.windows(needle.len()).position(|w| w == needle)
}

/// Decode an INS2 block's MA feature into `{macro code: values}`.
fn ins_macros(block: &[u8]) -> BTreeMap<i32, Vec<i32>> {
    let pos = find_subslice(block, b"MA").expect("MA feature");
    let size = u16::from_le_bytes([block[pos + 2], block[pos + 3]]) as usize;
    let mut cursor = pos + 4;
    // Furnace's macro header size (8), not a byte count to skip.
    let _header = u16::from_le_bytes([block[cursor], block[cursor + 1]]);
    cursor += 2;
    let end = pos + 4 + size;
    let mut out = BTreeMap::new();
    while cursor < end && block[cursor] != 0xFF {
        let code = block[cursor];
        let length = block[cursor + 1];
        let _loop = block[cursor + 2];
        let _rel = block[cursor + 3];
        let _mode = block[cursor + 4];
        let word = block[cursor + 5];
        let _delay = block[cursor + 6];
        let _speed = block[cursor + 7];
        cursor += 8;
        let width = word >> 6;
        let mut values: Vec<i32> = Vec::new();
        for _ in 0..length {
            if width == 2 {
                values.push(i16::from_le_bytes([block[cursor], block[cursor + 1]]) as i32);
                cursor += 2;
            } else if width == 3 {
                values.push(i32::from_le_bytes([
                    block[cursor],
                    block[cursor + 1],
                    block[cursor + 2],
                    block[cursor + 3],
                ]));
                cursor += 4;
            } else {
                values.push(block[cursor] as i32);
                cursor += 1;
            }
        }
        out.insert(code as i32, values);
    }
    out
}

fn wave_values(block: &[u8]) -> Vec<u32> {
    let name_end = block.iter().position(|&b| b == 0).expect("name terminator");
    let mut pos = name_end + 1;
    let length = u32::from_le_bytes([
        block[pos],
        block[pos + 1],
        block[pos + 2],
        block[pos + 3],
    ]) as usize;
    pos += 4;
    pos += 4; // min
    pos += 4; // max
    (0..length)
        .map(|i| {
            let p = pos + 4 * i;
            u32::from_le_bytes([block[p], block[p + 1], block[p + 2], block[p + 3]])
        })
        .collect()
}

fn payloads<'a>(data: &'a [u8], magic: &[u8; 4]) -> Vec<&'a [u8]> {
    iter_blocks(data)
        .expect("blocks")
        .into_iter()
        .filter(|b| b.magic == *magic)
        .map(|b| b.payload)
        .collect()
}

fn ins_types(data: &[u8]) -> Vec<i32> {
    payloads(data, b"INS2")
        .iter()
        .map(|b| u16::from_le_bytes([b[2], b[3]]) as i32)
        .collect()
}

fn write_song(song: &Song) -> Vec<u8> {
    write_fur(
        song,
        WriteOptions {
            include_pcm: true,
            include_fm: false,
            ..Default::default()
        },
    )
}

fn contains_bytes(hay: &[u8], needle: &[u8]) -> bool {
    find_subslice(hay, needle).is_some()
}

fn repeat(values: &[i32], n: usize) -> Vec<i32> {
    let mut out = Vec::with_capacity(values.len() * n);
    for _ in 0..n {
        out.extend_from_slice(values);
    }
    out
}

/// `_levels_vgm`: one SCC+ note whose volume register follows `levels`.
fn levels_vgm(levels: &[i32], wave: &[u8], origin: i64) -> VgmFile {
    let t0 = origin;
    let mut writes: Vec<ChipWrite> = wave
        .iter()
        .enumerate()
        .map(|(i, &b)| ChipWrite::new(t0, "k051649", 0, i as u32, b as u32))
        .collect();
    writes.push(ChipWrite::new(t0, "k051649", 0, 0x100, (253 & 0xFF) as u32));
    writes.push(ChipWrite::new(t0, "k051649", 0, 0x101, 0));
    writes.push(ChipWrite::new(t0, "k051649", 0, 0x200, (levels[0] & 0x0F) as u32));
    writes.push(ChipWrite::new(t0 + 100, "k051649", 0, 0x300, 0x01));
    for (i, &level) in levels[1..].iter().enumerate() {
        writes.push(ChipWrite::new(
            t0 + 100 + 735 * (i as i64 + 1),
            "k051649",
            0,
            0x200,
            (level & 0x0F) as u32,
        ));
    }
    let off = t0 + 100 + 735 * (levels.len() as i64 - 1) + 1;
    writes.push(ChipWrite::new(off, "k051649", 0, 0x300, 0x00));
    scc_song_vgm(writes, 1789772, true)
}

// --- parsing --------------------------------------------------------------

mod TestSccParsing {
    use super::*;

    #[test]
    fn test_d2_command_and_header() {
        let body = [
            0xD2, 0, 0, 0x00, 0xD2, 0, 1, 0x40, // wave voice 0
            0xD2, 1, 0, 0xFD, 0xD2, 1, 1, 0x00, // period 253
            0xD2, 2, 0, 0x0F, // volume
            0xD2, 3, 0, 0x01, // enable voice 0
        ];
        let p = temp_file(
            &tmp_dir(),
            "scc_header.vgm",
            &raw_vgm_scc_ay(&body, 1789773, 0, false),
        );
        let vgm = load_vgm(&p).unwrap();
        assert_eq!(vgm.k051649_clock, 1789773);
        assert!(!vgm.k051649_plus);
        let writes: Vec<(u32, u32)> = vgm
            .writes
            .iter()
            .filter(|w| w.chip == "k051649")
            .map(|w| (w.reg, w.val))
            .collect();
        assert!(writes.contains(&(0x000, 0x00)));
        assert!(writes.contains(&(0x100, 0xFD)));
        assert!(writes.contains(&(0x200, 0x0F)));
        assert!(writes.contains(&(0x300, 0x01)));
    }

    #[test]
    fn test_plus_flag_in_clock_field() {
        let p = temp_file(
            &tmp_dir(),
            "scc_plus.vgm",
            &raw_vgm_scc_ay(b"\x66", 1789773, 0, true),
        );
        let vgm = load_vgm(&p).unwrap();
        assert!(vgm.k051649_plus);
        let head = std::fs::read(&p).unwrap();
        let kinds = chips_in_head(&head);
        assert!(kinds.contains("k051649"));
        assert!(kinds.contains("scc_plus"));
    }

    #[test]
    fn test_ay_command_and_header() {
        let body = [
            0xA0, 7, 0xFE, 0xA0, 8, 0x0F, 0xA0, 0, 0xFE, 0xA0, 1, 0x00,
        ];
        let p = temp_file(
            &tmp_dir(),
            "ay_header.vgm",
            &raw_vgm_scc_ay(&body, 0, 1789773, false),
        );
        let vgm = load_vgm(&p).unwrap();
        assert_eq!(vgm.ay8910_clock, 1789773);
        assert_eq!(vgm.ay8910_type, 0);
        let writes: Vec<(u32, u32)> = vgm
            .writes
            .iter()
            .filter(|w| w.chip == "ay8910")
            .map(|w| (w.reg, w.val))
            .collect();
        assert_eq!(writes[0], (7, 0xFE));
        let head = std::fs::read(&p).unwrap();
        assert!(chips_in_head(&head).contains("ay8910"));
    }

    #[test]
    fn test_multi_pcm_is_not_a0() {
        use vgm2fur::vgm::UNCONVERTED_SAMPLE_CMDS;
        assert!(!UNCONVERTED_SAMPLE_CMDS.iter().any(|(c, _)| *c == 0xA0));
        assert!(!UNCONVERTED_SAMPLE_CMDS.iter().any(|(c, _)| *c == 0xB5));
    }
}

// --- SCC collection -------------------------------------------------------

mod TestSccCollect {
    use super::*;

    fn song(period: i32, volume: i32, plus: bool, wave: &[u8]) -> VgmFile {
        let mut writes = scc_wave(0, wave);
        writes.push(ChipWrite::new(0, "k051649", 0, 0x100, (period & 0xFF) as u32));
        writes.push(ChipWrite::new(0, "k051649", 0, 0x101, (period >> 8) as u32));
        writes.push(ChipWrite::new(0, "k051649", 0, 0x200, volume as u32));
        writes.push(ChipWrite::new(100, "k051649", 0, 0x300, 0x01));
        writes.push(ChipWrite::new(5000, "k051649", 0, 0x300, 0x00));
        scc_song_vgm(writes, 1789772, plus)
    }

    #[test]
    fn test_note_and_gate() {
        let sine = sine();
        let (notes, offs, _waves, _warn) = collect_scc(&song(253, 15, false, &sine));
        assert_eq!(notes.len(), 1);
        let hit = &notes[0].hit;
        assert_eq!(hit.voice, 0);
        assert_eq!(hit.period, 253);
        assert_eq!(hit.vol, 15);
        assert_eq!(hit.sample_time, 100);
        assert_eq!(offs.len(), 1);
        assert_eq!(offs[0].sample_time, 5000);
        assert_eq!(scc_rate(1789772, 253), 1789772.0 / (16.0 * 254.0));
        let (note, e5) = scc_note(1789772, 253);
        assert!(note >= 0);
        assert!(note <= 179);
        // the exported register is the note's pitch value minus one
        let table = PeriodPitchTable::new(440.0, 1789772.0, 16.0, 0xFFF);
        assert_eq!(table.value(note, e5 - 0x80) - 1, 253);
    }

    #[test]
    fn test_period_below_nine_is_silent() {
        // The chip halts a voice at period <= 8; the collector still reports
        // the gate (the key-off matters), the analyzer drops the note.
        let vgm_ = song(5, 15, false, &sine());
        let (notes, _offs, _waves, _warn) = collect_scc(&vgm_);
        assert_eq!(notes.len(), 1);
        let song = analyze(&vgm_, AnalyzeOptions::default()).unwrap();
        assert_eq!(song.scc_count, 0);
        assert!(!song.events.iter().any(|e| e.on && e.ch >= 8));
        assert!(song.events.iter().any(|e| !e.on && e.ch >= 8));
    }

    #[test]
    fn test_shared_wave_on_k051649() {
        // A write at 0x60 reaches voices 3 and 4 on the 5-voice chip.
        let sine = sine();
        let (notes, _offs, waves, _warn) = collect_scc(&song(253, 15, false, &sine));
        assert_eq!(notes.len(), 1);
        assert_eq!(waves.len(), 1);
        // On the K052539 the same address is voice 3's own wave only.
        let mut writes = scc_wave(0x60, &sine);
        writes.push(ChipWrite::new(0, "k051649", 0, 0x100, (253 & 0xFF) as u32));
        writes.push(ChipWrite::new(0, "k051649", 0, 0x101, (253 >> 8) as u32));
        writes.push(ChipWrite::new(0, "k051649", 0, 0x203, 15));
        writes.push(ChipWrite::new(100, "k051649", 0, 0x300, 0x08)); // voice 3
        let vgm_ = scc_song_vgm(writes, 1789772, false);
        let (notes, _offs, _waves, _warn) = collect_scc(&vgm_);
        assert_eq!(notes.len(), 1);
        assert_eq!(notes[0].hit.voice, 3);
        assert_eq!(notes[0].hit.wave, sine);
    }

    #[test]
    fn test_plus_voice4_wave_port() {
        let sine = sine();
        let mut writes = scc_wave(0x80, &sine);
        writes.push(ChipWrite::new(0, "k051649", 0, 0x108, (253 & 0xFF) as u32));
        writes.push(ChipWrite::new(0, "k051649", 0, 0x109, (253 >> 8) as u32));
        writes.push(ChipWrite::new(0, "k051649", 0, 0x204, 15));
        writes.push(ChipWrite::new(100, "k051649", 0, 0x300, 0x10)); // voice 4
        let vgm_ = scc_song_vgm(writes, 1789772, true);
        let (notes, _offs, _waves, _warn) = collect_scc(&vgm_);
        assert_eq!(notes.len(), 1);
        assert_eq!(notes[0].hit.voice, 4);
        assert_eq!(notes[0].hit.wave, sine);
    }

    #[test]
    fn test_plus_bank4_is_wave_ram() {
        // Yie Ar Kung-Fu II writes the waveforms through bank 4 and never
        // touches bank 0.
        let sine = sine();
        let reversed: Vec<u8> = sine.iter().rev().copied().collect();
        let voice4: Vec<u8> = sine.iter().map(|&b| (b.wrapping_add(3)) & 0xFF).collect();
        let mut writes = scc_wave(0x400, &sine);
        writes.extend(scc_wave(0x460, &reversed));
        writes.extend(scc_wave(0x480, &voice4));
        writes.push(ChipWrite::new(0, "k051649", 0, 0x4A0, 0xFF)); // shadow, ignored
        writes.push(ChipWrite::new(0, "k051649", 0, 0x100, (253 & 0xFF) as u32));
        writes.push(ChipWrite::new(0, "k051649", 0, 0x101, 0));
        writes.push(ChipWrite::new(0, "k051649", 0, 0x106, (253 & 0xFF) as u32));
        writes.push(ChipWrite::new(0, "k051649", 0, 0x107, 0));
        writes.push(ChipWrite::new(0, "k051649", 0, 0x108, (253 & 0xFF) as u32));
        writes.push(ChipWrite::new(0, "k051649", 0, 0x109, 0));
        writes.push(ChipWrite::new(0, "k051649", 0, 0x200, 15));
        writes.push(ChipWrite::new(0, "k051649", 0, 0x203, 15));
        writes.push(ChipWrite::new(0, "k051649", 0, 0x204, 15));
        writes.push(ChipWrite::new(100, "k051649", 0, 0x300, 0x19)); // voices 0, 3, 4
        let vgm_ = scc_song_vgm(writes, 1789772, true);
        let (notes, _offs, _waves, _warn) = collect_scc(&vgm_);
        let by_voice: BTreeMap<i32, Vec<u8>> =
            notes.iter().map(|n| (n.hit.voice, n.hit.wave.clone())).collect();
        let keys: BTreeSet<i32> = by_voice.keys().copied().collect();
        assert_eq!(keys, BTreeSet::from([0, 3, 4]));
        assert_eq!(by_voice[&0], sine);
        assert_eq!(by_voice[&3], reversed);
        assert_eq!(by_voice[&4], voice4);
        assert!(!by_voice[&4].contains(&0xFF));
    }
}

// --- AY collection --------------------------------------------------------

mod TestAyCollect {
    use super::*;

    fn writes(
        mixer: i32,
        volume: i32,
        period: i32,
        noise: i32,
        env_shape: Option<i32>,
        gate_at: i64,
        off_at: i64,
    ) -> Vec<ChipWrite> {
        let mut writes = vec![
            ChipWrite::new(0, "ay8910", 0, 7, mixer as u32),
            ChipWrite::new(0, "ay8910", 0, 6, noise as u32),
            ChipWrite::new(0, "ay8910", 0, 0, (period & 0xFF) as u32),
            ChipWrite::new(0, "ay8910", 0, 1, (period >> 8) as u32),
        ];
        if let Some(shape) = env_shape {
            writes.push(ChipWrite::new(0, "ay8910", 0, 11, 66)); // envelope period lo
            writes.push(ChipWrite::new(0, "ay8910", 0, 12, 0));
            writes.push(ChipWrite::new(0, "ay8910", 0, 13, shape as u32));
        }
        writes.push(ChipWrite::new(gate_at, "ay8910", 0, 8, volume as u32));
        writes.push(ChipWrite::new(off_at, "ay8910", 0, 8, 0));
        writes
    }

    fn defaults() -> Vec<ChipWrite> {
        writes(0xFE, 0x0F, 254, 0, None, 100, 5000)
    }

    #[test]
    fn test_tone_note() {
        let vgm_ = ay_vgm(defaults(), 1789773);
        let (hits, offs, _warn) = collect_ay8910(&vgm_);
        assert_eq!(hits.len(), 1);
        let h = &hits[0];
        assert_eq!(h.voice, 0);
        assert_eq!(h.period, 254);
        assert_eq!(h.vol, 15);
        assert!(h.tone);
        assert!(!h.noise);
        assert_eq!(offs.len(), 1);
        assert_almost_eq!(ay_rate(1789773, 254), 1789773.0 / (16.0 * 254.0));
        let (note, e5) = ay_note(1789773, 254, 8.0);
        let table = PeriodPitchTable::new(440.0, 1789773.0, 8.0, 0xFFF);
        assert_eq!(table.value(note, e5 - 0x80), 254);
    }

    #[test]
    fn test_percussion_attack_keeps_its_peak() {
        // Gate at 15, then the first decay step a frame later.
        let w = [
            ChipWrite::new(0, "ay8910", 0, 7, 0xFE),
            ChipWrite::new(0, "ay8910", 0, 0, 254),
            ChipWrite::new(0, "ay8910", 0, 1, 0),
            ChipWrite::new(100, "ay8910", 0, 8, 0x0F),
            ChipWrite::new(800, "ay8910", 0, 8, 0x0E),
            ChipWrite::new(1500, "ay8910", 0, 8, 0x0D),
            ChipWrite::new(6000, "ay8910", 0, 8, 0x00),
        ];
        let (hits, _offs, _warn) = collect_ay8910(&ay_vgm(w.to_vec(), 1789773));
        let hit = &hits[0];
        assert_eq!(hit.vol, 15);
        assert!(hit.vol_pts.contains(&(800, 14)));
    }

    #[test]
    fn test_level_jump_up_is_a_new_hit() {
        // A snare roll keeps the gate open and jumps the level back up.
        let w = [
            ChipWrite::new(0, "ay8910", 0, 7, 0xF6),
            ChipWrite::new(0, "ay8910", 0, 0, 254),
            ChipWrite::new(0, "ay8910", 0, 1, 0),
            ChipWrite::new(100, "ay8910", 0, 8, 0x0D),
            ChipWrite::new(1500, "ay8910", 0, 8, 0x07),
            ChipWrite::new(2500, "ay8910", 0, 8, 0x0F),
            ChipWrite::new(8000, "ay8910", 0, 8, 0x00),
        ];
        let (hits, _offs, _warn) = collect_ay8910(&ay_vgm(w.to_vec(), 1789773));
        let pairs: Vec<(i64, i32)> = hits.iter().map(|h| (h.sample_time, h.vol)).collect();
        assert_eq!(pairs, vec![(100, 13), (2500, 15)]);
        assert_eq!(hits[0].off_sample, 2500);
    }

    #[test]
    fn test_gate_opening_trick_is_absorbed() {
        // A loud level written only to open the gate, then the real quiet
        // level within a frame.
        let w = [
            ChipWrite::new(0, "ay8910", 0, 7, 0xFE),
            ChipWrite::new(0, "ay8910", 0, 0, 254),
            ChipWrite::new(0, "ay8910", 0, 1, 0),
            ChipWrite::new(100, "ay8910", 0, 8, 0x0F),
            ChipWrite::new(300, "ay8910", 0, 8, 0x04),
            ChipWrite::new(6000, "ay8910", 0, 8, 0x00),
        ];
        let (hits, _offs, _warn) = collect_ay8910(&ay_vgm(w.to_vec(), 1789773));
        assert_eq!(hits[0].vol, 4);
    }

    #[test]
    fn test_opening_envelope_cleared_in_the_burst_becomes_fixed_volume() {
        let w = [
            ChipWrite::new(0, "ay8910", 0, 7, 0xFE),
            ChipWrite::new(0, "ay8910", 0, 0, 254),
            ChipWrite::new(0, "ay8910", 0, 1, 0),
            ChipWrite::new(0, "ay8910", 0, 11, 66),
            ChipWrite::new(0, "ay8910", 0, 12, 0),
            ChipWrite::new(0, "ay8910", 0, 13, 8),
            ChipWrite::new(100, "ay8910", 0, 8, 0x12), // envelope, level 2
            ChipWrite::new(103, "ay8910", 0, 8, 0x08), // fixed level 8
            ChipWrite::new(6000, "ay8910", 0, 8, 0),
        ];
        let (hits, _offs, _warn) = collect_ay8910(&ay_vgm(w.to_vec(), 1789773));
        let hit = &hits[0];
        assert_eq!(hit.vol, 8);
        assert!(!hit.env);
        assert!(!hit.fx_pts.iter().any(|(_, fx, _)| *fx == FX_ENV_SET));
        let modes: Vec<i32> = hit
            .fx_pts
            .iter()
            .filter(|(_, fx, _)| *fx == FX_MODE)
            .map(|(_, _, v)| *v)
            .collect();
        assert!(!modes.is_empty());
        assert_eq!((modes[modes.len() - 1] + 1) & 4, 0);
    }

    #[test]
    fn test_opening_shape_change_settles_on_the_note_row() {
        let w = [
            ChipWrite::new(0, "ay8910", 0, 7, 0xFE),
            ChipWrite::new(0, "ay8910", 0, 0, 254),
            ChipWrite::new(0, "ay8910", 0, 1, 0),
            ChipWrite::new(0, "ay8910", 0, 11, 160),
            ChipWrite::new(0, "ay8910", 0, 12, 0),
            ChipWrite::new(0, "ay8910", 0, 13, 8),
            ChipWrite::new(100, "ay8910", 0, 8, 0x12),
            ChipWrite::new(117, "ay8910", 0, 13, 9),
            ChipWrite::new(6000, "ay8910", 0, 8, 0),
        ];
        let song = analyze(&ay_vgm(w.to_vec(), 1789773), AnalyzeOptions::default()).unwrap();
        let ev = song
            .events
            .iter()
            .find(|e| e.on && e.tone_chip == 2)
            .expect("tone event");
        let fx: BTreeMap<i32, i32> = ev.tone_fx_pts.iter().map(|(_, c, v)| (*c, *v)).collect();
        assert_eq!(fx[&FX_ENV_SET], 0x91);
        let ins = &song.ay_ins[ev.ins as usize - song.scc_ins.len()];
        assert!(ins.env_shape.is_empty());
    }

    #[test]
    fn test_tick_scale_uses_the_fastest_active_tone_voice() {
        // Voice 0 writes every ~700 samples and voice 1 is sparse.
        let mut w: Vec<ChipWrite> = vec![ChipWrite::new(0, "ay8910", 0, 7, 0x38)];
        for i in 0..10i64 {
            w.push(ChipWrite::new(i * 700, "ay8910", 0, 0, i as u32));
            w.push(ChipWrite::new(i * 700, "ay8910", 0, 2, i as u32));
        }
        w.push(ChipWrite::new(0, "ay8910", 0, 2, 0));
        for i in 0..10i64 {
            w.push(ChipWrite::new(10000 + i * 10000, "ay8910", 0, 2, i as u32));
        }
        assert_eq!(
            _tone_pitch_tick_scale(&ay_vgm(w, 1789773), 60.0, 1),
            2
        );
    }

    #[test]
    fn test_noise_enable_carries_the_period_written_while_off() {
        let w = [
            ChipWrite::new(0, "ay8910", 0, 7, 0xFE), // tone A on, noise A off
            ChipWrite::new(0, "ay8910", 0, 0, 254),
            ChipWrite::new(0, "ay8910", 0, 1, 0),
            ChipWrite::new(100, "ay8910", 0, 8, 12),
            ChipWrite::new(101, "ay8910", 0, 6, 13),  // noise period while off
            ChipWrite::new(110, "ay8910", 0, 7, 0xF6), // noise A on
            ChipWrite::new(8000, "ay8910", 0, 8, 0),
        ];
        let (hits, _offs, _warn) = collect_ay8910(&ay_vgm(w.to_vec(), 1789773));
        assert_eq!(hits.len(), 1);
        assert!(hits[0].fx_pts.contains(&(110, FX_NOISE, 31 - 13)));
    }

    #[test]
    fn test_noise_note() {
        let vgm_ = ay_vgm(writes(0xF7, 0x0F, 254, 8, None, 100, 5000), 1789773);
        let (hits, _offs, _warn) = collect_ay8910(&vgm_);
        assert_eq!(hits.len(), 1);
        assert!(hits[0].noise);
        assert!(!hits[0].tone);
        let fx: BTreeMap<i32, i32> = hits[0].fx_pts.iter().map(|(_, c, v)| (*c, *v)).collect();
        assert_eq!(fx[&0x20], 1); // mode: noise only (value = mode - 1)
        assert_eq!(fx[&0x21], 31 - 8);
    }

    #[test]
    fn test_noise_attack_on_a_sounding_tone_is_a_new_hit() {
        let w = [
            ChipWrite::new(0, "ay8910", 0, 7, 0xFE), // tone on, noise off
            ChipWrite::new(0, "ay8910", 0, 0, 254),
            ChipWrite::new(0, "ay8910", 0, 1, 0),
            ChipWrite::new(100, "ay8910", 0, 8, 15), // body
            ChipWrite::new(5000, "ay8910", 0, 0, 30), // attack pitch
            ChipWrite::new(5000, "ay8910", 0, 1, 0),
            ChipWrite::new(5000, "ay8910", 0, 6, 31),
            ChipWrite::new(5000, "ay8910", 0, 7, 0xF6), // tone + noise
            ChipWrite::new(6000, "ay8910", 0, 7, 0xFE), // next body: tone only
            ChipWrite::new(6000, "ay8910", 0, 8, 0x10), // envelope retrigger
            ChipWrite::new(6000, "ay8910", 0, 11, 66),
            ChipWrite::new(6000, "ay8910", 0, 12, 0),
            ChipWrite::new(6000, "ay8910", 0, 13, 9),
            ChipWrite::new(20000, "ay8910", 0, 8, 0),
        ];
        let (hits, _offs, _warn) = collect_ay8910(&ay_vgm(w.to_vec(), 1789773));
        let times: Vec<i64> = hits.iter().map(|h| h.sample_time).collect();
        assert_eq!(times, vec![100, 5000, 6000]);
        let first = &hits[0];
        assert!(!first.fx_pts.iter().any(|(t, cmd, _)| {
            (*cmd == FX_MODE || *cmd == FX_NOISE) && *t > first.sample_time
        }));
        assert_eq!(hits[1].period, 30);
        assert!(hits[1].fx_pts.contains(&(5000, FX_MODE, 2))); // tone + noise
        assert!(hits[1].fx_pts.contains(&(5000, FX_NOISE, 0)));
        assert!(!hits[2].noise);
        assert!(hits[2].env);
    }

    #[test]
    fn test_tone_to_noise_handover_stays_one_note() {
        let w = [
            ChipWrite::new(0, "ay8910", 0, 7, 0xFE),
            ChipWrite::new(0, "ay8910", 0, 0, 1024 & 0xFF),
            ChipWrite::new(0, "ay8910", 0, 1, 1024 >> 8),
            ChipWrite::new(100, "ay8910", 0, 8, 12),
            ChipWrite::new(2300, "ay8910", 0, 7, 0xF7), // tone off, noise on
            ChipWrite::new(3000, "ay8910", 0, 8, 0),
        ];
        let (hits, _offs, _warn) = collect_ay8910(&ay_vgm(w.to_vec(), 1789772));
        assert_eq!(hits.len(), 1);
    }

    #[test]
    fn test_envelope_note() {
        let vgm_ = ay_vgm(writes(0xFE, 0x12, 254, 0, Some(9), 100, 5000), 1789773);
        let (hits, _offs, _warn) = collect_ay8910(&vgm_);
        assert_eq!(hits.len(), 1);
        let h = &hits[0];
        assert!(h.env);
        let fx: Vec<(i32, i32)> = h.fx_pts.iter().map(|(_, c, v)| (*c, *v)).collect();
        assert!(fx.contains(&(0x22, (9 << 4) | 1))); // shape 9, enable this channel
        assert!(fx.contains(&(0x23, 66)));
        assert!(fx.contains(&(0x24, 0)));
    }

    #[test]
    fn test_volume_gate_retrigger() {
        let w = [
            ChipWrite::new(0, "ay8910", 0, 7, 0xFE),
            ChipWrite::new(0, "ay8910", 0, 0, 254),
            ChipWrite::new(0, "ay8910", 0, 1, 0),
            ChipWrite::new(100, "ay8910", 0, 8, 15),
            ChipWrite::new(3000, "ay8910", 0, 8, 0),
            ChipWrite::new(4000, "ay8910", 0, 8, 12),
            ChipWrite::new(8000, "ay8910", 0, 8, 0),
        ];
        let (hits, _offs, _warn) = collect_ay8910(&ay_vgm(w.to_vec(), 1789773));
        let times: Vec<i64> = hits.iter().map(|h| h.sample_time).collect();
        assert_eq!(times, vec![100, 4000]);
        let vols: Vec<i32> = hits.iter().map(|h| h.vol).collect();
        assert_eq!(vols, vec![15, 12]);
    }
}

// --- .fur emission --------------------------------------------------------

mod TestFurEmission {
    use super::*;

    fn scc_song(plus: bool) -> VgmFile {
        let sine = sine();
        let mut writes = scc_wave(0, &sine);
        writes.extend(scc_wave(0x20, &sine.iter().rev().copied().collect::<Vec<_>>()));
        writes.push(ChipWrite::new(0, "k051649", 0, 0x100, (253 & 0xFF) as u32));
        writes.push(ChipWrite::new(0, "k051649", 0, 0x101, (253 >> 8) as u32));
        writes.push(ChipWrite::new(0, "k051649", 0, 0x102, (672 & 0xFF) as u32));
        writes.push(ChipWrite::new(0, "k051649", 0, 0x103, (672 >> 8) as u32));
        writes.push(ChipWrite::new(0, "k051649", 0, 0x200, 15));
        writes.push(ChipWrite::new(0, "k051649", 0, 0x201, 12));
        writes.push(ChipWrite::new(100, "k051649", 0, 0x300, 0x01));
        writes.push(ChipWrite::new(20000, "k051649", 0, 0x200, 8));
        writes.push(ChipWrite::new(30000, "k051649", 0, 0x300, 0x00));
        writes.push(ChipWrite::new(40000, "k051649", 0, 0x300, 0x02));
        writes.push(ChipWrite::new(60000, "k051649", 0, 0x300, 0x00));
        scc_song_vgm(writes, 1789772, plus)
    }

    #[test]
    fn test_scc_module() {
        let song = analyze(&scc_song(true), AnalyzeOptions::default()).unwrap();
        assert_eq!(song.scc_ids, vec![0]);
        assert!(song.scc_waves.len() >= 2);
        assert_eq!(song.scc_count, 2);
        let notes: Vec<_> = song.events.iter().filter(|e| e.on && e.ch >= 8).collect();
        assert_eq!(notes.len(), 2);
        assert_eq!(notes[0].ch, 8);
        assert_eq!(notes[0].vol, 15);
        let data = write_song(&song);
        let mod_ = parse_fur(&data).unwrap();
        assert_eq!(mod_.chips, vec![CHIP_K052539]);
        assert_eq!(chip_channels(CHIP_K052539), SCC_VOICES);
        assert_eq!(mod_.n_ch, SCC_VOICES);
        let types = ins_types(&data);
        assert!(types.contains(&INS_SCC));
        assert_eq!(
            types.iter().filter(|&&t| t == INS_SCC).count(),
            song.scc_waves.len()
        );
        // wave blocks carry the unsigned image (signed + 128)
        let waves = payloads(&data, b"WAVE");
        assert_eq!(waves.len(), song.scc_waves.len());
        let expected: Vec<u32> = sine().iter().map(|&b| (b.wrapping_add(128)) as u32).collect();
        assert_eq!(wave_values(waves[0]), expected);
    }

    #[test]
    fn test_scc_nonplus_chip_byte() {
        let song = analyze(&scc_song(false), AnalyzeOptions::default()).unwrap();
        let data = write_song(&song);
        let mod_ = parse_fur(&data).unwrap();
        assert_eq!(mod_.chips, vec![CHIP_K051649]);
    }

    #[test]
    fn test_ay_module() {
        let w = [
            ChipWrite::new(0, "ay8910", 0, 7, 0xFE),
            ChipWrite::new(0, "ay8910", 0, 0, 254),
            ChipWrite::new(0, "ay8910", 0, 1, 0),
            ChipWrite::new(0, "ay8910", 0, 11, 66),
            ChipWrite::new(0, "ay8910", 0, 12, 0),
            ChipWrite::new(0, "ay8910", 0, 13, 9),
            ChipWrite::new(100, "ay8910", 0, 8, 0x12),
            ChipWrite::new(10000, "ay8910", 0, 8, 0),
            ChipWrite::new(12000, "ay8910", 0, 8, 9),
            ChipWrite::new(20000, "ay8910", 0, 8, 0),
        ];
        let song = analyze(&ay_vgm(w.to_vec(), 1789773), AnalyzeOptions::default()).unwrap();
        assert_eq!(song.ay_ids, vec![0]);
        assert_eq!(song.ay_count, 2);
        let data = write_song(&song);
        let mod_ = parse_fur(&data).unwrap();
        assert_eq!(mod_.chips, vec![CHIP_AY8910]);
        assert_eq!(mod_.n_ch, 3);
        let types = ins_types(&data);
        // One instrument for the envelope note and one plain tone instrument.
        assert_eq!(types, vec![INS_AY, INS_AY]);
        // Only the envelope instrument carries the phase-reset macro.
        let reset = [14u8, 2, 255, 255, 0, 1, 0, 1, 1, 0];
        let ins = payloads(&data, b"INS2");
        assert_eq!(ins.iter().filter(|b| contains_bytes(b, &reset)).count(), 1);
        // the envelope effects reach the pattern
        let fx: Vec<(u8, u8)> = mod_
            .patterns
            .values()
            .flat_map(|rows| rows.iter())
            .flat_map(|row| row.fx.iter().copied())
            .collect();
        let cmds: BTreeSet<u8> = fx.iter().map(|(c, _)| *c).collect();
        assert!(cmds.contains(&0x22));
        assert!(cmds.contains(&0x23));
        assert!(cmds.contains(&0x24));
    }

    #[test]
    fn test_mid_note_envelope_write_is_a_new_hit() {
        let w = [
            ChipWrite::new(0, "ay8910", 0, 7, 0xFE), // tone A on
            ChipWrite::new(0, "ay8910", 0, 0, 254),
            ChipWrite::new(0, "ay8910", 0, 1, 0),
            ChipWrite::new(0, "ay8910", 0, 11, 66),
            ChipWrite::new(0, "ay8910", 0, 12, 0),
            ChipWrite::new(0, "ay8910", 0, 13, 0x0A),
            ChipWrite::new(100, "ay8910", 0, 8, 0x10), // envelope on
            ChipWrite::new(5000, "ay8910", 0, 13, 0x0A), // same shape: re-arm
            ChipWrite::new(9000, "ay8910", 0, 13, 0x0A),
            ChipWrite::new(15000, "ay8910", 0, 8, 0),
        ];
        let song = analyze(&ay_vgm(w.to_vec(), 1789773), AnalyzeOptions::default()).unwrap();
        assert_eq!(song.ay_count, 3);
        let samples: Vec<i64> = song
            .events
            .iter()
            .filter(|e| e.on && e.ch == 8)
            .map(|e| e.sample)
            .collect();
        assert_eq!(samples, vec![100, 5000, 9000]);
    }

    #[test]
    fn test_ay_mode_macro_uses_the_pre_193_encoding() {
        let w = [
            ChipWrite::new(0, "ay8910", 0, 7, 0xF6), // tone + noise
            ChipWrite::new(0, "ay8910", 0, 0, 254),
            ChipWrite::new(0, "ay8910", 0, 1, 0),
            ChipWrite::new(0, "ay8910", 0, 11, 66),
            ChipWrite::new(0, "ay8910", 0, 12, 0),
            ChipWrite::new(0, "ay8910", 0, 13, 9),
            ChipWrite::new(100, "ay8910", 0, 8, 0x12),
            // Mid-note mode change: tone+noise+env (7) becomes tone+env (5).
            ChipWrite::new(5000, "ay8910", 0, 7, 0xFE),
            ChipWrite::new(15000, "ay8910", 0, 8, 0),
        ];
        let song = analyze(&ay_vgm(w.to_vec(), 1789773), AnalyzeOptions::default()).unwrap();
        let data = write_song(&song);
        let ins = payloads(&data, b"INS2");
        let macros = ins_macros(ins[0]);
        let mode = &macros[&3];
        assert_eq!(&mode[..2], &[6, 6]); // 7 -> 6
        assert_eq!(mode[mode.len() - 1], 4); // 5 -> 4
        // The row effect keeps the same encoding (value = bits - 1).
        let mod_ = parse_fur(&data).unwrap();
        let fx: Vec<(u8, u8)> = mod_
            .patterns
            .values()
            .flat_map(|rows| rows.iter())
            .flat_map(|row| row.fx.iter().copied())
            .collect();
        assert!(fx.contains(&(0x20, 6)));
    }

    #[test]
    fn test_short_ay_attack_survives_a_row_collision() {
        let at = 10000i64;
        let w = [
            ChipWrite::new(0, "ay8910", 0, 7, 0xFE),
            ChipWrite::new(0, "ay8910", 0, 0, 254),
            ChipWrite::new(0, "ay8910", 0, 1, 0),
            ChipWrite::new(0, "ay8910", 0, 6, 8),
            ChipWrite::new(at, "ay8910", 0, 8, 15),
            ChipWrite::new(at, "ay8910", 0, 7, 0xF6), // noise attack
            ChipWrite::new(at + 200, "ay8910", 0, 0, 40),
            ChipWrite::new(at + 200, "ay8910", 0, 1, 0),
            ChipWrite::new(at + 200, "ay8910", 0, 7, 0xFE), // body: tone only
            ChipWrite::new(at + 200, "ay8910", 0, 8, 0x10), // envelope retrigger
            ChipWrite::new(at + 200, "ay8910", 0, 11, 66),
            ChipWrite::new(at + 200, "ay8910", 0, 12, 0),
            ChipWrite::new(at + 200, "ay8910", 0, 13, 9),
            ChipWrite::new(40000, "ay8910", 0, 8, 0),
        ];
        let song = analyze(&ay_vgm(w.to_vec(), 1789773), AnalyzeOptions::default()).unwrap();
        let mut stats = indexmap::IndexMap::new();
        let data = write_fur(
            &song,
            WriteOptions {
                include_pcm: true,
                include_fm: false,
                stats: Some(&mut stats),
                ..Default::default()
            },
        );
        let mod_ = parse_fur(&data).unwrap();
        let mut rows: Vec<usize> = Vec::new();
        for (oi, &pidx) in mod_.orders[0].iter().enumerate() {
            if let Some(pat) = mod_.patterns.get(&(0u16, pidx)) {
                for (ri, row) in pat.iter().enumerate() {
                    if (0..180).contains(&row.note) {
                        rows.push(oi * 256 + ri);
                    }
                }
            }
        }
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[1], rows[0] + 1);
        assert_eq!(stats.get("note_shifted"), Some(&1));
        assert!(!stats.contains_key("note_collision"));
    }

    #[test]
    fn test_condensed_rows_keep_both_notes() {
        let at = 10000i64;
        let w = [
            ChipWrite::new(0, "ay8910", 0, 7, 0xFE),
            ChipWrite::new(0, "ay8910", 0, 0, 254),
            ChipWrite::new(0, "ay8910", 0, 1, 0),
            ChipWrite::new(at, "ay8910", 0, 8, 15),
            ChipWrite::new(at + 300, "ay8910", 0, 0, 200),
            ChipWrite::new(at + 300, "ay8910", 0, 1, 0),
            ChipWrite::new(at + 300, "ay8910", 0, 8, 15),
            ChipWrite::new(40000, "ay8910", 0, 8, 0),
        ];
        let song = analyze(
            &ay_vgm(w.to_vec(), 1789773),
            AnalyzeOptions {
                condense: 4.0,
                ..Default::default()
            },
        )
        .unwrap();
        let mut stats = indexmap::IndexMap::new();
        let data = write_fur(
            &song,
            WriteOptions {
                include_pcm: true,
                include_fm: false,
                stats: Some(&mut stats),
                ..Default::default()
            },
        );
        let mod_ = parse_fur(&data).unwrap();
        let mut rows: Vec<usize> = Vec::new();
        for (oi, &pidx) in mod_.orders[0].iter().enumerate() {
            if let Some(pat) = mod_.patterns.get(&(0u16, pidx)) {
                for (ri, row) in pat.iter().enumerate() {
                    if (0..180).contains(&row.note) {
                        rows.push(oi * 256 + ri);
                    }
                }
            }
        }
        assert_eq!(rows.len(), 2);
        let distinct: BTreeSet<usize> = rows.iter().copied().collect();
        assert_eq!(distinct.len(), 2);
        assert!(stats.get("note_shifted").copied().unwrap_or(0) >= 1);
    }
}

// --- tone macros ----------------------------------------------------------

mod TestToneMacros {
    use super::*;

    fn bend_song() -> VgmFile {
        let mut writes = scc_wave(0, &sine());
        writes.push(ChipWrite::new(0, "k051649", 0, 0x100, (3008 & 0xFF) as u32));
        writes.push(ChipWrite::new(0, "k051649", 0, 0x101, (3008 >> 8) as u32));
        writes.push(ChipWrite::new(0, "k051649", 0, 0x200, 12));
        writes.push(ChipWrite::new(100, "k051649", 0, 0x300, 0x01));
        // bend away and come back: the pair (low, high) is one value
        for (t, per) in [(44100i64, 3400i64), (88200, 3008)] {
            writes.push(ChipWrite::new(t, "k051649", 0, 0x100, (per & 0xFF) as u32));
            writes.push(ChipWrite::new(t, "k051649", 0, 0x101, (per >> 8) as u32));
        }
        writes.push(ChipWrite::new(132300, "k051649", 0, 0x300, 0x00));
        scc_song_vgm(writes, 1789772, true)
    }

    #[test]
    fn test_held_pitch_change_is_a_new_note() {
        let song = analyze(&bend_song(), AnalyzeOptions::default()).unwrap();
        let notes: Vec<_> = song
            .events
            .iter()
            .filter(|e| e.on && e.tone_chip == 1)
            .collect();
        assert_eq!(notes.len(), 3);
        assert!(notes.iter().all(|n| n.tone_pitch_pts.is_empty()));
        for spec in &song.scc_ins {
            assert!(spec.pitch.is_empty());
        }
        // the bend and the return each land on their own cell
        assert_eq!(notes[1].sample, 44100);
        assert_eq!(notes[2].sample, 88200);
    }

    #[test]
    fn test_vibrato_stays_on_the_note() {
        let mut writes = scc_wave(0, &sine());
        writes.push(ChipWrite::new(0, "k051649", 0, 0x100, (3008 & 0xFF) as u32));
        writes.push(ChipWrite::new(0, "k051649", 0, 0x101, (3008 >> 8) as u32));
        writes.push(ChipWrite::new(0, "k051649", 0, 0x200, 12));
        writes.push(ChipWrite::new(100, "k051649", 0, 0x300, 0x01));
        for (t, per) in [(22050i64, 3040i64), (44100, 2976), (66150, 3008)] {
            writes.push(ChipWrite::new(t, "k051649", 0, 0x100, (per & 0xFF) as u32));
            writes.push(ChipWrite::new(t, "k051649", 0, 0x101, (per >> 8) as u32));
        }
        writes.push(ChipWrite::new(88200, "k051649", 0, 0x300, 0x00));
        let song = analyze(&scc_song_vgm(writes, 1789772, true), AnalyzeOptions::default())
            .unwrap();
        let notes: Vec<_> = song
            .events
            .iter()
            .filter(|e| e.on && e.tone_chip == 1)
            .collect();
        assert_eq!(notes.len(), 1);
        assert!(!song.scc_ins[0].pitch.is_empty());
        assert_eq!(song.scc_ins[0].pitch[0], 0);
        let pmax = *song.scc_ins[0].pitch.iter().max().unwrap();
        let pmin = *song.scc_ins[0].pitch.iter().min().unwrap();
        assert!(pmax - pmin < 128);
    }

    #[test]
    fn test_melody_uses_the_measured_tempo() {
        let mut writes = scc_wave(0, &sine());
        writes.push(ChipWrite::new(0, "k051649", 0, 0x200, 12));
        writes.push(ChipWrite::new(0, "k051649", 0, 0x300, 0x01));
        for i in 0..32i64 {
            let per: i64 = if i % 2 == 0 { 253 } else { 400 };
            let t = i * 5880;
            writes.push(ChipWrite::new(t, "k051649", 0, 0x100, (per & 0xFF) as u32));
            writes.push(ChipWrite::new(t, "k051649", 0, 0x101, (per >> 8) as u32));
        }
        writes.push(ChipWrite::new(32 * 5880, "k051649", 0, 0x300, 0x00));
        let song = analyze(&scc_song_vgm(writes, 1789772, true), AnalyzeOptions::default())
            .unwrap();
        let notes: Vec<_> = song
            .events
            .iter()
            .filter(|e| e.on && e.tone_chip == 1)
            .collect();
        assert!(notes.len() >= 30);
        let bpm = 15.0 * song.hz / song.speed as f64;
        assert!(bpm > 80.0);
        assert!(bpm < 160.0);
        assert!(!song.grid_note.contains("60 Hz fallback"));
        assert_eq!(song.row_subdiv, 1.0);
        assert_eq!(song.hilight_a, 4);
        for spec in &song.scc_ins {
            if !spec.pitch.is_empty() {
                let pmax = *spec.pitch.iter().max().unwrap();
                let pmin = *spec.pitch.iter().min().unwrap();
                assert!(pmax - pmin < 256);
            }
        }
    }

    #[test]
    fn test_short_held_steps_are_notes() {
        let mut writes = scc_wave(0, &sine());
        writes.push(ChipWrite::new(0, "k051649", 0, 0x200, 12));
        writes.push(ChipWrite::new(0, "k051649", 0, 0x300, 0x01));
        for i in 0..8i64 {
            let per: i64 = if i % 2 == 0 { 3008 } else { 3400 };
            let t = i * 1000;
            writes.push(ChipWrite::new(t, "k051649", 0, 0x100, (per & 0xFF) as u32));
            writes.push(ChipWrite::new(t, "k051649", 0, 0x101, (per >> 8) as u32));
        }
        writes.push(ChipWrite::new(8 * 1000, "k051649", 0, 0x300, 0x00));
        let song = analyze(&scc_song_vgm(writes, 1789772, true), AnalyzeOptions::default())
            .unwrap();
        let notes: Vec<_> = song
            .events
            .iter()
            .filter(|e| e.on && e.tone_chip == 1)
            .collect();
        assert_eq!(notes.len(), 8);
        for spec in &song.scc_ins {
            if !spec.pitch.is_empty() {
                let pmax = *spec.pitch.iter().max().unwrap();
                let pmin = *spec.pitch.iter().min().unwrap();
                assert!(pmax - pmin < 128);
            }
        }
    }

    #[test]
    fn test_opening_pitch_keeps_its_cell() {
        let mut writes = scc_wave(0, &sine());
        writes.push(ChipWrite::new(0, "k051649", 0, 0x200, 12));
        writes.push(ChipWrite::new(0, "k051649", 0, 0x300, 0x01));
        writes.push(ChipWrite::new(0, "k051649", 0, 0x100, (3400 & 0xFF) as u32));
        writes.push(ChipWrite::new(0, "k051649", 0, 0x101, (3400 >> 8) as u32));
        writes.push(ChipWrite::new(1000, "k051649", 0, 0x100, (3008 & 0xFF) as u32));
        writes.push(ChipWrite::new(1000, "k051649", 0, 0x101, (3008 >> 8) as u32));
        writes.push(ChipWrite::new(40000, "k051649", 0, 0x300, 0x00));
        let song = analyze(&scc_song_vgm(writes, 1789772, true), AnalyzeOptions::default())
            .unwrap();
        let notes: Vec<_> = song
            .events
            .iter()
            .filter(|e| e.on && e.tone_chip == 1)
            .collect();
        assert_eq!(notes.len(), 2);
        assert_eq!(notes[0].sample, 0);
        assert_eq!(notes[0].note, scc_note(1789772, 3400).0);
        assert_eq!(notes[1].sample, 1000);
        assert_eq!(notes[1].note, scc_note(1789772, 3008).0);
    }

    #[test]
    fn test_close_melody_notes_keep_the_quarter_tempo() {
        let mut writes = scc_wave(0, &sine());
        writes.push(ChipWrite::new(0, "k051649", 0, 0x200, 12));
        writes.push(ChipWrite::new(0, "k051649", 0, 0x300, 0x01));
        let step = 5880i64;
        for i in 0..32i64 {
            let per: i64 = if i % 2 == 0 { 253 } else { 400 };
            let t = i * step;
            writes.push(ChipWrite::new(t, "k051649", 0, 0x100, (per & 0xFF) as u32));
            writes.push(ChipWrite::new(t, "k051649", 0, 0x101, (per >> 8) as u32));
            if i == 4 || i == 12 {
                // A third pitch, so the ornament is not the same note.
                let ornament = 180i64;
                let at = t + 2205;
                writes.push(ChipWrite::new(at, "k051649", 0, 0x100, (ornament & 0xFF) as u32));
                writes.push(ChipWrite::new(at, "k051649", 0, 0x101, (ornament >> 8) as u32));
            }
        }
        writes.push(ChipWrite::new(32 * step, "k051649", 0, 0x300, 0x00));
        let song = analyze(&scc_song_vgm(writes, 1789772, true), AnalyzeOptions::default())
            .unwrap();
        let notes: Vec<_> = song
            .events
            .iter()
            .filter(|e| e.on && e.tone_chip == 1)
            .collect();
        assert!(notes.len() >= 34);
        assert!(song.row_subdiv > 1.0);
        assert_eq!(song.hilight_a, py_round(4.0 * song.row_subdiv) as i32);
        assert_eq!(song.hilight_b, py_round(16.0 * song.row_subdiv) as i32);
        // The fit drops ticks per row. The tick itself stays one driver frame.
        let tick = 44100.0 / song.hz;
        assert!(tick > 700.0);
        assert!(tick < 780.0);
        let quarter = 60.0 * song.hz / (song.hilight_a * song.speed) as f64;
        assert!(quarter > 80.0);
        assert!(quarter < 160.0);
        assert!(song.bpm() > 80.0);
        assert!(song.bpm() < 160.0);
        let mut stats = indexmap::IndexMap::new();
        let data = write_fur(
            &song,
            WriteOptions {
                include_pcm: true,
                include_fm: false,
                stats: Some(&mut stats),
                ..Default::default()
            },
        );
        assert_eq!(stats.get("note_collision").copied().unwrap_or(0), 0);
        let info = payloads(&data, b"INFO");
        assert_eq!(info[0][12] as i32, song.hilight_a);
        assert_eq!(info[0][13] as i32, song.hilight_b);
    }

    fn change_at(song: &Song, level: i32) -> f64 {
        let spec = &song.scc_ins[0];
        assert!(!spec.vol.is_empty());
        let tick = 44100.0 / song.hz;
        for (i, &value) in spec.vol.iter().enumerate() {
            if value == level {
                let pos = if i == 0 {
                    0
                } else {
                    spec.vol_delay + 1 + (i as i32 - 1) * spec.vol_speed
                };
                return pos as f64 * tick;
            }
        }
        panic!("level {} missing from {:?}", level, spec.vol);
    }

    #[test]
    fn test_volume_macro_tracks_real_time_when_condensed() {
        let mut writes = scc_wave(0, &sine());
        writes.push(ChipWrite::new(0, "k051649", 0, 0x100, (253 & 0xFF) as u32));
        writes.push(ChipWrite::new(0, "k051649", 0, 0x101, (253 >> 8) as u32));
        writes.push(ChipWrite::new(0, "k051649", 0, 0x200, 15));
        writes.push(ChipWrite::new(100, "k051649", 0, 0x300, 0x01));
        writes.push(ChipWrite::new(100 + 735, "k051649", 0, 0x200, 10));
        writes.push(ChipWrite::new(100 + 1470, "k051649", 0, 0x200, 5));
        writes.push(ChipWrite::new(8000, "k051649", 0, 0x300, 0x00));
        let vgm_ = scc_song_vgm(writes, 1789772, true);

        for factor in [1.0, 4.0] {
            let song = analyze(
                &vgm_,
                AnalyzeOptions {
                    condense: factor,
                    ..Default::default()
                },
            )
            .unwrap();
            // the drop to 10 is one frame after the gate
            let delta = 44100.0 / song.hz;
            let got = change_at(&song, 10);
            assert!(
                (got - 735.0).abs() <= delta,
                "change_at(10) = {} vs 735 (delta {})",
                got,
                delta
            );
            let note = song
                .events
                .iter()
                .find(|e| e.on && e.tone_chip == 1)
                .expect("tone note");
            assert_eq!(note.vol, 15);
            assert!(note.vol_pts.is_empty());
        }
    }

    #[test]
    fn test_wave_morph_macro() {
        let sine = sine();
        let mut writes = scc_wave(0, &sine);
        writes.push(ChipWrite::new(0, "k051649", 0, 0x100, (252 & 0xFF) as u32));
        writes.push(ChipWrite::new(0, "k051649", 0, 0x101, (252 >> 8) as u32));
        writes.push(ChipWrite::new(0, "k051649", 0, 0x200, 12));
        writes.push(ChipWrite::new(100, "k051649", 0, 0x300, 0x01));
        // the driver rewrites the whole image, one byte-burst per morph step
        for (k, shift) in [2u8, 4, 6].into_iter().enumerate() {
            let t = 22050 + k as i64 * 22050;
            let ram: Vec<u8> = sine.iter().map(|&b| b.wrapping_add(shift)).collect();
            writes.extend(scc_wave_at(t, 0, &ram));
        }
        writes.push(ChipWrite::new(200000, "k051649", 0, 0x300, 0x00));
        let song = analyze(&scc_song_vgm(writes, 1789772, true), AnalyzeOptions::default())
            .unwrap();
        assert!(song.scc_waves.len() >= 4); // the morph images
        let spec: Vec<_> = song.scc_ins.iter().filter(|s| !s.waves.is_empty()).collect();
        assert!(!spec.is_empty(), "no wave macro was written");
        let macro_ = &spec[0].waves;
        let distinct: BTreeSet<i32> = macro_.iter().copied().collect();
        assert!(distinct.len() >= 4);
        assert_eq!(macro_[0], 0); // starts on the note's own image
        let data = write_song(&song);
        let waves = payloads(&data, b"WAVE");
        assert_eq!(waves.len(), song.scc_waves.len());
        // the instrument carries the wave macro (file code 3)
        let ins = payloads(&data, b"INS2");
        assert!(ins.iter().any(|b| contains_bytes(b, &[3u8])));
    }

    #[test]
    fn test_ay_period_change_is_a_new_note() {
        let w = [
            ChipWrite::new(0, "ay8910", 0, 7, 0xFE),
            ChipWrite::new(0, "ay8910", 0, 0, 254),
            ChipWrite::new(0, "ay8910", 0, 1, 0),
            ChipWrite::new(100, "ay8910", 0, 8, 12),
            ChipWrite::new(5880, "ay8910", 0, 0, 400 & 0xFF),
            ChipWrite::new(5880, "ay8910", 0, 1, 400 >> 8),
            ChipWrite::new(12000, "ay8910", 0, 8, 0),
        ];
        let song = analyze(&ay_vgm(w.to_vec(), 1789772), AnalyzeOptions::default()).unwrap();
        let notes: Vec<_> = song
            .events
            .iter()
            .filter(|e| e.on && e.tone_chip == 2)
            .collect();
        assert_eq!(notes.len(), 2);
        assert_eq!(notes[1].sample, 5880);
        for spec in &song.ay_ins {
            assert!(spec.pitch.is_empty());
        }
    }

    #[test]
    fn test_ay_attack_sweep_stays_on_the_note() {
        // The PSG tom walks 1104, 1024, 1536, 1792, 2048 across four frames.
        let gate = 100i64;
        let periods = [1104i64, 1024, 1536, 1792, 2048];
        let gaps = [521i64, 2, 734, 736];
        let mut w = vec![
            ChipWrite::new(0, "ay8910", 0, 7, 0xFE),
            ChipWrite::new(0, "ay8910", 0, 0, (periods[0] & 0xFF) as u32),
            ChipWrite::new(0, "ay8910", 0, 1, (periods[0] >> 8) as u32),
            ChipWrite::new(0, "ay8910", 0, 11, 160),
            ChipWrite::new(0, "ay8910", 0, 12, 0),
            ChipWrite::new(0, "ay8910", 0, 13, 9),
            ChipWrite::new(gate, "ay8910", 0, 8, 0x10),
        ];
        let mut t = gate;
        for (gap, per) in gaps.iter().zip(periods[1..].iter()) {
            t += gap;
            w.push(ChipWrite::new(t, "ay8910", 0, 0, (per & 0xFF) as u32));
            w.push(ChipWrite::new(t, "ay8910", 0, 1, (per >> 8) as u32));
        }
        w.push(ChipWrite::new(gate + 2716, "ay8910", 0, 8, 0));
        let song = analyze(&ay_vgm(w, 1789772), AnalyzeOptions::default()).unwrap();
        let notes: Vec<_> = song
            .events
            .iter()
            .filter(|e| e.on && e.tone_chip == 2)
            .collect();
        assert_eq!(notes.len(), 1);
        let pitched: Vec<_> = song.ay_ins.iter().filter(|s| !s.pitch.is_empty()).collect();
        assert_eq!(pitched.len(), 1);
        let span = pitched[0].pitch.iter().max().unwrap() - pitched[0].pitch.iter().min().unwrap();
        assert!(span > 128 * 3);
    }

    #[test]
    fn test_ay_glide_kept_when_the_tone_bit_ends_off() {
        let w = [
            ChipWrite::new(0, "ay8910", 0, 7, 0xFE),
            ChipWrite::new(0, "ay8910", 0, 0, 1024 & 0xFF),
            ChipWrite::new(0, "ay8910", 0, 1, 1024 >> 8),
            ChipWrite::new(100, "ay8910", 0, 8, 12),
            ChipWrite::new(849, "ay8910", 0, 0, 1152 & 0xFF),
            ChipWrite::new(849, "ay8910", 0, 1, 1152 >> 8),
            ChipWrite::new(1577, "ay8910", 0, 0, 1280 & 0xFF),
            ChipWrite::new(1577, "ay8910", 0, 1, 1280 >> 8),
            ChipWrite::new(2300, "ay8910", 0, 7, 0xF7),
            ChipWrite::new(3000, "ay8910", 0, 8, 0),
        ];
        let song = analyze(&ay_vgm(w.to_vec(), 1789772), AnalyzeOptions::default()).unwrap();
        let notes: Vec<_> = song
            .events
            .iter()
            .filter(|e| e.on && e.tone_chip == 2)
            .collect();
        assert_eq!(notes.len(), 1);
        let pitched: Vec<_> = song.ay_ins.iter().filter(|s| !s.pitch.is_empty()).collect();
        assert_eq!(pitched.len(), 1);
        let span = pitched[0].pitch.iter().max().unwrap() - pitched[0].pitch.iter().min().unwrap();
        assert!(span > 256);
    }

    #[test]
    fn test_ay_envelope_pitch_change_stays_one_note() {
        let w = [
            ChipWrite::new(0, "ay8910", 0, 7, 0xFE),
            ChipWrite::new(0, "ay8910", 0, 0, 254),
            ChipWrite::new(0, "ay8910", 0, 1, 0),
            ChipWrite::new(0, "ay8910", 0, 11, 66),
            ChipWrite::new(0, "ay8910", 0, 12, 0),
            ChipWrite::new(0, "ay8910", 0, 13, 9),
            ChipWrite::new(100, "ay8910", 0, 8, 0x10),
            ChipWrite::new(5880, "ay8910", 0, 0, 400 & 0xFF),
            ChipWrite::new(5880, "ay8910", 0, 1, 400 >> 8),
            ChipWrite::new(12000, "ay8910", 0, 8, 0),
        ];
        let song = analyze(&ay_vgm(w.to_vec(), 1789772), AnalyzeOptions::default()).unwrap();
        let notes: Vec<_> = song
            .events
            .iter()
            .filter(|e| e.on && e.tone_chip == 2)
            .collect();
        assert_eq!(notes.len(), 1);
        assert_eq!(notes[0].vol, 15);
        let shapes: Vec<i64> = song
            .channel_fx
            .iter()
            .filter(|(_, _, cmd, _)| *cmd == 0x22)
            .map(|(sample, _, _, _)| *sample)
            .collect();
        assert_eq!(shapes, vec![notes[0].sample]);
        let pitched: Vec<_> = song.ay_ins.iter().filter(|s| !s.pitch.is_empty()).collect();
        assert_eq!(pitched.len(), 1);
        let span = pitched[0].pitch.iter().max().unwrap() - pitched[0].pitch.iter().min().unwrap();
        assert!(span > 256);
    }

    #[test]
    fn test_ay_envelope_sweep_is_a_macro() {
        let mut w = vec![
            ChipWrite::new(0, "ay8910", 0, 7, 0xFE),
            ChipWrite::new(0, "ay8910", 0, 0, 254),
            ChipWrite::new(0, "ay8910", 0, 1, 0),
            ChipWrite::new(0, "ay8910", 0, 11, 40),
            ChipWrite::new(0, "ay8910", 0, 12, 0),
            ChipWrite::new(0, "ay8910", 0, 13, 9),
            ChipWrite::new(100, "ay8910", 0, 8, 0x10),
        ];
        for (i, period) in [80i64, 120, 200].iter().enumerate() {
            let t = 100 + (i as i64 + 1) * 735;
            w.push(ChipWrite::new(t, "ay8910", 0, 11, (period & 0xFF) as u32));
            w.push(ChipWrite::new(t, "ay8910", 0, 12, (period >> 8) as u32));
        }
        w.push(ChipWrite::new(8000, "ay8910", 0, 8, 0));
        let song = analyze(&ay_vgm(w, 1789772), AnalyzeOptions::default()).unwrap();
        assert_eq!(song.ay_count, 1);
        let spec = &song.ay_ins[0];
        assert!(spec.env_period.len() >= 2);
        assert!(spec.env_period.contains(&40));
        assert!(spec.env_period.contains(&200));
        let data = write_song(&song);
        let ins = payloads(&data, b"INS2");
        // file code 16 is the envelope-period macro
        assert!(contains_bytes(ins[0], &[16u8]));
    }

    #[test]
    fn test_ay_silence_gate_has_no_note() {
        let lo = ay_min_period(1789772, 8.0);
        let w = [
            ChipWrite::new(0, "ay8910", 0, 7, 0xFE), // both on
            ChipWrite::new(0, "ay8910", 0, 0, 254),
            ChipWrite::new(0, "ay8910", 0, 1, 0),
            ChipWrite::new(100, "ay8910", 0, 8, 12), // audible note
            ChipWrite::new(2000, "ay8910", 0, 0, 1), // mute: period below range
            ChipWrite::new(2000, "ay8910", 0, 1, 0),
            ChipWrite::new(2000, "ay8910", 0, 8, 2),
            ChipWrite::new(4000, "ay8910", 0, 8, 0),
        ];
        let (hits, offs, _warn) = collect_ay8910(&ay_vgm(w.to_vec(), 1789772));
        assert!(lo > 1);
        let hit_times: Vec<i64> = hits.iter().map(|h| h.sample_time).collect();
        assert_eq!(hit_times, vec![100]);
        let off_times: Vec<i64> = offs.iter().map(|o| o.sample_time).collect();
        assert_eq!(off_times, vec![2000]);
    }

    #[test]
    fn test_pitch_split_keeps_the_mode_it_starts_in() {
        let w = [
            ChipWrite::new(0, "ay8910", 0, 7, 0x38), // tone on, noise off
            ChipWrite::new(0, "ay8910", 0, 0, 0x00),
            ChipWrite::new(0, "ay8910", 0, 1, 0x04),
            ChipWrite::new(0, "ay8910", 0, 8, 12),
            ChipWrite::new(2000, "ay8910", 0, 0, 0x00),
            ChipWrite::new(2000, "ay8910", 0, 1, 0x02), // octave up: splits
            ChipWrite::new(3500, "ay8910", 0, 7, 0x31), // tone off, noise on
            ChipWrite::new(6000, "ay8910", 0, 8, 0),
        ];
        let song = analyze(&ay_vgm(w.to_vec(), 1789772), AnalyzeOptions::default()).unwrap();
        let notes: Vec<_> = song
            .events
            .iter()
            .filter(|e| e.on && e.tone_chip == 2)
            .collect();
        assert_eq!(
            notes.iter().map(|n| n.sample).collect::<Vec<_>>(),
            vec![0, 2000]
        );
        assert!(notes[1].tone_fx_pts.contains(&(2000, FX_MODE, 0)));
        assert!(notes[1].tone_fx_pts.contains(&(3500, FX_MODE, 1)));
        let spec = &song.ay_ins[notes[1].ins as usize];
        assert!(!spec.mode.is_empty());
        assert_eq!(spec.mode[0], 1); // tone at the segment start
        assert!(spec.mode.contains(&2)); // switches to noise later
    }
}

// --- volume saw -----------------------------------------------------------

mod TestVolumeSaw {
    use super::*;

    #[test]
    fn test_fold_saw_phase_and_partial_tooth() {
        let tooth = [13i32, 12, 11, 10, 9, 8, 7];
        assert_eq!(
            _fold_saw(&repeat(&tooth, 3)),
            Some((tooth.to_vec(), 1, 0))
        );
        // The note opened in the middle of a tooth.
        let mid = [4i32, 3, 2, 1, 6, 5, 4, 3, 2, 1, 6, 5];
        assert_eq!(_fold_saw(&mid), Some((vec![4, 3, 2, 1, 6, 5], 1, 0)));
        // The last tooth stops early.
        let mut short = tooth.to_vec();
        short.extend([13, 12, 11]);
        assert_eq!(_fold_saw(&short), Some((tooth.to_vec(), 1, 0)));
        let mut two = repeat(&tooth, 2);
        two.push(7);
        assert_eq!(_fold_saw(&two), Some((tooth.to_vec(), 1, 0)));
        // A real rest at the bottom is not a loop.
        let mut rest = repeat(&tooth, 2);
        rest.extend([7, 7, 7]);
        assert_eq!(_fold_saw(&rest), None);
        assert_eq!(_fold_saw(&tooth), None);
        assert!(!_unit_saw(&[15, 10, 5]));
        assert!(_unit_saw(&repeat(&tooth, 2)));
    }

    #[test]
    fn test_anchors_skip_the_running_slide() {
        let series = [6i32, 5, 4, 4, 4, 3, 2, 8];
        let anchors = _saw_anchors(&series, 100, 735.0);
        assert_eq!(
            anchors,
            vec![
                (100 + 735 * 3, 4, 0),
                (100 + 735 * 5, 3, 1),
                (100 + 735 * 7, 8, 1),
            ]
        );
    }

    #[test]
    fn test_repeating_tooth_is_a_looping_macro() {
        let tooth = [13i32, 12, 11, 10, 9, 8, 7];
        let song = analyze(
            &levels_vgm(&repeat(&tooth, 4), &sine(), 0),
            AnalyzeOptions {
                speed: Some(1),
                ..Default::default()
            },
        )
        .unwrap();
        let note = song
            .events
            .iter()
            .find(|e| e.on && e.tone_chip == 1)
            .expect("tone note");
        let spec = &song.scc_ins[note.ins as usize];
        assert_eq!(spec.vol, tooth.to_vec());
        assert_eq!(spec.vol_loop, 0);
        assert_eq!(spec.vol_speed, 1);
        assert_eq!(note.vol, 15);
        assert!(note.vol_pts.is_empty());
        assert_eq!(note.vol_slide, 0);
    }

    #[test]
    fn test_unit_saw_survives_the_coarse_instrument_list() {
        // Past 160 tone instruments a non-saw volume macro is rounded to
        // steps of 2. A one-level saw on a later note stays at every level.
        let base = sine();
        let mut writes: Vec<ChipWrite> = Vec::new();
        for i in 0..165u32 {
            let origin = i as i64 * 20000;
            let mut raw = base.clone();
            raw[0] = ((i + 1) & 0xFF) as u8;
            raw[1] = 0x40;
            writes.extend(levels_vgm(&[15, 10, 5], &raw, origin).writes);
        }
        let tooth = [13i32, 12, 11, 10, 9, 8, 7];
        let origin = 165 * 20000;
        let mut raw = base.clone();
        raw[0] = 200;
        raw[1] = 0x41;
        writes.extend(levels_vgm(&repeat(&tooth, 3), &raw, origin).writes);
        let vgm_ = scc_song_vgm(writes, 1789772, true);
        let song = analyze(
            &vgm_,
            AnalyzeOptions {
                speed: Some(1),
                ..Default::default()
            },
        )
        .unwrap();
        let notes: Vec<_> = song
            .events
            .iter()
            .filter(|e| e.on && e.tone_chip == 1)
            .collect();
        let note = notes[notes.len() - 1];
        let spec = &song.scc_ins[note.ins as usize];
        assert!(song.scc_ins.len() > 160);
        assert_eq!(spec.vol, tooth.to_vec());
        assert_eq!(spec.vol_loop, 0);
        assert_eq!(note.vol, 15);
        assert_eq!(note.vol_slide, 0);
    }

    #[test]
    fn test_overlong_irregular_saw_is_a_fast_slide() {
        let mut levels: Vec<i32> = Vec::new();
        let mut peak = 12i32;
        for k in 0..40 {
            levels.extend((1..=peak).rev());
            levels.extend(std::iter::repeat(1).take(k as usize));
            peak = 6 + ((peak + 3) % 7);
        }
        levels.truncate(320);
        assert!(levels.len() > 255);
        let song = analyze(
            &levels_vgm(&levels, &sine(), 0),
            AnalyzeOptions {
                speed: Some(1),
                ..Default::default()
            },
        )
        .unwrap();
        let note = song
            .events
            .iter()
            .find(|e| e.on && e.tone_chip == 1)
            .expect("tone note");
        let spec = &song.scc_ins[note.ins as usize];
        assert!(spec.vol.is_empty());
        assert_eq!(note.vol_slide, 1);
        assert_eq!(note.vol, levels[0]);
        assert!(!note.vol_pts.is_empty());
        let data = write_song(&song);
        let mod_ = parse_fur(&data).unwrap();
        let fx: Vec<(u8, u8)> = mod_
            .patterns
            .values()
            .flat_map(|rows| rows.iter())
            .flat_map(|row| row.fx.iter().copied())
            .collect();
        assert!(fx.contains(&(0xFA, 0x01)));
    }
}

// --- emission helpers -----------------------------------------------------

mod TestEmissionHelpers {
    use super::*;

    #[test]
    fn test_scc_wave_values_unsigned() {
        let mut block: Vec<u8> = vec![0u8];
        block.extend_from_slice(&3u32.to_le_bytes());
        block.extend_from_slice(&0u32.to_le_bytes());
        block.extend_from_slice(&255u32.to_le_bytes());
        for v in [0u32, 128, 255] {
            block.extend_from_slice(&v.to_le_bytes());
        }
        assert_eq!(wave_values(&block), vec![0, 128, 255]);
    }
}
