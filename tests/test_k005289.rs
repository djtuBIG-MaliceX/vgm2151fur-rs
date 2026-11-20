//! Konami K005289 (Bubble System WSG): parsing, collection, .fur emission.
//!
//! Port of `tests/test_k005289.py`.

mod common;
#[allow(unused_imports)]
use common::*;

use std::path::{Path, PathBuf};

use vgm2fur::analyze::{analyze, AnalyzeOptions};
use vgm2fur::diag::{
    _fur_wsg_stream, _runs_between, compare_fur_to_source, k005289_keyed_intervals,
    k005289_pitch_stream, tone_pitch_stream,
};
use vgm2fur::edit::read_chips_file;
use vgm2fur::furio::{iter_blocks, parse_fur, CHIP_CHANNELS};
use vgm2fur::furwrite::{write_fur, WriteOptions, CHIP_K005289, INS_SCC};
use vgm2fur::k005289::{
    collect_k005289, k005289_note, k005289_rate, K005289_PROM_SIZE,
};
use vgm2fur::model::Song;
use vgm2fur::parity::{py_round, py_round_to};
use vgm2fur::pitch::PeriodPitchTable;
use vgm2fur::vgm::{
    assemble_rom, chips_in_head, load_vgm, ChipWrite, RomSlice, VgmFile,
};

fn tmp() -> PathBuf {
    let p = std::env::temp_dir().join("vgm2151fur_k005289");
    std::fs::create_dir_all(&p).expect("temp dir");
    p
}

fn pack() -> Option<PathBuf> {
    let dir = samples_root()?.join("Gradius VGM Rip");
    dir.is_dir().then_some(dir)
}

/// A 4-bit ramp and its reverse; both are valid 32-sample PROM waveforms.
fn ramp() -> Vec<u8> {
    (0..32u8).map(|i| i % 16).collect()
}

fn ramp_down() -> Vec<u8> {
    (0..32u8).map(|i| 15 - (i % 16)).collect()
}

fn prom_custom(wave0: &[u8], wave1: &[u8], bank1: Option<&[u8]>) -> Vec<u8> {
    let mut prom = vec![0u8; K005289_PROM_SIZE];
    prom[0..32].copy_from_slice(wave0);
    let default_bank = ramp_down();
    prom[0x20..0x40].copy_from_slice(bank1.unwrap_or(&default_bank));
    prom[0x100..0x120].copy_from_slice(wave1);
    prom
}

fn prom() -> Vec<u8> {
    prom_custom(&ramp(), &ramp_down(), None)
}

