//! Yamaha Y8950: OPL2 FM plus one delta-T (ADPCM-B) voice.
//!
//! Port of `tests/test_vgm2151fur_y8950.py`.

mod common;

use std::path::{Path, PathBuf};

use vgm2fur::analyze::analyze;
use vgm2fur::fm::OpRegs;
use vgm2fur::furio::{chip_channels, iter_blocks, parse_fur};
use vgm2fur::furwrite::{
    write_fur, WriteOptions, CHIP_Y8950, CHIP_Y8950_DRUMS, INS_ADPCMB,
};
use vgm2fur::vgm::{chips_in_head, load_vgm, ChipWrite, RomSlice, VgmFile};
use vgm2fur::y8950::collect_y8950_delta_t;

const CLOCK: u32 = 3_579_545;

fn tmp_dir() -> PathBuf {
    let dir = std::env::temp_dir().join("vgm2fur_y8950");
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir
}

fn vgm(writes: Vec<ChipWrite>, roms: Vec<RomSlice>) -> VgmFile {
    let mut v = VgmFile::default();
    v.path = PathBuf::from("synthetic.vgm");
    v.version = 0x161;
    v.total_samples = 400_000;
    v.ym2151_clock = CLOCK;
    v.writes = writes;
    v.roms = roms;
    v
}

fn lattice() -> Vec<ChipWrite> {
    (0..60)
        .map(|k| ChipWrite::new(k * 5000, "ym2151", 0, 0x08, 0x78))
        .collect()
}

/// `_raw_vgm`: a 0x100-byte header with the Y8950 clock at 0x58 and the given
/// data blocks before the command stream.
fn raw_vgm(cmds: &[u8], blocks: &[(u8, u32, u32, &[u8])], clock: u32) -> Vec<u8> {
    let mut buf = vec![0u8; 0x100];
    buf[0..4].copy_from_slice(b"Vgm ");
    buf[8..12].copy_from_slice(&0x161u32.to_le_bytes());
    buf[0x18..0x1C].copy_from_slice(&44100u32.to_le_bytes());
    buf[0x30..0x34].copy_from_slice(&CLOCK.to_le_bytes());
    let hdr_len = buf.len() as u32;
    buf[0x34..0x38].copy_from_slice(&(hdr_len - 0x34).to_le_bytes());
    buf[0x58..0x5C].copy_from_slice(&clock.to_le_bytes());
    let mut body: Vec<u8> = Vec::new();
    for (typ, size, start, data) in blocks {
        let mut payload: Vec<u8> = Vec::new();
        payload.extend_from_slice(&size.to_le_bytes());
        payload.extend_from_slice(&start.to_le_bytes());
        payload.extend_from_slice(data);
        body.extend_from_slice(&[0x67, 0x66, *typ]);
        body.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        body.extend_from_slice(&payload);
    }
    body.extend_from_slice(cmds);
    body.push(0x66);
    let mut out = buf.clone();
    out.extend_from_slice(&body);
    let len = (out.len() - 4) as u32;
    out[4..8].copy_from_slice(&len.to_le_bytes());
    out
}

fn reg(t: i64, reg: u16, val: u16) -> ChipWrite {
    ChipWrite::new(t, "y8950", 0, reg as u32, (val & 0xFF) as u32)
}

/// One delta-T burst: start, end, pitch, level, then the control write.
fn adpcm_note(t: i64, start: u16, end: u16, pitch: u16, level: u16, loop_: bool) -> Vec<ChipWrite> {
    vec![
        reg(t, 0x09, start & 0xFF),
        reg(t, 0x0A, (start >> 8) & 0xFF),
        reg(t, 0x0B, end & 0xFF),
        reg(t, 0x0C, (end >> 8) & 0xFF),
        reg(t, 0x10, pitch & 0xFF),
        reg(t, 0x11, (pitch >> 8) & 0xFF),
        reg(t, 0x12, level),
        reg(t, 0x07, if loop_ { 0xB0 } else { 0xA0 }),
    ]
}

fn note(t: i64) -> Vec<ChipWrite> {
    adpcm_note(t, 0x100, 0x1FF, 0x1000, 0x80, false)
}

fn rom(start: i64, data: Vec<u8>) -> RomSlice {
    RomSlice { chip: 0, rom_size: 0, start, data, kind: "y8950_deltat", time: 0 }
}

fn body(n: usize) -> Vec<u8> {
    (0..n).map(|i| 0x80 | ((i * 7) % 126) as u8).collect()
}

