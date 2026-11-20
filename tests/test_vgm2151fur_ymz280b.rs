//! Yamaha YMZ280B (PCMD8): parsing, samples, loops and .fur emission.
//!
//! Port of `tests/test_vgm2151fur_ymz280b.py`.

mod common;
use common::*;

use std::collections::HashSet;
use std::path::PathBuf;

use vgm2fur::analyze::{analyze, AnalyzeOptions};
use vgm2fur::edit::read_chips;
use vgm2fur::furio::{iter_blocks, master_volume, parse_fur};
use vgm2fur::furwrite::{
    decompress_fur, ymz280b_flag_text, write_fur, WriteOptions, CHIP_OPL2, CHIP_YMZ280B,
    INS_YMZ280B, SMP_DEPTH_16BIT, SMP_DEPTH_8BIT, SMP_DEPTH_YMZ_ADPCM,
};
use vgm2fur::opl::{OPLState, N as OPL_N, SLOT_ADDR};
use vgm2fur::parity::{py_round, py_round_to};
use vgm2fur::vgm::{
    chips_in_head, load_vgm, ChipWrite, RomSlice, VgmFile,
};
use vgm2fur::ymz280b::{
    collect_ymz280b, ymz280b_rate, ymz_pan_fx, ymz_pan_ratio, YMZ_DEPTH_ADPCM, YMZ_DEPTH_PCM8,
    YMZ_DEPTH_PCM16,
};

const CLOCK: u32 = 16_934_400;

fn tmp_dir() -> PathBuf {
    let p = std::env::temp_dir().join("vgm2151fur_ymz280b");
    std::fs::create_dir_all(&p).expect("temp dir");
    p
}

fn _vgm(writes: Vec<ChipWrite>, roms: Vec<RomSlice>) -> VgmFile {
    let mut v = VgmFile::default();
    v.path = PathBuf::from("synthetic.vgm");
    v.buf = Vec::new();
    v.version = 0x154;
    v.total_samples = 400_000;
    v.loop_samples = 0;
    v.loop_file_pos = None;
    v.loop_sample = None;
    v.ym2151_clock = 3_579_545;
    v.k007232_clock = 0;
    v.gd3 = Default::default();
    v.roms = roms;
    v.writes = writes;
    v.ymz280b_clock = CLOCK;
    v
}

fn _w(t: i64, reg: u32, val: u32, chip: u8) -> ChipWrite {
    ChipWrite::new(t, "ymz280b", chip, reg, val)
}

fn _addr(t: i64, reg_hi: u32, reg_mid: u32, reg_lo: u32, value: u32, chip: u8) -> Vec<ChipWrite> {
    vec![
        _w(t, reg_hi, (value >> 16) & 0xFF, chip),
        _w(t, reg_mid, (value >> 8) & 0xFF, chip),
        _w(t, reg_lo, value & 0xFF, chip),
    ]
}

fn _enable(t: i64, chip: u8) -> ChipWrite {
    _w(t, 0xFF, 0x80, chip)
}

#[allow(clippy::too_many_arguments)]
fn _note(
    t: i64,
    start: u32,
    stop: u32,
    fnum: u32,
    vol: u32,
    pan: u32,
    mode: u32,
    loop_on: bool,
    loop_start: u32,
    loop_end: u32,
    chip: u8,
    off: Option<i64>,
) -> Vec<ChipWrite> {
    let base = 0u32;
    let mut out = _addr(t, 0x20 + base, 0x40 + base, 0x60 + base, start, chip);
    out.extend(_addr(t, 0x23 + base, 0x43 + base, 0x63 + base, stop, chip));
    if loop_on {
        out.extend(_addr(
            t,
            0x21 + base,
            0x41 + base,
            0x61 + base,
            loop_start,
            chip,
        ));
        out.extend(_addr(
            t,
            0x22 + base,
            0x42 + base,
            0x62 + base,
            loop_end,
            chip,
        ));
    }
    out.extend([
        _w(t, base + 0x02, vol, chip),
        _w(t, base + 0x03, pan, chip),
        _w(t, base + 0x00, fnum & 0xFF, chip),
        _w(
            t,
            base + 0x01,
            0x80 | (if loop_on { 0x10 } else { 0 }) | (mode & 0x60) | ((fnum >> 8) & 1),
            chip,
        ),
    ]);
    if let Some(off) = off {
        out.push(_w(off, base + 0x01, 0, chip));
    }
    out
}