/// 0xC3 RAM write: 16-bit address, then the 512 PROM bytes.
fn prom_block(prom: &[u8]) -> Vec<u8> {
    let mut out = vec![0x67, 0x66, 0xC3];
    out.extend_from_slice(&((2 + prom.len()) as u32).to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(prom);
    out
}

fn k(off: u8, val: u16) -> Vec<u8> {
    vec![0x42, (off << 4) | ((val >> 8) & 0x0F) as u8, (val & 0xFF) as u8]
}

fn delay(samples: u16) -> Vec<u8> {
    let mut out = vec![0x61];
    out.extend_from_slice(&samples.to_le_bytes());
    out
}

fn raw_vgm_k(body: &[u8], k005289_clock: u32, ay_clock: u32, ay_dual: bool) -> Vec<u8> {
    let mut buf = vec![0u8; 0x100];
    buf[0..4].copy_from_slice(b"Vgm ");
    buf[8..12].copy_from_slice(&0x172u32.to_le_bytes());
    buf[0x18..0x1C].copy_from_slice(&44100u32.to_le_bytes());
    buf[0x34..0x38].copy_from_slice(&(0x100u32 - 0x34).to_le_bytes());
    if k005289_clock != 0 {
        buf[0xEC..0xF0].copy_from_slice(&k005289_clock.to_le_bytes());
    }
    if ay_clock != 0 {
        let raw = ay_clock | if ay_dual { 0x4000_0000 } else { 0 };
        buf[0x74..0x78].copy_from_slice(&raw.to_le_bytes());
        buf[0x78] = 0;
    }
    let mut payload = buf;
    payload.extend_from_slice(body);
    payload.push(0x66);
    let len = (payload.len() - 4) as u32;
    payload[4..8].copy_from_slice(&len.to_le_bytes());
    payload
}

fn kv(writes: Vec<ChipWrite>, roms: Vec<RomSlice>, k005289_clock: u32) -> VgmFile {
    VgmFile {
        path: PathBuf::from("synthetic.vgm"),
        buf: Vec::new(),
        version: 0x172,
        total_samples: 400_000,
        loop_samples: 0,
        loop_file_pos: None,
        loop_sample: None,
        ym2151_clock: 3_579_545,
        k007232_clock: 0,
        gd3: Default::default(),
        roms,
        writes,
        k005289_clock,
        ..VgmFile::default()
    }
}

fn ramp_prom() -> Vec<RomSlice> {
    vec![RomSlice {
        chip: 0,
        rom_size: K005289_PROM_SIZE as i64,
        start: 0,
        data: prom(),
        kind: "k005289",
        time: 0,
    }]
}

fn song_writes(start: i64, off: i64, period: u32, vol: u32) -> Vec<ChipWrite> {
    vec![
        ChipWrite::new(0, "k005289", 0, 0, 0),
        ChipWrite::new(0, "k005289", 0, 1, 0),
        ChipWrite::new(start, "k005289", 0, 2, period),
        ChipWrite::new(start, "k005289", 0, 4, 0),
        ChipWrite::new(start, "k005289", 0, 0, vol),
        ChipWrite::new(off, "k005289", 0, 0, 0),
    ]
}

fn song_writes_default() -> Vec<ChipWrite> {
    song_writes(100, 20000, 0xF00, 12)
}

fn write_fur_tones(song: &Song) -> Vec<u8> {
    write_fur(
        song,
        WriteOptions {
            include_pcm: false,
            include_fm: false,
            ..WriteOptions::default()
        },
    )
}

fn analyze_default(vgm: &VgmFile) -> Song {
    analyze(vgm, AnalyzeOptions::default()).expect("analyze")
}

fn analyze_condense(vgm: &VgmFile, condense: f64) -> Song {
    analyze(
        vgm,
        AnalyzeOptions {
            condense,
            ..AnalyzeOptions::default()
        },
    )
    .expect("analyze")
}

mod TestK005289Parsing {
    use super::*;

    #[test]
    fn test_command_clock_and_prom() {
        let prom = prom();
        let mut body = prom_block(&prom);
        body.extend(k(2, 0xF00));
        body.extend(k(4, 0));
        body.extend(k(0, 0x0C));
        body.extend(delay(1000));
        body.extend(k(0, 0));
        let p = tmp().join("k005289_header.vgm");
        std::fs::write(&p, raw_vgm_k(&body, 3_579_545, 0, false)).unwrap();
        let vgm = load_vgm(&p).expect("load");
        assert_eq!(vgm.k005289_clock, 3_579_545);
        let writes: Vec<(i64, u32, u32)> = vgm
            .writes
            .iter()
            .filter(|w| w.chip == "k005289")
            .map(|w| (w.sample, w.reg, w.val))
            .collect();
        assert!(writes.contains(&(0, 2, 0xF00)));
        assert!(writes.contains(&(0, 4, 0x000)));
        assert!(writes.contains(&(0, 0, 0x00C)));
        assert_eq!(vgm.roms.len(), 1);
        assert_eq!(vgm.roms[0].kind, "k005289");
        assert_eq!(vgm.roms[0].rom_size, K005289_PROM_SIZE as i64);
        assert_eq!(
            assemble_rom(&vgm, "k005289", 0, 0, false, true),
            prom
        );
        let head = std::fs::read(&p).unwrap();
        assert!(chips_in_head(&head).contains("k005289"));
        let warning_hits: Vec<&String> = vgm
            .warnings
            .iter()
            .filter(|w| w.contains("PROM") || w.contains("clock"))
            .collect();
        assert!(warning_hits.is_empty());
    }

    #[test]
    fn test_missing_clock_and_prom_warn() {
        let mut body = k(2, 0xF00);
        body.extend(k(4, 0));
        body.extend(k(0, 0x0C));
        body.extend(delay(2000));
        let p = tmp().join("k005289_incomplete.vgm");
        std::fs::write(&p, raw_vgm_k(&body, 0, 0, false)).unwrap();
        let vgm = load_vgm(&p).expect("load");
        assert_eq!(vgm.k005289_clock, 0);
        let text = vgm.warnings.join(" ");
        assert!(text.contains("K005289 clock"));
        assert!(text.contains("PROM"));
    }

    #[test]
    fn test_chips_in_head_needs_uncompressed_bytes() {
        let p = tmp().join("k005289_kinds.vgm");
        std::fs::write(&p, raw_vgm_k(b"\x66", 3_579_545, 0, false)).unwrap();
        let head = std::fs::read(&p).unwrap();
        assert!(chips_in_head(&head).contains("k005289"));
    }
}

mod TestK005289Collect {
    use super::*;

    #[test]
    fn test_note_and_gate() {
        let vgm = kv(song_writes_default(), ramp_prom(), 3_579_545);
        let (notes, offs, waves, warn) = collect_k005289(&vgm);
        assert_eq!(notes.len(), 1);
        assert!(warn.is_empty());
        let hit = &notes[0].hit;
        assert_eq!(hit.voice, 0);
        assert_eq!(hit.period, 0xFFF - 0xF00);
        assert_eq!(hit.vol, 12);
        assert_eq!(hit.wave, ramp());
        assert_eq!(notes[0].end_sample, 20000);
        assert_eq!(offs[0].sample_time, 20000);
        assert_eq!(waves, vec![ramp()]);
        assert_almost_eq!(
            k005289_rate(3_579_545, 0xFF),
            3_579_545.0 / (32.0 * 0x100 as f64)
        );
        let (note, e5) = k005289_note(3_579_545, 0xFF);
        assert!(note >= 0);
        assert!(note <= 179);
        let table = PeriodPitchTable::new(440.0, 3_579_545.0, 32.0, 0xFFF);
        assert_eq!(table.value(note, e5 - 0x80) - 1, 0xFF);
    }

    #[test]
    fn test_tg_pitch_change_is_recorded() {
        let mut writes = song_writes(100, 60000, 0xF00, 12);
        writes.insert(5, ChipWrite::new(30000, "k005289", 0, 2, 0xE00));
        writes.insert(6, ChipWrite::new(30000, "k005289", 0, 4, 0));
        let (notes, _offs, _waves, _warn) =
            collect_k005289(&kv(writes, ramp_prom(), 3_579_545));
        assert_eq!(notes.len(), 1);
        assert!(notes[0]
            .hit
            .freq_pts
            .contains(&(30000, 0xFFF - 0xE00)));
    }

    #[test]
    fn test_volume_steps() {
        let mut writes = song_writes(100, 20000, 0xF00, 12);
        writes.insert(5, ChipWrite::new(5000, "k005289", 0, 0, 9));
        let (notes, _offs, _waves, _warn) =
            collect_k005289(&kv(writes, ramp_prom(), 3_579_545));
        assert!(notes[0].hit.vol_pts.contains(&(5000, 9)));
    }

    #[test]
    fn test_wave_bank_change() {
        let mut writes = song_writes(100, 20000, 0xF00, 12);
        writes.insert(5, ChipWrite::new(10000, "k005289", 0, 0, (1 << 5) | 12));
        let (notes, _offs, _waves, _warn) =
            collect_k005289(&kv(writes, ramp_prom(), 3_579_545));
        assert_eq!(notes[0].hit.wave_pts.last().unwrap().1, ramp_down());
    }

    #[test]
    fn test_short_gate_is_a_glitch() {
        let (notes, offs, _waves, warn) =
            collect_k005289(&kv(song_writes(100, 150, 0xF00, 12), ramp_prom(), 3_579_545));
        assert!(notes.is_empty());
        assert!(offs.is_empty());
        assert!(warn.iter().any(|w| w.contains("shorter")));
    }

    #[test]
    fn test_missing_prom_gives_no_notes() {
        let vgm = kv(song_writes_default(), Vec::new(), 3_579_545);
        let (notes, offs, waves, _warn) = collect_k005289(&vgm);
        assert!(notes.is_empty());
        assert!(offs.is_empty());
        assert!(waves.is_empty());
    }
}

fn song_pair() -> (VgmFile, Song) {
    let mut writes = song_writes(100, 60000, 0xF00, 12);
    writes.insert(5, ChipWrite::new(30000, "k005289", 0, 2, 0xE00));
    writes.insert(6, ChipWrite::new(30000, "k005289", 0, 4, 0));
    let mut vgm = kv(writes, ramp_prom(), 3_579_545);
    vgm.total_samples = 80000;
    let song = analyze_default(&vgm);
    (vgm, song)
}

mod TestK005289Analysis {
    use super::*;

    #[test]
    fn test_note_split_and_emit() {
        let (_vgm_obj, song) = song_pair();
        assert_eq!(song.k005289_ids, vec![0]);
        assert_eq!(song.k005289_count, 2);
        let notes: Vec<_> = song
            .events
            .iter()
            .filter(|e| e.on && e.tone_chip == 16)
            .collect();
        assert_eq!(notes.len(), 2);
        assert_eq!(notes[0].ch, 8); // no FM/sample block in this log
        let data = write_fur_tones(&song);
        let mod_ = parse_fur(&data).expect("parse");
        assert_eq!(mod_.chips, vec![CHIP_K005289]);
        assert_eq!(mod_.n_ch, 2);
        assert_eq!(
            CHIP_CHANNELS
                .iter()
                .find(|(c, _)| *c == CHIP_K005289)
                .unwrap()
                .1,
            2
        );
        let ins_types: Vec<i32> = iter_blocks(&data)
            .unwrap()
            .into_iter()
            .filter(|b| &b.magic == b"INS2")
            .map(|b| u16::from_le_bytes([b.payload[2], b.payload[3]]) as i32)
            .collect();
        assert!(ins_types.contains(&INS_SCC));
        let waves: Vec<Vec<u8>> = iter_blocks(&data)
            .unwrap()
            .into_iter()
            .filter(|b| &b.magic == b"WAVE")
            .map(|b| b.payload.to_vec())
            .collect();
        assert_eq!(waves.len(), song.k005289_waves.len());
        assert_eq!(wave_values(&waves[0]), ramp().iter().map(|&b| b as u32).collect::<Vec<_>>());
        assert_eq!(wave_max(&waves[0]), 15);
    }

    #[test]
    fn test_chip_name_for_edit() {
        let (_vgm_obj, song) = song_pair();
        let fur = tmp().join("k005289_edit.fur");
        std::fs::write(&fur, write_fur_tones(&song)).unwrap();
        let chips = read_chips_file(&fur).expect("chips");
        assert_eq!(
            chips.iter().map(|c| c.name()).collect::<Vec<_>>(),
            vec!["Bubble System WSG"]
        );
    }

    #[test]
    fn test_volume_decay_macro() {
        let mut writes = song_writes(100, 40000, 0xF00, 12);
        writes.insert(5, ChipWrite::new(5000, "k005289", 0, 0, 9));
        writes.insert(6, ChipWrite::new(10000, "k005289", 0, 0, 6));
        let mut vgm = kv(writes, ramp_prom(), 3_579_545);
        vgm.total_samples = 60000;
        let song = analyze_default(&vgm);
        assert!(!song.k005289_ins.is_empty());
        let spec = &song.k005289_ins[0];
        assert!(spec.vol.contains(&9));
        assert!(spec.vol.contains(&6));
    }

    #[test]
    fn test_wave_morph_macro() {
        let mut writes = song_writes(100, 40000, 0xF00, 12);
        writes.insert(5, ChipWrite::new(10000, "k005289", 0, 0, (1 << 5) | 12));
        let mut vgm = kv(writes, ramp_prom(), 3_579_545);
        vgm.total_samples = 60000;
        let song = analyze_default(&vgm);
        let specs: Vec<_> = song.k005289_ins.iter().filter(|s| !s.waves.is_empty()).collect();
        assert!(!specs.is_empty());
        assert!(song.k005289_waves.len() >= 2);
        let data = write_fur_tones(&song);
        let ins: Vec<&[u8]> = iter_blocks(&data)
            .unwrap()
            .into_iter()
            .filter(|b| &b.magic == b"INS2")
            .map(|b| b.payload)
            .collect();
        assert!(ins.iter().any(|block| block.contains(&3)));
    }

    #[test]
    fn test_split_note_starts_on_its_attack() {
        // The driver latches/triggers the next pitch, then writes its volume
        // a few samples later.  The split note must start on that attack, not
        // on the previous note's tail (4), or it begins quiet and pops a tick
        // later.
        let writes = vec![
            ChipWrite::new(0, "k005289", 0, 0, 0),
            ChipWrite::new(0, "k005289", 0, 1, 0),
            ChipWrite::new(100, "k005289", 0, 2, 0xF00),
            ChipWrite::new(100, "k005289", 0, 4, 0),
            ChipWrite::new(106, "k005289", 0, 0, 12),
            ChipWrite::new(5000, "k005289", 0, 0, 4), // decay tail
            ChipWrite::new(10000, "k005289", 0, 2, 0xE00),
            ChipWrite::new(10000, "k005289", 0, 4, 0),
            ChipWrite::new(10006, "k005289", 0, 0, 12), // next note's attack
            ChipWrite::new(30000, "k005289", 0, 0, 0),
        ];
        let mut vgm = kv(writes, ramp_prom(), 3_579_545);
        vgm.total_samples = 40000;
        let song = analyze_default(&vgm);
        let notes: Vec<_> = song
            .events
            .iter()
            .filter(|e| e.on && e.tone_chip == 16)
            .collect();
        assert_eq!(notes.len(), 2);
        assert_eq!(notes[1].sample, 10000);
        assert_eq!(notes[1].vol, 12);
        assert!(!notes[1].vol_pts.iter().any(|(t, _, _)| *t == 10006));
    }

    #[test]
    fn test_condensed_module_keeps_wsg() {
        let mut vgm = kv(song_writes(100, 60000, 0xF00, 12), ramp_prom(), 3_579_545);
        vgm.total_samples = 80000;
        let song = analyze_condense(&vgm, 2.0);
        assert!(song.k005289_count > 0);
        let data = write_fur_tones(&song);
        let mod_ = parse_fur(&data).expect("parse");
        assert_eq!(mod_.chips, vec![CHIP_K005289]);
        assert_eq!(mod_.n_ch, 2);
    }
}

/// WAVE payload: name\0, length u32, then three more words before the values.
fn wave_values(block: &[u8]) -> Vec<u32> {
    let name_end = block.iter().position(|&b| b == 0).expect("name end");
    let mut pos = name_end + 1;
    let length = u32::from_le_bytes(block[pos..pos + 4].try_into().unwrap()) as usize;
    pos += 4 + 4 + 4;
    (0..length)
        .map(|i| {
            let at = pos + 4 * i;
            u32::from_le_bytes(block[at..at + 4].try_into().unwrap())
        })
        .collect()
}

fn wave_max(block: &[u8]) -> u32 {
    let name_end = block.iter().position(|&b| b == 0).expect("name end");
    let pos = name_end + 1 + 4;
    u32::from_le_bytes(block[pos + 4..pos + 8].try_into().unwrap())
}

mod TestK005289Compare {
    use super::*;

    #[test]
    fn test_pitch_and_keyed_stream() {
        let vgm = kv(song_writes_default(), ramp_prom(), 3_579_545);
        assert_eq!(
            k005289_pitch_stream(&vgm, 0, 0)[0],
            (100, py_round(-768.0 * (0x100u32 as f64).log2()) as i32)
        );
        let intervals = k005289_keyed_intervals(&vgm, 0, 0);
        assert_eq!(intervals[0].0, 100);
        assert_eq!(intervals[0].1, 20000);
    }

    #[test]
    fn test_module_stream_matches_source() {
        let mut writes = song_writes(100, 60000, 0xF00, 12);
        writes.insert(5, ChipWrite::new(30000, "k005289", 0, 2, 0xE00));
        writes.insert(6, ChipWrite::new(30000, "k005289", 0, 4, 0));
        let mut vgm = kv(writes, ramp_prom(), 3_579_545);
        vgm.total_samples = 80000;
        let song = analyze_default(&vgm);
        let fur = tmp().join("k005289_compare.fur");
        std::fs::write(&fur, write_fur_tones(&song)).unwrap();
        let (mod_stream, mod_intervals) = _fur_wsg_stream(&fur, 0, 3_579_545).unwrap();
        assert!(!mod_stream.is_empty());
        assert!(!mod_intervals.is_empty());
        let a: Vec<(i64, f64)> = k005289_pitch_stream(&vgm, 0, 0)
            .into_iter()
            .map(|(s, v)| (s, v as f64))
            .collect();
        let b: Vec<(i64, f64)> = mod_stream
            .into_iter()
            .map(|(s, v)| (s, v as f64))
            .collect();
        let intervals = k005289_keyed_intervals(&vgm, 0, 0);
        let runs = _runs_between(&a, &b, Some(&intervals), None, 0.5, 100.0);
        assert!(runs.is_empty());
    }

    #[test]
    fn test_tone_layout_maps_ay_then_wsg() {
        let ay = 1_789_772u32;
        let mut writes = vec![
            ChipWrite::new(0, "ay8910", 0, 7, 0xFE),
            ChipWrite::new(0, "ay8910", 0, 0, 254),
            ChipWrite::new(0, "ay8910", 0, 1, 0),
            ChipWrite::new(100, "ay8910", 0, 8, 12),
        ];
        writes.extend(song_writes_default());
        let mut vgm = kv(writes, ramp_prom(), 3_579_545);
        vgm.ay8910_clock = ay;
        vgm.ay8910_dual = true;
        let layout: Vec<(&str, u8, i32)> = vec![
            ("ay8910", 0, 3),
            ("ay8910", 1, 3),
            ("k005289", 0, 2),
        ];
        // AY chips 0 and 1 own channels 0-5; the WSG is 6-7.
        assert_eq!(tone_pitch_stream(&vgm, 0, &layout).len(), 1);
        assert_eq!(tone_pitch_stream(&vgm, 6, &layout)[0].0, 100);
        assert!(tone_pitch_stream(&vgm, 7, &layout).is_empty());
    }
}

fn vgz_paths(dir: &Path) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = std::fs::read_dir(dir)
        .expect("read dir")
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().map(|x| x == "vgz").unwrap_or(false))
        .collect();
    out.sort();
    out
}

