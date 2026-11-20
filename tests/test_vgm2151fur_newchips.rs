//! SegaPCM, Namco C140 and Namco C352 support: parsing, collection, .fur
//! emission.
//!
//! Port of `tests/test_vgm2151fur_newchips.py` at vgm2151fur a8edb6e.

mod common;
#[allow(unused_imports)]
use common::*;

use std::collections::BTreeSet;
use std::path::PathBuf;

use indexmap::IndexMap;

use vgm2fur::analyze::{analyze, AnalyzeOptions};
use vgm2fur::c140::{c140_rate, mulaw_i16};
use vgm2fur::c352::{c352_rate, collect_c352};
use vgm2fur::furio::{iter_blocks, list_fur_samples, master_volume, parse_fur, FurModule, FurRow};
use vgm2fur::furwrite::{
    write_fur, WriteOptions, CHIP_C140, CHIP_C352, CHIP_MSM6295, CHIP_SEGAPCM, CHIP_SMS,
    INS_C140, INS_C352, INS_SEGAPCM, _player_chip_volume, _sample_chip_volume,
};
use vgm2fur::model::Song;
use vgm2fur::segapcm::{segapcm_rate, _advance_voice, _Voice};
use vgm2fur::vgm::{
    c352_effective_clock, chips_in_head, load_vgm, ChipWrite, RomSlice, VgmFile,
};

fn tmp_dir() -> PathBuf {
    let p = std::env::temp_dir().join("vgm2151fur_newchips");
    std::fs::create_dir_all(&p).expect("create temp dir");
    p
}

fn _lattice() -> Vec<ChipWrite> {
    lattice(60, 5_000)
}

/// Keyword subset of the Python `_vgm(...)` helper.
#[derive(Clone, Default)]
struct VgmKw {
    segapcm_clock: u32,
    segapcm_bankshift: u8,
    segapcm_bankmask: u8,
    c140_clock: u32,
    c140_type: u8,
    c352_clock: u32,
    c352_divider: u8,
    c352_mute_rear: bool,
    msm6295_dual: bool,
    extra_volumes: IndexMap<u8, (u8, u16)>,
}

fn _vgm(writes: Vec<ChipWrite>, roms: Vec<RomSlice>, kw: VgmKw) -> VgmFile {
    let mut v = VgmFile::default();
    v.path = PathBuf::from("synthetic.vgm");
    v.version = 0x161;
    v.total_samples = 400_000;
    v.loop_samples = 0;
    v.loop_file_pos = None;
    v.loop_sample = None;
    v.ym2151_clock = 3_579_545;
    v.k007232_clock = 0;
    v.gd3 = IndexMap::new();
    v.roms = roms;
    v.writes = writes;
    v.segapcm_clock = kw.segapcm_clock;
    v.segapcm_bankshift = kw.segapcm_bankshift;
    v.segapcm_bankmask = kw.segapcm_bankmask;
    v.c140_clock = kw.c140_clock;
    v.c140_type = kw.c140_type;
    v.c352_clock = kw.c352_clock;
    v.c352_divider = kw.c352_divider;
    v.c352_mute_rear = kw.c352_mute_rear;
    v.msm6295_dual = kw.msm6295_dual;
    v.extra_volumes = kw.extra_volumes;
    v
}

fn mk_rom(chip: u8, rom_size: i64, start: i64, data: Vec<u8>, kind: &'static str) -> RomSlice {
    RomSlice {
        chip,
        rom_size,
        start,
        data,
        kind,
        time: 0,
    }
}

fn analyze_pcm(vgm: &VgmFile) -> Song {
    analyze(
        vgm,
        AnalyzeOptions {
            pcm: true,
            ..Default::default()
        },
    )
    .expect("analyze")
}