fn collect(writes: Vec<ChipWrite>, roms: Vec<RomSlice>) -> (Vec<vgm2fur::ym2610::AdpcmBSample>, Vec<String>) {
    collect_y8950_delta_t(&vgm(writes, roms), None)
}

mod TestY8950Parsing {
    use super::*;

    #[test]
    fn test_register_writes_clock_and_rom() {
        let cmds = b"\x5c\x07\xa0\x5c\x09\x34\x5c\x12\x80";
        let data: Vec<u8> = (0..32u8).collect();
        let raw = raw_vgm(cmds, &[(0x88, 0x1000, 0x200, &data)], CLOCK);
        let path = tmp_dir().join("parse.vgm");
        std::fs::write(&path, &raw).expect("write");
        let v = load_vgm(&path).expect("load");
        assert_eq!(v.y8950_clock, CLOCK);
        let regs: Vec<(u32, u32)> = v
            .writes
            .iter()
            .filter(|w| w.chip == "y8950")
            .map(|w| (w.reg, w.val))
            .collect();
        assert_eq!(regs, vec![(0x07, 0xA0), (0x09, 0x34), (0x12, 0x80)]);
        let roms: Vec<(&str, i64)> = v.roms.iter().map(|s| (s.kind, s.start)).collect();
        assert_eq!(roms, vec![("y8950_deltat", 0x200)]);
        assert!(chips_in_head(&raw[..0x100]).contains("y8950"));
    }

    #[test]
    fn test_no_unconverted_warning() {
        let raw = raw_vgm(b"\x5c\x07\xa0", &[], CLOCK);
        let path = tmp_dir().join("warn.vgm");
        std::fs::write(&path, &raw).expect("write");
        let v = load_vgm(&path).expect("load");
        assert!(!v.warnings.iter().any(|w| w.contains("does not convert")));
    }
}

mod TestY8950DeltaT {
    use super::*;

    #[test]
    fn test_one_control_write_is_one_hit() {
        let mut writes = lattice();
        writes.extend(note(1000));
        let (samples, _warn) = collect(writes, vec![rom(0x2000, body(512))]);
        assert_eq!(samples.len(), 1);
        let smp = &samples[0];
        assert_eq!((smp.start, smp.end), ((0x100 << 5), ((0x1FF + 1) << 5) - 1));
        assert_eq!(smp.rate, 0x1000);
        assert_eq!(smp.hits.len(), 1);
        assert_eq!(smp.hits[0].vol, 0x80);
    }

    #[test]
    fn test_reset_stops_the_voice() {
        let mut writes = lattice();
        writes.extend(note(1000));
        writes.push(reg(5000, 0x07, 0x01));
        let (samples, _warn) = collect(writes, vec![rom(0x2000, body(512))]);
        assert_eq!(samples[0].hits[0].end_time, 5000);
    }

    #[test]
    fn test_a_second_start_closes_the_first() {
        let mut writes = lattice();
        writes.extend(note(1000));
        writes.extend(adpcm_note(2000, 0x100, 0x1FF, 0x2000, 0x80, false));
        let (samples, _warn) = collect(writes, vec![rom(0x2000, body(512))]);
        assert_eq!(samples.len(), 2);
        assert_eq!(samples[0].hits[0].end_time, 2000);
    }

    #[test]
    fn test_loop_bit_is_kept() {
        let mut writes = lattice();
        writes.extend(adpcm_note(1000, 0x100, 0x1FF, 0x1000, 0x80, true));
        let (samples, _warn) = collect(writes, vec![rom(0x2000, body(512))]);
        assert!(samples[0].hits[0].r#loop);
    }

    #[test]
    fn test_a_missing_rom_warns() {
        let mut writes = lattice();
        writes.extend(note(1000));
        let (_samples, warn) = collect_y8950_delta_t(&vgm(writes, Vec::new()), Some(&[]));
        assert!(warn.iter().any(|w| w.contains("delta-T")), "{warn:?}");
    }

    #[test]
    fn test_rate_and_body() {
        let mut writes = lattice();
        writes.extend(adpcm_note(1000, 0x100, 0x1FF, 0x8000, 0x80, false));
        let (samples, _warn) = collect(writes, vec![rom(0x2000, body(512))]);
        assert_eq!(samples[0].rate, 0x8000);
        assert!(!samples[0].data.is_empty());
    }
}

mod TestY8950Writer {
    use super::*;