/// One OPL2 voice, used to mix an FM melody beside the YMZ samples.
fn _opl2_note(t: i64, ch: usize, block: u32, fnum: u32, off: Option<i64>) -> Vec<ChipWrite> {
    fn ow(when: i64, reg: u32, val: u32) -> ChipWrite {
        ChipWrite::new(when, "opl2", 0, reg, val)
    }

    let st = OPLState::new("opl2", false);
    let mut out = Vec::new();
    for row in 0..st.slots.len() {
        let slot = st.slots[row][ch];
        if slot == OPL_N {
            continue;
        }
        let addr = SLOT_ADDR[slot as usize] as u32;
        out.extend([
            ow(t, addr + 0x20, 0x01),
            ow(t, addr + 0x40, if row == 0 { 0x30 } else { 0x10 }),
            ow(t, addr + 0x60, 0xF5),
            ow(t, addr + 0x80, 0x94),
            ow(t, addr + 0xE0, row as u32),
        ]);
    }
    let cm = st.chan_map[ch] as u32;
    let hi = (block << 2) | ((fnum >> 8) & 3);
    out.extend([
        ow(t + 1, cm + 0xA0, fnum & 0xFF),
        ow(t + 2, cm + 0xB0, hi | 0x20),
        ow(t + 2, cm + 0xC0, 0x30),
    ]);
    if let Some(off) = off {
        out.push(ow(off, cm + 0xB0, hi));
    }
    out
}

fn _body(n: usize) -> Vec<u8> {
    (0..n).map(|i| ((i * 37 + 11) & 0xFF) as u8).collect()
}

fn _rom(chip: u8, data: &[u8]) -> RomSlice {
    RomSlice {
        chip,
        rom_size: data.len() as i64,
        start: 0,
        data: data.to_vec(),
        kind: "ymz280b",
        time: 0,
    }
}

/// A VGM 1.54 file with YMZ header clock and 0x86 ROM blocks.
///
/// `rom_blocks` is (chip_id, rom_size, start, data). The data block size
/// carries the chip id in bit 31, exactly as real dual-chip logs do.
fn _raw_vgm(
    cmds: &[u8],
    rom_blocks: &[(u8, u32, u32, Vec<u8>)],
    clock: u32,
    dual: bool,
) -> Vec<u8> {
    let mut buf = vec![0u8; 0xC0];
    buf[0..4].copy_from_slice(b"Vgm ");
    buf[8..12].copy_from_slice(&0x154u32.to_le_bytes());
    buf[0x18..0x1C].copy_from_slice(&44100u32.to_le_bytes());
    buf[0x34..0x38].copy_from_slice(&(0xC0u32 - 0x34).to_le_bytes());
    let raw = clock | (if dual { 0x4000_0000 } else { 0 });
    buf[0x68..0x6C].copy_from_slice(&raw.to_le_bytes());
    let mut body: Vec<u8> = Vec::new();
    for (chip_id, rom_size, start, data) in rom_blocks {
        let mut payload: Vec<u8> = Vec::new();
        payload.extend_from_slice(&rom_size.to_le_bytes());
        payload.extend_from_slice(&start.to_le_bytes());
        payload.extend_from_slice(data);
        let size = (payload.len() as u32) | (((chip_id & 1) as u32) << 31);
        body.extend_from_slice(&[0x67, 0x66, 0x86]);
        body.extend_from_slice(&size.to_le_bytes());
        body.extend_from_slice(&payload);
    }
    body.extend_from_slice(cmds);
    body.push(0x66);
    buf.extend_from_slice(&body);
    buf
}