mod TestGradiusPack {
    use super::*;

    #[test]
    fn test_every_track_parses_and_collects() {
        let Some(dir) = pack() else {
            eprintln!("skipping: Gradius pack is not present");
            return;
        };
        for path in vgz_paths(&dir) {
            eprintln!("track={}", path.file_name().unwrap().to_string_lossy());
            let vgm = load_vgm(&path).expect("load");
            assert_eq!(vgm.k005289_clock, 3_579_545);
            let (notes, _offs, waves, warn) = collect_k005289(&vgm);
            assert!(warn.is_empty());
            assert!(!waves.is_empty());
            assert!(!notes.is_empty());
        }
    }

    #[test]
    fn test_convert_and_compare_wsg() {
        if vgm2fur::furnace::find_furnace().is_none() {
            eprintln!("skipping: needs the Furnace console build");
            return;
        }
        let dir = pack().expect("Gradius pack is not present");
        let src = dir.join("03 Challenger 1985 (Stage 1).vgz");
        let song = analyze_default(&load_vgm(&src).expect("load"));
        let fur = tmp().join("gradius03.fur");
        std::fs::write(
            &fur,
            write_fur(
                &song,
                WriteOptions {
                    include_pcm: true,
                    include_fm: false,
                    ..WriteOptions::default()
                },
            ),
        )
        .unwrap();
        let chips = read_chips_file(&fur).expect("chips");
        assert_eq!(
            chips.iter().map(|c| c.name()).collect::<Vec<_>>(),
            vec!["AY8910", "AY8910", "Bubble System WSG"]
        );
        // Reference mix: the dual AY pair is halved (0.73 each after the AY
        // core correction) and the WSG's 4x-hot core takes 0.25.
        assert_eq!(
            chips
                .iter()
                .map(|c| py_round_to(c.volume, 2))
                .collect::<Vec<_>>(),
            vec![0.73, 0.73, 0.25]
        );
        for ch in [6, 7] {
            let export_to = tmp().join(format!("gradius03_ch{ch}.vgm"));
            let (runs, _exported) =
                compare_fur_to_source(&src, &fur, ch, 0.5, 100.0, Some(&export_to))
                    .expect("compare");
            assert!(runs.is_empty());
        }
    }
}