fn _raw_vgm(cmds: &[u8], rom_blocks: &[(u8, u32, Vec<u8>)], shift: u8, mask: u8) -> Vec<u8> {
    let mut buf = vec![0u8; 0xC0];
    buf[0..4].copy_from_slice(b"Vgm ");
    buf[8..12].copy_from_slice(&0x161u32.to_le_bytes());
    buf[0x18..0x1C].copy_from_slice(&44100u32.to_le_bytes());
    buf[0x30..0x34].copy_from_slice(&3_579_545u32.to_le_bytes());
    buf[0x34..0x38].copy_from_slice(&(0xC0u32 - 0x34).to_le_bytes());
    buf[0x38..0x3C].copy_from_slice(&4_000_000u32.to_le_bytes()); // SegaPCM clock
    buf[0x3C] = shift; // SegaPCM bank shift
    buf[0x3E] = mask; // SegaPCM bank mask
    buf[0x96] = 1; // C140: System 21
    buf[0xA8..0xAC].copy_from_slice(&12_288_000u32.to_le_bytes()); // C140 clock
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

fn _msm6295_vgm(clock: u32, cmds: &[u8]) -> Vec<u8> {
    let mut buf = vec![0u8; 0xC0];
    buf[0..4].copy_from_slice(b"Vgm ");
    buf[8..12].copy_from_slice(&0x161u32.to_le_bytes());
    buf[0x18..0x1C].copy_from_slice(&44100u32.to_le_bytes());
    buf[0x34..0x38].copy_from_slice(&(0xC0u32 - 0x34).to_le_bytes());
    buf[0x98..0x9C].copy_from_slice(&clock.to_le_bytes());
    let mut out = buf;
    out.extend_from_slice(cmds);
    out.push(0x66);
    out
}

fn _raw_c352(
    cmds: &[u8],
    rom_blocks: &[(u8, u32, Vec<u8>)],
    clock: u32,
    divider: u8,
    mute_rear: bool,
) -> Vec<u8> {
    // VGM 1.71 with a 0x100 header so the C352 clock at 0xDC is in the header.
    let mut buf = vec![0u8; 0x100];
    buf[0..4].copy_from_slice(b"Vgm ");
    buf[8..12].copy_from_slice(&0x171u32.to_le_bytes());
    buf[0x18..0x1C].copy_from_slice(&44100u32.to_le_bytes());
    buf[0x30..0x34].copy_from_slice(&3_579_545u32.to_le_bytes());
    buf[0x34..0x38].copy_from_slice(&(0x100u32 - 0x34).to_le_bytes());
    let mut raw = clock & 0x3FFF_FFFF;
    if mute_rear {
        raw |= 0x8000_0000;
    }
    buf[0xDC..0xE0].copy_from_slice(&raw.to_le_bytes());
    buf[0xD6] = divider;
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

fn _e1(addr: u32, val: u32) -> Vec<u8> {
    vec![
        0xE1,
        ((addr >> 8) & 0xFF) as u8,
        (addr & 0xFF) as u8,
        ((val >> 8) & 0xFF) as u8,
        (val & 0xFF) as u8,
    ]
}

fn contains(hay: &[u8], needle: &[u8]) -> bool {
    hay.windows(needle.len()).any(|w| w == needle)
}

fn packed_f32(x: f64) -> Vec<u8> {
    (x as f32).to_le_bytes().to_vec()
}

fn le_i16(data: &[u8], off: usize) -> i16 {
    i16::from_le_bytes([data[off], data[off + 1]])
}

fn assert_delta(a: f64, b: f64, delta: f64) {
    assert!(
        (a - b).abs() <= delta,
        "{a} != {b} within delta {delta}"
    );
}

fn all_rows(mod_: &FurModule, ch: u16) -> Vec<FurRow> {
    let mut rows: Vec<FurRow> = Vec::new();
    for ((c, _idx), pat) in mod_.patterns.iter() {
        if *c == ch {
            rows.extend(pat.iter().cloned());
        }
    }
    rows
}

fn ins_types(data: &[u8]) -> Vec<i32> {
    let mut out: Vec<i32> = Vec::new();
    for b in iter_blocks(data).expect("blocks") {
        if b.magic == *b"INS2" {
            out.push(u16::from_le_bytes([b.payload[2], b.payload[3]]) as i32);
        }
    }
    out
}

fn flag_texts(data: &[u8]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for b in iter_blocks(data).expect("blocks") {
        if b.magic == *b"FLAG" {
            let end = b.payload.iter().position(|&x| x == 0).unwrap_or(b.payload.len());
            out.push(String::from_utf8_lossy(&b.payload[..end]).into_owned());
        }
    }
    out
}

// ---------------------------------------------------------------------------
// SegaPCM helpers shared by the segapcm classes.
// ---------------------------------------------------------------------------

fn segapcm_region() -> Vec<u8> {
    let mut rom = vec![0u8; 0x1000];
    for i in 0x400..0x500 {
        rom[i] = 0xC0;
    }
    rom
}

#[allow(clippy::too_many_arguments)]
fn segapcm_key(
    t: i64,
    end: u8,
    delta: u8,
    ctrl: u8,
    start: i32,
    voice: u8,
) -> Vec<ChipWrite> {
    let base = (voice as i32) << 3;
    vec![
        ChipWrite::new(t, "segapcm", 0, (base + 0x04) as u32, (start & 0xFF) as u32),
        ChipWrite::new(t, "segapcm", 0, (base + 0x05) as u32, ((start >> 8) & 0xFF) as u32),
        ChipWrite::new(t, "segapcm", 0, (base + 0x84) as u32, (start & 0xFF) as u32),
        ChipWrite::new(t, "segapcm", 0, (base + 0x85) as u32, ((start >> 8) & 0xFF) as u32),
        ChipWrite::new(t + 2, "segapcm", 0, (base + 0x06) as u32, end as u32),
        ChipWrite::new(t + 2, "segapcm", 0, (base + 0x07) as u32, delta as u32),
        ChipWrite::new(t + 2, "segapcm", 0, (base + 0x02) as u32, 0x40),
        ChipWrite::new(t + 2, "segapcm", 0, (base + 0x03) as u32, 0x40),
        ChipWrite::new(t + 2, "segapcm", 0, (base + 0x86) as u32, ctrl as u32),
    ]
}

fn segapcm_song() -> Song {
    let mut rom = vec![0u8; 0x1000];
    for i in 0x400..0x500 {
        rom[i] = 0x80 + ((i & 0x3F) as u8);
    }
    let mut writes = _lattice();
    // voice 0: loop/start 0x0400 0x0400, end page 0x04 (end_byte 0x500)
    writes.extend([
        ChipWrite::new(60000, "segapcm", 0, 0x04, 0x00),
        ChipWrite::new(60000, "segapcm", 0, 0x05, 0x04),
        ChipWrite::new(60000, "segapcm", 0, 0x06, 0x04),
        ChipWrite::new(60000, "segapcm", 0, 0x07, 0x84), // delta 132
        ChipWrite::new(60000, "segapcm", 0, 0x02, 0x20), // vol L
        ChipWrite::new(60000, "segapcm", 0, 0x03, 0x10), // vol R
        ChipWrite::new(60000, "segapcm", 0, 0x84, 0x00),
        ChipWrite::new(60000, "segapcm", 0, 0x85, 0x04),
        ChipWrite::new(60000, "segapcm", 0, 0x86, 0x00), // enable (loop)
    ]);
    let roms = vec![mk_rom(0, 0x1000, 0, rom, "segapcm")];
    analyze_pcm(&_vgm(
        writes,
        roms,
        VgmKw {
            segapcm_clock: 4_000_000,
            ..Default::default()
        },
    ))
}

fn segapcm_fade_song(tail: Vec<ChipWrite>, delta: u8, volume: u8) -> Song {
    let region = segapcm_region();
    let mut writes = _lattice();
    writes.extend(segapcm_key(100000, 0x04, delta, 0x00, 0x0400, 0));
    // segapcm_key always writes 0x40/0x40. Replace the volume with this hit's
    // own value when the test needs a different key-on level.
    if volume != 0x40 {
        writes = writes
            .into_iter()
            .map(|w| ChipWrite {
                val: if w.reg == 0x02 || w.reg == 0x03 {
                    volume as u32
                } else {
                    w.val
                },
                ..w
            })
            .collect();
    }
    writes.extend(tail);
    let roms = vec![mk_rom(0, 0x1000, 0, region, "segapcm")];
    analyze_pcm(&_vgm(
        writes,
        roms,
        VgmKw {
            segapcm_clock: 4_000_000,
            ..Default::default()
        },
    ))
}

// ---------------------------------------------------------------------------
// C140 helpers.
// ---------------------------------------------------------------------------

fn c140_body() -> Vec<u8> {
    // No zero bytes, so trailing-silence trim keeps all 200 frames.
    (0..200).map(|i| ((i * 3 + 1) & 0xFF) as u8).collect()
}

struct C140SongOpts {
    compressed: bool,
    loop_: bool,
    mix: Option<u16>,
    tail: Vec<ChipWrite>,
    key_vol: (u32, u32),
}

impl Default for C140SongOpts {
    fn default() -> Self {
        C140SongOpts {
            compressed: false,
            loop_: true,
            mix: None,
            tail: Vec::new(),
            key_vol: (0xC8, 0x64),
        }
    }
}

fn c140_song(o: C140SongOpts) -> Song {
    let body = c140_body();
    let mut rom = vec![0u8; 0x200];
    rom[0x100..0x100 + body.len()].copy_from_slice(&body);
    let mut mode = 0x80;
    if o.loop_ {
        mode |= 0x10;
    }
    if o.compressed {
        mode |= 0x08;
    }
    let mut writes = _lattice();
    writes.extend([
        ChipWrite::new(60000, "c140", 0, 0x04, 0),      // bank 0 → addr == start
        ChipWrite::new(60000, "c140", 0, 0x02, 0x64),   // freq MSB (0x6400)
        ChipWrite::new(60000, "c140", 0, 0x03, 0x00),
        ChipWrite::new(60000, "c140", 0, 0x01, o.key_vol.0), // vol L
        ChipWrite::new(60000, "c140", 0, 0x00, o.key_vol.1), // vol R
        ChipWrite::new(60000, "c140", 0, 0x06, 0x01),
        ChipWrite::new(60000, "c140", 0, 0x07, 0x00),
        ChipWrite::new(60000, "c140", 0, 0x08, 0x01),
        ChipWrite::new(60000, "c140", 0, 0x09, 0xC8),
        ChipWrite::new(60000, "c140", 0, 0x0A, 0x01),
        ChipWrite::new(60000, "c140", 0, 0x0B, 0x20),
        ChipWrite::new(60000, "c140", 0, 0x05, mode),
    ]);
    writes.extend(o.tail);
    let roms = vec![mk_rom(0, rom.len() as i64, 0, rom, "c140")];
    let extra = match o.mix {
        Some(mix) => IndexMap::from([(0x1Cu8, (0u8, mix))]),
        None => IndexMap::new(),
    };
    analyze_pcm(&_vgm(
        writes,
        roms,
        VgmKw {
            c140_clock: 12_288_000,
            c140_type: 1,
            extra_volumes: extra,
            ..Default::default()
        },
    ))
}

// ---------------------------------------------------------------------------
// C352 helpers.
// ---------------------------------------------------------------------------

fn c352_body(n: usize) -> Vec<u8> {
    (0..n).map(|i| ((i * 3 + 1) & 0xFF) as u8).collect()
}

fn c352_regs(sample: i64, voice: u8, chip: u8, regs: &[(u8, u32)]) -> Vec<ChipWrite> {
    regs.iter()
        .map(|&(i, v)| {
            ChipWrite::new(
                sample,
                "c352",
                chip,
                voice as u32 * 8 + i as u32,
                v & 0xFFFF,
            )
        })
        .collect()
}

fn c352_strobe(sample: i64, chip: u8, val: u32) -> ChipWrite {
    ChipWrite::new(sample, "c352", chip, 0x202, val)
}

fn c352_song(writes: Vec<ChipWrite>, rom_data: Vec<u8>, chip: u8, rom_size: Option<i64>, kw: VgmKw) -> Song {
    let mut all = _lattice();
    all.extend(writes);
    let roms = vec![mk_rom(
        chip,
        rom_size.unwrap_or(rom_data.len() as i64),
        0,
        rom_data,
        "c352",
    )];
    let mut kw = kw;
    kw.c352_clock = 24_192_000;
    kw.c352_divider = 72;
    analyze_pcm(&_vgm(all, roms, kw))
}

fn c352_quad_song(writes: Vec<ChipWrite>, rom_data: Vec<u8>) -> Song {
    let mut all = _lattice();
    all.extend(writes);
    let roms = vec![mk_rom(0, rom_data.len() as i64, 0, rom_data, "c352")];
    analyze(
        &_vgm(
            all,
            roms,
            VgmKw {
                c352_clock: 24_192_000,
                c352_divider: 72,
                ..Default::default()
            },
        ),
        AnalyzeOptions {
            pcm: true,
            c352_quad: true,
            ..Default::default()
        },
    )
    .expect("analyze quad")
}

fn oneshot_writes(when: i64, over: &[(u8, u32)]) -> Vec<ChipWrite> {
    let mut base: [(u8, u32); 8] = [
        (0, 0x4000),
        (1, 0x0010),
        (2, 0x8000),
        (3, 0x4000),
        (4, 0),
        (5, 0x100),
        (6, 0x1C8),
        (7, 0),
    ];
    for &(i, v) in over {
        base[i as usize] = (i, v);
    }
    c352_regs(when, 0, 0, &base)
}

// ---------------------------------------------------------------------------
// TestVgmParsing
// ---------------------------------------------------------------------------

mod TestVgmParsing {
    use super::*;

    #[test]
    fn test_msm6295_command_reads_the_dual_flag() {
        // Regression: the command walker used a bare `msm6295_dual` name
        // (NameError) instead of the VgmFile field. Every MSM6295 write hit
        // it, dual or not.
        let p = tmp_dir().join("msm6295_single.vgm");
        std::fs::write(&p, _msm6295_vgm(2_000_000, &[0xB8, 0x01, 0x20])).unwrap();
        let v = load_vgm(&p).expect("load");
        assert!(!v.msm6295_dual);
        assert_eq!(
            v.writes
                .iter()
                .filter(|w| w.chip == "msm6295")
                .map(|w| (w.chip_id, w.reg, w.val))
                .collect::<Vec<_>>(),
            vec![(0, 0x01, 0x20)]
        );

        let p = tmp_dir().join("msm6295_dual.vgm");
        std::fs::write(
            &p,
            _msm6295_vgm(
                0x40000000 | 2_000_000,
                &[0xB8, 0x81, 0x0F, 0xB8, 0x01, 0x20],
            ),
        )
        .unwrap();
        let v = load_vgm(&p).expect("load");
        assert!(v.msm6295_dual);
        assert_eq!(
            v.writes
                .iter()
                .filter(|w| w.chip == "msm6295")
                .map(|w| (w.chip_id, w.reg, w.val))
                .collect::<Vec<_>>(),
            vec![(1, 0x01, 0x0F), (0, 0x01, 0x20)]
        );
        assert!(!v.warnings.iter().any(|w| w.contains("0xB8")));
    }

    #[test]
    fn test_segapcm_write_and_rom() {
        let raw = _raw_vgm(
            b"\xc0\x84\x00\x10\xc0\x85\x00\x02\xc0\x86\x00\x00",
            &[(0x80, 0x0400, (0..64u8).collect())],
            0,
            0,
        );
        let p = tmp_dir().join("spcm.vgm");
        std::fs::write(&p, raw).unwrap();
        let v = load_vgm(&p).expect("load");
        assert_eq!(v.segapcm_clock, 4_000_000);
        assert_eq!(v.segapcm_bankshift, 0);
        assert_eq!(v.segapcm_bankmask, 0);
        assert_eq!(
            v.roms.iter().map(|s| s.kind).collect::<Vec<_>>(),
            vec!["segapcm"]
        );
        assert_eq!(v.roms[0].start, 0x0400);
        let w: Vec<_> = v.writes.iter().filter(|x| x.chip == "segapcm").collect();
        assert_eq!(
            w.iter().map(|x| (x.reg, x.val)).collect::<Vec<_>>(),
            vec![(0x84, 0x10), (0x85, 0x02), (0x86, 0x00)]
        );
    }

    #[test]
    fn test_c140_write_and_rom() {
        let raw = _raw_vgm(b"\xd4\x00\x35\xd0", &[(0x8D, 0x80000, vec![0u8; 32])], 0, 0);
        let p = tmp_dir().join("c140.vgm");
        std::fs::write(&p, raw).unwrap();
        let v = load_vgm(&p).expect("load");
        assert_eq!(v.c140_clock, 12_288_000);
        assert_eq!(v.c140_type, 1);
        assert_eq!(
            v.roms.iter().map(|s| s.kind).collect::<Vec<_>>(),
            vec!["c140"]
        );
        assert_eq!(v.roms[0].start, 0x80000);
        let w: Vec<_> = v.writes.iter().filter(|x| x.chip == "c140").collect();
        assert_eq!(
            w.iter().map(|x| (x.reg, x.val)).collect::<Vec<_>>(),
            vec![(0x035, 0xD0)]
        );
    }

    #[test]
    fn test_legacy_clock_selects_system_21_mapping() {
        // 1.51 file: no 1.61 type field, but the legacy 8MHz/374 rate in the
        // clock slot means System 21.
        let mut raw = _raw_vgm(b"", &[(0x8D, 0x80000, vec![0u8; 32])], 0, 0);
        raw[8..12].copy_from_slice(&0x151u32.to_le_bytes());
        raw[0xA8..0xAC].copy_from_slice(&21390u32.to_le_bytes());
        let p = tmp_dir().join("c140_legacy.vgm");
        std::fs::write(&p, raw).unwrap();
        let v = load_vgm(&p).expect("load");
        assert_eq!(v.c140_clock, 12_288_000);
        assert_eq!(v.c140_type, 1);
    }

    #[test]
    fn test_upd7759_write_is_parsed() {
        let raw = _raw_vgm(b"\xb6\x00\x00", &[], 0, 0); // uPD7759 register write
        let p = tmp_dir().join("upd.vgm");
        std::fs::write(&p, raw).unwrap();
        let v = load_vgm(&p).expect("load");
        assert_eq!(
            v.writes
                .iter()
                .map(|x| (x.chip, x.chip_id, x.reg, x.val))
                .collect::<Vec<_>>(),
            vec![("upd7759", 0, 0, 0)]
        );
    }

    #[test]
    fn test_segapcm_interface_bytes() {
        let raw = _raw_vgm(b"", &[], 12, 0x70);
        let p = tmp_dir().join("spcm_intf.vgm");
        std::fs::write(&p, raw).unwrap();
        let v = load_vgm(&p).expect("load");
        assert_eq!(v.segapcm_bankshift, 12);
        assert_eq!(v.segapcm_bankmask, 0x70);
    }
}

// ---------------------------------------------------------------------------
// TestSegapcm
// ---------------------------------------------------------------------------

mod TestSegapcm {
    use super::*;

    #[test]
    fn test_hit_and_sample() {
        let song = segapcm_song();
        assert_eq!(song.segapcm_samples.len(), 1);
        let smp = &song.segapcm_samples[0];
        assert_eq!(smp.start, 0x400);
        assert_eq!(smp.length(), 256);
        assert_eq!(smp.loop_start, Some(0));
        assert_eq!(smp.loop_end, Some(256));
        assert_eq!(smp.mode_freq(), 0x84);
        assert_delta(smp.c4_rate(4_000_000) as f64, 4_000_000.0 * 132.0 / 32768.0, 1.0);
    }

    #[test]
    fn test_fur_chip_and_instrument() {
        let song = segapcm_song();
        let events: Vec<_> = song.events.iter().filter(|e| e.on && e.ch >= 8).collect();
        assert!(!events.is_empty());
        let ev = events[0];
        assert_eq!(ev.ch, 8);
        assert_eq!(ev.ins, song.fm_patches.len() as i32); // first sample instrument
        assert_eq!(ev.vol, 0x20); // native 7-bit register value
        let data = write_fur(&song, WriteOptions::default());
        let mod_ = parse_fur(&data).expect("parse");
        assert_eq!(mod_.chips, vec![0x82, CHIP_SEGAPCM]);
        assert_eq!(mod_.n_ch, 8 + 16);
        assert!(mod_.pcm_channels.iter().min().copied().unwrap_or(usize::MAX) >= 8);
        assert!(ins_types(&data).contains(&INS_SEGAPCM));
        let smps = list_fur_samples(&data).expect("samples");
        assert_eq!(smps.len(), 1);
        assert_eq!(smps[0].n_frames, 256);
        assert_eq!(smps[0].loop_start, 0);
    }

    #[test]
    fn test_disabled_voice_is_not_a_hit() {
        let mut writes = _lattice();
        writes.extend([
            ChipWrite::new(60000, "segapcm", 0, 0x84, 0x00),
            ChipWrite::new(60000, "segapcm", 0, 0x85, 0x04),
            ChipWrite::new(60000, "segapcm", 0, 0x86, 0x01), // disable
            ChipWrite::new(70000, "segapcm", 0, 0x86, 0x00), // a bare re-enable
        ]);
        let roms = vec![mk_rom(0, 0x1000, 0, vec![0u8; 0x1000], "segapcm")];
        let song = analyze_pcm(&_vgm(
            writes,
            roms,
            VgmKw {
                segapcm_clock: 4_000_000,
                ..Default::default()
            },
        ));
        assert_eq!(song.segapcm_samples, vec![]);
    }

    #[test]
    fn test_bank_offset_selects_rom_window() {
        // shift 12, mask 0 → 0x70. ctrl 0xC0 → (0x40 << 12) | 0x90 = 0x40090.
        let mut rom = vec![0u8; 0x40100];
        let raw: Vec<u8> = (0..0x70).map(|i| 0xA0u8.wrapping_add((i & 0x0F) as u8)).collect();
        rom[0x40090..0x40100].copy_from_slice(&raw);
        let mut writes = _lattice();
        writes.extend([
            ChipWrite::new(60000, "segapcm", 0, 0x06, 0x00), // end page 0 → 0x100
            ChipWrite::new(60000, "segapcm", 0, 0x07, 0x40),
            ChipWrite::new(60000, "segapcm", 0, 0x84, 0x90),
            ChipWrite::new(60000, "segapcm", 0, 0x85, 0x00),
            ChipWrite::new(60000, "segapcm", 0, 0x86, 0xC2), // bank, one-shot
        ]);
        let roms = vec![mk_rom(0, rom.len() as i64, 0, rom, "segapcm")];
        let song = analyze_pcm(&_vgm(
            writes,
            roms,
            VgmKw {
                segapcm_clock: 4_000_000,
                segapcm_bankshift: 12,
                segapcm_bankmask: 0,
                ..Default::default()
            },
        ));
        assert_eq!(song.segapcm_samples.len(), 1);
        let smp = &song.segapcm_samples[0];
        assert_eq!(smp.start, 0x40090);
        assert_eq!(
            smp.data,
            raw.iter().map(|b| b ^ 0x80).collect::<Vec<u8>>()
        );
        assert_eq!(smp.loop_start, None);
    }

    #[test]
    fn test_register_dump_split_across_two_samples_is_one_note() {
        // End and delta arrive two samples after the address. The note uses
        // those later values, and the dump is one hit.
        let region = segapcm_region();
        let mut writes = _lattice();
        writes.extend([
            ChipWrite::new(1000, "segapcm", 0, 0x06, 0x20),
            ChipWrite::new(1000, "segapcm", 0, 0x07, 0x01),
        ]);
        writes.extend(segapcm_key(50000, 0x04, 0x84, 0x02, 0x0400, 0));
        let roms = vec![mk_rom(0, 0x1000, 0, region, "segapcm")];
        let song = analyze_pcm(&_vgm(
            writes,
            roms,
            VgmKw {
                segapcm_clock: 4_000_000,
                ..Default::default()
            },
        ));
        assert_eq!(song.segapcm_samples.len(), 1);
        let smp = &song.segapcm_samples[0];
        assert_eq!(smp.start, 0x400);
        assert_eq!(smp.length(), 256);
        assert_eq!(smp.mode_freq(), 0x84);
        assert_eq!(smp.hits.len(), 1);
    }

    #[test]
    fn test_same_address_after_the_cursor_moves_is_a_second_note() {
        // 13 samples at delta 0xB4 advances the cursor several bytes, so
        // rewriting the same start seeks. The two dumps stay separate.
        let region = segapcm_region();
        let mut writes = _lattice();
        writes.extend(segapcm_key(100000, 0x04, 0xB4, 0x02, 0x0400, 0));
        writes.extend(segapcm_key(100013, 0x04, 0xB4, 0x02, 0x0400, 0));
        let roms = vec![mk_rom(0, 0x1000, 0, region, "segapcm")];
        let song = analyze_pcm(&_vgm(
            writes,
            roms,
            VgmKw {
                segapcm_clock: 4_000_000,
                ..Default::default()
            },
        ));
        assert_eq!(song.segapcm_samples.len(), 1);
        assert_eq!(song.segapcm_samples[0].hits.len(), 2);
    }

    #[test]
    fn test_rewrite_before_the_byte_advances_is_the_same_note() {
        // delta 1 over 5 samples moves the fraction only. The byte address
        // is unchanged, so the rewrite is not a seek.
        let region = segapcm_region();
        let mut writes = _lattice();
        writes.extend(segapcm_key(100000, 0x04, 0x01, 0x02, 0x0400, 0));
        writes.extend(segapcm_key(100005, 0x04, 0x01, 0x02, 0x0400, 0));
        let roms = vec![mk_rom(0, 0x1000, 0, region, "segapcm")];
        let song = analyze_pcm(&_vgm(
            writes,
            roms,
            VgmKw {
                segapcm_clock: 4_000_000,
                ..Default::default()
            },
        ));
        assert_eq!(song.segapcm_samples.len(), 1);
        assert_eq!(song.segapcm_samples[0].hits.len(), 1);
    }

    #[test]
    fn test_volume_and_delta_refresh_is_not_a_new_note() {
        let region = segapcm_region();
        let mut writes = _lattice();
        writes.extend(segapcm_key(100000, 0x04, 0x40, 0x02, 0x0400, 0));
        writes.extend([
            ChipWrite::new(101000, "segapcm", 0, 0x02, 0x10),
            ChipWrite::new(101000, "segapcm", 0, 0x07, 0x20),
        ]);
        let roms = vec![mk_rom(0, 0x1000, 0, region, "segapcm")];
        let song = analyze_pcm(&_vgm(
            writes,
            roms,
            VgmKw {
                segapcm_clock: 4_000_000,
                ..Default::default()
            },
        ));
        assert_eq!(song.segapcm_samples[0].hits.len(), 1);
        assert_eq!(song.segapcm_samples[0].hits[0].freq, 0x40);
    }

    #[test]
    fn test_one_shot_replay_after_the_phrase_ends() {
        // The chip sets the disable bit itself when the end page is reached.
        // That write is not in the log. Rewriting the same start later seeks
        // back and plays the phrase again. A far write on another voice must
        // not drop the first note.
        let region = segapcm_region();
        let mut writes = _lattice();
        writes.extend(segapcm_key(10000, 0x04, 0xFF, 0x02, 0x0400, 0));
        writes.extend(segapcm_key(80000, 0x04, 0xFF, 0x02, 0x0400, 1));
        writes.extend(segapcm_key(80000, 0x04, 0xFF, 0x02, 0x0400, 0));
        let roms = vec![mk_rom(0, 0x1000, 0, region, "segapcm")];
        let song = analyze_pcm(&_vgm(
            writes,
            roms,
            VgmKw {
                segapcm_clock: 4_000_000,
                ..Default::default()
            },
        ));
        let voice0: Vec<_> = song
            .segapcm_samples
            .iter()
            .flat_map(|s| s.hits.iter())
            .filter(|h| h.voice == 0)
            .collect();
        assert_eq!(voice0.len(), 2);
        assert!(voice0.iter().all(|h| h.start == 0x0400));
        assert_eq!(song.segapcm_samples[0].length(), 256);
    }

    #[test]
    fn test_restarts_get_their_own_rows_inside_the_order_budget() {
        let region = segapcm_region();
        let mut writes = _lattice();
        let t = 100000;
        // One pair closer than any row the 256-order budget can give an
        // 8e6-sample song, then a stream of 800-sample restarts.
        writes.extend(segapcm_key(t, 0x04, 0xB4, 0x02, 0x0400, 0));
        writes.extend(segapcm_key(t + 13, 0x04, 0xB4, 0x02, 0x0400, 0));
        for k in 1..12 {
            writes.extend(segapcm_key(t + 13 + 800 * k, 0x04, 0xB4, 0x02, 0x0400, 0));
        }
        let roms = vec![mk_rom(0, 0x1000, 0, region, "segapcm")];
        let mut vgm = _vgm(
            writes,
            roms,
            VgmKw {
                segapcm_clock: 4_000_000,
                ..Default::default()
            },
        );
        vgm.total_samples = 8_000_000;
        let song = analyze(
            &vgm,
            AnalyzeOptions {
                speed: Some(8),
                pcm: true,
                ..Default::default()
            },
        )
        .expect("analyze");
        let hits: Vec<_> = song
            .segapcm_samples
            .iter()
            .flat_map(|s| s.hits.iter())
            .filter(|h| h.voice == 0)
            .collect();
        assert_eq!(hits.len(), 13);
        let spr = song.samples_per_row();
        assert!(spr > 400.0);
        assert!(spr < 900.0);
        let rows: Vec<i32> = hits.iter().map(|h| song.place(h.sample_time, None).0).collect();
        // The 13-sample pair shares a row. Every 800-sample restart has its own.
        let unique: BTreeSet<i32> = rows.iter().copied().collect();
        assert_eq!(rows.len() - unique.len(), 1);
    }

    #[test]
    fn test_advance_matches_a_tick_loop() {
        fn tick(v: &mut _Voice) {
            if v.ctrl & 1 != 0 {
                v.addr &= 0xFFFF00;
                return;
            }
            let stop = (v.end + 1) & 0xFF;
            if ((v.addr >> 16) & 0xFF) == stop {
                if v.ctrl & 2 != 0 {
                    v.ctrl |= 1;
                    return;
                }
                v.addr = (v.r#loop & 0xFFFF) << 8;
            }
            v.addr = (v.addr + (v.freq & 0xFF)) & 0xFFFFFF;
        }

        let cases = [
            _Voice { addr: 0x040000, r#loop: 0x0400, end: 0x04, freq: 0xB4, ctrl: 0x00, ..Default::default() },
            _Voice { addr: 0x040000, r#loop: 0x0400, end: 0x04, freq: 0xFF, ctrl: 0x02, ..Default::default() },
            _Voice { addr: 0xFE8000, r#loop: 0x0100, end: 0x00, freq: 0x7F, ctrl: 0x00, ..Default::default() },
            _Voice { addr: 0x040080, r#loop: 0x0400, end: 0x04, freq: 0x00, ctrl: 0x00, ..Default::default() },
            _Voice { addr: 0x123456, r#loop: 0xFFFF, end: 0xFF, freq: 0x10, ctrl: 0x01, ..Default::default() },
        ];
        for start in cases {
            let copy = || _Voice {
                addr: start.addr,
                r#loop: start.r#loop,
                end: start.end,
                freq: start.freq,
                lvol: start.lvol,
                rvol: start.rvol,
                ctrl: start.ctrl,
            };
            let mut fast = copy();
            let mut slow = copy();
            for n in [1i64, 2, 7, 64, 500, 4096] {
                let mut probe = copy();
                _advance_voice(&mut probe, n);
                let mut walked = copy();
                for _ in 0..n {
                    tick(&mut walked);
                }
                assert_eq!(probe.addr, walked.addr, "{:?} {} {}", start.addr, start.freq, n);
                assert_eq!(probe.ctrl, walked.ctrl, "{:?} {} {}", start.addr, start.freq, n);
            }
            for _ in 0..3000 {
                _advance_voice(&mut fast, 1);
                tick(&mut slow);
            }
            assert_eq!(fast.addr, slow.addr);
            assert_eq!(fast.ctrl, slow.ctrl);
        }
    }
}

// ---------------------------------------------------------------------------
// TestSegapcmFadesAndSlides
// ---------------------------------------------------------------------------

mod TestSegapcmFadesAndSlides {
    use super::*;

    #[test]
    fn test_mid_note_volume_steps_and_pitch_bend() {
        let tail = vec![
            ChipWrite::new(110000, "segapcm", 0, 0x02, 0x20),
            ChipWrite::new(110000, "segapcm", 0, 0x03, 0x20),
            ChipWrite::new(120000, "segapcm", 0, 0x02, 0x00),
            ChipWrite::new(120000, "segapcm", 0, 0x03, 0x00),
            ChipWrite::new(130000, "segapcm", 0, 0x07, 0xA0),
        ];
        let song = segapcm_fade_song(tail, 0x80, 0x40);
        let ons: Vec<_> = song.events.iter().filter(|e| e.on && e.pcm).collect();
        assert_eq!(ons.len(), 1);
        let ev = ons[0];
        assert_eq!(ev.vol, 0x40);
        assert_eq!(
            ev.vol_pts
                .iter()
                .map(|&(t, v, p)| (t, v, p))
                .collect::<Vec<_>>(),
            vec![(110000, 0x20, Some(0xFF)), (120000, 0x00, None)]
        );
        // 0x80 -> 0xA0 is 3.86 semitones above the key-on pitch.
        assert_eq!(ev.sweep.len(), 1);
        assert_delta(ev.sweep[0].1 as f64, 247.0, 1.0);

        let data = write_fur(&song, WriteOptions::default());
        let rows = all_rows(&parse_fur(&data).expect("parse"), 8);
        assert!(rows.iter().any(|r| r.vol == 0x20));
        assert!(rows.iter().any(|r| r.vol == 0x00));
        let pitch_fx: Vec<u8> = rows
            .iter()
            .flat_map(|r| r.fx.iter())
            .filter(|(c, _v)| [0x01u8, 0x02, 0xE8, 0xE9, 0xE5].contains(c))
            .map(|(c, _)| *c)
            .collect();
        assert!(!pitch_fx.is_empty());
    }

    #[test]
    fn test_key_on_volume_in_the_dump_is_not_a_step() {
        // A second note while the voice is still keyed: its volume/end/delta
        // dump carries the note's own level, not a step away from the first.
        let region = segapcm_region();
        let mut writes = _lattice();
        writes.extend(segapcm_key(100000, 0x04, 0x80, 0x02, 0x0400, 0));
        writes.extend(segapcm_key(120000, 0x04, 0xA0, 0x02, 0x0400, 0));
        let roms = vec![mk_rom(0, 0x1000, 0, region, "segapcm")];
        let song = analyze_pcm(&_vgm(
            writes,
            roms,
            VgmKw {
                segapcm_clock: 4_000_000,
                ..Default::default()
            },
        ));
        let hits: Vec<_> = song.segapcm_samples.iter().flat_map(|s| s.hits.iter()).collect();
        assert_eq!(hits.len(), 2);
        assert!(hits.iter().all(|h| h.vol_pts.is_empty()));
        // The second note's pitch came in its dump and is not a bend.
        assert!(hits.iter().all(|h| h.freq_pts.is_empty()));
        assert_eq!(hits[1].freq, 0xA0);
    }

    #[test]
    fn test_fade_in_starts_silent() {
        let tail = vec![
            ChipWrite::new(110000, "segapcm", 0, 0x02, 0x20),
            ChipWrite::new(110000, "segapcm", 0, 0x03, 0x20),
        ];
        let song = segapcm_fade_song(tail, 0x80, 0x00);
        let ev = song.events.iter().find(|e| e.on && e.pcm).expect("note");
        assert_eq!(ev.vol, 0);
        assert!(ev.no_pan);
        assert_eq!(ev.vol_pts.iter().map(|&(_t, v, _p)| v).collect::<Vec<_>>(), vec![0x20]);
    }

    #[test]
    fn test_zero_key_on_frequency_takes_the_first_later_write() {
        // Some drivers key the voice before setting its pitch. The hit's
        // reference rate comes from that write, not from the zero snapshot.
        let song = segapcm_fade_song(
            vec![ChipWrite::new(110000, "segapcm", 0, 0x07, 0x80)],
            0x00,
            0x40,
        );
        let smp = &song.segapcm_samples[0];
        assert_eq!(smp.mode_freq(), 0x80);
        let ev = song.events.iter().find(|e| e.on && e.pcm).expect("note");
        assert_eq!(ev.note, 108);
        assert!(ev.sweep.is_empty());
    }
}

// ---------------------------------------------------------------------------
// TestC140
// ---------------------------------------------------------------------------

mod TestC140 {
    use super::*;

    #[test]
    fn test_hit_and_sample() {
        let song = c140_song(C140SongOpts::default());
        assert_eq!(song.c140_samples.len(), 1);
        let smp = &song.c140_samples[0];
        assert_eq!(smp.start, 0x100);
        assert_eq!(smp.length(), 200);
        assert_eq!(smp.depth, 8);
        assert_eq!(smp.data, c140_body());
        assert_eq!(smp.loop_start, Some(0x20));
        assert_eq!(smp.loop_end, Some(200));
        assert_eq!(smp.mode_freq(), 0x6400);
        let r = smp.c4_rate(12_288_000) as f64;
        assert_delta(r, 12_288_000.0 * 0x6400 as f64 / 18_874_368.0, 1.0);
    }

    #[test]
    fn test_fur_chip_and_instrument() {
        let song = c140_song(C140SongOpts::default());
        let events: Vec<_> = song.events.iter().filter(|e| e.on && e.ch >= 8).collect();
        assert!(!events.is_empty());
        assert_eq!(events[0].ch, 8);
        assert_eq!(events[0].vol, 0xC8);
        let data = write_fur(&song, WriteOptions::default());
        let mod_ = parse_fur(&data).expect("parse");
        assert_eq!(mod_.chips, vec![0x82, CHIP_C140]);
        assert_eq!(mod_.n_ch, 8 + 24);
        assert!(ins_types(&data).contains(&INS_C140));
        let smps = list_fur_samples(&data).expect("samples");
        assert_eq!(smps[0].n_frames, 200);
        assert_eq!((smps[0].loop_start, smps[0].loop_end), (0x20, 200));
        assert_eq!(smps[0].loop_mode, 0);
    }

    #[test]
    fn test_compressed_mode_decodes() {
        let song = c140_song(C140SongOpts {
            compressed: true,
            loop_: false,
            ..Default::default()
        });
        let smp = &song.c140_samples[0];
        assert_eq!(smp.depth, 16);
        assert_eq!(smp.length(), 200);
        assert_eq!(smp.data.len(), 400);
        // Full mulaw value of byte 0x01 is 256, not the old >>8 collapse to 1.
        assert_eq!(le_i16(&smp.data, 0), 256);
        assert_eq!(smp.loop_start, None);
        let data = write_fur(&song, WriteOptions::default());
        let smps = list_fur_samples(&data).expect("samples");
        assert_eq!(smps[0].depth, 16);
        assert_eq!(smps[0].n_frames, 200);
    }

    #[test]
    fn test_system2_bank_is_byte_address() {
        // bank 21 + start 0x8A82 → 0x58A82. A decoy at the low address must lose.
        let start = 0x8A82usize;
        let n = 64usize;
        let real: Vec<u8> = (0..n).map(|i| 0x10u8 + i as u8).collect();
        let mut rom = vec![0u8; 0x58A82 + n];
        rom[start..start + n].fill(0x7F);
        rom[0x58A82..0x58A82 + n].copy_from_slice(&real);
        let end = start + n;
        let mut writes = _lattice();
        writes.extend([
            ChipWrite::new(60000, "c140", 0, 0x04, 21),
            ChipWrite::new(60000, "c140", 0, 0x02, 0x10),
            ChipWrite::new(60000, "c140", 0, 0x03, 0x00),
            ChipWrite::new(60000, "c140", 0, 0x06, ((start >> 8) & 0xFF) as u32),
            ChipWrite::new(60000, "c140", 0, 0x07, (start & 0xFF) as u32),
            ChipWrite::new(60000, "c140", 0, 0x08, ((end >> 8) & 0xFF) as u32),
            ChipWrite::new(60000, "c140", 0, 0x09, (end & 0xFF) as u32),
            ChipWrite::new(60000, "c140", 0, 0x05, 0x80),
        ]);
        let roms = vec![mk_rom(0, rom.len() as i64, 0, rom, "c140")];
        let song = analyze_pcm(&_vgm(
            writes,
            roms,
            VgmKw {
                c140_clock: 12_288_000,
                c140_type: 0,
                ..Default::default()
            },
        ));
        assert_eq!(song.c140_samples.len(), 1);
        assert_eq!(song.c140_samples[0].data, real);
        assert_eq!(song.c140_samples[0].depth, 8);
    }

    #[test]
    fn test_undumped_rom_is_counted() {
        let mut writes = _lattice();
        writes.extend([
            ChipWrite::new(60000, "c140", 0, 0x04, 9), // bank 9, not dumped
            ChipWrite::new(60000, "c140", 0, 0x06, 0x40),
            ChipWrite::new(60000, "c140", 0, 0x07, 0x00),
            ChipWrite::new(60000, "c140", 0, 0x08, 0x40),
            ChipWrite::new(60000, "c140", 0, 0x09, 0x10),
            ChipWrite::new(60000, "c140", 0, 0x05, 0x80),
        ]);
        let roms = vec![
            mk_rom(0, 0x40000, 0x80000, vec![0u8; 0x1000], "c140"),
            mk_rom(0, 0x40000, 0x90000, vec![0u8; 0x1000], "c140"),
        ];
        let song = analyze_pcm(&_vgm(
            writes,
            roms,
            VgmKw {
                c140_clock: 12_288_000,
                ..Default::default()
            },
        ));
        assert!(song.warnings.iter().any(|w| w.contains("not dump")));
    }

    #[test]
    fn test_key_off_emits_note_off() {
        let body = c140_body();
        let mut rom = vec![0u8; 0x200];
        rom[0x100..0x100 + body.len()].copy_from_slice(&body);
        let mut writes = _lattice();
        writes.extend([
            ChipWrite::new(60000, "c140", 0, 0x06, 0x01),
            ChipWrite::new(60000, "c140", 0, 0x07, 0x00),
            ChipWrite::new(60000, "c140", 0, 0x08, 0x01),
            ChipWrite::new(60000, "c140", 0, 0x09, 0xC8),
            ChipWrite::new(60000, "c140", 0, 0x0A, 0x01),
            ChipWrite::new(60000, "c140", 0, 0x0B, 0x20),
            ChipWrite::new(60000, "c140", 0, 0x05, 0x90), // key on, looping
            ChipWrite::new(90000, "c140", 0, 0x05, 0x10), // key off (bit 7 clear)
        ]);
        let roms = vec![mk_rom(0, rom.len() as i64, 0, rom, "c140")];
        let song = analyze_pcm(&_vgm(
            writes,
            roms,
            VgmKw {
                c140_clock: 12_288_000,
                c140_type: 1,
                ..Default::default()
            },
        ));
        let offs: Vec<_> = song.events.iter().filter(|e| !e.on && e.ch == 8).collect();
        assert_eq!(offs.len(), 1);
        assert_eq!(offs[0].sample, 90000);
        let data = write_fur(&song, WriteOptions::default());
        assert_eq!(parse_fur(&data).expect("parse").chips, vec![0x82, CHIP_C140]);
    }
}

// ---------------------------------------------------------------------------
// TestChipVolumes
// ---------------------------------------------------------------------------

mod TestChipVolumes {
    use super::*;

    fn song_with_vgm(vgm: VgmFile) -> Song {
        let mut song = analyze(
            &note_song(),
            AnalyzeOptions {
                pcm: false,
                ..Default::default()
            },
        )
        .expect("base song");
        song.vgm = vgm;
        song
    }

    #[test]
    fn test_player_mix_defaults_and_entries() {
        let vgm = _vgm(
            vec![],
            vec![],
            VgmKw {
                extra_volumes: IndexMap::from([(0x1Cu8, (0u8, 0x80u16))]),
                ..Default::default()
            },
        );
        assert_almost_eq!(_player_chip_volume(&vgm, 0x03, 1), 1.0);
        assert_almost_eq!(_player_chip_volume(&vgm, 0x04, 1), 1.5);
        assert_almost_eq!(_player_chip_volume(&vgm, 0x1C, 1), 85.0 / 256.0);
        let vgm = _vgm(
            vec![],
            vec![],
            VgmKw {
                extra_volumes: IndexMap::from([(0x18u8, (0u8, 0x60u16))]),
                ..Default::default()
            },
        );
        assert_almost_eq!(_player_chip_volume(&vgm, 0x18, 1), 0.375);
        // Bit 15 makes an entry relative to the default; instances divide first.
        let vgm = _vgm(
            vec![],
            vec![],
            VgmKw {
                extra_volumes: IndexMap::from([(0x04u8, (0u8, 0x8080u16))]),
                ..Default::default()
            },
        );
        assert_almost_eq!(_player_chip_volume(&vgm, 0x04, 1), 0.75);
        let vgm = _vgm(
            vec![],
            vec![],
            VgmKw {
                extra_volumes: IndexMap::from([(0x18u8, (0u8, 0x60u16))]),
                ..Default::default()
            },
        );
        assert_almost_eq!(_player_chip_volume(&vgm, 0x18, 2), 0.375);
    }

    #[test]
    fn test_c140_fur_volume_follows_the_player_mix() {
        // No entry: C140 default 2/3, Furnace scale 0.7. Entry 0x80: 85/256.
        for (mix, expected) in [
            (None, 0.7 * (0xAB as f64 / 0x100 as f64)),
            (Some(0x80u16), 0.7 * (85.0 / 0x100 as f64)),
        ] {
            let song = c140_song(C140SongOpts {
                mix,
                ..Default::default()
            });
            let data = write_fur(&song, WriteOptions::default());
            assert!(contains(&data, &packed_f32(expected)));
        }
    }

    #[test]
    fn test_segapcm_fur_volume_is_the_player_default() {
        let song = segapcm_song();
        let data = write_fur(&song, WriteOptions::default());
        // The player default is 1.5. A master of 1.5 carries the boost, so the
        // per-chip float is the master-relative share.
        assert_eq!(master_volume(&data).expect("master volume"), 1.5);
        assert!(contains(&data, &packed_f32(1.0)));
        assert!(contains(&data, &packed_f32(1.0 / 1.5)));
    }

    #[test]
    fn test_dual_msm6295_instances_are_halved() {
        let song = song_with_vgm(_vgm(
            vec![],
            vec![],
            VgmKw {
                msm6295_dual: true,
                ..Default::default()
            },
        ));
        assert_almost_eq!(_sample_chip_volume(&song, CHIP_MSM6295, 1), 0.5);
        let song = song_with_vgm(_vgm(
            vec![],
            vec![],
            VgmKw {
                msm6295_dual: false,
                ..Default::default()
            },
        ));
        assert_almost_eq!(_sample_chip_volume(&song, CHIP_MSM6295, 1), 1.0);
    }

    #[test]
    fn test_sn76489_default_fur_volume_is_half() {
        let song = song_with_vgm(_vgm(vec![], vec![], VgmKw::default()));
        assert_almost_eq!(_sample_chip_volume(&song, CHIP_SMS, 1), 0.5);
    }
}

// ---------------------------------------------------------------------------
// TestC140FadesAndSlides
// ---------------------------------------------------------------------------

mod TestC140FadesAndSlides {
    use super::*;

    #[test]
    fn test_mid_note_volume_steps_and_hard_pitch_jump() {
        let tail = vec![
            ChipWrite::new(80000, "c140", 0, 0x01, 0x90), // L 200 -> 144
            ChipWrite::new(84000, "c140", 0, 0x01, 0x30), // L 144 -> 48 (R holds 100)
            ChipWrite::new(85000, "c140", 0, 0x00, 0x30), // R 100 -> 48
            ChipWrite::new(80000, "c140", 0, 0x02, 0x32), // 0x6400 -> 0x3200: -1 octave
            ChipWrite::new(90000, "c140", 0, 0x03, 0x00), // LSB write, freq unchanged
            ChipWrite::new(100000, "c140", 0, 0x05, 0x00), // key off
        ];
        let song = c140_song(C140SongOpts {
            tail,
            ..Default::default()
        });
        let ons: Vec<_> = song.events.iter().filter(|e| e.on && e.pcm).collect();
        assert_eq!(ons.len(), 1);
        let ev = ons[0];
        // Volume steps carry the L/R pair (the loud side is the level): the
        // intermediate (48, 100) state is real hardware, and a write that does
        // not change the pair (the LSB of an unchanged frequency) records
        // nothing.
        assert_eq!(
            ev.vol_pts.iter().map(|&(_t, v, _p)| v).collect::<Vec<_>>(),
            vec![144, 100, 48]
        );
        assert_eq!(
            ev.sweep.iter().map(|&(_t, d)| d).collect::<Vec<_>>(),
            vec![-768]
        );
        assert_eq!(ev.end, 100000);

        let data = write_fur(&song, WriteOptions::default());
        let mod_ = parse_fur(&data).expect("parse");
        let rows = all_rows(&mod_, 8);
        assert!(rows.iter().any(|r| r.vol == 144));
        assert!(rows.iter().any(|r| r.vol == 48));
        // The octave is one hard step with still rows around it, so it lands at
        // once: a whole-semitone transpose, never a rate-limited slur that would
        // spend the rest of the sample climbing toward a pitch already heard.
        let jump: Vec<u8> = rows
            .iter()
            .flat_map(|r| r.fx.iter())
            .filter(|(c, _v)| *c == 0xE9)
            .map(|(_c, v)| *v)
            .collect();
        assert_eq!(jump, vec![12]);
        let rates: Vec<u8> = rows
            .iter()
            .flat_map(|r| r.fx.iter())
            .filter(|(c, v)| [0x01u8, 0x02].contains(c) && *v != 0)
            .map(|(_c, v)| *v)
            .collect();
        assert_eq!(rates, Vec::<u8>::new());
        // The note row keeps its own pitch: nothing rides on the key-on.
        let note_row = rows.iter().find(|r| 0 <= r.note && r.note < 180).expect("note row");
        assert!(!note_row
            .fx
            .iter()
            .any(|(c, _v)| [0x01u8, 0x02, 0xE8, 0xE9].contains(c)));
    }

    #[test]
    fn test_big_jump_lands_as_quick_legato() {
        // A move bigger than the slide can deliver inside a row must land as an
        // instant E8xx/E9xx transpose instead of a rate-limited slur, on the
        // C140 and YM2151 platforms alike.
        let body = c140_body();
        let mut rom_data = vec![0u8; 0x200];
        rom_data[0x100..0x100 + body.len()].copy_from_slice(&body);
        let mut writes = _lattice();
        writes.extend([
            ChipWrite::new(60000, "c140", 0, 0x04, 0),
            ChipWrite::new(60000, "c140", 0, 0x02, 0x64),
            ChipWrite::new(60000, "c140", 0, 0x03, 0x00),
            ChipWrite::new(60000, "c140", 0, 0x01, 0xC8),
            ChipWrite::new(60000, "c140", 0, 0x00, 0x64),
            ChipWrite::new(60000, "c140", 0, 0x06, 0x01),
            ChipWrite::new(60000, "c140", 0, 0x07, 0x00),
            ChipWrite::new(60000, "c140", 0, 0x08, 0x01),
            ChipWrite::new(60000, "c140", 0, 0x09, 0xC0),
            ChipWrite::new(60000, "c140", 0, 0x0A, 0x01),
            ChipWrite::new(60000, "c140", 0, 0x0B, 0x20),
            ChipWrite::new(60000, "c140", 0, 0x05, 0x80 | 0x10),
            ChipWrite::new(61500, "c140", 0, 0x02, 0x32), // one octave down, two rows later
            ChipWrite::new(61600, "c140", 0, 0x03, 0x00),
            ChipWrite::new(100000, "c140", 0, 0x05, 0x00), // key off
        ]);
        let roms = vec![mk_rom(0, rom_data.len() as i64, 0, rom_data, "c140")];
        let song = analyze(
            &_vgm(
                writes,
                roms,
                VgmKw {
                    c140_clock: 12_288_000,
                    c140_type: 1,
                    ..Default::default()
                },
            ),
            AnalyzeOptions {
                speed: Some(1),
                pcm: true,
                ..Default::default()
            },
        )
        .expect("analyze");
        let data = write_fur(&song, WriteOptions::default());
        let rows = all_rows(&parse_fur(&data).expect("parse"), 8);
        let xpose: Vec<(u8, u8)> = rows
            .iter()
            .flat_map(|r| r.fx.iter())
            .filter(|(c, _v)| [0xE8u8, 0xE9].contains(c))
            .map(|(c, v)| (*c, *v))
            .collect();
        assert!(!xpose.is_empty(), "expected an E8/E9 quick-legato transpose");
        let down: Vec<u8> = xpose
            .iter()
            .filter(|(c, _v)| *c == 0xE9)
            .map(|(_c, v)| *v)
            .collect();
        assert!(
            !down.is_empty() && down[0] >= 8,
            "octave drop should transpose down hard, got {:?}",
            xpose
        );
    }

    #[test]
    fn test_frequency_after_key_on_refits_note_and_reference() {
        // A driver that keys the voice on with stale zero frequency registers
        // (the common "write the pitch right after the mode byte" pattern)
        // must not pin the sample's reference rate to 0: the played frequency
        // is the post-key-on write. Voice 0 gets 0xC800 that way, voice 1 keys
        // on normally at half that, so voice 1 is one octave under.
        let body = c140_body();
        let mut rom_data = vec![0u8; 0x200];
        rom_data[0x100..0x100 + body.len()].copy_from_slice(&body);
        let mode = 0x80 | 0x10;
        let common = |v: u32| -> Vec<ChipWrite> {
            vec![
                ChipWrite::new(60000, "c140", 0, (v << 4) | 0x04, 0),
                ChipWrite::new(60000, "c140", 0, (v << 4) | 0x01, 0xC8),
                ChipWrite::new(60000, "c140", 0, (v << 4) | 0x00, 0x64),
                ChipWrite::new(60000, "c140", 0, (v << 4) | 0x06, 0x01),
                ChipWrite::new(60000, "c140", 0, (v << 4) | 0x07, 0x00),
                ChipWrite::new(60000, "c140", 0, (v << 4) | 0x08, 0x01),
                ChipWrite::new(60000, "c140", 0, (v << 4) | 0x09, 0xC8),
                ChipWrite::new(60000, "c140", 0, (v << 4) | 0x0A, 0x01),
                ChipWrite::new(60000, "c140", 0, (v << 4) | 0x0B, 0x20),
            ]
        };
        let mut writes = _lattice();
        writes.extend(common(0));
        writes.extend([
            ChipWrite::new(60000, "c140", 0, 0x02, 0x00), // freq still 0 at key-on
            ChipWrite::new(60000, "c140", 0, 0x03, 0x00),
            ChipWrite::new(60000, "c140", 0, 0x05, mode),
            ChipWrite::new(60030, "c140", 0, 0x02, 0xC8), // real pitch right after
            ChipWrite::new(60035, "c140", 0, 0x03, 0x00),
        ]);
        writes.extend(common(1));
        writes.extend([
            ChipWrite::new(60000, "c140", 0, 0x12, 0x64), // normal: pitch pre-key-on
            ChipWrite::new(60000, "c140", 0, 0x13, 0x00),
            ChipWrite::new(60100, "c140", 0, 0x15, mode),
        ]);
        let roms = vec![mk_rom(0, rom_data.len() as i64, 0, rom_data, "c140")];
        let song = analyze_pcm(&_vgm(
            writes,
            roms,
            VgmKw {
                c140_clock: 12_288_000,
                c140_type: 1,
                ..Default::default()
            },
        ));
        let smp = &song.c140_samples[0];
        assert_eq!(smp.mode_freq(), 0xC800);
        let ons: Vec<_> = song.events.iter().filter(|e| e.on && e.pcm).collect();
        let mut notes: Vec<i32> = ons.iter().map(|e| e.note).collect();
        notes.sort();
        assert_eq!(notes, vec![96, 108]);
    }

    #[test]
    fn test_note_rows_keep_their_volume_with_fade_points() {
        // Fade points land between (and never on top of) note rows: a note's
        // own volume wins, and the stepped values show up as their own rows.
        let tail = vec![
            ChipWrite::new(80000, "c140", 0, 0x01, 0x40), // L 200 -> 64, R still 100
            ChipWrite::new(82000, "c140", 0, 0x00, 0x28), // R 100 -> 40: max 64
            ChipWrite::new(83000, "c140", 0, 0x05, 0x80), // retrigger voice 0
        ];
        let song = c140_song(C140SongOpts {
            tail,
            ..Default::default()
        });
        let data = write_fur(&song, WriteOptions::default());
        let mod_ = parse_fur(&data).expect("parse");
        let rows = all_rows(&mod_, 8);
        let notes: Vec<_> = rows.iter().filter(|r| 0 <= r.note && r.note < 180).collect();
        assert_eq!(notes.iter().map(|r| r.vol).collect::<Vec<_>>(), vec![200, 64]);
        assert!(rows.iter().any(|r| r.vol == 100));
    }

    #[test]
    fn test_fade_in_from_zero_volume_keeps_default_pan() {
        // A hit keyed with both registers at zero must not write 08 00: the
        // fade steps only move the volume column, so a "both off" pan would
        // mute the voice forever. The note starts silent, pan untouched, and
        // the balance steps ride along with the volume points.
        let tail = vec![
            ChipWrite::new(80000, "c140", 0, 0x01, 0x01), // L 0 -> 1
            ChipWrite::new(81000, "c140", 0, 0x00, 0x01), // R 0 -> 1
            ChipWrite::new(84000, "c140", 0, 0x01, 0x40), // L -> 64
            ChipWrite::new(85000, "c140", 0, 0x00, 0x40), // R -> 64
            ChipWrite::new(100000, "c140", 0, 0x05, 0x00), // key off
        ];
        let song = c140_song(C140SongOpts {
            key_vol: (0, 0),
            tail,
            ..Default::default()
        });
        let ons: Vec<_> = song.events.iter().filter(|e| e.on && e.pcm).collect();
        let ev = ons[0];
        assert_eq!(ev.vol, 0);
        assert!(ev.no_pan);
        assert_eq!(
            ev.vol_pts.iter().map(|&(_t, v, _p)| v).collect::<Vec<_>>(),
            vec![1, 1, 64, 64]
        );
        assert_eq!(ev.vol_pts[0].2, Some(0xF0)); // left-only step
        assert_eq!(ev.vol_pts[ev.vol_pts.len() - 1].2, Some(0xFF)); // both full

        let data = write_fur(&song, WriteOptions::default());
        let mod_ = parse_fur(&data).expect("parse");
        let rows = all_rows(&mod_, 8);
        let notes: Vec<_> = rows.iter().filter(|r| 0 <= r.note && r.note < 180).collect();
        assert_eq!(notes.len(), 1);
        assert!(!notes[0].fx.iter().any(|(c, _v)| *c == 0x08));
        assert_eq!(notes[0].vol, 0);
        assert!(rows.iter().any(|r| r.vol == 64));
        assert!(rows
            .iter()
            .flat_map(|r| r.fx.iter())
            .any(|(c, v)| *c == 0x08 && *v == 0xFF));
    }
}

// ---------------------------------------------------------------------------
// TestCollectHelpers
// ---------------------------------------------------------------------------

mod TestCollectHelpers {
    use super::*;

    #[test]
    fn test_rate_scales() {
        // Chip step-rate conventions: SegaPCM 32768, C140/C352 reference
        // divisor 18874368. 24192000 * 0x8000 / 18874368 is 42000 exactly.
        assert_delta(segapcm_rate(4_000_000, 132), 4_000_000.0 * 132.0 / 32768.0, 1.0);
        assert_delta(
            c140_rate(12_288_000, 25600),
            12_288_000.0 * 25600.0 / 18_874_368.0,
            1.0,
        );
        assert_eq!(c352_rate(24_192_000, 0x8000), 42000.0);
    }
}

// ---------------------------------------------------------------------------
// TestC352
// ---------------------------------------------------------------------------

mod TestC352 {
    use super::*;

    fn body_song() -> (Vec<u8>, Vec<u8>) {
        let body = c352_body(200);
        let mut rom = vec![0u8; 0x300];
        rom[0x100..0x1C8].copy_from_slice(&body);
        (rom, body)
    }

    #[test]
    fn test_clock_formula_and_header() {
        assert_eq!(c352_effective_clock(24_192_000, 72), 24_192_000);
        assert_eq!(c352_effective_clock(24_192_000, 0), 24_192_000);
        assert_eq!(c352_effective_clock(336_000, 1), 24_192_000);
        assert_eq!(c352_effective_clock(336_000, 0), 24_192_000);
        assert_eq!(c352_effective_clock(0, 72), 0);

        let mut cmds = _e1(2, 0x8000);
        cmds.extend(_e1(0x8002, 0x1000));
        let raw = _raw_c352(&cmds, &[(0x92, 0, vec![0u8; 16])], 24_192_000, 72, true);
        assert!(chips_in_head(&raw[..0x100]).contains("c352"));
        assert!(!chips_in_head(&_raw_vgm(b"", &[], 0, 0)).contains("c352"));
        let p = tmp_dir().join("c352_hdr.vgm");
        std::fs::write(&p, &raw).unwrap();
        let v = load_vgm(&p).expect("load");
        assert_eq!(v.c352_clock, 24_192_000);
        assert_eq!(v.c352_divider, 72);
        assert!(v.c352_mute_rear);
        assert_eq!(
            v.roms.iter().map(|s| s.kind).collect::<Vec<_>>(),
            vec!["c352"]
        );
        assert_eq!(
            v.writes
                .iter()
                .filter(|w| w.chip == "c352")
                .map(|w| (w.chip_id, w.reg, w.val))
                .collect::<Vec<_>>(),
            vec![(0, 2, 0x8000), (1, 2, 0x1000)]
        );
        assert!(!v.warnings.iter().any(|w| w.contains("does not convert")));

        std::fs::write(&p, _raw_c352(b"", &[], 336_000, 1, false)).unwrap();
        assert_eq!(load_vgm(&p).expect("load").c352_clock, 24_192_000);
        std::fs::write(&p, _raw_c352(b"", &[], 24_192_000, 0, false)).unwrap();
        assert_eq!(load_vgm(&p).expect("load").c352_clock, 24_192_000);

        let mut buf = _raw_c352(b"", &[], 0, 36, false);
        buf[0xBC..0xC0].copy_from_slice(&4u32.to_le_bytes());
        buf[0xC0..0xC4].copy_from_slice(&12u32.to_le_bytes());
        buf[0xC4..0xC8].copy_from_slice(&8u32.to_le_bytes());
        buf[0xCC] = 1;
        buf[0xCD] = 0x27;
        buf[0xCE..0xD2].copy_from_slice(&12_000_000u32.to_le_bytes());
        std::fs::write(&p, &buf).unwrap();
        let v = load_vgm(&p).expect("load");
        assert_eq!(v.c352_clock, 12_000_000);
        assert_eq!(v.c352_divider, 36);
        assert!(chips_in_head(&buf).contains("c352"));
    }

    #[test]
    fn test_oneshot_is_exclusive_and_needs_the_strobe() {
        let body = c352_body(200);
        let mut rom = vec![0u8; 0x300];
        rom[0x100..0x1C8].copy_from_slice(&body);
        let mut writes = oneshot_writes(50000, &[]);
        writes.push(c352_strobe(60000, 0, 0));
        let song = c352_song(writes, rom, 0, None, VgmKw::default());
        assert_eq!(song.c352_samples.len(), 1);
        let smp = &song.c352_samples[0];
        assert_eq!(smp.data, body);
        assert_eq!(smp.length(), 200);
        assert_eq!(smp.depth, 8);
        assert_eq!(smp.loop_start, None);
        assert_eq!(smp.hits[0].sample_time, 60000);
        assert_eq!(smp.mode_freq(), 0x8000);
        assert_eq!(smp.c4_rate(24_192_000), 42000);
        assert_eq!((smp.hits[0].vol_l, smp.hits[0].vol_r), (0x40, 0x10));
        let ev = song.events.iter().find(|e| e.on && e.pcm).expect("event");
        assert_eq!(ev.ch, 8);
        assert_eq!(ev.vol, 0x40);
        assert_eq!(ev.pan, 0xF4);
        assert_eq!(ev.ins, song.fm_patches.len() as i32);

        let (rom2, _) = body_song();
        let silent = c352_song(oneshot_writes(60000, &[]), rom2, 0, None, VgmKw::default());
        assert_eq!(silent.c352_samples, vec![]);

        let (rom3, _) = body_song();
        let mut again_writes = oneshot_writes(50000, &[]);
        again_writes.push(c352_strobe(60000, 0, 0));
        again_writes.push(c352_strobe(70000, 0, 0xFFFF));
        let again = c352_song(again_writes, rom3, 0, None, VgmKw::default());
        assert_eq!(again.c352_samples[0].hits.len(), 1);
    }

    #[test]
    fn test_rear_pair_kept_apart_for_quad_output() {
        let mut rom = vec![0u8; 0x300];
        rom[0x100..0x1C8].copy_from_slice(&c352_body(200));
        let mut writes = oneshot_writes(50000, &[]);
        writes.push(c352_strobe(60000, 0, 0));

        let stereo = c352_song(writes.clone(), rom.clone(), 0, None, VgmKw::default());
        assert!(!stereo.c352_quad);
        assert_eq!(
            stereo.events.iter().find(|e| e.on && e.pcm).expect("event").quad,
            None
        );
        // vol_f 0x4000, vol_r 0x0010: the stereo rendition carries the sum.
        let hit = &stereo.c352_samples[0].hits[0];
        assert_eq!((hit.vol_l, hit.vol_r), (0x40, 0x10));

        let quad = c352_quad_song(writes, rom);
        assert!(quad.c352_quad);
        let hit = &quad.c352_samples[0].hits[0];
        assert_eq!(
            (hit.front_l, hit.front_r, hit.rear_l, hit.rear_r),
            (0x40, 0x00, 0x00, 0x10)
        );
        let ev = quad.events.iter().find(|e| e.on && e.pcm).expect("event");
        assert_eq!(ev.quad, Some((0x40, 0x00, 0x00, 0x10)));
        // the four registers carry the level, so the column is held wide open
        assert_eq!(ev.vol, 255);
        assert_eq!(ev.vol_pts, Vec::new());

        let mut stats = IndexMap::new();
        let data = write_fur(
            &quad,
            WriteOptions {
                stats: Some(&mut stats),
                ..Default::default()
            },
        );
        assert!(flag_texts(&data).join("\n").contains("quadOutput=true"));
        let mod_ = parse_fur(&data).expect("parse");
        let c352_ch = (mod_.n_ch - 32)..mod_.n_ch;
        let mut fx: BTreeSet<u8> = BTreeSet::new();
        for ((ch, _idx), rows) in mod_.patterns.iter() {
            if c352_ch.contains(&(*ch as usize)) {
                for r in rows {
                    for (c, _v) in &r.fx {
                        fx.insert(*c);
                    }
                }
            }
        }
        for c in [0x81u8, 0x82, 0x89, 0x8A] {
            assert!(fx.contains(&c));
        }
        // the folded 4-bit pan never appears on a quad channel
        assert!(!fx.contains(&0x08));
    }

    #[test]
    fn test_quad_records_mid_note_rear_changes() {
        let mut rom = vec![0u8; 0x300];
        rom[0x100..0x1C8].copy_from_slice(&c352_body(200));
        let mut writes = oneshot_writes(50000, &[]);
        writes.push(c352_strobe(60000, 0, 0));
        writes.extend(c352_regs(60100, 0, 0, &[(1, 0x0020)]));
        let song = c352_quad_song(writes, rom);
        let ev = song.events.iter().find(|e| e.on && e.pcm).expect("event");
        assert_eq!(ev.quad_pts, vec![(60100, 0x40, 0x00, 0x00, 0x20)]);
    }

    #[test]
    fn test_loop_includes_the_end_byte() {
        let mut rom = vec![0u8; 0x300];
        rom[0x100..0x1C8].copy_from_slice(&c352_body(200));
        rom[0x1C7] = 0x5A;
        let mut writes = _lattice();
        writes.extend(c352_regs(
            60000,
            0,
            0,
            &[
                (0, 0x4000),
                (1, 0),
                (2, 0x8000),
                (3, 0x4002),
                (4, 0),
                (5, 0x100),
                (6, 0x1C7),
                (7, 0x120),
            ],
        ));
        writes.push(c352_strobe(60000, 0, 0));
        let song = c352_song(writes, rom.clone(), 0, None, VgmKw::default());
        let smp = &song.c352_samples[0];
        assert_eq!(smp.length(), 200);
        assert_eq!(smp.data[smp.data.len() - 1], 0x5A);
        assert_eq!(smp.data, rom[0x100..0x1C8].to_vec());
        assert_eq!(
            (smp.loop_start, smp.loop_end, smp.loop_mode),
            (Some(0x20), Some(200), 0)
        );
    }

    #[test]
    fn test_key_off_write_stops_at_the_write() {
        let mut rom = vec![0u8; 0x300];
        rom[0x100..0x1C8].copy_from_slice(&c352_body(200));
        let mut writes = _lattice();
        writes.extend(c352_regs(
            50000,
            0,
            0,
            &[
                (0, 0x4000),
                (1, 0),
                (2, 0x8000),
                (3, 0x4002),
                (4, 0),
                (5, 0x100),
                (6, 0x1C7),
                (7, 0x120),
            ],
        ));
        writes.push(c352_strobe(60000, 0, 0));
        // the KEYOFF write clears BUSY: the chip is silent from the write
        writes.extend(c352_regs(80000, 0, 0, &[(3, 0x2002)]));
        writes.push(c352_strobe(90000, 0, 0));
        let song = c352_song(writes, rom, 0, None, VgmKw::default());
        let offs: Vec<_> = song.events.iter().filter(|e| !e.on && e.ch == 8).collect();
        assert_eq!(offs.iter().map(|e| e.sample).collect::<Vec<_>>(), vec![80000]);
    }

    #[test]
    fn test_mid_note_keyoff_write_stops_at_the_write() {
        // The driver writes flags 0x2000 mid-note. The write clears BUSY, so
        // libvgm is silent from the write on (no run to wave_end); the later
        // strobe finds it already stopped. One note-off, at the write.
        let mut rom = vec![0u8; 0x300];
        rom[0x100..0x1C8].copy_from_slice(&c352_body(200));
        let mut writes = _lattice();
        writes.extend(c352_regs(
            50000,
            0,
            0,
            &[
                (0, 0x4000),
                (1, 0),
                (2, 0x8000),
                (3, 0x4002),
                (4, 0),
                (5, 0x100),
                (6, 0x1C7),
                (7, 0x120),
            ],
        ));
        writes.push(c352_strobe(60000, 0, 0));
        writes.extend(c352_regs(80000, 0, 0, &[(3, 0x2000)]));
        writes.push(c352_strobe(90000, 0, 0));
        let song = c352_song(writes, rom, 0, None, VgmKw::default());
        let offs: Vec<_> = song.events.iter().filter(|e| !e.on && e.ch == 8).collect();
        assert_eq!(offs.len(), 1);
        assert!((offs[0].sample - 80000).abs() < 4);
    }

    #[test]
    fn test_retrigger_does_not_emit_a_note_off() {
        let mut rom = vec![0u8; 0x300];
        rom[0x100..0x1C8].copy_from_slice(&c352_body(200));
        let mut writes = _lattice();
        writes.extend(oneshot_writes(50000, &[]));
        writes.push(c352_strobe(60000, 0, 0));
        writes.extend(oneshot_writes(80000, &[]));
        writes.push(c352_strobe(90000, 0, 0));
        let song = c352_song(writes, rom, 0, None, VgmKw::default());
        let smp = &song.c352_samples[0];
        assert_eq!(smp.hits.len(), 2);
        assert_eq!(smp.hits[0].off_sample, 90000);
        assert_eq!(smp.hits[1].off_sample, 0);
        assert_eq!(
            song.events.iter().filter(|e| !e.on && e.pcm).count(),
            0
        );
    }

    #[test]
    fn test_mute_rear_drops_the_rear_pair() {
        let mut rom = vec![0u8; 0x300];
        rom[0x100..0x1C8].copy_from_slice(&c352_body(200));
        let mut writes = oneshot_writes(60000, &[]);
        writes.push(c352_strobe(60000, 0, 0));
        let song = c352_song(
            writes,
            rom,
            0,
            None,
            VgmKw {
                c352_mute_rear: true,
                ..Default::default()
            },
        );
        let hit = &song.c352_samples[0].hits[0];
        assert_eq!((hit.vol_l, hit.vol_r), (0x40, 0));
        let ev = song.events.iter().find(|e| e.on && e.pcm).expect("event");
        assert_eq!(ev.vol, 0x40);
        assert_eq!(ev.pan, 0xF0);
    }

    #[test]
    fn test_mulaw_is_the_c352_table() {
        let mut rom = vec![0u8; 0x120];
        rom[0x100] = 0x01;
        rom[0x101] = 0x81;
        for i in 0x102..0x110 {
            rom[i] = 0x02;
        }
        let mut writes = _lattice();
        writes.extend(c352_regs(
            60000,
            0,
            0,
            &[
                (0, 0x4000),
                (1, 0),
                (2, 0x8000),
                (3, 0x4008),
                (4, 0),
                (5, 0x100),
                (6, 0x110),
                (7, 0),
            ],
        ));
        writes.push(c352_strobe(60000, 0, 0));
        let song = c352_song(writes, rom, 0, None, VgmKw::default());
        let smp = &song.c352_samples[0];
        assert_eq!(smp.depth, 16);
        assert_eq!(smp.length(), 16);
        assert_eq!(le_i16(&smp.data, 0), 32);
        assert_eq!(le_i16(&smp.data, 2), -64);
        assert_eq!(mulaw_i16(1), 256);
    }

    #[test]
    fn test_reverse_plays_down_to_the_end() {
        let mut rom = vec![0u8; 0x300];
        for i in 0x101..0x1C9 {
            rom[i] = ((i * 3 + 1) & 0xFF) as u8;
        }
        let mut writes = _lattice();
        writes.extend(c352_regs(
            60000,
            0,
            0,
            &[
                (0, 0x4000),
                (1, 0),
                (2, 0x8000),
                (3, 0x4001),
                (4, 0),
                (5, 0x1C8),
                (6, 0x100),
                (7, 0),
            ],
        ));
        writes.push(c352_strobe(60000, 0, 0));
        let song = c352_song(writes, rom.clone(), 0, None, VgmKw::default());
        let smp = &song.c352_samples[0];
        let heard: Vec<u8> = (0x101..=0x1C8).rev().map(|i| rom[i]).collect();
        assert_eq!(smp.data, heard);
        assert_eq!(smp.length(), 200);
    }

    #[test]
    fn test_reverse_wrap_crosses_the_bank() {
        let n = 0x3D1Cusize;
        let body: Vec<u8> = (0..n).map(|i| ((i * 7 + 5) & 0xFF) as u8).collect();
        let roms = vec![mk_rom(0, 0x1000000, 0x5C2E5, body.clone(), "c352")];
        let mut writes = _lattice();
        writes.extend(c352_regs(
            60000,
            0,
            0,
            &[
                (0, 0x4000),
                (1, 0),
                (2, 0x8000),
                (3, 0x4001),
                (4, 0x0006),
                (5, 0x0000),
                (6, 0xC2E4),
                (7, 0),
            ],
        ));
        writes.push(c352_strobe(60000, 0, 0));
        let song = analyze_pcm(&_vgm(
            writes,
            roms,
            VgmKw {
                c352_clock: 24_192_000,
                ..Default::default()
            },
        ));
        let smp = &song.c352_samples[0];
        assert_eq!(smp.length(), n);
        let mut rev = body.clone();
        rev.reverse();
        assert_eq!(smp.data, rev);
        assert!(!song.warnings.iter().any(|w| w.contains("wrap")));
    }

    #[test]
    fn test_reverse_wrap_at_bank_zero_is_silent() {
        // the walk would leave the ROM window; the reference reads nothing there
        let roms = vec![mk_rom(0, 0x1000000, 0xFFFFFF, vec![0xAA], "c352")];
        let mut writes = _lattice();
        writes.extend(c352_regs(
            60000,
            0,
            0,
            &[
                (0, 0x4000),
                (1, 0),
                (2, 0x8000),
                (3, 0x4001),
                (4, 0x0000),
                (5, 0x0000),
                (6, 0xFFF8),
                (7, 0),
            ],
        ));
        writes.push(c352_strobe(60000, 0, 0));
        let song = analyze_pcm(&_vgm(
            writes,
            roms,
            VgmKw {
                c352_clock: 24_192_000,
                ..Default::default()
            },
        ));
        assert_eq!(song.c352_samples, vec![]);
        assert!(song.warnings.iter().any(|w| w.contains("wrap")));
    }

    #[test]
    fn test_loop_clear_stop_survives_other_strobes() {
        let mut rom = vec![0u8; 0x300];
        rom[0x100..0x1C8].copy_from_slice(&c352_body(200));
        let mut writes = _lattice();
        writes.extend(c352_regs(
            60000,
            0,
            0,
            &[
                (0, 0x4000),
                (1, 0),
                (2, 0x8000),
                (3, 0x4002),
                (4, 0),
                (5, 0x100),
                (6, 0x1C7),
                (7, 0x120),
            ],
        ));
        writes.push(c352_strobe(60000, 0, 0));
        writes.extend(c352_regs(70000, 0, 0, &[(3, 0x0000)]));
        writes.push(c352_strobe(70001, 0, 0));
        let vgm = _vgm(
            writes,
            vec![mk_rom(0, 0x300, 0, rom, "c352")],
            VgmKw {
                c352_clock: 24_192_000,
                ..Default::default()
            },
        );
        let (_rom, _samples, offs, _warnings) = collect_c352(&vgm, false);
        assert!(!offs.is_empty(), "the natural end must still fire");
        assert!(offs.iter().all(|o| o.sample_time >= 70000));
    }

    #[test]
    fn test_flags_clear_of_busy_stops_the_voice() {
        let mut rom = vec![0u8; 0x300];
        rom[0x100..0x1C8].copy_from_slice(&c352_body(200));
        let mut writes = _lattice();
        writes.extend(c352_regs(
            60000,
            0,
            0,
            &[
                (0, 0x4000),
                (1, 0),
                (2, 0x8000),
                (3, 0x4000),
                (4, 0),
                (5, 0x100),
                (6, 0x1C7),
                (7, 0),
            ],
        ));
        writes.push(c352_strobe(60000, 0, 0));
        writes.extend(c352_regs(70000, 0, 0, &[(3, 0x0000)]));
        let vgm = _vgm(
            writes,
            vec![mk_rom(0, 0x300, 0, rom, "c352")],
            VgmKw {
                c352_clock: 24_192_000,
                ..Default::default()
            },
        );
        let (_rom, _samples, offs, _warnings) = collect_c352(&vgm, false);
        assert_eq!(offs.iter().map(|o| o.sample_time).collect::<Vec<_>>(), vec![70000]);
    }

    #[test]
    fn test_busy_clear_before_retrigger_keeps_one_note() {
        let mut rom = vec![0u8; 0x300];
        rom[0x100..0x1C8].copy_from_slice(&c352_body(200));
        let mut writes = _lattice();
        writes.extend(c352_regs(
            60000,
            0,
            0,
            &[
                (0, 0x4000),
                (1, 0),
                (2, 0x8000),
                (3, 0x4002),
                (4, 0),
                (5, 0x100),
                (6, 0x1C7),
                (7, 0x120),
            ],
        ));
        writes.push(c352_strobe(60000, 0, 0));
        writes.extend(c352_regs(70000, 0, 0, &[(3, 0x0000)]));
        writes.extend(c352_regs(70010, 0, 0, &[(3, 0x4002)]));
        writes.push(c352_strobe(70020, 0, 0));
        let vgm = _vgm(
            writes,
            vec![mk_rom(0, 0x300, 0, rom, "c352")],
            VgmKw {
                c352_clock: 24_192_000,
                ..Default::default()
            },
        );
        let (_rom, samples, offs, _warnings) = collect_c352(&vgm, false);
        assert_eq!(offs, vec![]);
        assert_eq!(samples[0].hits.len(), 2);
    }

    #[test]
    fn test_geometry_rewrite_keeps_the_note() {
        // a mid-note geometry rewrite (no strobe) does not start a new note:
        // the module keeps the body the key-on extracted
        let mut rom0 = vec![0u8; 0x200];
        rom0[0x110..0x191].copy_from_slice(&c352_body(0x81));
        let mut writes = _lattice();
        writes.extend(c352_regs(
            60000,
            0,
            0,
            &[
                (0, 0x4000),
                (1, 0),
                (2, 0x8000),
                (3, 0x4022),
                (4, 0x0000),
                (5, 0x0110),
                (6, 0x0190),
                (7, 0x0300),
            ],
        ));
        writes.push(c352_strobe(60000, 0, 0));
        writes.extend(c352_regs(
            70000,
            0,
            0,
            &[(5, 0x0120), (6, 0x01A0), (7, 0x0310)],
        ));
        writes.push(c352_strobe(70100, 0, 0));
        let vgm = _vgm(
            writes,
            vec![mk_rom(0, 0x1000000, 0, rom0, "c352")],
            VgmKw {
                c352_clock: 24_192_000,
                ..Default::default()
            },
        );
        let (_rom, samples, _offs, _warnings) = collect_c352(&vgm, false);
        let hits: Vec<_> = samples
            .iter()
            .flat_map(|s| s.hits.iter())
            .filter(|h| h.voice == 0)
            .collect();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].sample_time, 60000);
    }

    #[test]
    fn test_ping_pong_loop_mode() {
        let mut rom = vec![0u8; 0x300];
        rom[0x120..0x1C8].copy_from_slice(&c352_body(168));
        let mut writes = _lattice();
        writes.extend(c352_regs(
            60000,
            0,
            0,
            &[
                (0, 0x4000),
                (1, 0),
                (2, 0x8000),
                (3, 0x4003),
                (4, 0),
                (5, 0x120),
                (6, 0x1C7),
                (7, 0x120),
            ],
        ));
        writes.push(c352_strobe(60000, 0, 0));
        let song = c352_song(writes, rom.clone(), 0, None, VgmKw::default());
        let smp = &song.c352_samples[0];
        assert_eq!(smp.data, rom[0x120..0x1C8].to_vec());
        assert_eq!(
            (smp.loop_start, smp.loop_end, smp.loop_mode),
            (Some(0), Some(168), 2)
        );
    }

    #[test]
    fn test_ping_pong_start_below_the_region_keeps_the_attack() {
        let mut rom = vec![0u8; 0x300];
        rom[0x100..0x1C8].copy_from_slice(&c352_body(200));
        let mut writes = _lattice();
        writes.extend(c352_regs(
            60000,
            0,
            0,
            &[
                (0, 0x4000),
                (1, 0),
                (2, 0x8000),
                (3, 0x4003),
                (4, 0),
                (5, 0x100),
                (6, 0x1C7),
                (7, 0x140),
            ],
        ));
        writes.push(c352_strobe(60000, 0, 0));
        let song = c352_song(writes, rom.clone(), 0, None, VgmKw::default());
        let smp = &song.c352_samples[0];
        assert_eq!(smp.data, rom[0x100..0x1C8].to_vec());
        assert_eq!(
            (smp.loop_start, smp.loop_end, smp.loop_mode),
            (Some(0x40), Some(200), 2)
        );
    }

    #[test]
    fn test_link_outside_the_dump_keeps_the_intro() {
        let mut rom = vec![0u8; 0x80];
        rom[0x20..0x31].copy_from_slice(&c352_body(17));
        let mut writes = _lattice();
        writes.extend(c352_regs(
            60000,
            0,
            0,
            &[
                (0, 0x4000),
                (1, 0),
                (2, 0x8000),
                (3, 0x4022),
                (4, 0),
                (5, 0x20),
                (6, 0x30),
                (7, 0x10),
            ],
        ));
        writes.push(c352_strobe(60000, 0, 0));
        let song = c352_song(writes, rom.clone(), 0, Some(0x1000000), VgmKw::default());
        assert_eq!(song.c352_samples.len(), 1);
        let smp = &song.c352_samples[0];
        assert_eq!(smp.data, rom[0x20..0x31].to_vec());
        assert_eq!(smp.loop_start, None);
        assert!(song.warnings.iter().any(|w| w.contains("link")));
    }

    #[test]
    fn test_undumped_base_is_not_a_sample() {
        let mut writes = _lattice();
        writes.extend(c352_regs(
            60000,
            0,
            0,
            &[
                (0, 0x4000),
                (1, 0),
                (2, 0x8000),
                (3, 0x4000),
                (4, 0),
                (5, 0),
                (6, 0x20),
                (7, 0),
            ],
        ));
        writes.push(c352_strobe(60000, 0, 0));
        let roms = vec![mk_rom(0, 0x20000, 0x10000, vec![0u8; 0x20], "c352")];
        let song = analyze_pcm(&_vgm(
            writes,
            roms,
            VgmKw {
                c352_clock: 24_192_000,
                ..Default::default()
            },
        ));
        assert_eq!(song.c352_samples, vec![]);
        assert!(song.warnings.iter().any(|w| w.contains("not dump")));
    }

    #[test]
    fn test_high_bank_bits_fold_through_the_rom_window() {
        // libvgm reads wave[pos & pow2_mask(size)], so a driver address with
        // bits above the window aliases its low bits: 0x8001:0x0000 in a 16 MB
        // window is the byte at 0x10000.
        let body = c352_body(200);
        let mut writes = _lattice();
        writes.extend(c352_regs(
            60000,
            0,
            0,
            &[
                (0, 0x4000),
                (1, 0),
                (2, 0x8000),
                (3, 0x4000),
                (4, 0x8001),
                (5, 0x0000),
                (6, body.len() as u32),
                (7, 0),
            ],
        ));
        writes.push(c352_strobe(60000, 0, 0));
        let roms = vec![mk_rom(0, 0x1000000, 0x10000, body.clone(), "c352")];
        let song = analyze_pcm(&_vgm(
            writes,
            roms,
            VgmKw {
                c352_clock: 24_192_000,
                ..Default::default()
            },
        ));
        assert_eq!(song.c352_samples.len(), 1);
        assert_eq!(&song.c352_samples[0].data[..body.len()], &body[..]);
        assert!(!song.warnings.iter().any(|w| w.contains("not dump")));
    }

    #[test]
    fn test_high_bank_bits_that_fold_into_a_hole_stay_silent() {
        // the folded offset must still land in a dumped slice: this one lands
        // past the body, where the reference reads the hole
        let body = c352_body(200);
        let mut writes = _lattice();
        writes.extend(c352_regs(
            60000,
            0,
            0,
            &[
                (0, 0x4000),
                (1, 0),
                (2, 0x8000),
                (3, 0x4000),
                (4, 0x8002),
                (5, 0x0000),
                (6, body.len() as u32),
                (7, 0),
            ],
        ));
        writes.push(c352_strobe(60000, 0, 0));
        let roms = vec![mk_rom(0, 0x1000000, 0x10000, body, "c352")];
        let song = analyze_pcm(&_vgm(
            writes,
            roms,
            VgmKw {
                c352_clock: 24_192_000,
                ..Default::default()
            },
        ));
        assert_eq!(song.c352_samples, vec![]);
        assert!(song.warnings.iter().any(|w| w.contains("not dump")));
    }

    #[test]
    fn test_late_pitch_write_within_a_frame_is_the_note() {
        // these drivers strobe first and write the note's own pitch inside the
        // same 60 Hz frame; the key-on register is the previous note's value,
        // not a slide start
        let mut rom = vec![0u8; 0x300];
        rom[0x100..0x1C8].copy_from_slice(&c352_body(200));
        let mut writes = _lattice();
        writes.extend(oneshot_writes(60000, &[(2, 0x4000)]));
        writes.push(c352_strobe(60000, 0, 0));
        writes.push(ChipWrite::new(60400, "c352", 0, 2, 0x8000));
        let song = c352_song(writes, rom, 0, None, VgmKw::default());
        let ev = song.events.iter().find(|e| e.on && e.pcm).expect("event");
        assert!(ev.sweep.is_empty());
        assert_eq!(song.c352_samples[0].mode_freq(), 0x8000);
    }

    #[test]
    fn test_late_volume_write_within_a_frame_sets_the_attack() {
        // A driver that zeroes the volume when it stops a voice leaves the
        // strobe with 0; the real balance arrives a few ms later, and the note
        // must start at the written balance or the attack plays silent.
        let mut rom = vec![0u8; 0x300];
        rom[0x100..0x1C8].copy_from_slice(&c352_body(200));
        let mut writes = _lattice();
        writes.extend(oneshot_writes(60000, &[(0, 0x0000)]));
        writes.push(c352_strobe(60000, 0, 0));
        writes.push(ChipWrite::new(60400, "c352", 0, 0, 0x8000));
        let song = c352_song(writes, rom, 0, None, VgmKw::default());
        let ev = song.events.iter().find(|e| e.on && e.pcm).expect("event");
        assert_eq!(ev.vol, 0x80);
        assert_eq!(ev.pan, 0xF2);
    }

    #[test]
    fn test_loop_point_one_past_the_end_extends_the_body() {
        // A loop point one past wave_end: the chip jumps to the contiguous
        // byte, plays a full bank there and repeats. The body has to carry
        // that region or the sample plays its intro once.
        let mut rom = vec![0u8; 0x20000];
        let body = c352_body(200);
        for i in 0..rom.len() {
            rom[i] = ((i * 7 + 3) & 0xFF) as u8;
        }
        rom[0x100..0x1C8].copy_from_slice(&body);
        let mut writes = _lattice();
        writes.extend(c352_regs(
            60000,
            0,
            0,
            &[
                (0, 0x4000),
                (1, 0),
                (2, 0x8000),
                (3, 0x4002),
                (4, 0),
                (5, 0x0100),
                (6, 0x01C7),
                (7, 0x01C8),
            ],
        ));
        writes.push(c352_strobe(60000, 0, 0));
        let song = c352_song(writes, rom, 0, Some(0x20000), VgmKw::default());
        assert_eq!(song.c352_samples.len(), 1);
        let smp = &song.c352_samples[0];
        assert_eq!(smp.loop_start, Some(0xC8));
        assert_eq!(smp.data.len(), 0xC8 + 0x10000);
        let expect: Vec<u8> = (0..4)
            .map(|k| ((0x1C8 * 7 + 3 + 7 * k) & 0xFF) as u8)
            .collect();
        assert_eq!(smp.data[0xC8..0xC8 + 4], expect);
        assert_eq!(smp.loop_mode, 0);
    }

    #[test]
    fn test_geometry_rewrite_before_the_crossing_moves_the_loop() {
        // A geometry rewrite shortly after key-on can point the LINK jump at
        // the byte after the intro, the sample's real continuation. The chip
        // reads the live registers at the crossing, so the body has to use
        // the rewrite, not the key-on values.
        let mut rom = vec![0u8; 0x40000];
        for i in 0..rom.len() {
            rom[i] = ((i * 7 + 3) & 0xFF) as u8;
        }
        let mut writes = _lattice();
        writes.extend(c352_regs(
            60000,
            0,
            0,
            &[
                (0, 0x4000),
                (1, 0),
                (2, 0x8000),
                (3, 0x4022),
                (4, 0),
                (5, 0x0100),
                (6, 0x01C7),
                (7, 0x2000),
            ],
        ));
        writes.push(c352_strobe(60000, 0, 0));
        writes.push(ChipWrite::new(60100, "c352", 0, 7, 0x01C8)); // loop one past the end
        writes.push(ChipWrite::new(60100, "c352", 0, 5, 0x0000)); // start as the target's high half
        let song = c352_song(writes, rom, 0, Some(0x40000), VgmKw::default());
        assert_eq!(song.c352_samples.len(), 1);
        let smp = &song.c352_samples[0];
        assert_eq!(smp.loop_start, Some(0xC8));
        assert_eq!(smp.data.len(), 0xC8 + 0x10000);
        let expect: Vec<u8> = (0..4)
            .map(|k| ((0x1C8 * 7 + 3 + 7 * k) & 0xFF) as u8)
            .collect();
        assert_eq!(smp.data[0xC8..0xC8 + 4], expect);
    }

    #[test]
    fn test_geometry_rewrite_after_the_crossing_is_ignored() {
        // a write that lands after the voice already crossed wave_end is for
        // the next pass and must not move the body
        let mut rom = vec![0u8; 0x40000];
        for i in 0..rom.len() {
            rom[i] = ((i * 5 + 1) & 0xFF) as u8;
        }
        let mut writes = _lattice();
        writes.extend(c352_regs(
            60000,
            0,
            0,
            &[
                (0, 0x4000),
                (1, 0),
                (2, 0x8000),
                (3, 0x4002),
                (4, 0),
                (5, 0x0100),
                (6, 0x01C7),
                (7, 0x0101),
            ],
        ));
        writes.push(c352_strobe(60000, 0, 0));
        writes.push(ChipWrite::new(60700, "c352", 0, 7, 0x01C8)); // well past the crossing
        writes.push(ChipWrite::new(60700, "c352", 0, 5, 0x0000));
        let song = c352_song(writes, rom, 0, Some(0x40000), VgmKw::default());
        assert_eq!(song.c352_samples.len(), 1);
        let smp = &song.c352_samples[0];
        assert_eq!(smp.loop_start, Some(0x01));
    }

    #[test]
    fn test_next_note_preload_does_not_move_the_loop() {
        // the driver preloads the next note's geometry on this voice before
        // its strobe. The end write must not re-arm the crossing guard: the
        // next end is below the key-on start, so the old live-end span wrapped
        // and the write after the crossing was taken for a mid-note rewrite.
        let mut rom = vec![0u8; 0x40000];
        for i in 0..rom.len() {
            rom[i] = ((i * 5 + 1) & 0xFF) as u8;
        }
        let mut writes = _lattice();
        writes.extend(c352_regs(
            60000,
            0,
            0,
            &[
                (0, 0x4000),
                (1, 0),
                (2, 0x8000),
                (3, 0x4002),
                (4, 0),
                (5, 0x0300),
                (6, 0x03C7),
                (7, 0x0320),
            ],
        ));
        writes.push(c352_strobe(60000, 0, 0));
        writes.extend(c352_regs(
            60700,
            0,
            0,
            &[(5, 0x0100), (6, 0x01C7), (7, 0x0100)],
        ));
        writes.extend(c352_regs(60750, 0, 0, &[(3, 0x0000)]));
        writes.extend(c352_regs(60760, 0, 0, &[(3, 0x4002)]));
        writes.push(c352_strobe(60800, 0, 0));
        let song = c352_song(writes, rom, 0, Some(0x40000), VgmKw::default());
        let smp = song
            .c352_samples
            .iter()
            .find(|s| s.start == 0x300)
            .expect("sample 0x300");
        assert_eq!((smp.loop_start, smp.loop_end), (Some(0x20), Some(0xC8)));
    }

    #[test]
    fn test_stopped_note_that_reaches_the_loop_carries_it() {
        // the reach test works in ROM frames, not VGM samples: at this
        // frequency 200 frames fit inside 150 samples
        let mut rom = vec![0u8; 0x20000];
        for i in 0..rom.len() {
            rom[i] = ((i * 7 + 3) & 0xFF) as u8;
        }
        let mut writes = _lattice();
        writes.extend(c352_regs(
            60000,
            0,
            0,
            &[
                (0, 0x4000),
                (1, 0),
                (2, 0xFFFF),
                (3, 0x4002),
                (4, 0),
                (5, 0x0100),
                (6, 0x01C7),
                (7, 0x01C8),
            ],
        ));
        writes.push(c352_strobe(60000, 0, 0));
        writes.extend(c352_regs(60150, 0, 0, &[(3, 0x0000)]));
        writes.push(c352_strobe(60150, 0, 0));
        let song = c352_song(writes, rom, 0, Some(0x20000), VgmKw::default());
        let smp = &song.c352_samples[0];
        assert_eq!(
            (smp.loop_start, smp.length()),
            (Some(0xC8), 0xC8 + 0x10000)
        );
    }

    #[test]
    fn test_stopped_note_does_not_carry_the_loop_region() {
        // a note the driver stops before the running address reaches wave_end
        // never loops, so the payload stays the intro - the big loop bodies
        // are only built for notes that actually reach them
        let mut rom = vec![0u8; 0x20000];
        let body = c352_body(200);
        for i in 0..rom.len() {
            rom[i] = ((i * 7 + 3) & 0xFF) as u8;
        }
        rom[0x100..0x1C8].copy_from_slice(&body);
        let mut writes = _lattice();
        writes.extend(c352_regs(
            60000,
            0,
            0,
            &[
                (0, 0x4000),
                (1, 0),
                (2, 0x8000),
                (3, 0x4002),
                (4, 0),
                (5, 0x0100),
                (6, 0x01C7),
                (7, 0x01C8),
            ],
        ));
        writes.push(c352_strobe(60000, 0, 0));
        writes.push(ChipWrite::new(60050, "c352", 0, 3, 0x0000)); // stopped 50 samples in
        writes.push(c352_strobe(60050, 0, 0));
        let song = c352_song(writes, rom, 0, Some(0x20000), VgmKw::default());
        assert_eq!(song.c352_samples.len(), 1);
        let smp = &song.c352_samples[0];
        assert_eq!(smp.loop_start, None);
        assert_eq!(smp.length(), 0xC8);
    }

    #[test]
    fn test_freq_slide_and_post_key_refit() {
        let mut rom = vec![0u8; 0x300];
        rom[0x100..0x1C8].copy_from_slice(&c352_body(200));
        let mut writes = _lattice();
        writes.extend(oneshot_writes(60000, &[]));
        writes.push(c352_strobe(60000, 0, 0));
        writes.push(ChipWrite::new(80000, "c352", 0, 2, 0x4000));
        let song = c352_song(writes, rom.clone(), 0, None, VgmKw::default());
        let ev = song.events.iter().find(|e| e.on && e.pcm).expect("event");
        assert_eq!(ev.sweep, vec![(80000, -768)]);

        let mut writes = _lattice();
        writes.extend(oneshot_writes(60000, &[(2, 0)]));
        writes.push(c352_strobe(60000, 0, 0));
        writes.push(ChipWrite::new(60050, "c352", 0, 2, 0x8000));
        let song = c352_song(writes, rom, 0, None, VgmKw::default());
        let smp = &song.c352_samples[0];
        let ev = song.events.iter().find(|e| e.on && e.pcm).expect("event");
        assert_eq!(smp.mode_freq(), 0x8000);
        assert_eq!((ev.note, ev.e5), (108, 0x80));
        assert!(ev.sweep.is_empty());
    }

    #[test]
    fn test_fur_chip_instrument_and_volume() {
        let mut rom = vec![0u8; 0x300];
        rom[0x100..0x1C8].copy_from_slice(&c352_body(200));
        let mut writes = oneshot_writes(60000, &[]);
        writes.push(c352_strobe(60000, 0, 0));
        let song = c352_song(writes, rom, 0, None, VgmKw::default());
        let data = write_fur(&song, WriteOptions::default());
        let mod_ = parse_fur(&data).expect("parse");
        assert_eq!(mod_.chips, vec![0x82, CHIP_C352]);
        assert_eq!(mod_.n_ch, 8 + 32);
        assert!(ins_types(&data).contains(&INS_C352));
        let smps = list_fur_samples(&data).expect("samples");
        assert_eq!(smps[0].n_frames, 200);
        assert_eq!(smps[0].depth, 8);
        assert_eq!(smps[0].c4_rate, 42000);
        assert_eq!(smps[0].loop_mode, 0);
        assert!(contains(&data, &packed_f32(0.25)));
        assert!(contains(&data, b"customClock=24192000"));
        assert!(contains(&data, b"Furnace 0.6.8.3"));
        assert!(!song.warnings.iter().any(|w| w.contains("Furnace")));

        let empty = _vgm(vec![], vec![], VgmKw::default());
        assert_almost_eq!(_player_chip_volume(&empty, 0x27, 1), 0.25);
        assert_almost_eq!(_player_chip_volume(&empty, 0x27, 2), 0.125);
    }

    #[test]
    fn test_dual_chip_channels() {
        let mut rom = vec![0u8; 0x300];
        rom[0x100..0x1C8].copy_from_slice(&c352_body(200));
        let mut writes = _lattice();
        writes.extend(c352_regs(
            60000,
            0,
            0,
            &[
                (0, 0x4000),
                (1, 0),
                (2, 0x8000),
                (3, 0x4000),
                (4, 0),
                (5, 0x100),
                (6, 0x1C8),
                (7, 0),
            ],
        ));
        writes.push(c352_strobe(60000, 0, 0));
        writes.extend(c352_regs(
            60000,
            0,
            1,
            &[
                (0, 0x4000),
                (1, 0),
                (2, 0x8000),
                (3, 0x4000),
                (4, 0),
                (5, 0x100),
                (6, 0x1C8),
                (7, 0),
            ],
        ));
        writes.push(c352_strobe(60000, 1, 0));
        let roms = vec![
            mk_rom(0, rom.len() as i64, 0, rom.clone(), "c352"),
            mk_rom(1, rom.len() as i64, 0, rom, "c352"),
        ];
        let song = analyze_pcm(&_vgm(
            writes,
            roms,
            VgmKw {
                c352_clock: 24_192_000,
                ..Default::default()
            },
        ));
        let ons: BTreeSet<i32> = song
            .events
            .iter()
            .filter(|e| e.on && e.pcm)
            .map(|e| e.ch)
            .collect();
        assert_eq!(ons.into_iter().collect::<Vec<_>>(), vec![8, 40]);
        let data = write_fur(&song, WriteOptions::default());
        let mod_ = parse_fur(&data).expect("parse");
        assert_eq!(mod_.chips, vec![0x82, CHIP_C352, CHIP_C352]);
        assert_eq!(mod_.n_ch, 8 + 64);
        assert!(contains(&data, &packed_f32(0.125)));
    }

    #[test]
    fn test_ping_pong_fur_loop_mode() {
        let mut rom = vec![0u8; 0x300];
        rom[0x120..0x1C8].copy_from_slice(&c352_body(168));
        let mut writes = _lattice();
        writes.extend(c352_regs(
            60000,
            0,
            0,
            &[
                (0, 0x4000),
                (1, 0),
                (2, 0x8000),
                (3, 0x4003),
                (4, 0),
                (5, 0x120),
                (6, 0x1C7),
                (7, 0x120),
            ],
        ));
        writes.push(c352_strobe(60000, 0, 0));
        let song = c352_song(writes, rom, 0, None, VgmKw::default());
        let data = write_fur(&song, WriteOptions::default());
        let smp = &list_fur_samples(&data).expect("samples")[0];
        assert_eq!(smp.loop_mode, 2);
        assert_eq!((smp.loop_start, smp.loop_end), (0, 168));
    }
}