mod TestYmz280bParsing {
    use super::*;

    #[test]
    fn test_header_dual_command_and_rom() {
        let raw = _raw_vgm(
            b"\x5d\xff\xd0\x5d\x23\x00\x5d\x43\x00\x5d\x63\x20\
              \x5d\x00\x10\x5d\x01\xc0\
              \xad\xff\xd0\xad\x23\x00\xad\x43\x01\xad\x63\x00\
              \xad\x00\x20\xad\x01\xc0",
            &[
                (0, 0x200, 0, (0u8..0x20).collect()),
                (1, 0x100, 0, (0u8..0x10).collect()),
            ],
            CLOCK,
            true,
        );
        let p = tmp_dir().join("parse.vgm");
        std::fs::write(&p, &raw).expect("write vgm");
        let v = load_vgm(&p).expect("load");
        assert_eq!(v.ymz280b_clock, CLOCK);
        assert!(v.ymz280b_dual);
        assert_eq!(v.ymz280b_clock_2, 0);
        let writes: Vec<&ChipWrite> = v.writes.iter().filter(|w| w.chip == "ymz280b").collect();
        assert!(writes.iter().any(|w| w.chip_id == 0));
        assert!(writes.iter().any(|w| w.chip_id == 1));
        let roms: Vec<&RomSlice> = v.roms.iter().filter(|r| r.kind == "ymz280b").collect();
        let chips: HashSet<u8> = roms.iter().map(|r| r.chip).collect();
        assert_eq!(chips, HashSet::from([0u8, 1u8]));
        let head = chips_in_head(&raw);
        assert!(head.contains("ymz280b"));
        assert!(head.contains("ymz280b_dual"));
    }

    #[test]
    fn test_single_chip_data_block() {
        let raw = _raw_vgm(
            b"\x5d\x23\x00\x5d\x43\x00\x5d\x63\x20\
              \x5d\x00\x10\x5d\x01\xc0",
            &[(0, 0x200, 0, (0u8..0x20).collect())],
            CLOCK,
            false,
        );
        let p = tmp_dir().join("single.vgm");
        std::fs::write(&p, &raw).expect("write vgm");
        let v = load_vgm(&p).expect("load");
        assert!(!v.ymz280b_dual);
        assert_eq!(
            v.roms.iter().map(|r| (r.kind, r.chip)).collect::<Vec<_>>(),
            vec![("ymz280b", 0u8)]
        );
    }
}

mod TestYmz280bNotes {
    use super::*;

    #[test]
    fn test_pcm8_note_rate_volume_and_pan() {
        let rom = _body(256);
        let mut writes = vec![_enable(0, 0)];
        writes.extend(_note(
            1000,
            0,
            200,
            0x10,
            200,
            15,
            0x40,
            false,
            0,
            0,
            0,
            Some(5000),
        ));
        let (samples, offs, warn) = collect_ymz280b(&_vgm(writes.clone(), vec![_rom(0, &rom)]));
        assert!(warn.is_empty());
        assert_eq!(samples.len(), 1);
        let smp = &samples[0];
        assert_eq!(smp.depth, YMZ_DEPTH_PCM8);
        assert_eq!(smp.data, rom[..200].to_vec());
        assert_eq!(smp.mode_fnum(), 0x10);
        assert_eq!(
            smp.c4_rate(),
            py_round(ymz280b_rate(CLOCK, 0x10, YMZ_DEPTH_PCM8)) as i64
        );
        let hit = &smp.hits[0];
        assert_eq!(hit.vol, 200);
        assert_eq!(hit.pan, 15);
        assert_eq!(hit.off_sample, 5000);
        assert_eq!(offs.iter().map(|o| o.sample_time).collect::<Vec<_>>(), vec![5000]);

        let song = analyze(&_vgm(writes, vec![_rom(0, &rom)]), AnalyzeOptions::default()).unwrap();
        assert_eq!(song.ymz280b_count, 1);
        let ev = song.events.iter().find(|e| e.on && e.pcm).expect("pcm event");
        assert_eq!(ev.vol, 200);
        assert_eq!(ev.pan, ymz_pan_fx(15));
        assert_eq!(ev.note, 108);
        let natural =
            1000 + py_round(200.0 / ymz280b_rate(CLOCK, 0x10, YMZ_DEPTH_PCM8) * 44100.0) as i64;
        assert_eq!(ev.end, i64::min(5000, natural));
    }

