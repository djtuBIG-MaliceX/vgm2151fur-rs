//! Konami K053260: VGM parsing, register model and .fur emission.
//!
//! Port of `tests/test_vgm2151fur_k053260.py` at vgm2151fur a8edb6e.

mod common;
#[allow(unused_imports)]
use common::*;

use std::path::PathBuf;

use vgm2fur::analyze::{analyze, AnalyzeOptions};
use vgm2fur::furio::{iter_blocks, list_fur_samples, master_volume, parse_fur};
use vgm2fur::furwrite::{
    k053260_flag_text, write_fur, WriteOptions, CHIP_K053260, INS_K053260,
    SMP_DEPTH_ADPCM_K,
};
use vgm2fur::k053260::{k053260_pan_fx, k053260_rate};
use vgm2fur::model::Song;
use vgm2fur::vgm::{
    chips_in_head, load_vgm, ChipWrite, RomSlice, VgmFile,
};

const CLOCK: u32 = 3_579_545;

fn tmp_dir() -> PathBuf {
    let p = std::env::temp_dir().join("vgm2151fur_k053260");
    std::fs::create_dir_all(&p).expect("create temp dir");
    p
}

fn _lattice() -> Vec<ChipWrite> {
    lattice(40, 5_000)
}

fn make_rom(chip: u8, start: i64, data: Vec<u8>) -> RomSlice {
    RomSlice {
        chip,
        rom_size: data.len() as i64,
        start,
        data,
        kind: "k053260",
        time: 0,
    }
}

fn _vgm(writes: Vec<ChipWrite>, roms: Vec<RomSlice>) -> VgmFile {
    let mut v = vgm(writes);
    v.roms = roms;
    v
}

fn _raw_vgm(cmds: &[u8], rom_blocks: &[(u8, u32, Vec<u8>)]) -> Vec<u8> {
    let mut buf = vec![0u8; 0xC0];
    buf[0..4].copy_from_slice(b"Vgm ");
    buf[8..12].copy_from_slice(&0x161u32.to_le_bytes());
    buf[0x18..0x1C].copy_from_slice(&44100u32.to_le_bytes());
    buf[0x30..0x34].copy_from_slice(&CLOCK.to_le_bytes());
    buf[0x34..0x38].copy_from_slice(&(0xC0u32 - 0x34).to_le_bytes());
    buf[0xAC..0xB0].copy_from_slice(&CLOCK.to_le_bytes());
    let mut body: Vec<u8> = Vec::new();
    for (typ, start, data) in rom_blocks {
        let mut blk: Vec<u8> = Vec::new();
        blk.extend_from_slice(&0x10000u32.to_le_bytes());
        blk.extend_from_slice(&start.to_le_bytes());
        blk.extend_from_slice(data);
        body.extend_from_slice(&[0x67, 0x66, *typ]);
        body.extend_from_slice(&(blk.len() as u32).to_le_bytes());
        body.extend_from_slice(&blk);
    }
    body.extend_from_slice(cmds);
    body.push(0x66);
    buf.extend_from_slice(&body);
    buf
}

fn _body() -> Vec<u8> {
    // No zero bytes, so the trailing-silence trim does not shorten it.
    (0..200).map(|i| ((i * 3 + 1) & 0xFF) as u8).collect()
}

fn _regs(t: i64, voice: u32, chip: u8, regs: &[(u32, i32)]) -> Vec<ChipWrite> {
    regs.iter()
        .map(|&(i, val)| {
            ChipWrite::new(t, "k053260", chip, 8 + voice * 8 + i, (val & 0xFF) as u32)
        })
        .collect()
}

fn _key(t: i64, value: i32, chip: u8) -> ChipWrite {
    ChipWrite::new(t, "k053260", chip, 0x28, (value & 0xFF) as u32)
}