    fn song(drums: bool) -> vgm2fur::model::Song {
        let mut writes = lattice();
        writes.push(ChipWrite::new(2000, "y8950", 0, 0xB0, 0x20));
        writes.push(ChipWrite::new(3000, "y8950", 0, 0xB0, 0x00));
        if drums {
            writes.push(ChipWrite::new(4000, "y8950", 0, 0xBD, 0x20));
            writes.push(ChipWrite::new(4100, "y8950", 0, 0xBD, 0x30));
        }
        writes.extend(note(1000));
        analyze(&vgm(writes, vec![rom(0x2000, body(512))]), Default::default())
            .expect("analyze")
    }

    fn fur(song: &vgm2fur::model::Song, opts: WriteOptions<'_>) -> Vec<u8> {
        write_fur(song, opts)
    }

    #[test]
    fn test_chip_and_channel_count() {
        let s = song(false);
        let m = parse_fur(&fur(&s, WriteOptions::default())).expect("parse");
        assert_eq!(*m.chips.last().unwrap(), CHIP_Y8950);
        assert_eq!(m.n_ch, 8 + 10);
        let total: usize = m.chips.iter().map(|c| chip_channels(*c)).sum();
        assert_eq!(total, m.n_ch);
    }

    #[test]
    fn test_drums_selects_the_bigger_system() {
        let s = song(true);
        let m = parse_fur(&fur(&s, WriteOptions::default())).expect("parse");
        assert_eq!(*m.chips.last().unwrap(), CHIP_Y8950_DRUMS);
        assert_eq!(m.n_ch, 8 + 12);
    }

    #[test]
    fn test_sample_only_chip_survives_without_fm() {
        let s = analyze(
            &vgm(note(1000), vec![rom(0x2000, body(512))]),
            Default::default(),
        )
        .expect("analyze");
        let m = parse_fur(&fur(
            &s,
            WriteOptions { include_fm: false, include_pcm: true, ..Default::default() },
        ))
        .expect("parse");
        assert_eq!(m.chips, vec![CHIP_Y8950]);
        assert_eq!(m.n_ch, 10);
    }

    #[test]
    fn test_adpcm_instrument_type() {
        let s = song(false);
        let raw = fur(&s, WriteOptions::default());
        let plain = vgm2fur::furwrite::decompress_fur(&raw);
        let types: Vec<u16> = iter_blocks(&plain)
            .expect("blocks")
            .iter()
            .filter(|b| b.magic == *b"INS2")
            .map(|b| u16::from_le_bytes([b.payload[2], b.payload[3]]))
            .collect();
        assert!(types.contains(&(INS_ADPCMB as u16)), "{types:?}");
    }
}

mod TestY8950RoundTrip {
    use super::*;

    #[test]
    fn test_delta_t_and_clock_survive_the_module() {
        skip_without_furnace!();
        let mut writes = vec![
            ChipWrite::new(2000, "y8950", 0, 0xB0, 0x20),
            ChipWrite::new(3000, "y8950", 0, 0xB0, 0x00),
        ];
        writes.extend(note(1000));
        let mut src = vgm(writes, vec![rom(0x2000, body(512))]);
        src.y8950_clock = CLOCK;
        let s = analyze(&src, Default::default()).expect("analyze");
        assert!(!s.y8950_b_samples.is_empty());
        let kinds: Vec<&str> = s.fm_systems.iter().map(|x| x.kind.as_str()).collect();
        assert_eq!(kinds, vec!["y8950"]);

        let dir = tmp_dir();
        let fur_path = dir.join("delta_t_roundtrip.fur");
        std::fs::write(&fur_path, fur_bytes(&s)).expect("write fur");
        let out = dir.join("delta_t_roundtrip.vgm");
        vgm2fur::furnace::export_vgm(&fur_path, &out).expect("export");
        let exp = load_vgm(&out).expect("load export");
        assert_eq!(exp.y8950_clock, src.y8950_clock);
        let starts: Vec<i64> = exp
            .writes
            .iter()
            .filter(|w| {
                w.chip == "y8950"
                    && (w.reg & 0xFF) == 0x07
                    && (w.val & 0x80) != 0
                    && (w.val & 0x01) == 0
            })
            .map(|w| w.sample)
            .collect();
        assert!(!starts.is_empty());
    }

    fn fur_bytes(song: &vgm2fur::model::Song) -> Vec<u8> {
        write_fur(song, WriteOptions { compress: true, ..Default::default() })
    }
}