    #[test]
    fn test_16bit_rom_is_byteswapped() {
        // Two big-endian frames: 0x1234 and 0xF00F.
        let rom = vec![0x12u8, 0x34, 0xF0, 0x0F];
        let mut writes = vec![_enable(0, 0)];
        writes.extend(_note(
            1000, 0, 4, 0x20, 200, 8, 0x60, false, 0, 0, 0, Some(9000),
        ));
        let (samples, _offs, warn) = collect_ymz280b(&_vgm(writes, vec![_rom(0, &rom)]));
        assert!(warn.is_empty());
        let smp = &samples[0];
        assert_eq!(smp.depth, YMZ_DEPTH_PCM16);
        assert_eq!(smp.data, vec![0x34u8, 0x12, 0x0F, 0xF0]);
        assert_eq!(smp.length(), 2);
        assert_eq!(
            smp.c4_rate(),
            py_round(ymz280b_rate(CLOCK, 0x20, YMZ_DEPTH_PCM16)) as i64
        );
    }

    #[test]
    fn test_adpcm_body_and_loop_points() {
        let rom = vec![0x12u8, 0x34, 0x56, 0x78];
        let mut writes = vec![_enable(0, 0)];
        writes.extend(_note(
            1000, 0, 4, 0x30, 200, 8, 0x20, true, 2, 4, 0, Some(9000),
        ));
        let (samples, _offs, warn) = collect_ymz280b(&_vgm(writes, vec![_rom(0, &rom)]));
        assert!(warn.is_empty());
        let smp = &samples[0];
        assert_eq!(smp.depth, YMZ_DEPTH_ADPCM);
        assert_eq!(smp.data, rom);
        assert_eq!(smp.length(), 8); // nibbles
        assert_eq!(smp.loop_start, Some(4)); // nibbles
        assert_eq!(smp.loop_end, Some(8));
        assert_eq!(
            smp.c4_rate(),
            py_round(ymz280b_rate(CLOCK, 0x30, YMZ_DEPTH_ADPCM)) as i64
        );
    }

    #[test]
    fn test_pcm8_loop_points() {
        let rom = _body(256);
        let mut writes = vec![_enable(0, 0)];
        writes.extend(_note(
            1000, 0, 100, 0x40, 200, 8, 0x40, true, 10, 60, 0, Some(9000),
        ));
        let (samples, _offs, warn) = collect_ymz280b(&_vgm(writes, vec![_rom(0, &rom)]));
        assert!(warn.is_empty());
        assert_eq!(samples[0].loop_start, Some(10));
        assert_eq!(samples[0].loop_end, Some(60));
    }

    #[test]
    fn test_same_sample_setup_after_trigger_is_folded() {
        let rom = _body(256);
        let writes = vec![
            _enable(0, 0),
            _w(1000, 0x23, 0, 0),
            _w(1000, 0x43, 0, 0),
            _w(1000, 0x63, 100, 0),
            _w(1000, 0x00, 0x55, 0),
            _w(1000, 0x01, 0xC0, 0), // trigger first, still old vol/pan
            _w(1000, 0x02, 0x99, 0), // volume written after the trigger
            _w(1000, 0x03, 0x02, 0), // pan written after the trigger
            _w(1000, 0x01, 0xC1, 0), // frequency high after the trigger
        ];
        let (samples, _offs, _warn) = collect_ymz280b(&_vgm(writes, vec![_rom(0, &rom)]));
        let hit = &samples[0].hits[0];
        assert_eq!(hit.vol, 0x99);
        assert_eq!(hit.pan, 0x02);
        assert_eq!(hit.fnum, 0x155);
    }