fn _loop(t: i64, value: i32, chip: u8) -> ChipWrite {
    ChipWrite::new(t, "k053260", chip, 0x2A, (value & 0xFF) as u32)
}

fn _pan(t: i64, value: i32, chip: u8, second: bool) -> Vec<ChipWrite> {
    vec![ChipWrite::new(
        t,
        "k053260",
        chip,
        if second { 0x2D } else { 0x2C },
        (value & 0xFF) as u32,
    )]
}

fn _start_regs(start: i32) -> (i32, i32, i32) {
    (
        start & 0xFF,
        (start >> 8) & 0xFF,
        (start >> 16) & 0x1F,
    )
}

#[derive(Clone, Copy)]
struct KW {
    start: i32,
    length: i32,
    pitch: i32,
    volume: i32,
    voice: u32,
    when: i64,
    loop_: bool,
    adpcm: bool,
    reverse: bool,
    chip: u8,
}

impl Default for KW {
    fn default() -> Self {
        KW {
            start: 0x100,
            length: 200,
            pitch: 0xFC0,
            volume: 100,
            voice: 0,
            when: 60000,
            loop_: false,
            adpcm: false,
            reverse: false,
            chip: 0,
        }
    }
}

fn _key_writes(a: KW) -> Vec<ChipWrite> {
    let (lo, mid, hi) = _start_regs(a.start);
    let mut writes = _regs(
        a.when,
        a.voice,
        a.chip,
        &[
            (0, a.pitch & 0xFF),
            (1, (a.pitch >> 8) & 0x0F),
            (2, a.length & 0xFF),
            (3, (a.length >> 8) & 0xFF),
            (4, lo),
            (5, mid),
            (6, hi),
            (7, a.volume),
        ],
    );
    if a.loop_ || a.adpcm {
        writes.push(_loop(
            a.when,
            if a.loop_ { 1 << a.voice } else { 0 },
            a.chip,
        ));
        if a.adpcm {
            writes.push(_loop(
                a.when,
                (0x10 << a.voice) | (if a.loop_ { 1 << a.voice } else { 0 }),
                a.chip,
            ));
        }
    }
    writes.push(_key(
        a.when,
        (1 << a.voice) | if a.reverse { 0x10 << a.voice } else { 0 },
        a.chip,
    ));
    writes
}

fn _song(writes: Vec<ChipWrite>, rom: Option<Vec<u8>>, clock: u32) -> Song {
    let rom = match rom {
        Some(r) => r,
        None => {
            let body = _body();
            let mut r = vec![0u8; 0x300];
            r[0x101..0x101 + body.len()].copy_from_slice(&body);
            r
        }
    };
    let roms = vec![make_rom(0, 0, rom)];
    let mut v = _vgm(_lattice().into_iter().chain(writes).collect(), roms);
    v.k053260_clock = clock;
    analyze(
        &v,
        AnalyzeOptions {
            pcm: true,
            ..Default::default()
        },
    )
    .expect("analyze")
}

fn song_default(writes: Vec<ChipWrite>) -> Song {
    _song(writes, None, CLOCK)
}