    #[test]
    fn test_repeated_keyon_does_not_retrigger() {
        let rom = _body(256);
        let mut writes = vec![_enable(0, 0)];
        writes.extend(_note(
            1000, 0, 200, 0x10, 200, 8, 0x40, false, 0, 0, 0, None,
        ));
        writes.extend([_w(2000, 0x01, 0xC0, 0), _w(3000, 0x01, 0xC0, 0)]);
        let (samples, offs, _warn) = collect_ymz280b(&_vgm(writes, vec![_rom(0, &rom)]));
        assert_eq!(samples.len(), 1);
        assert_eq!(samples[0].hits.len(), 1);
        assert!(offs.is_empty());
    }

    #[test]
    fn test_keyon_before_enable_is_reported() {
        let rom = _body(256);
        let writes = _note(
            1000, 0, 200, 0x10, 200, 8, 0x40, false, 0, 0, 0, None,
        );
        let (samples, _offs, warn) = collect_ymz280b(&_vgm(writes, vec![_rom(0, &rom)]));
        assert!(samples.is_empty());
        assert!(warn.iter().any(|w| w.contains("0xFF bit 7")));
    }

    #[test]
    fn test_keyon_enable_is_per_chip() {
        let rom0 = _body(128);
        let rom1: Vec<u8> = (0..128usize).map(|i| ((i * 13 + 7) & 0xFF) as u8).collect();
        let mut writes = vec![_enable(0, 1)];
        writes.extend(_note(
            1000, 0, 100, 0x10, 200, 8, 0x40, false, 0, 0, 0, None,
        ));
        writes.extend(_note(
            1200, 0, 100, 0x10, 200, 8, 0x40, false, 0, 0, 1, None,
        ));
        let (samples, _offs, warn) =
            collect_ymz280b(&_vgm(writes, vec![_rom(0, &rom0), _rom(1, &rom1)]));
        let chips: HashSet<i32> = samples.iter().map(|s| s.chip_id).collect();
        assert_eq!(chips, HashSet::from([1i32]));
        assert!(warn.iter().any(|w| w.contains("0xFF bit 7")));
    }

    #[test]
    fn test_clock_flags() {
        assert_eq!(ymz280b_flag_text(16_934_400), "");
        assert_eq!(ymz280b_flag_text(16_000_000), "clockSel=3");
        assert_eq!(ymz280b_flag_text(14_187_580), "clockSel=2");
        assert_eq!(ymz280b_flag_text(12_345_678), "customClock=12345678");
    }

    #[test]
    fn test_pan_law() {
        assert_eq!(ymz_pan_ratio(8), (15, 15));
        assert_eq!(ymz_pan_ratio(1), (15, 0));
        assert_eq!(ymz_pan_ratio(15), (0, 15));
        assert_eq!(ymz_pan_fx(1), 0xF0);
        assert_eq!(ymz_pan_fx(15), 0x0F);
    }
}

mod TestYmz280bModule {
    use super::*;

    fn _song(roms: Vec<RomSlice>, writes: Vec<ChipWrite>) -> vgm2fur::model::Song {
        analyze(&_vgm(writes, roms), AnalyzeOptions::default()).unwrap()
    }

    #[test]
    fn test_dual_chip_channels_and_fur() {
        let rom0 = _body(128);
        let rom1: Vec<u8> = (0..128usize).map(|i| ((i * 11 + 3) & 0xFF) as u8).collect();
        let mut writes = vec![_enable(0, 0), _enable(0, 1)];
        writes.extend(_note(
            1000, 0, 100, 0x10, 200, 8, 0x40, false, 0, 0, 0, None,
        ));
        writes.extend(_note(
            1200, 0, 100, 0x20, 200, 8, 0x40, false, 0, 0, 1, None,
        ));
        let song = _song(vec![_rom(0, &rom0), _rom(1, &rom1)], writes);
        assert_eq!(song.ymz280b_ids, vec![0, 1]);
        assert_eq!(song.ymz280b_count, 2);
        let chans: HashSet<i32> = song
            .events
            .iter()
            .filter(|e| e.on && e.pcm)
            .map(|e| e.ch)
            .collect();
        assert_eq!(chans.len(), 2);
        assert_eq!(chans.iter().max().unwrap() - chans.iter().min().unwrap(), 8);

        let data = write_fur(
            &song,
            WriteOptions {
                include_pcm: true,
                ..Default::default()
            },
        );
        let mod_ = parse_fur(&data).unwrap();
        assert_eq!(mod_.chips, vec![CHIP_YMZ280B, CHIP_YMZ280B]);
        assert_eq!(mod_.n_ch, 16);
        assert_eq!(mod_.samples.len(), 2);
        let raw = decompress_fur(&data);
        let types: Vec<u16> = iter_blocks(&raw)
            .unwrap()
            .into_iter()
            .filter(|b| &b.magic == b"INS2")
            .map(|b| u16::from_le_bytes([b.payload[2], b.payload[3]]))
            .collect();
        assert_eq!(types, vec![INS_YMZ280B as u16, INS_YMZ280B as u16]);
    }

    #[test]
    fn test_opl2_beside_ymz_points_rows_at_sample_instruments() {
        // A mixed OPL2 + YMZ280B log: the YMZ pattern rows must name the
        // final sample instruments, not an earlier snapshot taken before the
        // OPL2's operator-level instrument copies were added.
        let rom = _body(256);
        let mut writes = vec![_enable(0, 0)];
        writes.extend(_note(
            1000, 0, 64, 0x10, 200, 8, 0x40, false, 0, 0, 0, Some(3000),
        ));
        writes.extend(_note(
            4000, 0, 32, 0x20, 200, 8, 0x60, false, 0, 0, 0, Some(6000),
        ));
        writes.extend(_note(
            7000, 0, 8, 0x30, 200, 8, 0x20, false, 0, 0, 0, Some(9000),
        ));
        writes.extend(_opl2_note(500, 0, 4, 580, Some(30000)));
        writes.extend(_opl2_note(5500, 4, 4, 580, Some(30000)));
        writes.sort_by_key(|w| w.sample);
        let mut v = _vgm(writes, vec![_rom(0, &rom)]);
        v.ym2151_clock = 0;
        v.opl2_clock = 3_579_545;
        let song = analyze(&v, AnalyzeOptions::default()).unwrap();
        assert_eq!(song.ymz280b_ids, vec![0]);
        assert_eq!(song.ymz280b_count, 3);
        assert_eq!(song.ymz280b_samples.len(), 3);

        let data = write_fur(
            &song,
            WriteOptions {
                include_pcm: true,
                ..Default::default()
            },
        );
        let mod_ = parse_fur(&data).unwrap();
        assert!(mod_.chips.contains(&CHIP_OPL2));
        assert!(mod_.chips.contains(&CHIP_YMZ280B));
        assert_eq!(mod_.n_ch, 9 + 8); // 9 OPL2 + 8 YMZ
        assert_eq!(mod_.samples.len(), 3);
        // The YMZ rows point at the final three sample instruments, which are
        // the only instruments that carry a sample index.
        let mut used: HashSet<i32> = HashSet::new();
        for &ch in &mod_.pcm_channels {
            for &oi in &mod_.orders[ch] {
                for row in mod_.patterns.get(&(ch as u16, oi)).into_iter().flatten() {
                    if 0 <= row.note && row.note < 180 && row.ins >= 0 {
                        used.insert(row.ins);
                    }
                }
            }
        }
        let first = mod_.ins_sample.len() - mod_.samples.len();
        let expected: HashSet<i32> = (first as i32..mod_.ins_sample.len() as i32).collect();
        assert_eq!(used, expected);
        assert_eq!(&mod_.ins_sample[first..], &[0, 1, 2]);
    }