fn pack_f3(values: [f32; 3]) -> Vec<u8> {
    values.iter().flat_map(|v| v.to_le_bytes()).collect()
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

mod TestK053260Parsing {
    use super::*;

    #[test]
    fn test_write_clock_and_rom() {
        let raw = _raw_vgm(b"\xba\x08\x01\xba\x89\x02", &[(0x8E, 0, (0..32).collect())]);
        let p = temp_file(&tmp_dir(), "parse.vgm", &raw);
        let v = load_vgm(&p).expect("load_vgm");
        assert_eq!(v.k053260_clock, CLOCK);
        assert_eq!(
            v.roms.iter().map(|s| s.kind).collect::<Vec<_>>(),
            vec!["k053260"]
        );
        assert_eq!(v.roms[0].start, 0);
        let w: Vec<(u8, u32, u32)> = v
            .writes
            .iter()
            .filter(|x| x.chip == "k053260")
            .map(|x| (x.chip_id, x.reg, x.val))
            .collect();
        assert_eq!(w, vec![(0, 0x08, 0x01), (1, 0x09, 0x02)]);
        assert!(chips_in_head(&raw[..0xC0]).contains("k053260"));
        assert!(!v.warnings.iter().any(|x| x.contains("does not convert")));
    }

    #[test]
    fn test_missing_clock_uses_default() {
        let mut raw = _raw_vgm(b"\xba\x08\x01", &[]);
        raw[0xAC..0xB0].copy_from_slice(&0u32.to_le_bytes());
        let p = temp_file(&tmp_dir(), "noclk.vgm", &raw);
        let v = load_vgm(&p).expect("load_vgm");
        assert_eq!(v.k053260_clock, 0);
        assert!(v.warnings.iter().any(|x| x.contains("K053260 clock")));
    }

    #[test]
    fn test_extra_header_clock() {
        let raw = _raw_vgm(b"", &[]);
        let mut header = raw[..0xC0].to_vec();
        header[0xAC..0xB0].copy_from_slice(&0u32.to_le_bytes());
        header[0xBC..0xC0].copy_from_slice(&4u32.to_le_bytes());
        header[0x34..0x38].copy_from_slice(&(0xD0u32 - 0x34).to_le_bytes());
        let mut extra = vec![0u8; 16];
        extra[0..4].copy_from_slice(&12u32.to_le_bytes());
        extra[4..8].copy_from_slice(&4u32.to_le_bytes());
        extra[8] = 1;
        extra[9] = 0x1D;
        extra[10..14].copy_from_slice(&8_000_000u32.to_le_bytes());
        let mut buf = header;
        buf.extend_from_slice(&extra);
        buf.extend_from_slice(&raw[0xC0..]);
        let p = temp_file(&tmp_dir(), "extra.vgm", &buf);
        let v = load_vgm(&p).expect("load_vgm");
        assert_eq!(v.k053260_clock, 8_000_000);
        assert!(chips_in_head(&buf).contains("k053260"));
    }
}

mod TestK053260RegisterModel {
    use super::*;

    fn f3(data: &[u8], values: [f32; 3]) -> bool {
        contains(data, &pack_f3(values))
    }

    #[test]
    fn test_pcm_body_skips_the_start_byte() {
        let body = _body();
        let mut rom = vec![0u8; 0x300];
        rom[0x100] = 0x7F; // never played
        rom[0x101..0x101 + body.len()].copy_from_slice(&body);
        let song = _song(_key_writes(KW::default()), Some(rom), CLOCK);
        assert_eq!(song.k053260_samples.len(), 1);
        let smp = &song.k053260_samples[0];
        assert_eq!(smp.start, 0x100);
        assert_eq!(smp.depth, 8);
        assert_eq!(smp.data, body);
        assert_eq!(smp.length(), 200);
        assert_eq!(smp.mode_pitch(), 0xFC0);
        assert_eq!(smp.c4_rate(CLOCK), 55930);
        assert!(smp.loop_start.is_none());

        let ev = song.events.iter().find(|e| e.on && e.pcm).expect("note");
        assert_eq!((ev.note, ev.e5), (108, 0x80));
        assert_eq!(ev.vol, 100);
        assert_eq!(ev.pan, 0xFF);
        assert_eq!(ev.ch, 8);

        let data = write_fur(
            &song,
            WriteOptions {
                include_pcm: true,
                include_fm: true,
                ..Default::default()
            },
        );
        let mod_ = parse_fur(&data).expect("parse_fur");
        assert_eq!(mod_.chips, vec![0x82, CHIP_K053260]);
        assert_eq!(mod_.n_ch, 8 + 4);
        let ins_types: Vec<u16> = iter_blocks(&data)
            .expect("iter_blocks")
            .iter()
            .filter(|b| b.magic == *b"INS2")
            .map(|b| u16::from_le_bytes([b.payload[2], b.payload[3]]))
            .collect();
        assert!(ins_types.contains(&(INS_K053260 as u16)));
        let smps = list_fur_samples(&data).expect("list_fur_samples");
        assert_eq!(smps[0].depth, 8);
        assert_eq!(smps[0].n_frames, 200);
        assert_eq!(smps[0].data, body);
        assert_eq!(smps[0].c4_rate, 55930);
        // The Furnace core is half the reference core. The raised master
        // carries the boost; the chip float is its master-relative share.
        assert_eq!(master_volume(&data).expect("master_volume"), 2.0);
        assert!(f3(&data, [0.5, 0.0, 0.0]));
        assert!(f3(&data, [1.0, 0.0, 0.0]));
    }

    #[test]
    fn test_adpcm_body_is_stored_as_depth_7() {
        let body: Vec<u8> = (0..120).map(|i| ((i * 7 + 3) & 0xFF) as u8).collect();
        let mut rom = vec![0u8; 0x200];
        rom[0x101..0x101 + body.len()].copy_from_slice(&body);
        let song = _song(
            _key_writes(KW {
                length: 120,
                adpcm: true,
                ..Default::default()
            }),
            Some(rom),
            CLOCK,
        );
        let smp = &song.k053260_samples[0];
        assert_eq!(smp.depth, 7);
        assert_eq!(smp.data, body);
        assert_eq!(smp.length(), 240);

        let data = write_fur(
            &song,
            WriteOptions {
                include_pcm: true,
                include_fm: true,
                ..Default::default()
            },
        );
        let fur = &list_fur_samples(&data).expect("list_fur_samples")[0];
        assert_eq!(fur.depth as i32, SMP_DEPTH_ADPCM_K);
        assert_eq!(fur.n_frames, 240); // nibbles
        assert_eq!(fur.data, body); // raw nibble bytes
        assert_eq!(fur.loop_start, -1);
    }

    #[test]
    fn test_adpcm_trailing_zero_bytes_are_kept() {
        let body = vec![0x01, 0x02, 0x00, 0x00];
        let mut rom = vec![0u8; 0x200];
        rom[0x101..0x101 + body.len()].copy_from_slice(&body);
        let song = _song(
            _key_writes(KW {
                length: body.len() as i32,
                adpcm: true,
                ..Default::default()
            }),
            Some(rom),
            CLOCK,
        );
        assert_eq!(song.k053260_samples[0].data, body);
    }

    #[test]
    fn test_loop_marks_the_whole_body() {
        let body: Vec<u8> = (0..64).map(|i| ((i * 5 + 1) & 0xFF) as u8).collect();
        let mut rom = vec![0u8; 0x200];
        rom[0x101..0x101 + body.len()].copy_from_slice(&body);
        let song = _song(
            _key_writes(KW {
                length: 64,
                loop_: true,
                ..Default::default()
            }),
            Some(rom),
            CLOCK,
        );
        let smp = &song.k053260_samples[0];
        assert_eq!((smp.loop_start, smp.loop_end), (Some(0), Some(64)));
        let data = write_fur(
            &song,
            WriteOptions {
                include_pcm: true,
                include_fm: true,
                ..Default::default()
            },
        );
        let fur = &list_fur_samples(&data).expect("list_fur_samples")[0];
        assert_eq!((fur.loop_start, fur.loop_end), (0, 64));
        assert_eq!(fur.loop_mode, 0);
    }

    #[test]
    fn test_adpcm_loop_uses_nibble_loop_points() {
        let body: Vec<u8> = (0..64).map(|i| ((i * 9 + 5) & 0xFF) as u8).collect();
        let mut rom = vec![0u8; 0x200];
        rom[0x101..0x101 + body.len()].copy_from_slice(&body);
        let song = _song(
            _key_writes(KW {
                length: 64,
                adpcm: true,
                loop_: true,
                ..Default::default()
            }),
            Some(rom),
            CLOCK,
        );
        let smp = &song.k053260_samples[0];
        assert_eq!((smp.loop_start, smp.loop_end), (Some(0), Some(128)));
        let data = write_fur(
            &song,
            WriteOptions {
                include_pcm: true,
                include_fm: true,
                ..Default::default()
            },
        );
        let fur = &list_fur_samples(&data).expect("list_fur_samples")[0];
        assert_eq!(fur.depth as i32, 7);
        assert_eq!(fur.n_frames, 128);
        assert_eq!((fur.loop_start, fur.loop_end), (0, 128));
    }

    #[test]
    fn test_reverse_pcm_plays_the_previous_bytes() {
        let body: Vec<u8> = (0..50).map(|i| ((i * 3 + 1) & 0xFF) as u8).collect();
        let mut rom = vec![0u8; 0x300];
        rom[0x100 - body.len()..0x100].copy_from_slice(&body);
        let length = body.len() as i32;
        let song = _song(
            _key_writes(KW {
                start: 0x100,
                length,
                reverse: true,
                ..Default::default()
            }),
            Some(rom),
            CLOCK,
        );
        let smp = &song.k053260_samples[0];
        assert!(smp.reverse);
        assert_eq!(smp.data, body.iter().rev().copied().collect::<Vec<u8>>());
        assert_eq!(smp.c4_rate(CLOCK), 55930);
    }

    #[test]
    fn test_reverse_adpcm_swaps_nibbles() {
        let body = vec![0x12, 0x34, 0x56, 0x78];
        let mut rom = vec![0u8; 0x100];
        rom[0x100 - body.len()..0x100].copy_from_slice(&body);
        let song = _song(
            _key_writes(KW {
                start: 0x100,
                length: body.len() as i32,
                adpcm: true,
                reverse: true,
                ..Default::default()
            }),
            Some(rom),
            CLOCK,
        );
        let smp = &song.k053260_samples[0];
        assert_eq!(smp.depth, 7);
        assert_eq!(smp.data, vec![0x87, 0x65, 0x43, 0x21]);
    }

    #[test]
    fn test_key_off_emits_note_off() {
        let mut writes = _key_writes(KW {
            when: 60000,
            ..Default::default()
        });
        writes.push(_key(90000, 0, 0));
        let song = song_default(writes);
        let offs: Vec<_> = song
            .events
            .iter()
            .filter(|e| !e.on && e.ch == 8)
            .collect();
        assert_eq!(offs.len(), 1);
        assert_eq!(offs[0].sample, 90000);
        let data = write_fur(
            &song,
            WriteOptions {
                include_pcm: true,
                include_fm: true,
                ..Default::default()
            },
        );
        assert_eq!(
            parse_fur(&data).expect("parse_fur").chips,
            vec![0x82, CHIP_K053260]
        );
    }

    #[test]
    fn test_volume_and_pan_changes_become_points() {
        let mut writes = _key_writes(KW {
            volume: 100,
            ..Default::default()
        });
        writes.extend(_regs(70000, 0, 0, &[(7, 40)]));
        writes.extend(_pan(71000, 1, 0, false)); // voice 0 hard left
        writes.extend(_pan(72000, 2, 0, false)); // voice 0 left-of-centre
        let song = song_default(writes);
        let ev = song.events.iter().find(|e| e.on && e.pcm).expect("note");
        assert_eq!(ev.vol, 100);
        assert_eq!(ev.pan, 0xFF);
        let pts: Vec<(i32, Option<i32>)> =
            ev.vol_pts.iter().map(|&(_t, v, p)| (v, p)).collect();
        assert_eq!(pts, vec![(40, None), (40, Some(0xF0)), (40, Some(0xF7))]);
    }

    #[test]
    fn test_pitch_change_becomes_a_hard_jump() {
        // Halving the sample rate mid-note is one step of an octave that then
        // holds, so it lands as an instant transpose. A 02xx slur down there
        // whistles through every pitch in between, which the driver never wrote.
        let mut writes = _key_writes(KW {
            pitch: 0xFC0,
            ..Default::default()
        });
        writes.extend(_regs(80000, 0, 0, &[(0, 0x80), (1, 0x0F)])); // 0xF80, half rate
        let song = song_default(writes);
        let ev = song.events.iter().find(|e| e.on && e.pcm).expect("note");
        assert_eq!(ev.sweep, vec![(80000, -768)]);
        let data = write_fur(
            &song,
            WriteOptions {
                include_pcm: true,
                include_fm: true,
                ..Default::default()
            },
        );
        let mod_ = parse_fur(&data).expect("parse_fur");
        let mut rows: Vec<&vgm2fur::furio::FurRow> = Vec::new();
        for ((ch, _idx), pat) in mod_.patterns.iter() {
            if *ch == 8 {
                rows.extend(pat.iter());
            }
        }
        let down: Vec<u8> = rows
            .iter()
            .flat_map(|r| r.fx.iter())
            .filter(|(c, _v)| *c == 0xE9)
            .map(|(_c, v)| *v)
            .collect();
        assert_eq!(down, vec![12]);
        let rates: Vec<u8> = rows
            .iter()
            .flat_map(|r| r.fx.iter())
            .filter(|(c, v)| (*c == 0x01 || *c == 0x02) && *v != 0)
            .map(|(_c, v)| *v)
            .collect();
        assert_eq!(rates, Vec::<u8>::new());
        let note_row = rows
            .iter()
            .find(|r| (0..180).contains(&r.note))
            .expect("note row");
        assert!(!note_row
            .fx
            .iter()
            .any(|(c, _v)| [0x01u8, 0x02, 0xE8, 0xE9].contains(c)));
    }

    #[test]
    fn test_second_instance_uses_its_own_channels() {
        let body = _body();
        let mut arr = vec![0u8; 0x300];
        arr[0x101..0x101 + body.len()].copy_from_slice(&body);
        let roms = vec![
            make_rom(0, 0, arr.clone()),
            make_rom(1, 0, arr.clone()),
        ];
        let mut writes = _key_writes(KW {
            voice: 0,
            when: 60000,
            ..Default::default()
        });
        writes.push(_key(90000, 0, 0));
        writes.extend(_key_writes(KW {
            voice: 0,
            when: 100000,
            chip: 1,
            ..Default::default()
        }));
        let mut v = _vgm(_lattice().into_iter().chain(writes).collect(), roms);
        v.k053260_clock = CLOCK;
        let song = analyze(
            &v,
            AnalyzeOptions {
                pcm: true,
                ..Default::default()
            },
        )
        .expect("analyze");
        let ons: Vec<i32> = song
            .events
            .iter()
            .filter(|e| e.on && e.pcm)
            .map(|e| e.ch)
            .collect();
        assert_eq!(ons, vec![8, 12]);
        let data = write_fur(
            &song,
            WriteOptions {
                include_pcm: true,
                include_fm: true,
                ..Default::default()
            },
        );
        let mod_ = parse_fur(&data).expect("parse_fur");
        assert_eq!(
            mod_.chips,
            vec![0x82, CHIP_K053260, CHIP_K053260]
        );
        assert_eq!(mod_.n_ch, 8 + 8);
    }

    #[test]
    fn test_attack_refit_uses_the_first_frame_write() {
        let mut writes = _key_writes(KW {
            pitch: 0,
            volume: 0,
            ..Default::default()
        });
        writes.extend(_regs(
            60200,
            0,
            0,
            &[(0, 0xC0), (1, 0x0F), (7, 90)],
        ));
        let song = song_default(writes);
        let smp = &song.k053260_samples[0];
        let ev = song.events.iter().find(|e| e.on && e.pcm).expect("note");
        assert_eq!(smp.mode_pitch(), 0xFC0);
        assert_eq!((ev.note, ev.e5), (108, 0x80));
        assert_eq!(ev.vol, 90);
        assert_eq!(ev.sweep, Vec::<(i64, i32)>::new());
    }

    #[test]
    fn test_pan_zero_key_on_is_silent_until_it_moves() {
        let mut writes = _key_writes(KW {
            volume: 100,
            ..Default::default()
        });
        writes.extend(_pan(59000, 0, 0, false));
        writes.extend(_pan(80000, 4, 0, false));
        let song = song_default(writes);
        let ev = song.events.iter().find(|e| e.on && e.pcm).expect("note");
        assert_eq!(ev.vol, 0);
        assert!(ev.no_pan);
        let pts: Vec<(i32, Option<i32>)> =
            ev.vol_pts.iter().map(|&(_t, v, p)| (v, p)).collect();
        assert_eq!(pts, vec![(100, Some(0xFF))]);
    }

    #[test]
    fn test_same_body_shared_and_different_loop_split() {
        let mut writes = _key_writes(KW {
            when: 60000,
            ..Default::default()
        });
        writes.push(_key(70000, 0, 0));
        writes.extend(_key_writes(KW {
            when: 80000,
            loop_: true,
            ..Default::default()
        }));
        let song = song_default(writes);
        assert_eq!(song.k053260_samples.len(), 2);
        assert_eq!(
            song.k053260_samples
                .iter()
                .map(|s| s.loop_start)
                .collect::<Vec<_>>(),
            vec![None, Some(0)]
        );
        assert!(song.k053260_samples.iter().all(|s| s.hits.len() == 1));
    }

    #[test]
    fn test_undumped_rom_warns() {
        let song = song_default(_key_writes(KW {
            start: 0x400,
            ..Default::default()
        }));
        assert!(song.k053260_samples.is_empty());
        assert!(song.warnings.iter().any(|w| w.contains("not dump")));
    }

    #[test]
    fn test_mode_change_at_a_key_off_is_not_a_warning() {
        let mut writes = _key_writes(KW {
            when: 60000,
            ..Default::default()
        });
        writes.push(_loop(70000, 0x10, 0));
        writes.push(_key(70000, 0, 0));
        let song = song_default(writes);
        assert!(!song
            .warnings
            .iter()
            .any(|w| w.contains("KADPCM mode changes")));
    }

    #[test]
    fn test_no_rom_block_warns() {
        let mut v = _vgm(
            _lattice()
                .into_iter()
                .chain(_key_writes(KW::default()))
                .collect(),
            Vec::new(),
        );
        v.k053260_clock = CLOCK;
        let song = analyze(
            &v,
            AnalyzeOptions {
                pcm: true,
                ..Default::default()
            },
        )
        .expect("analyze");
        assert!(song.k053260_samples.is_empty());
        assert!(song
            .warnings
            .iter()
            .any(|w| w.contains("no K053260 ROM block")));
    }

    #[test]
    fn test_clock_rate_and_pan_helpers() {
        assert_almost_eq!(k053260_rate(CLOCK, 0xFC0), 55930.39, 1);
        assert_almost_eq!(k053260_rate(CLOCK, 0xF80), 27965.2, 1);
        assert_eq!(
            (0..8).map(k053260_pan_fx).collect::<Vec<_>>(),
            vec![0x00, 0xF0, 0xF7, 0xFB, 0xFF, 0xBF, 0x7F, 0x0F]
        );
        assert_eq!(k053260_flag_text(CLOCK), "clockSel=0");
        assert_eq!(k053260_flag_text(4_000_000), "clockSel=1");
        assert_eq!(k053260_flag_text(8_000_000), "customClock=8000000");
    }
}