    #[test]
    fn test_dual_ymz_mix_is_full_scale_without_an_fm_placeholder() {
        // Two YMZ instances run at full output. Furnace's core is 1.27x
        // quieter than the player's, so the master carries that gain while
        // each chip volume stays at unity.
        let rom0 = _body(256);
        let rom1: Vec<u8> = (0..256usize).map(|i| ((i * 13 + 7) & 0xFF) as u8).collect();
        let mut writes = vec![_enable(0, 0), _enable(0, 1)];
        for i in 0..4u32 {
            let start = i * 32;
            writes.extend(_note(
                1000 + i as i64 * 2000,
                start,
                start + 24,
                0x10 + i,
                200,
                8,
                0x40,
                false,
                0,
                0,
                0,
                Some(1000 + i as i64 * 2000 + 1900),
            ));
            writes.extend(_note(
                1200 + i as i64 * 2000,
                start,
                start + 24,
                0x20 + i,
                200,
                8,
                0x40,
                false,
                0,
                0,
                1,
                Some(1200 + i as i64 * 2000 + 1900),
            ));
        }
        writes.sort_by_key(|w| w.sample);
        let song = _song(vec![_rom(0, &rom0), _rom(1, &rom1)], writes);
        assert_eq!(song.ymz280b_ids, vec![0, 1]);
        assert_eq!(song.ymz280b_count, 8);
        assert_eq!(song.ymz280b_samples.len(), 8);
        let data = write_fur(
            &song,
            WriteOptions {
                include_pcm: true,
                ..Default::default()
            },
        );
        let mod_ = parse_fur(&data).unwrap();
        // A YMZ-only log must not gain a silent YM2151 placeholder.
        assert_eq!(mod_.chips, vec![CHIP_YMZ280B, CHIP_YMZ280B]);
        assert_eq!(mod_.n_ch, 16);
        assert_eq!(mod_.samples.len(), 8);
        assert!(song.fm_patches.len() + song.ymz280b_samples.len() <= 250);
        // The two instances run at full output. The measured core difference
        // is carried by the master, not by halving each chip.
        assert_almost_eq!(master_volume(&data).unwrap(), 1.27, 2);
        assert_eq!(
            read_chips(&data)
                .unwrap()
                .iter()
                .map(|c| py_round_to(c.volume, 3))
                .collect::<Vec<_>>(),
            vec![1.0, 1.0]
        );
    }

    #[test]
    fn test_sample_depths_match_furnace() {
        let rom = _body(256);
        let mut writes = vec![_enable(0, 0)];
        writes.extend(_note(
            1000, 0, 64, 0x10, 200, 8, 0x40, false, 0, 0, 0, Some(3000),
        ));
        writes.extend(_note(
            4000, 0, 32, 0x20, 200, 8, 0x60, false, 0, 0, 0, Some(6000),
        ));
        writes.extend(_note(
            7000, 0, 8, 0x30, 200, 8, 0x20, false, 0, 0, 0, Some(9000),
        ));
        let song = _song(vec![_rom(0, &rom)], writes);
        let data = write_fur(
            &song,
            WriteOptions {
                include_pcm: true,
                ..Default::default()
            },
        );
        let mod_ = parse_fur(&data).unwrap();
        let mut depths: Vec<i32> = mod_.samples.iter().map(|s| s.depth as i32).collect();
        depths.sort();
        let mut expected = vec![SMP_DEPTH_8BIT, SMP_DEPTH_16BIT, SMP_DEPTH_YMZ_ADPCM];
        expected.sort();
        assert_eq!(depths, expected);
    }
}
