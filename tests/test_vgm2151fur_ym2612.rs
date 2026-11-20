//! YM2612 (OPN2) support: parsing, pitch mapping, analysis and .fur emission.
//!
//! Port of `tests/test_vgm2151fur_ym2612.py` at vgm2151fur a8edb6e.

mod common;
use common::*;

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::path::{Path, PathBuf};

use vgm2fur::analyze::{analyze, AnalyzeOptions};
use vgm2fur::diag::{pitch_runs, ym2612_keyed_intervals, ym2612_pitch_stream};
use vgm2fur::furio::{iter_blocks, parse_fur, FurRow, CHIP_CHANNELS};
use vgm2fur::furwrite::{
    decompress_fur, write_fur, WriteOptions, CHIP_YM2151, CHIP_YM2612, CHIP_YM2612_EXT, INS_FM,
};
use vgm2fur::model::furnace_note_name;
use vgm2fur::pitch::pitch_table;
use vgm2fur::sn76489::{
    collect_sn76489, psg_note, psg_pitch_table, PSG_DIVIDER, PSG_MAX_PERIOD,
};
use vgm2fur::vgm::{
    chips_in_head, load_vgm, ChipWrite, VgmFile, _decompress_data_block,
    _parse_compression_table,
};
use vgm2fur::ym2612::{
    ym2612_note_fnum, ym2612_note_for, ym2612_pitch_units, YM2612State, YM2612_FREQ_BASE,
};
use vgm2fur::ym2612dac::{
    collect_dac, group_dac_plays, CollectDacOptions, DacPlay, DacSample, DacStats,
    GroupDacOptions, SourceRun, DAC_RELEASE_SAMPLES, DAC_SILENCE_FRACTION,
    DAC_SILENCE_MIN_LEN, DAC_TARGET_SAMPLES, _cluster_similar_samples, _collect_source,
    _longest_silence, _merge_one_shot_samples, _split_long_silence, _split_silent_runs,
};

const CLOCK: u32 = 7670453;

fn tmp_dir() -> PathBuf {
    let p = std::env::temp_dir().join("vgm2fur_ym2612");
    std::fs::create_dir_all(&p).expect("create temp dir");
    p
}

fn raw_vgm(body: &[u8], clock: u32, dual: bool) -> Vec<u8> {
    let sn_clock: u32 = 3579545;
    let mut buf = vec![0u8; 0xC0];
    buf[0..4].copy_from_slice(b"Vgm ");
    buf[8..12].copy_from_slice(&0x161u32.to_le_bytes());
    buf[0xC..0x10].copy_from_slice(&(sn_clock & 0x3FFF_FFFF).to_le_bytes());
    buf[0x18..0x1C].copy_from_slice(&44100u32.to_le_bytes());
    buf[0x34..0x38].copy_from_slice(&(0xC0u32 - 0x34).to_le_bytes());
    let raw = (clock & 0x3FFF_FFFF) | if dual { 0x4000_0000 } else { 0 };
    buf[0x2C..0x30].copy_from_slice(&raw.to_le_bytes());
    let mut out = buf;
    out.extend_from_slice(body);
    out.push(0x66);
    let len = (out.len() as u32 - 4).to_le_bytes();
    out[4..8].copy_from_slice(&len);
    out
}

/// `_vgm(writes, *, total_samples=400000, clock=CLOCK, **kw)`.
fn vgm_with(writes: Vec<ChipWrite>, total_samples: i64, clock: u32) -> VgmFile {
    let mut writes = writes;
    writes.sort_by_key(|w| w.sample);
    VgmFile {
        path: PathBuf::from("synthetic.vgm"),
        buf: Vec::new(),
        version: 0x161,
        total_samples,
        loop_samples: 0,
        loop_file_pos: None,
        loop_sample: None,
        ym2151_clock: 0,
        k007232_clock: 0,
        gd3: Default::default(),
        roms: Vec::new(),
        writes,
        ym2612_clock: clock,
        ..Default::default()
    }
}

fn vgm_default(writes: Vec<ChipWrite>) -> VgmFile {
    vgm_with(writes, 400_000, CLOCK)
}

/// `_patch(t, cid, bank, ch, sr, ssg, dt)`.
fn patch(
    t: i64,
    cid: u8,
    bank: bool,
    ch: u16,
    sr: [u32; 4],
    ssg: [u32; 4],
    dt: [u32; 4],
) -> Vec<ChipWrite> {
    let mut out: Vec<ChipWrite> = Vec::new();
    let base = if bank { 0x100u32 } else { 0 };
    for page in [0x30u32, 0x40, 0x50, 0x60, 0x70, 0x80, 0x90] {
        for op in 0..4usize {
            let val = match page {
                0x30 => 0x01 | ((dt[op] & 7) << 4),
                0x50 => 0x1F,
                0x70 => sr[op],
                0x90 => ssg[op],
                _ => 0x00,
            };
            out.push(ChipWrite::new(
                t,
                "ym2612",
                cid,
                base | page + op as u32 * 4 + ch as u32,
                val,
            ));
        }
    }
    out.push(ChipWrite::new(t, "ym2612", cid, base | 0xB0 + ch as u32, 0x07));
    out.push(ChipWrite::new(t, "ym2612", cid, base | 0xB4 + ch as u32, 0xC0));
    out
}

/// `_fm_note(block, fnum, *, t=100, off=60000, cid=0, ch=0, patch_t=None, ...)`.
#[allow(clippy::too_many_arguments)]
fn fm_note(
    block: i32,
    fnum: i32,
    t: i64,
    off: i64,
    cid: u8,
    ch: u16,
    patch_t: Option<i64>,
    sr: [u32; 4],
    ssg: [u32; 4],
    dt: [u32; 4],
) -> Vec<ChipWrite> {
    let bank = ch >= 3;
    let sub = if ch >= 3 { ch - 3 } else { ch };
    let kon = if ch < 3 { ch } else { ch + 1 };
    let base = if bank { 0x100u32 } else { 0 };
    let mut out = patch(
        patch_t.unwrap_or(t),
        cid,
        bank,
        sub,
        sr,
        ssg,
        dt,
    );
    out.push(ChipWrite::new(
        t,
        "ym2612",
        cid,
        base | 0xA4 + sub as u32,
        ((block << 3) | ((fnum >> 8) & 7)) as u32,
    ));
    out.push(ChipWrite::new(
        t,
        "ym2612",
        cid,
        base | 0xA0 + sub as u32,
        (fnum & 0xFF) as u32,
    ));
    out.push(ChipWrite::new(t, "ym2612", cid, 0x28, (0xF0 | kon) as u32));
    if off != 0 {
        out.push(ChipWrite::new(off, "ym2612", cid, 0x28, kon as u32));
    }
    out
}

fn fm_note_default(block: i32, fnum: i32) -> Vec<ChipWrite> {
    fm_note(block, fnum, 100, 60_000, 0, 0, None, [0; 4], [0; 4], [0; 4])
}

fn ins_types(data: &[u8]) -> Vec<i32> {
    let mut out = Vec::new();
    for block in iter_blocks(data).expect("iter_blocks") {
        if &block.magic == b"INS2" && block.payload.len() >= 4 {
            out.push(i32::from(u16::from_le_bytes([
                block.payload[2],
                block.payload[3],
            ])));
        }
    }
    out
}

fn fm_feature(data: &[u8]) -> Vec<u8> {
    for block in iter_blocks(data).expect("iter_blocks") {
        if &block.magic != b"INS2" {
            continue;
        }
        let payload = block.payload;
        let mut pos = 0usize;
        while pos + 2 <= payload.len() {
            if &payload[pos..pos + 2] == b"FM" && pos + 4 <= payload.len() {
                let size = u16::from_le_bytes([payload[pos + 2], payload[pos + 3]]) as usize;
                let end_index = (pos + 4 + size).min(payload.len());
                return payload[pos + 4..end_index].to_vec();
            }
            pos += 1;
        }
    }
    Vec::new()
}

fn signed(data: &[u8]) -> Vec<u8> {
    data.iter().map(|&x| x.wrapping_sub(0x80)).collect()
}

fn contains_subslice(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

fn find_subslice(haystack: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    if needle.is_empty() || from > haystack.len() {
        return None;
    }
    haystack[from..]
        .windows(needle.len())
        .position(|w| w == needle)
        .map(|i| i + from)
}

/// `collect_dac(vgm, slices=True)`.
fn collect_slices(vgm: &VgmFile) -> (Vec<DacSample>, Vec<DacPlay>, Vec<String>, DacStats) {
    collect_dac(
        vgm,
        &CollectDacOptions {
            slices: true,
            ..Default::default()
        },
    )
}

/// `collect_dac(vgm)`.
fn collect_packed(vgm: &VgmFile) -> (Vec<DacSample>, Vec<DacPlay>, Vec<String>, DacStats) {
    collect_dac(vgm, &CollectDacOptions::default())
}

/// Python's `collect_dac` with `ym2612dac.DAC_RELEASE_SAMPLES` patched.
fn collect_packed_with_budget(
    vgm: &VgmFile,
    release_samples: usize,
) -> (Vec<DacSample>, Vec<DacPlay>, Vec<String>, DacStats) {
    collect_dac(
        vgm,
        &CollectDacOptions {
            release_samples: Some(release_samples),
            ..Default::default()
        },
    )
}

// ---------------------------------------------------------------------------
// Pass 1: parsing and collectors.
// ---------------------------------------------------------------------------

#[allow(non_snake_case)]
mod TestYm2612Parsing {
    use super::*;

    #[test]
    fn test_header_and_banks() {
        let body = [
            0x52, 0x30, 0x01, // bank 0, channel 0 operator
            0x53, 0x30, 0x02, // bank 1, channel 4 operator
            0x52, 0xA0, 0x03, // channel 0 fnum low
            0xA2, 0x40, 0x04, // second chip
            0xA3, 0x40, 0x05, // second chip, bank 1
        ];
        let p = tmp_dir().join("ym2612_dual.vgm");
        std::fs::write(&p, raw_vgm(&body, CLOCK, true)).expect("write vgm");
        let vgm = load_vgm(&p).expect("load vgm");
        assert_eq!(vgm.ym2612_clock, CLOCK);
        assert!(vgm.ym2612_dual);
        let got: Vec<(&str, u8, u32, u32)> = vgm
            .writes
            .iter()
            .map(|w| (w.chip, w.chip_id, w.reg, w.val))
            .collect();
        assert_eq!(
            got,
            vec![
                ("ym2612", 0, 0x030, 0x01),
                ("ym2612", 0, 0x130, 0x02),
                ("ym2612", 0, 0x0A0, 0x03),
                ("ym2612", 1, 0x040, 0x04),
                ("ym2612", 1, 0x140, 0x05),
            ]
        );
    }

    #[test]
    fn test_clock_is_masked() {
        let p = tmp_dir().join("ym2612_clock.vgm");
        std::fs::write(&p, raw_vgm(&[0x52, 0x30, 0x01], CLOCK, true)).expect("write vgm");
        assert_eq!(load_vgm(&p).expect("load").ym2612_clock, CLOCK);
    }

    #[test]
    fn test_chips_in_head() {
        assert!(chips_in_head(&raw_vgm(&[0x66], CLOCK, false)).contains("ym2612"));
        let kinds = chips_in_head(&raw_vgm(&[0x66], CLOCK, true));
        assert!(kinds.contains("ym2612"));
        assert!(kinds.contains("ym2612_dual"));
        // The YM2151 lives at 0x30; a YM2612-only header must not claim it.
        assert!(!kinds.contains("ym2151"));
    }

    #[test]
    fn test_compressed_dac_block_uses_the_7f_table() {
        // Type 0x40 is the compressed form of the YM2612 PCM bank. Some
        // rips compress it with 4-bit Delta-PCM and a 0x7F table.
        let table_payload: Vec<u8> = [0x01, 0x00, 0x08, 0x04, 0x04, 0x00, 0x01, 0x02, 0xFF, 0x00]
            .to_vec();
        let table = _parse_compression_table(&table_payload);
        assert_eq!(table, Some((1, 0, 8, 4, vec![1, 2, 255, 0])));
        // four 8-bit outputs from 0x80, deltas +1, +2, -1, 0
        let block: Vec<u8> = [
            0x01, 0x04, 0x00, 0x00, 0x00, 0x08, 0x04, 0x00, 0x80, 0x00, 0x01, 0x23,
        ]
        .to_vec();
        assert_eq!(
            _decompress_data_block(0x40, &block, table.as_ref()),
            Some(vec![0x81, 0x83, 0x82, 0x82]),
        );
    }

    #[test]
    fn test_stray_second_chip_is_first_with_warning() {
        let p = tmp_dir().join("ym2612_stray.vgm");
        std::fs::write(&p, raw_vgm(&[0xA2, 0x40, 0x02], CLOCK, false)).expect("write vgm");
        let vgm = load_vgm(&p).expect("load");
        assert!(!vgm.ym2612_dual);
        let got: Vec<(&str, u8)> = vgm.writes.iter().map(|w| (w.chip, w.chip_id)).collect();
        assert_eq!(got, vec![("ym2612", 0)]);
        assert!(vgm
            .warnings
            .iter()
            .any(|w| w.contains("without the dual-instance")));
    }

    #[test]
    fn test_missing_clock_warning() {
        let p = tmp_dir().join("ym2612_noclock.vgm");
        std::fs::write(&p, raw_vgm(&[0x52, 0x30, 0x01], 0, false)).expect("write vgm");
        let vgm = load_vgm(&p).expect("load");
        assert_eq!(vgm.ym2612_clock, 0);
        assert!(vgm.warnings.iter().any(|w| w.contains("YM2612 clock")));
    }

    #[test]
    fn test_stream_end_wins_over_header_length() {
        let body = [0x61, 0x10, 0x27, 0x52, 0x30, 0x01]; // wait 10000
        let p = tmp_dir().join("ym2612_shortstream.vgm");
        std::fs::write(&p, raw_vgm(&body, CLOCK, false)).expect("write vgm");
        let vgm = load_vgm(&p).expect("load");
        assert_eq!(vgm.total_samples, 10000);
        assert!(vgm
            .warnings
            .iter()
            .any(|w| w.contains("stream ends at 10000")));
    }
}

#[allow(non_snake_case)]
mod TestGenesisPsgDac {
    use super::*;

    #[test]
    fn test_sn76489_and_dac_commands_parse() {
        let bank = [0x80u8, 0x90, 0xA0, 0x80];
        let mut body = vec![0x50, 0x96, 0x67, 0x66, 0x00, 4, 0, 0, 0];
        body.extend_from_slice(&bank);
        body.extend_from_slice(&[0xE0, 0, 0, 0, 0, 0x80, 0x50, 0x8F]);
        let p = tmp_dir().join("genesis_psg_dac.vgm");
        std::fs::write(&p, raw_vgm(&body, CLOCK, false)).expect("write vgm");
        let vgm = load_vgm(&p).expect("load");
        assert_eq!(vgm.sn76489_clock, 3579545);
        assert!(chips_in_head(&std::fs::read(&p).expect("read")).contains("sn76489"));
        let vals: Vec<u32> = vgm
            .writes
            .iter()
            .filter(|w| w.chip == "sn76489")
            .map(|w| w.val)
            .collect();
        assert_eq!(vals, vec![0x96, 0x8F]);
        assert_eq!(vgm.dac_data, bank.to_vec());
        assert_eq!(vgm.dac_events[0], (0, 0, 0));
        let plays: Vec<(i64, u8, i64)> = vgm
            .dac_events
            .iter()
            .copied()
            .filter(|e| e.1 == 1)
            .collect();
        assert_eq!(plays, vec![(0, 1, 0)]);
    }

    #[test]
    fn test_game_gear_stereo_command_parses() {
        let p = tmp_dir().join("gamegear_stereo.vgm");
        std::fs::write(&p, raw_vgm(&[0x4F, 0xBB, 0x50, 0x96], CLOCK, false)).expect("write vgm");
        let vgm = load_vgm(&p).expect("load");
        assert!(vgm.sn76489_stereo);
        let got: Vec<(u32, u32)> = vgm
            .writes
            .iter()
            .filter(|w| w.chip == "sn76489")
            .map(|w| (w.reg, w.val))
            .collect();
        assert_eq!(got, vec![(1, 0xBB), (0, 0x96)]);
    }

    #[test]
    fn test_dense_dac_is_signed_once_and_uses_split_pan() {
        let mut bank: Vec<u8> = vec![0x80, 0x90];
        bank.extend((0..298).map(|i| (0x80 + (i % 17) - 8) as u8));
        let mut events: Vec<(i64, u8, i64)> = vec![(0, 0, 0)];
        events.extend((0..300).map(|i| (i * 2, 1, 4)));
        for (raw, expected) in [(0xC0u32, 0xFFi32), (0x80, 0xF0), (0x40, 0x0F), (0x00, 0x00)] {
            let writes = vec![
                ChipWrite::new(0, "ym2612", 0, 0x22, 0),
                ChipWrite::new(0, "ym2612", 0, 0x1B6, raw),
            ];
            let mut src = vgm_default(writes);
            src.dac_data = bank.clone();
            src.dac_events = events.clone();
            let (samples, plays, warn, _stats) = collect_slices(&src);
            assert!(!warn.iter().any(|w| !w.is_empty()));
            assert_eq!(samples.len(), 1);
            assert_eq!(samples[0].data[0], 0x00);
            assert_eq!(samples[0].data[1], 0x10);
            assert_eq!(plays[0].pan, expected);
        }
    }

    #[test]
    fn test_slow_glide_stays_one_note() {
        // Each period step is below a semitone, but the glide as a whole
        // passes one. Comparing against the note's original period split it
        // into a new note every semitone; compare with the current pitch.
        let mut ws = vec![ChipWrite::new(0, "sn76489", 0, 0, 0x96)]; // ch0 attenuation 6
        let mut t = 100i64;
        for period in [200i64, 202, 204, 206, 208, 210, 212, 214] {
            ws.push(ChipWrite::new(t, "sn76489", 0, 0, (0x80 | (period & 0x0F)) as u32));
            ws.push(ChipWrite::new(t, "sn76489", 0, 0, ((period >> 4) & 0x3F) as u32));
            t += 100;
        }
        ws.push(ChipWrite::new(t, "sn76489", 0, 0, 0x9F));
        let mut src = vgm_default(ws);
        src.sn76489_clock = 3579545;
        let (hits, _offs, _warn) = collect_sn76489(&src, None);
        assert_eq!(hits.len(), 1);
        assert!(hits[0].pitch_pts.len() >= 6);
    }

    #[test]
    fn test_collect_psg_tone_noise_and_pitch() {
        let ws = vec![
            ChipWrite::new(0, "ym2612", 0, 0x22, 0),
            // tone ch0: period 31, attenuation 6
            ChipWrite::new(100, "sn76489", 0, 0, 0x8F),
            ChipWrite::new(100, "sn76489", 0, 0, 0x01),
            ChipWrite::new(100, "sn76489", 0, 0, 0x96),
            ChipWrite::new(200, "sn76489", 0, 0, 0x80),
            ChipWrite::new(200, "sn76489", 0, 0, 0x02),
            ChipWrite::new(400, "sn76489", 0, 0, 0x9F),
            // noise: mode 7, attenuation 6
            ChipWrite::new(500, "sn76489", 0, 0, 0xE7),
            ChipWrite::new(500, "sn76489", 0, 0, 0xF6),
            ChipWrite::new(800, "sn76489", 0, 0, 0xFF),
        ];
        let mut src = vgm_default(ws);
        src.sn76489_clock = 3579545;
        let (hits, offs, warn) = collect_sn76489(&src, None);
        assert!(!warn.iter().any(|w| !w.is_empty()));
        assert_eq!(hits.len(), 2);
        let tone = hits.iter().find(|h| !h.noise).expect("tone hit");
        assert_eq!((tone.period, tone.vol, tone.voice), (31, 9, 0));
        assert_eq!(tone.pitch_pts, vec![(200, 32)]);
        assert_eq!(offs[0].sample_time, 400);
        let noise = hits.iter().find(|h| h.noise).expect("noise hit");
        assert_eq!((noise.vol, noise.noise_mode), (9, 7));
    }

    #[test]
    fn test_psg_period_zero_is_a_gate_off() {
        // A driver silences a tone voice by writing period 0, then its volume
        // mute. The period write must end the note; otherwise the pattern has
        // no note-off and the low note sustains into the next one.
        let ws = vec![
            ChipWrite::new(0, "ym2612", 0, 0x22, 0),
            ChipWrite::new(100, "sn76489", 0, 0, 0xCF), // ch2 tone period 1023
            ChipWrite::new(100, "sn76489", 0, 0, 0x3F),
            ChipWrite::new(100, "sn76489", 0, 0, 0xD0), // ch2 volume 0
            ChipWrite::new(500, "sn76489", 0, 0, 0xC0), // period 0: gate off
            ChipWrite::new(500, "sn76489", 0, 0, 0x00),
            ChipWrite::new(500, "sn76489", 0, 0, 0xDF),
        ];
        let mut src = vgm_default(ws);
        src.sn76489_clock = 3579545;
        let (hits, offs, _warn) = collect_sn76489(&src, None);
        let tone = hits.iter().find(|h| h.voice == 2).expect("tone");
        assert_eq!((tone.period, tone.off_sample), (1023, 500));
        assert!(offs
            .iter()
            .any(|o| (o.sample_time, o.voice) == (500, 2)));
    }

    #[test]
    fn test_psg_noise_mode_3_tracks_tone_channel_3() {
        // Noise mode 3 borrows tone generator 3's period. The noise note must
        // follow that register, or Furnace writes a fixed low period and the
        // channel goes effectively silent.
        let ws = vec![
            ChipWrite::new(0, "ym2612", 0, 0x22, 0),
            ChipWrite::new(0, "sn76489", 0, 0, 0xCE), // ch2 tone period 14
            ChipWrite::new(0, "sn76489", 0, 0, 0x00),
            ChipWrite::new(100, "sn76489", 0, 0, 0xE7), // mode 3 + white
            ChipWrite::new(100, "sn76489", 0, 0, 0xF0),
            ChipWrite::new(400, "sn76489", 0, 0, 0xCE), // ch2 period 30
            ChipWrite::new(400, "sn76489", 0, 0, 0x01),
            ChipWrite::new(800, "sn76489", 0, 0, 0xFF),
        ];
        let mut src = vgm_default(ws);
        src.sn76489_clock = 3579545;
        let (hits, _offs, _warn) = collect_sn76489(&src, None);
        let noise = hits.iter().find(|h| h.noise).expect("noise");
        assert_eq!((noise.period, noise.noise_mode), (14, 7));
        assert_eq!(noise.pitch_pts, vec![(400, 30)]);
    }

    #[test]
    fn test_mid_note_noise_rate_change_is_a_new_note() {
        // Modes 4 and 6 are both white, so 20xy does not change. Furnace
        // still takes the fixed rate from the note, so a rate write inside a
        // gate has to become a new note or the old rate keeps playing.
        let ws = vec![
            ChipWrite::new(0, "sn76489", 0, 0, 0xE4),
            ChipWrite::new(0, "sn76489", 0, 0, 0xF0),
            ChipWrite::new(500, "sn76489", 0, 0, 0xE6),
            ChipWrite::new(900, "sn76489", 0, 0, 0xFF),
        ];
        let mut src = vgm_default(ws);
        src.sn76489_clock = 3579545;
        let (hits, _offs, _warn) = collect_sn76489(&src, None);
        let noise: Vec<(i64, i32)> = hits
            .iter()
            .filter(|h| h.noise)
            .map(|h| (h.sample_time, h.noise_mode))
            .collect();
        assert_eq!(noise, vec![(0, 4), (500, 6)]);
    }

    #[test]
    fn test_same_period_rewrite_with_a_volume_write_retriggers() {
        // Some GG drivers retrigger a held tone by writing the same period
        // again and then the new volume on one sample. Without the volume
        // pair a period refresh is housekeeping; with it the driver means a
        // new note, and the pattern must show the note-on.
        let ws = vec![
            ChipWrite::new(0, "sn76489", 0, 0, 0x84),
            ChipWrite::new(0, "sn76489", 0, 0, 0x06), // period 100
            ChipWrite::new(0, "sn76489", 0, 0, 0x90),
            ChipWrite::new(300, "sn76489", 0, 0, 0x84), // refresh only
            ChipWrite::new(300, "sn76489", 0, 0, 0x06),
            ChipWrite::new(600, "sn76489", 0, 0, 0x84), // retrigger
            ChipWrite::new(600, "sn76489", 0, 0, 0x06),
            ChipWrite::new(600, "sn76489", 0, 0, 0x97),
            ChipWrite::new(900, "sn76489", 0, 0, 0x9F),
        ];
        let mut src = vgm_default(ws);
        src.sn76489_clock = 3579540;
        let (hits, _offs, _warn) = collect_sn76489(&src, None);
        let tone: Vec<(i64, i64, i32)> = hits
            .iter()
            .filter(|h| h.voice == 0)
            .map(|h| (h.sample_time, h.off_sample, h.vol))
            .collect();
        assert_eq!(tone, vec![(0, 600, 15), (600, 900, 8)]);
    }

    #[test]
    fn test_rate_change_and_mute_on_one_sample_still_emits_off() {
        // A Game Gear driver changes the noise rate and mutes the voice on the
        // same sample. The rate split opens a zero-length note, which is
        // discarded, but the note it replaced must still get its key-off or
        // the periodic noise rings until the next key-on.
        let ws = vec![
            ChipWrite::new(0, "sn76489", 0, 0, 0xE0),
            ChipWrite::new(0, "sn76489", 0, 0, 0xF0),
            ChipWrite::new(500, "sn76489", 0, 0, 0xE1),
            ChipWrite::new(500, "sn76489", 0, 0, 0xFF),
        ];
        let mut src = vgm_default(ws);
        src.sn76489_clock = 3579540;
        let (hits, offs, _warn) = collect_sn76489(&src, None);
        let noise: Vec<(i64, i64)> = hits
            .iter()
            .filter(|h| h.noise)
            .map(|h| (h.sample_time, h.off_sample))
            .collect();
        assert_eq!(noise, vec![(0, 500)]);
        assert!(offs
            .iter()
            .any(|o| (o.sample_time, o.voice) == (500, 3)));
    }

    #[test]
    fn test_large_channel3_noise_jump_is_a_legato_note() {
        // A period jump beyond what E8xx can carry on one row must land at
        // once. The source writes the register in one go; splitting the note
        // and marking it legato keeps the new period exact without
        // restarting the noise generator.
        let ws = vec![
            ChipWrite::new(0, "sn76489", 0, 0, 0xCE), // ch2 tone period 14
            ChipWrite::new(0, "sn76489", 0, 0, 0x00),
            ChipWrite::new(0, "sn76489", 0, 0, 0xE7), // mode 3 + white
            ChipWrite::new(0, "sn76489", 0, 0, 0xF0),
            ChipWrite::new(500, "sn76489", 0, 0, 0xC0), // ch2 period 0x200
            ChipWrite::new(500, "sn76489", 0, 0, 0x20),
            ChipWrite::new(900, "sn76489", 0, 0, 0xFF),
        ];
        let mut src = vgm_default(ws);
        src.sn76489_clock = 3579545;
        let (hits, _offs, _warn) = collect_sn76489(&src, None);
        let noise: Vec<&_> = hits.iter().filter(|h| h.noise).collect();
        assert_eq!(noise.len(), 2);
        assert_eq!((noise[0].period, noise[0].legato), (14, false));
        assert_eq!((noise[1].period, noise[1].legato), (0x200, true));
    }

    #[test]
    fn test_psg_system_disables_easy_noise() {
        // Furnace's easy-noise shortcut clamps channel-3 noise periods below
        // 16 to 0. The converter writes real periods, so the flag must say so.
        let ws = vec![
            ChipWrite::new(0, "sn76489", 0, 0, 0x8F),
            ChipWrite::new(0, "sn76489", 0, 0, 0x01),
            ChipWrite::new(0, "sn76489", 0, 0, 0x90),
        ];
        let mut src = vgm_with(ws, 400_000, 0);
        src.sn76489_clock = 3579545;
        let song = analyze(&src, AnalyzeOptions::default()).expect("analyze");
        let data = write_fur(&song, WriteOptions::default());
        assert!(contains_subslice(&data, b"noEasyNoise=true"));
    }

    #[test]
    fn test_psg_rows_snap_to_the_write_lattice() {
        // A PSG-only driver writes one frame at a time. The row grid follows
        // that interval, so a volume step and the next note never land on the
        // same row and a column step cannot be pulled back to the row start.
        let step = 1470i64;
        let mut writes = Vec::new();
        for i in 0..64i64 {
            let t = i * step;
            let per = 200 + (i % 4) * 40;
            writes.push(ChipWrite::new(t, "sn76489", 0, 0, (0x80 | (per & 0x0F)) as u32));
            writes.push(ChipWrite::new(t, "sn76489", 0, 0, (per >> 4) as u32));
            writes.push(ChipWrite::new(t, "sn76489", 0, 0, 0x90));
        }
        writes.push(ChipWrite::new(64 * step, "sn76489", 0, 0, 0x9F));
        let mut src = vgm_with(writes, 64 * step, 0);
        src.sn76489_clock = 3579545;
        let song = analyze(&src, AnalyzeOptions::default()).expect("analyze");
        let per_write = step as f64 / song.samples_per_row();
        assert_almost_eq!(per_write, vgm2fur::parity::py_round(per_write) as f64, 6);
        assert!(vgm2fur::parity::py_round(per_write) >= 1.0);
    }

    #[test]
    fn test_psg_note_is_the_same_period() {
        let tbl = psg_pitch_table();
        for period in [2, 14, 31, 100, 511, 1023] {
            let (note, e5) = psg_note(period, None);
            assert_eq!(
                tbl.value(note, e5 - 0x80),
                period,
                "{:?}",
                (period, note, e5)
            );
        }
    }

    #[test]
    fn test_psg_note_uses_the_source_clock() {
        // Ice1.vgz runs two SN76489s from a 2,858,084 Hz crystal. Looking the
        // period up in the 3,579,545 Hz default table made the module rescale
        // every register and play the whole conversion ~4 semitones sharp.
        let clock = 2_858_084u32;
        let tbl = pitch_table(clock as f64, PSG_DIVIDER as f64, PSG_MAX_PERIOD);
        for period in [2, 14, 31, 100, 254, 511, 1023] {
            let (note, e5) = psg_note(period, Some(clock as f64));
            assert_eq!(
                tbl.value(note, e5 - 0x80),
                period,
                "{:?}",
                (period, note, e5)
            );
        }
    }

    #[test]
    fn test_psg_custom_clock_reaches_the_module_flag() {
        let ws = vec![
            ChipWrite::new(0, "sn76489", 0, 0, 0x8F),
            ChipWrite::new(0, "sn76489", 0, 0, 0x01),
            ChipWrite::new(0, "sn76489", 0, 0, 0x90),
        ];
        let mut src = vgm_with(ws, 400_000, 0);
        src.sn76489_clock = 2_858_084;
        let song = analyze(&src, AnalyzeOptions::default()).expect("analyze");
        let data = write_fur(&song, WriteOptions::default());
        assert!(contains_subslice(&data, b"customClock=2858084"));
    }

    #[test]
    fn test_psg_module_round_trip() {
        skip_without_furnace!();
        let ws = vec![
            ChipWrite::new(0, "ym2612", 0, 0x22, 0),
            ChipWrite::new(100, "sn76489", 0, 0, 0x8F),
            ChipWrite::new(100, "sn76489", 0, 0, 0x01),
            ChipWrite::new(100, "sn76489", 0, 0, 0x96),
            ChipWrite::new(400, "sn76489", 0, 0, 0x9F),
        ];
        let mut src = vgm_default(ws);
        src.sn76489_clock = 3579545;
        let song = analyze(&src, AnalyzeOptions::default()).expect("analyze");
        assert_eq!(song.psg_count, 1);
        let fur = tmp_dir().join("genesis_psg.fur");
        let data = write_fur(
            &song,
            WriteOptions {
                include_pcm: false,
                ..Default::default()
            },
        );
        std::fs::write(&fur, &data).expect("write fur");
        let m = parse_fur(&std::fs::read(&fur).expect("read fur")).expect("parse");
        assert_eq!(m.n_ch, 10);
        assert!(m.chips.contains(&0x3));
        let vgm_path = tmp_dir().join("genesis_psg.vgm");
        vgm2fur::furnace::export_vgm(&fur, &vgm_path).expect("export vgm");
        let exp = load_vgm(&vgm_path).expect("load export");
        let (hits, _offs, _warn) = collect_sn76489(&exp, None);
        assert!(hits.iter().any(|h| h.period == 31 && h.vol == 9));
    }

    #[test]
    fn test_dac_stream_module_round_trip() {
        skip_without_furnace!();
        let bank: Vec<u8> = (0..300).map(|i| (0x80 + (i % 31) - 15) as u8).collect();
        let mut events: Vec<(i64, u8, i64)> = vec![(0, 0, 0)];
        events.extend((0..300).map(|i| (i * 4, 1, 4)));
        let ws = vec![ChipWrite::new(0, "ym2612", 0, 0x22, 0)];
        let mut src = vgm_default(ws);
        src.dac_data = bank;
        src.dac_events = events;
        let (samples, plays, warn, _stats) = collect_slices(&src);
        assert!(!warn.iter().any(|w| !w.is_empty()));
        assert_eq!(samples.len(), 1);
        assert_eq!(plays.len(), 1);
        assert_eq!(samples[0].c4_rate, 11025);
        let song = analyze(&src, AnalyzeOptions::default()).expect("analyze");
        assert_eq!(song.dac_count, 1);
        let fur = tmp_dir().join("genesis_dac.fur");
        let data = write_fur(&song, WriteOptions::default());
        std::fs::write(&fur, &data).expect("write fur");
        let m = parse_fur(&std::fs::read(&fur).expect("read fur")).expect("parse");
        assert_eq!(m.samples.len(), 1);
        let vgm_path = tmp_dir().join("genesis_dac.vgm");
        vgm2fur::furnace::export_vgm(&fur, &vgm_path).expect("export vgm");
        let exp = load_vgm(&vgm_path).expect("load export");
        assert_eq!(exp.dac_blocks.len(), 1);
        assert!(!exp.dac_block_plays.is_empty());
        assert_eq!(exp.dac_block_plays[0].2, 11025);
        let got: Vec<u8> = exp.dac_blocks[0].iter().map(|&b| b.wrapping_sub(0x80)).collect();
        assert_eq!(got, samples[0].data);
    }

    #[test]
    fn test_psg_uses_the_sn_instrument_after_a_dac_and_pitch_copy() {
        // The pitch-copy base recompute must keep the DAC maps.
        let mut ws = fm_note(4, 1204, 100, 60_000, 0, 0, None, [0; 4], [0; 4], [0; 4]);
        ws.extend([
            ChipWrite::new(30000, "ym2612", 0, 0xA4, (4 << 3) | ((1250 >> 8) & 7)),
            ChipWrite::new(30000, "ym2612", 0, 0xA0, 1250 & 0xFF),
        ]);
        ws.extend([
            ChipWrite::new(100, "sn76489", 0, 0, 0xE7), // white noise, mode 7
            ChipWrite::new(100, "sn76489", 0, 0, 0xF6), // noise volume 6
        ]);
        let mut src = vgm_with(ws, 60_000, CLOCK);
        src.dac_events = vec![(0, 2, 0x80), (2, 2, 0x90), (4, 2, 0x70)];
        src.sn76489_clock = 3579545;
        let song = analyze(
            &src,
            AnalyzeOptions {
                speed: Some(6),
                dac_slices: true,
                ..Default::default()
            },
        )
        .expect("analyze");
        assert_eq!(song.dac_count, 1);
        assert_eq!(song.psg_count, 1);
        let fur = tmp_dir().join("genesis_psg_instrument.fur");
        let data = write_fur(&song, WriteOptions::default());
        std::fs::write(&fur, &data).expect("write fur");
        let m = parse_fur(&std::fs::read(&fur).expect("read fur")).expect("parse");
        let psg_notes: Vec<_> = m
            .patterns
            .get(&(9, 0))
            .map(|rows| rows.iter().filter(|r| r.note >= 0).collect())
            .unwrap_or_default();
        assert!(!psg_notes.is_empty());
        let types = ins_types(&decompress_fur(&std::fs::read(&fur).expect("read fur")));
        assert_eq!(types[psg_notes[0].ins as usize], 0); // DIV_INS_STD ("SN")
    }
}

#[allow(non_snake_case)]
mod TestYm2612DacSlices {
    use super::*;

    /// Build (bank, dac_events) for [(onset, bytes, wait), ...].
    fn runs(spec: &[(i64, Vec<u8>, i64)]) -> (Vec<u8>, Vec<(i64, u8, i64)>) {
        let mut bank: Vec<u8> = Vec::new();
        let mut events: Vec<(i64, u8, i64)> = Vec::new();
        for (t, data, wait) in spec {
            let off = bank.len() as i64;
            bank.extend_from_slice(data);
            events.push((*t, 0, off));
            for i in 0..data.len() {
                events.push((t + i as i64 * wait, 1, *wait));
            }
        }
        (bank, events)
    }

    /// One YM2612 write marks the chip present for `analyze`.
    fn marker() -> Vec<ChipWrite> {
        vec![ChipWrite::new(0, "ym2612", 0, 0x22, 0)]
    }

    #[test]
    fn test_same_time_burst_keeps_the_final_value() {
        let events = vec![(0, 2, 0x90), (0, 2, 0xA0), (0, 2, 0xB0), (100, 2, 0x80)];
        let mut src = vgm_with(marker(), 1000, CLOCK);
        src.dac_events = events;
        let (samples, _plays, warn, stats) = collect_slices(&src);
        assert_eq!(stats.int("dac_same_time_bytes"), 3);
        assert_eq!(stats.int("dac_same_time_dropped"), 2);
        assert!(warn.iter().any(|w| w.contains("share one VGM sample")));
        let final_sample = samples
            .iter()
            .find(|s| s.data == vec![0x30])
            .expect("final value sample");
        assert_eq!(final_sample.c4_rate, 44100);
        let song = analyze(
            &src,
            AnalyzeOptions {
                speed: Some(6),
                dac_slices: true,
                ..Default::default()
            },
        )
        .expect("analyze");
        assert_eq!(song.dac_count, 1);
        assert_eq!(song.dac_samples[0].data[0], 0x30);
        assert_eq!(*song.dac_samples[0].data.last().unwrap(), 0x00);
    }

    #[test]
    fn test_long_phrase_chunks_keep_the_source_rate() {
        // A phrase past 0.25 s is split into several sample-map chunks. Each
        // chunk used to take the phrase's trailing zero as its own tail, so
        // every chunk stretched to the phrase end and the overlapping group
        // doubled its playback rate (44.1 kHz became 88.2 kHz).
        // Vary the bytes: a constant level is a flat DC line and the collector
        // now drops it before it reaches the sample map.
        let mut events: Vec<(i64, u8, i64)> =
            (0..60000).map(|i| (i, 2, 0x40 + (i % 7))).collect();
        events.extend((0..10000).map(|i| (70000 + i, 2, 0x20 + (i % 5))));
        let mut src = vgm_with(
            marker()
                .into_iter()
                .chain(std::iter::once(ChipWrite::new(0, "ym2612", 0, 0x2B, 0x80)))
                .collect(),
            90000,
            CLOCK,
        );
        src.dac_events = events;
        let (samples, plays, _warn, _stats) = collect_packed(&src);
        let rates: BTreeSet<i32> = samples.iter().map(|s| s.c4_rate).collect();
        assert_eq!(rates, BTreeSet::from([44100]));
        let mut ordered = plays.clone();
        ordered.sort_by_key(|p| p.onset);
        for pair in ordered.windows(2) {
            let (a, b) = (&pair[0], &pair[1]);
            assert!(a.onset + a.span <= b.onset + 1, "{:?} {:?}", a, b);
        }
    }

    #[test]
    fn test_timeline_fallback_is_explicit() {
        let bank: Vec<u8> = (0..300).map(|i| (0x80 + (i % 31) - 15) as u8).collect();
        let mut events: Vec<(i64, u8, i64)> = vec![(0, 0, 0)];
        events.extend((0..300).map(|i| (i * 4, 1, 4)));
        let mut src = vgm_with(marker(), 400_000, CLOCK);
        src.dac_data = bank;
        src.dac_events = events;
        let (samples, plays, _warn, stats) = collect_dac(
            &src,
            &CollectDacOptions {
                timeline: true,
                ..Default::default()
            },
        );
        assert_eq!(stats.int("dac_timeline"), 1);
        assert_eq!(samples.len(), 1);
        assert_eq!(samples[0].c4_rate, 22050);
        assert_eq!(plays.len(), 1);
    }

    #[test]
    fn test_direct_2a_stream_is_one_slice() {
        let values = [0x80u8, 0x90, 0x70, 0x60];
        let events: Vec<(i64, u8, i64)> = values
            .iter()
            .enumerate()
            .map(|(i, &v)| (i as i64 * 2, 2, v as i64))
            .collect();
        let mut src = vgm_with(marker(), 1000, CLOCK);
        src.dac_events = events;
        let (samples, plays, warn, _stats) = collect_slices(&src);
        assert!(!warn.iter().any(|w| !w.is_empty()));
        assert_eq!(samples.len(), 1);
        assert_eq!(samples[0].data, signed(&values));
        assert_eq!(plays[0].onset, 0);
        let song = analyze(
            &src,
            AnalyzeOptions {
                speed: Some(6),
                dac_slices: true,
                ..Default::default()
            },
        )
        .expect("analyze");
        assert_eq!(song.dac_count, 1);
        assert_eq!(song.dac_samples[0].data, samples[0].data);
    }

    #[test]
    fn test_long_gap_is_not_materialised() {
        let (bank, events) = runs(&[
            (0, vec![0x81, 0x82, 0x83, 0x84], 4),
            (4410, vec![0x85, 0x86, 0x87, 0x88], 4),
        ]);
        let mut src = vgm_with(marker(), 9000, CLOCK);
        src.dac_data = bank;
        src.dac_events = events;
        let song = analyze(
            &src,
            AnalyzeOptions {
                speed: Some(6),
                dac_slices: true,
                ..Default::default()
            },
        )
        .expect("analyze");
        assert_eq!(song.dac_count, 2);
        // Each row owns its own run; the 4,410-sample pause stores no frames.
        let mut lengths: Vec<usize> = song.dac_samples.iter().map(|s| s.data.len()).collect();
        lengths.sort();
        assert_eq!(lengths, vec![4, 4]);
    }

    #[test]
    fn test_held_byte_is_one_frame_plus_the_dac_hold() {
        let (bank, events) = runs(&[
            (0, vec![0x80; 32], 4),
            (4410, vec![0x90, 0x91, 0x92, 0x93], 4),
        ]);
        let mut src = vgm_with(marker(), 9000, CLOCK);
        src.dac_data = bank;
        src.dac_events = events;
        let (samples, _plays, _warn, stats) = collect_slices(&src);
        assert_eq!(samples.len(), 2);
        let held = samples.iter().find(|s| s.data.len() == 1).expect("held");
        assert_eq!(held.data, vec![0x00]);
        assert_eq!(stats.int("dac_collapsed_bytes"), 32);
        let song = analyze(
            &src,
            AnalyzeOptions {
                speed: Some(6),
                dac_slices: true,
                ..Default::default()
            },
        )
        .expect("analyze");
        // A pure-silence run produces no note.
        assert_eq!(song.dac_count, 1);
        assert_eq!(song.dac_samples.len(), 1);
        assert_eq!(song.dac_samples[0].data.len(), 4);
        assert_eq!(song.dac_stats.int("dac_silent_groups"), 1);
        assert_eq!(song.dac_stats.int("dac_silent_notes"), 1);
    }

    #[test]
    fn test_silent_rows_get_no_note_and_previous_sample_holds_silence() {
        let a = vec![0x91u8, 0x92, 0x93, 0x94];
        let b = vec![0x95u8, 0x96, 0x97, 0x98];
        let (bank, events) = runs(&[(0, a, 4), (4410, vec![0x80; 4], 4), (8820, b, 4)]);
        let mut src = vgm_with(marker(), 14000, CLOCK);
        src.dac_data = bank;
        src.dac_events = events;
        let song = analyze(
            &src,
            AnalyzeOptions {
                speed: Some(6),
                dac_slices: true,
                ..Default::default()
            },
        )
        .expect("analyze");
        assert_eq!(song.dac_count, 2);
        assert_eq!(song.dac_samples.len(), 2);
        assert_eq!(*song.dac_samples[0].data.last().unwrap(), 0x00);
        assert_eq!(song.dac_stats.int("dac_silent_groups"), 1);
        assert_eq!(song.dac_stats.int("dac_silent_notes"), 1);
        let fur = tmp_dir().join("dac_silence.fur");
        std::fs::write(&fur, write_fur(&song, WriteOptions::default())).expect("write");
        let m = parse_fur(&std::fs::read(&fur).expect("read")).expect("parse");
        let notes: Vec<_> = m
            .patterns
            .get(&(5, 0))
            .map(|rows| rows.iter().filter(|r| r.note >= 0).collect())
            .unwrap_or_default();
        assert_eq!(notes.len(), 2);
    }

    #[test]
    fn test_same_row_runs_merge_without_a_drop() {
        let a = vec![0x01u8, 0x02, 0x03, 0x04];
        let b = vec![0x05u8, 0x06, 0x07, 0x08];
        let (bank, events) = runs(&[(0, a.clone(), 4), (100, b.clone(), 4)]);
        let mut src = vgm_with(marker(), 1000, CLOCK);
        src.dac_data = bank;
        src.dac_events = events;
        let song = analyze(
            &src,
            AnalyzeOptions {
                speed: Some(6),
                dac_slices: true,
                ..Default::default()
            },
        )
        .expect("analyze");
        assert_eq!(song.dac_count, 1);
        assert_eq!(song.dac_samples.len(), 1);
        let data = &song.dac_samples[0].data;
        let ia = find_subslice(data, &signed(&a), 0).expect("a present");
        let ib = find_subslice(data, &signed(&b), ia + a.len()).expect("b present");
        assert!(ib > ia);
        let mut stats = indexmap::IndexMap::new();
        write_fur(
            &song,
            WriteOptions {
                stats: Some(&mut stats),
                ..Default::default()
            },
        );
        assert_eq!(stats.get("note_collision").copied().unwrap_or(0), 0);
    }

    #[test]
    fn test_more_than_120_slices_use_multiple_maps() {
        let mut spec = Vec::new();
        for i in 0..130i64 {
            // Start at 0x81 so the first run is not a pure 0x80 rest.
            let data = vec![((0x81 + (i & 0x7F)) & 0xFF) as u8, (0x80 + (i >> 7)) as u8];
            spec.push((i * 4410, data, 4));
        }
        let (bank, events) = runs(&spec);
        let mut src = vgm_with(marker(), 580_000, CLOCK);
        src.dac_data = bank;
        src.dac_events = events;
        let song = analyze(
            &src,
            AnalyzeOptions {
                speed: Some(6),
                dac_slices: true,
                ..Default::default()
            },
        )
        .expect("analyze");
        assert_eq!(song.dac_count, 130);
        assert_eq!(song.dac_samples.len(), 130);
        assert_eq!((song.dac_samples.len() + 119) / 120, 2);
        let ins_set: BTreeSet<i32> = song
            .events
            .iter()
            .filter(|e| e.dac)
            .map(|e| e.ins)
            .collect();
        assert_eq!(ins_set, BTreeSet::from([0, 1]));
        // Sample 120 is the first of the second map: slot 0, instrument 1.
        let mut by_onset: Vec<&DacSample> = song.dac_samples.iter().collect();
        by_onset.sort_by_key(|s| s.onset);
        assert_eq!(by_onset[120].onset, 120 * 4410);
        let cell = song
            .events
            .iter()
            .find(|e| e.dac && e.sample == 120 * 4410)
            .expect("second map note");
        assert_eq!((cell.note, cell.ins), (60, 1));
        let fur = tmp_dir().join("dac_maps.fur");
        std::fs::write(&fur, write_fur(&song, WriteOptions::default())).expect("write");
        let m = parse_fur(&std::fs::read(&fur).expect("read")).expect("parse");
        assert_eq!(m.samples.len(), 130);
        // Every DAC map reports base 0 for release Furnace; the map entries
        // still point at samples 0..119 and 120..129.
        assert_eq!(&m.ins_sample[..2], &[0, 0]);
    }

    #[test]
    fn test_pan_change_between_rows_reaches_the_note() {
        let (bank, events) = runs(&[
            (0, vec![0x91, 0x92, 0x93, 0x94], 4),
            (4410, vec![0x95, 0x96, 0x97, 0x98], 4),
        ]);
        let writes = vec![ChipWrite::new(2000, "ym2612", 0, 0x1B6, 0x40)];
        let mut src = vgm_with(writes, 9000, CLOCK);
        src.dac_data = bank;
        src.dac_events = events;
        let song = analyze(
            &src,
            AnalyzeOptions {
                speed: Some(6),
                dac_slices: true,
                ..Default::default()
            },
        )
        .expect("analyze");
        let pans: Vec<i32> = song
            .events
            .iter()
            .filter(|e| e.dac)
            .map(|e| e.pan)
            .collect();
        assert_eq!(pans, vec![0xFF, 0x0F]);
    }

    #[test]
    fn test_pan_change_inside_a_row_is_a_channel_effect() {
        let (bank, events) = runs(&[
            (0, vec![0x91, 0x92, 0x93, 0x94], 4),
            (100, vec![0x95, 0x96, 0x97, 0x98], 4),
        ]);
        let writes = vec![ChipWrite::new(50, "ym2612", 0, 0x1B6, 0x40)];
        let mut src = vgm_with(writes, 1000, CLOCK);
        src.dac_data = bank;
        src.dac_events = events;
        let song = analyze(
            &src,
            AnalyzeOptions {
                speed: Some(6),
                dac_slices: true,
                ..Default::default()
            },
        )
        .expect("analyze");
        let notes: Vec<_> = song.events.iter().filter(|e| e.dac).collect();
        assert_eq!(notes.len(), 1);
        assert_eq!(notes[0].pan, 0xFF);
        let pans: Vec<(i64, i32, i32, i32)> = song
            .channel_fx
            .iter()
            .copied()
            .filter(|fx| fx.1 == 5 && fx.2 == 0x08)
            .collect();
        assert!(!pans.is_empty());
        assert_eq!(pans[0].3, 0x0F);
    }

    #[test]
    fn test_disabled_dac_leaves_fm6_alone() {
        // Track 01 writes 0x2B=0 and never enables the DAC.  Its 0x2A
        // writes are inaudible and its FM6 key-on must stay an FM note.
        let mut ws = vec![ChipWrite::new(0, "ym2612", 0, 0x2B, 0x00)];
        ws.extend(fm_note(4, 1204, 100, 0, 0, 5, None, [0; 4], [0; 4], [0; 4]));
        ws.push(ChipWrite::new(200, "ym2612", 0, 0x2A, 0x90));
        let mut src = vgm_with(ws, 1000, CLOCK);
        src.dac_events = vec![(200, 2, 0x90)];
        let song = analyze(
            &src,
            AnalyzeOptions {
                speed: Some(6),
                dac_slices: true,
                ..Default::default()
            },
        )
        .expect("analyze");
        assert_eq!(song.dac_count, 0);
        assert!(song.warnings.iter().any(|w| w.contains("disabled")));
        let fm6: Vec<_> = song
            .events
            .iter()
            .filter(|e| e.on && e.ch == 5 && !e.dac && !e.pcm)
            .collect();
        assert!(!fm6.is_empty());
        assert!(fm6.iter().all(|e| e.ins >= 0));
    }

    #[test]
    fn test_modern_block_play_is_a_slice() {
        let body = vec![0x80u8, 0x90, 0x70, 0x60, 0x50];
        let mut src = vgm_with(marker(), 9000, CLOCK);
        src.dac_blocks = vec![body.clone()];
        src.dac_block_plays = vec![(0, 0, 11025), (4410, 0, 11025)];
        let (samples, plays, warn, _stats) = collect_slices(&src);
        assert!(!warn.iter().any(|w| !w.is_empty()));
        assert_eq!(samples.len(), 1);
        assert_eq!(samples[0].data, signed(&body));
        assert_eq!(samples[0].c4_rate, 11025);
        assert_eq!(plays.len(), 2);
        let song = analyze(
            &src,
            AnalyzeOptions {
                speed: Some(6),
                dac_slices: true,
                ..Default::default()
            },
        )
        .expect("analyze");
        assert_eq!(song.dac_count, 2);
    }

    #[test]
    fn test_second_block_play_cuts_the_first() {
        // Starting a 0x95 stream stops the previous block.  Expanding both
        // copies to their full length leaves two PCM streams interleaved, and
        // the later word wins at each frame (a garbled waveform).
        let body: Vec<u8> = (0x80u8..0x80 + 100).collect();
        let mut src = vgm_with(marker(), 9000, CLOCK);
        src.dac_blocks = vec![body.clone()];
        src.dac_block_plays = vec![(0, 0, 11025), (200, 0, 11025)];
        let (samples, plays, warn, _stats) = collect_slices(&src);
        assert!(!warn.iter().any(|w| !w.is_empty()));
        // 200 VGM samples at 11025 Hz is 50 source bytes.
        assert_eq!(samples.len(), 2);
        assert_eq!(samples[0].data, signed(&body[..50]));
        assert_eq!(samples[1].data, signed(&body));
        let mut onsets: Vec<i64> = plays.iter().map(|p| p.onset).collect();
        onsets.sort();
        assert_eq!(onsets, vec![0, 200]);

        let (_s, _p, _w, stats) = collect_packed(&src);
        assert_eq!(stats.int("dac_block_plays_cut"), 1);
        assert!(!stats.contains("dac_same_time_runs"));
    }

    #[test]
    fn test_export_starts_and_bodies_match_the_slices() {
        skip_without_furnace!();
        let spec = vec![
            (0, vec![0x91, 0x92, 0x93, 0x94], 4),
            (4410, vec![0x95, 0x96, 0x97, 0x98], 4),
            (8820, vec![0x99, 0x9A, 0x9B, 0x9C], 4),
        ];
        let (bank, events) = runs(&spec);
        let mut src = vgm_with(marker(), 14000, CLOCK);
        src.dac_data = bank;
        src.dac_events = events;
        let song = analyze(
            &src,
            AnalyzeOptions {
                speed: Some(6),
                dac_slices: true,
                ..Default::default()
            },
        )
        .expect("analyze");
        let fur = tmp_dir().join("dac_slices.fur");
        std::fs::write(&fur, write_fur(&song, WriteOptions::default())).expect("write");
        let vgm_path = tmp_dir().join("dac_slices.vgm");
        vgm2fur::furnace::export_vgm(&fur, &vgm_path).expect("export vgm");
        let exp = load_vgm(&vgm_path).expect("load export");
        let mut notes: Vec<i64> = song.events.iter().filter(|e| e.dac).map(|e| e.sample).collect();
        notes.sort();
        let mut starts: Vec<i64> = exp
            .dac_block_plays
            .iter()
            .map(|p| p.0)
            .filter(|&t| t < song.vgm.total_samples)
            .collect();
        starts.sort();
        assert_eq!(notes.len(), 3);
        for note in notes {
            assert!(
                starts.iter().any(|&start| (start - note).abs() <= 2),
                "{:?} {:?}",
                note,
                starts
            );
        }
        let bodies: HashSet<Vec<u8>> = exp.dac_blocks.iter().cloned().collect();
        let ours: HashSet<Vec<u8>> = song
            .dac_samples
            .iter()
            .map(|s| s.data.iter().map(|&x| x.wrapping_add(0x80)).collect())
            .collect();
        assert!(ours.is_subset(&bodies));
    }

    #[test]
    fn test_packed_dense_stream_is_one_real_sample_and_note() {
        let bank: Vec<u8> = (0..300).map(|i| (0x80 + (i % 31) - 15) as u8).collect();
        let (bank, events) = runs(&[(0, bank.clone(), 4)]);
        let mut src = vgm_with(marker(), 400_000, CLOCK);
        src.dac_data = bank.clone();
        src.dac_events = events;
        let (samples, plays, warn, stats) = collect_packed(&src);
        assert!(!warn.iter().any(|w| !w.is_empty()));
        assert_eq!(stats.text("dac_mode", ""), "source");
        assert_eq!(samples.len(), 1);
        assert_eq!(plays.len(), 1);
        assert_eq!(samples[0].c4_rate, 11025);
        assert_eq!(samples[0].data, signed(&bank));
        let song = analyze(
            &src,
            AnalyzeOptions {
                speed: Some(6),
                ..Default::default()
            },
        )
        .expect("analyze");
        assert_eq!(song.dac_count, 1);
        assert_eq!(song.dac_samples.len(), 1);
        let fur = tmp_dir().join("dac_packed.fur");
        std::fs::write(&fur, write_fur(&song, WriteOptions::default())).expect("write");
        let raw = std::fs::read(&fur).expect("read");
        let m = parse_fur(&raw).expect("parse");
        assert_eq!(m.samples.len(), 1);
        let notes: Vec<_> = m
            .patterns
            .get(&(5, 0))
            .map(|rows| rows.iter().filter(|r| r.note >= 0).collect())
            .unwrap_or_default();
        assert_eq!(notes.len(), 1);
        let mut sm_bases = Vec::new();
        for block in iter_blocks(&decompress_fur(&raw)).expect("iter") {
            if &block.magic != b"INS2" {
                continue;
            }
            let blk = block.payload;
            let mut pos = 4usize;
            let mut name = String::new();
            let mut sm: Option<&[u8]> = None;
            while pos + 4 <= blk.len() {
                let feat = &blk[pos..pos + 2];
                let size = u16::from_le_bytes([blk[pos + 2], blk[pos + 3]]) as usize;
                if feat == b"NA" {
                    let raw_name = &blk[pos + 4..(pos + 4 + size).min(blk.len())];
                    name = String::from_utf8_lossy(
                        raw_name.split(|&b| b == 0).next().unwrap_or(&[]),
                    )
                    .into_owned();
                }
                if feat == b"SM" {
                    sm = Some(&blk[pos + 4..(pos + 4 + size).min(blk.len())]);
                }
                if feat == b"EN" {
                    break;
                }
                pos += 4 + size;
            }
            if let Some(sm) = sm {
                if name.starts_with("DAC") && sm.len() >= 2 {
                    sm_bases.push(u16::from_le_bytes([sm[0], sm[1]]) as i32);
                }
            }
        }
        assert_eq!(sm_bases, vec![0]);
    }

    #[test]
    fn test_packed_short_internal_silence_stays_one_phrase() {
        let (bank, events) = runs(&[
            (0, vec![0x91, 0x92, 0x93, 0x94], 4),
            (400, vec![0x80; 4], 4),
            (800, vec![0x95, 0x96, 0x97, 0x98], 4),
        ]);
        let mut src = vgm_with(marker(), 2000, CLOCK);
        src.dac_data = bank;
        src.dac_events = events;
        let (samples, plays, _warn, stats) = collect_packed(&src);
        assert_eq!(stats.int("dac_silent_phrases"), 0);
        assert_eq!(samples.len(), 1);
        assert_eq!(plays.len(), 1);
        let song = analyze(
            &src,
            AnalyzeOptions {
                speed: Some(6),
                ..Default::default()
            },
        )
        .expect("analyze");
        assert_eq!(song.dac_count, 1);
    }

    #[test]
    fn test_packed_long_gap_splits_audio_not_silence() {
        let (bank, events) = runs(&[
            (0, vec![0x91, 0x92, 0x93, 0x94], 4),
            (4410, vec![0x80; 4], 4),
            (8820, vec![0x95, 0x96, 0x97, 0x98], 4),
        ]);
        let mut src = vgm_with(marker(), 14000, CLOCK);
        src.dac_data = bank;
        src.dac_events = events;
        let (samples, plays, _warn, stats) = collect_packed(&src);
        assert_eq!(stats.int("dac_source_silent_runs"), 1);
        // All three runs are played once and in sequence, so the one-shot
        // coalescer joins them into a single body. The gap is held inside it
        // (the DAC holds its last value, then the source writes 0x00).
        assert_eq!(stats.int("dac_source_coalesced_chains"), 1);
        assert_eq!(samples.len(), 1);
        assert_eq!(plays.len(), 1);
        assert!(samples[0].data.iter().any(|&b| b != 0));
        assert!(contains_subslice(&samples[0].data, &[0, 0, 0, 0]));
        let song = analyze(
            &src,
            AnalyzeOptions {
                speed: Some(6),
                ..Default::default()
            },
        )
        .expect("analyze");
        assert_eq!(song.dac_count, 1);
        let fur = tmp_dir().join("dac_packed_gap.fur");
        std::fs::write(&fur, write_fur(&song, WriteOptions::default())).expect("write");
        let m = parse_fur(&std::fs::read(&fur).expect("read")).expect("parse");
        let notes: Vec<_> = m
            .patterns
            .get(&(5, 0))
            .map(|rows| rows.iter().filter(|r| r.note >= 0).collect())
            .unwrap_or_default();
        assert_eq!(notes.len(), 1);
    }

    #[test]
    fn test_packed_silence_only_stream_has_no_notes() {
        let (bank, events) = runs(&[(0, vec![0x80; 32], 4)]);
        let mut src = vgm_with(marker(), 2000, CLOCK);
        src.dac_data = bank;
        src.dac_events = events;
        let (samples, plays, warn, _stats) = collect_packed(&src);
        assert!(samples.is_empty());
        assert!(plays.is_empty());
        assert!(warn.iter().any(|w| w.contains("only silence")));
        let song = analyze(
            &src,
            AnalyzeOptions {
                speed: Some(6),
                ..Default::default()
            },
        )
        .expect("analyze");
        assert_eq!(song.dac_count, 0);
    }

    #[test]
    fn test_packed_rate_is_the_smallest_source_interval() {
        let events = vec![(0, 2, 0x91), (3, 2, 0x92), (7, 2, 0x93), (12, 2, 0x94)];
        let mut src = vgm_with(marker(), 100, CLOCK);
        src.dac_events = events;
        let (samples, plays, _warn, _stats) = collect_packed(&src);
        assert_eq!(samples.len(), 1);
        assert_eq!(plays.len(), 1);
        assert_eq!(samples[0].c4_rate, 14700);
        // Waits 3, 4, 5 at 14,700 Hz: frames 0, 1, 2, 4; frame 3 holds 0x93.
        assert_eq!(
            samples[0].data,
            signed(&[0x91, 0x92, 0x93, 0x93, 0x94])
        );
    }

    #[test]
    fn test_long_pcm_is_one_source_sample_with_mid_note_pan() {
        // 20 s at 4-wait with pan changes: one bank sample, one note, and the
        // pan moves on the channel while the sample keeps playing.
        let bank: Vec<u8> = (0..220_500).map(|i| (0x80 + (i % 31) - 15) as u8).collect();
        let (bank, events) = runs(&[(0, bank, 4)]);
        let mut writes = marker();
        writes.extend([
            ChipWrite::new(0, "ym2612", 0, 0x1B6, 0xC0),
            ChipWrite::new(5 * 44100, "ym2612", 0, 0x1B6, 0x80),
            ChipWrite::new(10 * 44100, "ym2612", 0, 0x1B6, 0x40),
        ]);
        let mut src = vgm_with(writes, 20 * 44100, CLOCK);
        src.dac_data = bank;
        src.dac_events = events;
        let (samples, plays, _warn, stats) = collect_packed(&src);
        assert_eq!(stats.text("dac_mode", ""), "source");
        assert_eq!(samples.len(), 1);
        assert_eq!(plays.len(), 1);
        assert_eq!(samples[0].c4_rate, 11025);
        let pan_points: Vec<i32> = stats.points("dac_pan_points").iter().map(|p| p.1).collect();
        assert_eq!(pan_points, vec![0xFF, 0xF0, 0x0F]);
        assert_eq!(plays[0].pan, 0xFF);
        let song = analyze(
            &src,
            AnalyzeOptions {
                speed: Some(6),
                ..Default::default()
            },
        )
        .expect("analyze");
        assert_eq!(song.dac_count, 1);
        let note_pans: BTreeSet<i32> =
            song.events.iter().filter(|e| e.dac).map(|e| e.pan).collect();
        assert_eq!(note_pans, BTreeSet::from([0xFF]));
        let pans: BTreeSet<i32> = song
            .channel_fx
            .iter()
            .filter(|fx| fx.2 == 0x08 && (fx.3 == 0xF0 || fx.3 == 0x0F))
            .map(|fx| fx.3)
            .collect();
        assert_eq!(pans, BTreeSet::from([0xF0, 0x0F]));
        let fur = tmp_dir().join("dac_chunks.fur");
        std::fs::write(&fur, write_fur(&song, WriteOptions::default())).expect("write");
        let m = parse_fur(&std::fs::read(&fur).expect("read")).expect("parse");
        assert_eq!(m.samples.len(), 1);
        let notes: Vec<_> = m
            .patterns
            .get(&(5, 0))
            .map(|rows| rows.iter().filter(|r| r.note >= 0).collect())
            .unwrap_or_default();
        assert_eq!(notes.len(), 1);
        let mut pans = BTreeSet::new();
        for rows in m.patterns.values() {
            for r in rows {
                for &(cmd, val) in &r.fx {
                    if cmd == 0x08 {
                        pans.insert(val);
                    }
                }
            }
        }
        for want in [0xFFu8, 0xF0, 0x0F] {
            assert!(pans.contains(&want), "{:?}", pans);
        }
    }

    #[test]
    fn test_port0_b6_is_channel_three_not_the_dac_pan() {
        // $B6 on port 0 panning channel 3 used to be read as the DAC's pan.
        // The DAC is FM6, whose pan lives on port 1 ($1B6).
        let (bank, events) = runs(&[(0, vec![0x80, 0x90, 0xA0, 0xB0], 4)]);
        let mut writes = marker();
        writes.push(ChipWrite::new(0, "ym2612", 0, 0xB6, 0x40));
        let mut src = vgm_with(writes, 1000, CLOCK);
        src.dac_data = bank.clone();
        src.dac_events = events.clone();
        let (_samples, plays, _warn, _stats) = collect_packed(&src);
        assert_eq!(plays[0].pan, -1);

        let mut writes = marker();
        writes.push(ChipWrite::new(0, "ym2612", 0, 0x1B6, 0x40));
        let mut src = vgm_with(writes, 1000, CLOCK);
        src.dac_data = bank;
        src.dac_events = events;
        let (_samples, plays, _warn, _stats) = collect_packed(&src);
        assert_eq!(plays[0].pan, 0x0F);
    }
}

#[allow(non_snake_case)]
mod TestYm2612Pitch {
    use super::*;

    #[test]
    fn test_note_fnum_is_the_furnace_mapping() {
        for note in [60, 72, 84, 96, 108, 115] {
            let (block, fnum) = ym2612_note_fnum(note, CLOCK, 0x80, YM2612_FREQ_BASE);
            let fbase = 440.0 * 2.0f64.powf(((note * 128 - 7296) as f64) / 1536.0);
            let bf = vgm2fur::parity::py_round(fbase * (YM2612_FREQ_BASE / CLOCK as f64)) as i64;
            let want_block = (0.max(7.min((note - 60) / 12))) as i64;
            assert_eq!((block as i64, fnum as i64), (want_block, bf >> want_block));
        }
    }

    #[test]
    fn test_note_fnum_matches_exported_register() {
        // The Genesis platform uses fmFreqBase 9437184, so note 96 is C-4
        // (261 Hz) at 7.67 MHz: block 3, f-num 643.
        assert_eq!(ym2612_note_fnum(96, CLOCK, 0x80, YM2612_FREQ_BASE), (3, 643));
        assert_eq!(furnace_note_name(96), "C-3");
    }

    #[test]
    fn test_e5_is_one_128th_semitone() {
        let (note, e5) = ym2612_note_for(3, 1646, CLOCK, YM2612_FREQ_BASE);
        let (b, f) = ym2612_note_fnum(note, CLOCK, e5, YM2612_FREQ_BASE);
        assert!((ym2612_pitch_units(b, f) - ym2612_pitch_units(3, 1646)).abs() < 0.2);
    }

    #[test]
    fn test_round_trip_through_e5() {
        for (block, fnum) in [(4, 1100), (4, 1232), (3, 923), (3, 1466), (2, 2016)] {
            let (note, e5) = ym2612_note_for(block, fnum, CLOCK, YM2612_FREQ_BASE);
            let (b, f) = ym2612_note_fnum(note, CLOCK, e5, YM2612_FREQ_BASE);
            assert!(
                (ym2612_pitch_units(b, f) - ym2612_pitch_units(block, fnum)).abs() < 0.2
            );
        }
    }

    #[test]
    fn test_pitch_units_ignore_block_equivalents() {
        assert_almost_eq!(ym2612_pitch_units(3, 1000), ym2612_pitch_units(4, 500), 9);
    }
}

#[allow(non_snake_case)]
mod TestYm2612State {
    use super::*;

    #[test]
    fn test_frequency_latch_needs_high_then_low() {
        let mut st = YM2612State::new();
        st.write(0xA0, 0xBE); // low before any high: no commit
        assert_eq!(st.frequency(0), (0, 0));
        st.write(0xA4, 0x12); // high
        assert_eq!(st.frequency(0), (0, 0));
        st.write(0xA0, 0xA5); // low commits
        assert_eq!(st.frequency(0), (2, 0x2A5));
        st.write(0xA4, 0x1A); // high alone changes nothing
        assert_eq!(st.frequency(0), (2, 0x2A5));
        st.write(0xA0, 0xD3);
        assert_eq!(st.frequency(0), (3, 0x2D3));
    }

    #[test]
    fn test_bank_one_is_channels_four_to_six() {
        let mut st = YM2612State::new();
        st.write(0x130, 0x25);
        assert_eq!(st.ops[3][0].dt_mul & 0x0F, 0x05);
        assert_eq!(st.ops[0][0].dt_mul, 0);
        st.write(0x1A4, 0x1A);
        st.write(0x1A0, 0xD3);
        assert_eq!(st.frequency(3), (3, 0x2D3));
        assert_eq!(st.frequency(0), (0, 0));
    }

    #[test]
    fn test_special_mode_pairs() {
        let mut st = YM2612State::new();
        st.write(0x27, 0x40); // special mode
        st.write(0xAC, 0x22); // group 0 high
        st.write(0xA8, 0x05); // group 0 low
        st.write(0xAD, 0x24); // group 1
        st.write(0xA9, 0xB9);
        st.write(0xAE, 0x1A); // group 2
        st.write(0xAA, 0xD3);
        assert_eq!(st.frequency_op(2, 0), (4, 0x205));
        assert_eq!(st.frequency_op(2, 1), (4, 0x4B9));
        assert_eq!(st.frequency_op(2, 2), (3, 0x2D3));
        assert_eq!(st.frequency_op(2, 3), (0, 0));
    }
}

#[allow(non_snake_case)]
mod TestYm2612DacSourceSamples {
    use super::*;

    #[test]
    fn test_short_read_is_a_prefix_of_the_longer_bank_run() {
        let body: Vec<u8> = (0..64u8).map(|i| 0x80 + i).collect();
        let mut events: Vec<(i64, u8, i64)> = vec![(0, 0, 0)];
        events.extend((0..64).map(|i| (i * 4, 1, 4)));
        events.push((20000, 0, 0));
        events.extend((0..16).map(|i| (20000 + i * 4, 1, 4)));
        events.push((40000, 0, 64));
        events.extend((0..8).map(|i| (40000 + i * 4, 1, 4)));
        let mut bank = body.clone();
        bank.extend((0..8u8).map(|i| 0x80 + i));
        let mut src = vgm_with(vec![ChipWrite::new(0, "ym2612", 0, 0x22, 0)], 60000, CLOCK);
        src.dac_data = bank;
        src.dac_events = events;
        let (samples, plays, warn, stats) = collect_packed(&src);
        assert!(!warn.iter().any(|w| !w.is_empty()));
        assert_eq!(stats.text("dac_mode", ""), "source");
        assert_eq!(samples.len(), 2);
        assert_eq!(plays.len(), 3);
        assert_eq!(samples[0].data.len(), 64);
        let song = analyze(
            &src,
            AnalyzeOptions {
                speed: Some(6),
                ..Default::default()
            },
        )
        .expect("analyze");
        // The 16-byte read reuses the 64-byte body and is cut by the next
        // note.  With only two bodies there is no reason to cluster further,
        // so the distinct 8-byte one-shot keeps its own slot.
        assert_eq!(song.dac_samples.len(), 2);
        assert_eq!(song.dac_count, 3);
    }

    #[test]
    fn test_similar_bodies_move_the_note_by_their_offset() {
        // The same bank audio reached from a different row position has a
        // few frames of extra leading fill. One body serves both, and the
        // shorter play's note moves so the shared main waveform stays put.
        let common: Vec<u8> = (0..400).map(|i| ((i * 7) % 251) as u8).collect();
        let mut long_data = vec![0u8; 20];
        long_data.extend_from_slice(&common);
        let mut short_data = vec![0x09u8; 5];
        short_data.extend_from_slice(&common);
        let long = DacSample {
            start: 0,
            length: 20 + common.len() as i64,
            data: long_data,
            c4_rate: 44100,
            onset: 0,
        };
        let short = DacSample {
            start: 0,
            length: 5 + common.len() as i64,
            data: short_data,
            c4_rate: 44100,
            onset: 0,
        };
        let notes = vec![(1000i64, 0i32, 255i32), (5000, 1, 255)];
        let (samples, moved, offs) = _cluster_similar_samples(
            vec![long.clone(), short.clone()],
            notes,
            vec![(1usize, 9000i64)],
            &mut DacStats::new(),
        );
        assert_eq!(samples, vec![long, short]);
        let idxs: BTreeSet<i32> = moved.iter().map(|m| m.1).collect();
        assert_eq!(idxs, BTreeSet::from([0]));
        assert_eq!(moved[0], (1000, 0, 255));
        // b[i] == a[i + 15], so the note starts 15 frames earlier at 44.1k.
        assert_eq!(moved[1], (4985, 0, 255));
        assert_eq!(offs, vec![(1usize, 8985i64)]);
    }

    #[test]
    fn test_unique_contiguous_samples_become_one_body() {
        let first = DacSample {
            start: 0,
            length: 4,
            data: vec![0x01, 0x02, 0x03, 0x04],
            c4_rate: 1000,
            onset: 0,
        };
        let second = DacSample {
            start: 0,
            length: 4,
            data: vec![0x05, 0x06, 0x07, 0x08],
            c4_rate: 1000,
            onset: 0,
        };
        let notes = vec![(0i64, 0i32, 255i32), (176, 1, 255)];
        let (samples, merged, offs) = _merge_one_shot_samples(
            vec![first, second],
            notes,
            vec![(0usize, 176i64), (1usize, 352i64)],
            &mut DacStats::new(),
        );
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].0, 0);
        assert_eq!(merged[0].2, 255);
        assert_eq!(
            samples[merged[0].1 as usize].data,
            vec![0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08]
        );
        // The intermediate note's cut is gone; the chain's own end stays.
        assert_eq!(offs, vec![(0usize, 176i64)]);
    }

    #[test]
    fn test_long_held_gap_splits_the_bank_run() {
        // Offset 0 is a kick; the driver then holds silence for 20,000
        // samples and continues at offset 4 with a snare. The post part must
        // keep its own offset and onset, not play 0.45 s late inside the kick.
        let run = SourceRun {
            t: 0,
            off: 0,
            fresh: false,
            pitch: 11025,
            bytes: vec![1, 2, 3, 0, 9, 9, 9, 9],
            times: vec![0, 4, 8, 12, 20012, 20016, 20020, 20024],
            ..Default::default()
        };
        let mut stats = DacStats::new();
        let pieces = _split_silent_runs(vec![run], &mut stats);
        assert_eq!(stats.int("dac_source_silence_splits"), 1);
        assert_eq!(pieces.len(), 2);
        let (pre, post) = (&pieces[0], &pieces[1]);
        assert_eq!((pre.t, pre.off, pre.bytes.clone()), (0, 0, vec![1, 2, 3, 0]));
        assert_eq!(
            (post.t, post.off, post.bytes.clone()),
            (20012, 4, vec![9, 9, 9, 9])
        );
    }

    #[test]
    fn test_body_over_40_percent_silence_is_split() {
        let mut body = vec![0x01u8; 1000];
        body.extend(vec![0x00u8; 4500]);
        body.extend(vec![0x02u8; 4500]);
        let smp = DacSample {
            start: 0,
            length: body.len() as i64,
            data: body,
            c4_rate: 44100,
            onset: 0,
        };
        let notes = vec![(0i64, 0i32, 255i32)];
        let (samples, moved, offs) = _split_long_silence(
            vec![smp],
            notes,
            vec![],
            &mut DacStats::new(),
        );
        assert_eq!(samples.len(), 3); // original plus the two parts
        assert_eq!(moved.len(), 2);
        assert_eq!(moved[0].0, 0);
        assert_eq!(moved[1].0, 5500); // 1,000 + 4,500 frames at 44.1k
        assert_eq!(samples[moved[0].1 as usize].data, vec![0x01u8; 1000]);
        assert_eq!(samples[moved[1].1 as usize].data, vec![0x02u8; 4500]);
        assert!(offs.is_empty());
    }

    #[test]
    fn test_short_body_keeps_its_silence() {
        // The 8,192-frame floor is the whole body, not the silent run.
        let mut body = vec![0x01u8; 2000];
        body.extend(vec![0x00u8; 3000]);
        let smp = DacSample {
            start: 0,
            length: body.len() as i64,
            data: body,
            c4_rate: 44100,
            onset: 0,
        };
        let (samples, moved, _offs) = _split_long_silence(
            vec![smp],
            vec![(0i64, 0i32, 255i32)],
            vec![],
            &mut DacStats::new(),
        );
        assert_eq!(samples.len(), 1);
        assert_eq!(moved, vec![(0, 0, 255)]);
    }

    #[test]
    fn test_over_budget_tail_is_one_final_sample() {
        // When the chunk fallback still needs more than the budget, the whole
        // remaining sequence goes into the last sample instead of vanishing.
        // NOTE: the Python test patches `DAC_RELEASE_SAMPLES` down to 4; the
        // Rust constant is not injectable, so the call uses the release value.
        let mut events: Vec<(i64, u8, i64)> = Vec::new();
        let mut t = 0i64;
        for k in 0..10 {
            // Vary the bytes inside each hit: a constant body is a flat DC
            // line and is now dropped before the sample map.
            for i in 0..20i64 {
                events.push((t + i * 4, 2, 0x91 + ((k + i as i64) % 7)));
            }
            t += 20 * 4 + 3000;
        }
        let mut src = vgm_with(vec![ChipWrite::new(0, "ym2612", 0, 0x2B, 0x80)], t + 1000, CLOCK);
        src.dac_events = events;
        let (samples, plays, warn, stats) = collect_packed_with_budget(&src, 4);
        assert!(
            samples.len() <= 4,
            "samples={} stats={:?}",
            samples.len(),
            stats
        );
        // The ten hits share seven distinct bodies (the body repeats every
        // seven hits); three stay exact and the four distinct tail bodies
        // join the final sample instead of being dropped.
        assert_eq!(stats.int("dac_release_merged_tail"), 4);
        assert_eq!(stats.int("dac_release_trimmed"), 0);
        assert!(plays.len() <= 10);
        assert!(!plays.is_empty());
        assert!(warn
            .iter()
            .any(|w| w.contains("joined into the final sample")));
    }

    #[test]
    fn test_dac_disable_splits_the_pointer_run() {
        // Gunstar Heroes holds the pointer while $2B turns the DAC off and on
        // again. The post-enable bytes resume further into the bank, so they
        // are a new source segment; a single held run would lose the snare.
        let bank: Vec<u8> = (0..64u8).map(|i| 0x80 + i).collect();
        let mut events: Vec<(i64, u8, i64)> = vec![(0, 0, 0)];
        events.extend((0..32).map(|i| (i * 4, 1, 4)));
        events.extend((0..32).map(|i| (20000 + i * 4, 1, 4)));
        let writes = vec![
            ChipWrite::new(0, "ym2612", 0, 0x22, 0),
            ChipWrite::new(0, "ym2612", 0, 0x2B, 0x80),
            ChipWrite::new(128, "ym2612", 0, 0x2B, 0x00),
            ChipWrite::new(20000, "ym2612", 0, 0x2B, 0x80),
        ];
        let mut src = vgm_with(writes, 40000, CLOCK);
        src.dac_data = bank;
        src.dac_events = events;
        let (samples, plays, warn, stats) = collect_packed(&src);
        assert!(!warn.iter().any(|w| !w.is_empty()));
        assert_eq!(stats.text("dac_mode", ""), "source");
        assert_eq!(stats.int("dac_source_runs"), 2);
        assert_eq!(samples.len(), 2);
        assert_eq!(
            samples.iter().map(|s| s.start).collect::<Vec<_>>(),
            vec![0, 32]
        );
        let mut onsets: Vec<i64> = plays.iter().map(|p| p.onset).collect();
        onsets.sort();
        assert_eq!(onsets, vec![0, 20000]);
        let song = analyze(
            &src,
            AnalyzeOptions {
                speed: Some(6),
                ..Default::default()
            },
        )
        .expect("analyze");
        assert_eq!(song.dac_count, 2);
        let notes: Vec<i32> = song
            .events
            .iter()
            .filter(|e| e.dac)
            .map(|e| e.note)
            .collect();
        assert_eq!(notes, vec![60, 61]);
    }
}

#[allow(non_snake_case)]
mod TestYm2612DacBudgetReuse {
    use super::*;

    #[test]
    fn test_repeated_bank_reads_share_one_sample() {
        // A drum driver points back at the same bank offset for every hit.
        // Each run must reuse the stored body by offset instead of adding a
        // module sample per hit, or a busy track needs hundreds of slots.
        let body: Vec<u8> = (0..64).map(|i| 0x80 + (i % 31)).collect();
        let mut events: Vec<(i64, u8, i64)> = Vec::new();
        for hit in 0..40i64 {
            let t = hit * 5000;
            events.push((t, 0, 0));
            events.extend((0..8).map(|i| (t + i * 4, 1, 4)));
        }
        let mut src = vgm_with(
            fm_note(4, 1204, 0, 500, 0, 0, None, [0; 4], [0; 4], [0; 4]),
            210000,
            CLOCK,
        );
        src.dac_data = body;
        src.dac_events = events;
        let song = analyze(
            &src,
            AnalyzeOptions {
                speed: Some(6),
                ..Default::default()
            },
        )
        .expect("analyze");
        assert_eq!(song.dac_stats.text("dac_mode", ""), "source");
        assert_eq!(song.dac_samples.len(), 1);
        assert!(song.dac_count as usize > song.dac_samples.len());
        assert!(song.dac_samples.len() <= DAC_RELEASE_SAMPLES);
        assert_eq!(song.dac_stats.int("dac_release_trimmed"), 0);
        assert!(!song
            .warnings
            .iter()
            .any(|w| w.contains("dropped to stay inside")));
    }
}

#[allow(non_snake_case)]
mod TestYm2612DacBanking {
    use super::*;

    /// `dac_events` for `[(time, offset, count, wait)]`.
    ///
    /// Each entry points the bank cursor at `offset` and reads `count`
    /// bytes from that fixed address, so the same bank body is deliberately
    /// read at two different cadences.
    fn bank_events(spec: &[(i64, i64, usize, i64)]) -> Vec<(i64, u8, i64)> {
        let mut events: Vec<(i64, u8, i64)> = Vec::new();
        for &(t, offset, count, wait) in spec {
            events.push((t, 0, offset));
            events.extend((0..count).map(|i| (t + i as i64 * wait, 1, wait)));
        }
        events
    }

    fn marker2() -> Vec<ChipWrite> {
        vec![ChipWrite::new(0, "ym2612", 0, 0x2B, 0x80)]
    }

    #[test]
    fn test_cadence_jitter_shares_the_longest_bank_body() {
        // The same bank offset is read long and short, both at the same
        // nominal cadence with the usual 4/5-sample wait jitter.  The trimmed
        // mean keeps them one cluster; the longest read must serve both and
        // the short note is cut later.
        let body: Vec<u8> = (0..64).map(|i| 0x80 + (i % 37 + 1)).collect();
        let mut events: Vec<(i64, u8, i64)> = vec![(0, 0, 0)];
        let mut t = 0i64;
        for i in 0..64i64 {
            let wait = if i % 2 == 0 { 4 } else { 5 };
            events.push((t, 1, wait));
            t += wait;
        }
        events.push((20000, 0, 0));
        t = 20000;
        for i in 0..16i64 {
            let wait = if i % 2 == 0 { 5 } else { 4 };
            events.push((t, 1, wait));
            t += wait;
        }
        let mut src = vgm_with(marker2(), 40000, CLOCK);
        src.dac_data = body;
        src.dac_events = events;
        let (samples, plays, warn, stats) = collect_packed(&src);
        // Mixed 4/5 waits necessarily collide on a 4-sample frame grid; that
        // is the rate-exact layout warning, not a clustering failure.
        assert!(!warn
            .iter()
            .any(|message| !message.contains("rounded to an already-written")));
        assert_eq!(stats.text("dac_mode", ""), "source");
        assert_eq!(samples.len(), 1);
        assert_eq!(plays.len(), 2);
        assert_eq!(
            plays.iter().map(|p| p.sample).collect::<BTreeSet<_>>(),
            BTreeSet::from([0])
        );
        assert_eq!(stats.int("dac_source_runs_reused"), 1);
    }

    #[test]
    fn test_flatline_and_near_silent_bodies_take_no_slot() {
        let mut bank: Vec<u8> = Vec::new();
        bank.extend((0..16).map(|i| 0x80 + (i % 29 + 1)));
        bank.extend(vec![0x90u8; 16]); // flat DC
        bank.extend((0..16).map(|i| 0x80 + (i % 2))); // peak 1
        let mut events: Vec<(i64, u8, i64)> = Vec::new();
        events.push((0, 0, 0));
        events.extend((0..16).map(|i| (i * 4, 1, 4)));
        events.push((10000, 0, 16));
        events.extend((0..16).map(|i| (10000 + i * 4, 1, 4)));
        events.push((20000, 0, 32));
        events.extend((0..16).map(|i| (20000 + i * 4, 1, 4)));
        let mut src = vgm_with(marker2(), 30000, CLOCK);
        src.dac_data = bank;
        src.dac_events = events;
        let (samples, plays, _warn, stats) = collect_packed(&src);
        assert_eq!(stats.text("dac_mode", ""), "source");
        assert_eq!(samples.len(), 1);
        assert_eq!(plays.len(), 1);
        assert!(stats.int("dac_source_quiet_runs") > 0);
    }

    #[test]
    fn test_attack_fallback_reuses_repeated_hits() {
        // Direct 0x2A writes, so there is no bank offset to bank on.  The
        // fallback still has to split at the rests and store the repeated hit
        // once instead of cutting the whole stream into time chunks.
        let hit: Vec<i64> = (0..20).map(|i| 0x90 + (i % 7)).collect();
        let mut events: Vec<(i64, u8, i64)> = Vec::new();
        let mut t = 0i64;
        for _ in 0..6 {
            events.extend(hit.iter().enumerate().map(|(i, &v)| (t + i as i64 * 4, 2, v)));
            t += hit.len() as i64 * 4 + 4000;
        }
        let mut src = vgm_with(marker2(), t + 1000, CLOCK);
        src.dac_events = events;
        let (samples, plays, warn, stats) = collect_packed(&src);
        assert!(!warn.iter().any(|w| !w.is_empty()));
        assert_eq!(stats.text("dac_mode", ""), "attacks");
        assert_eq!(samples.len(), 1);
        assert_eq!(plays.len(), 6);
        assert_eq!(stats.int("dac_attack_segments"), 6);
    }

    #[test]
    fn test_attack_fallback_prefers_a_longer_prefix_body() {
        // The first hit is a prefix of the second at the same rate; the short
        // play must point at the longer body and be cut by its own span.
        let body: Vec<i64> = (0..32).map(|i| 0x90 + (i % 7)).collect();
        let mut events: Vec<(i64, u8, i64)> = Vec::new();
        let mut t = 0i64;
        for length in [16usize, 32] {
            events.extend(
                body[..length]
                    .iter()
                    .enumerate()
                    .map(|(i, &v)| (t + i as i64 * 4, 2, v)),
            );
            t += length as i64 * 4 + 4000;
        }
        let mut src = vgm_with(marker2(), t + 1000, CLOCK);
        src.dac_events = events;
        let (samples, plays, _warn, stats) = collect_packed(&src);
        assert_eq!(stats.text("dac_mode", ""), "attacks");
        assert_eq!(samples.len(), 1);
        assert_eq!(plays.len(), 2);
        assert_eq!(
            plays.iter().map(|p| p.sample).collect::<BTreeSet<_>>(),
            BTreeSet::from([0])
        );
        assert_eq!(stats.int("dac_attack_prefix_reused"), 1);
    }
}

#[allow(non_snake_case)]
mod TestYm2612DacFragmentClustering {
    use super::*;

    fn body(seed: u64) -> Vec<u8> {
        let mut value = seed * 1234567 + 1;
        let mut out = Vec::new();
        for _ in 0..64 {
            value = (value * 1103515245 + 12345) & 0x7FFFFFFF;
            out.push(((value >> 16) & 0xFF) as u8);
        }
        out
    }

    fn song() -> (Vec<DacSample>, Vec<DacPlay>) {
        let samples: Vec<DacSample> = (0..40i64)
            .map(|i| DacSample {
                start: i,
                length: 64,
                data: body(i as u64),
                c4_rate: 14700,
                onset: i * 4410,
            })
            .collect();
        let mut plays = Vec::new();
        // Sample 0 is the hot drum: ten uses.  The rest are one-shots.
        for hit in 0..10i64 {
            plays.push(DacPlay::with_flags(
                hit * 4410,
                0,
                (hit * 37) as i32 & 0xFF,
                1000,
                false,
                true,
                false,
                None,
            ));
        }
        for i in 1..40i64 {
            plays.push(DacPlay::with_flags(
                (10 + i) * 4410,
                i as i32,
                (i * 53) as i32 & 0xFF,
                1000,
                false,
                true,
                false,
                None,
            ));
        }
        plays.sort_by_key(|p| p.onset);
        (samples, plays)
    }

    #[test]
    fn test_sample_map_stays_within_the_target_when_enabled() {
        let (samples, plays) = song();
        let (out, notes, _offs, _pans, warn, stats) = group_dac_plays(
            &samples,
            &plays,
            &GroupDacOptions {
                hz: 60.0,
                speed: 6,
                total_samples: 300000,
                trim: false,
                cluster_fragments: true,
                ..Default::default()
            },
        );
        assert!(!warn.iter().any(|w| !w.is_empty()));
        assert!(out.len() <= DAC_TARGET_SAMPLES as usize);
        assert_eq!(notes.len(), plays.len());
        assert!(notes.iter().all(|&(_t, idx, _p)| 0 <= idx && (idx as usize) < out.len()));
        // The ten-use body is one of the anchors.
        let bodies: BTreeSet<Vec<u8>> = out.iter().map(|s| s.data.clone()).collect();
        assert!(bodies.contains(&samples[0].data));
        assert!(stats.int("dac_fragment_clustered") >= 1);
    }

    #[test]
    fn test_clustering_is_off_by_default() {
        let (samples, plays) = song();
        let (out, notes, _offs, _pans, warn, _stats) = group_dac_plays(
            &samples,
            &plays,
            &GroupDacOptions {
                hz: 60.0,
                speed: 6,
                total_samples: 300000,
                trim: false,
                ..Default::default()
            },
        );
        assert!(!warn.iter().any(|w| !w.is_empty()));
        assert!(out.len() > DAC_TARGET_SAMPLES as usize);
        assert_eq!(notes.len(), plays.len());
    }
}

#[allow(non_snake_case)]
mod TestYm2612DacPitchClusters {
    use super::*;

    fn run(t: i64, waits: &[i64], seed: i64) -> SourceRun {
        let mut times = vec![t];
        for &wait in waits {
            times.push(times.last().unwrap() + wait);
        }
        SourceRun {
            t,
            off: 1515,
            bytes: (0..=waits.len())
                .map(|i| (0x80 + (seed * 17 + i as i64 * 7) % 61) as u8)
                .collect(),
            times,
            fresh: false,
            ..Default::default()
        }
    }

    #[test]
    fn test_jitter_merges_but_a_real_pitch_step_splits() {
        let repeats = |a: i64, b: i64| {
            let mut v = Vec::new();
            for _ in 0..8 {
                v.push(a);
                v.push(b);
            }
            v
        };
        let runs = vec![
            run(0, &repeats(4, 5), 1),
            run(10_000, &repeats(5, 4), 2),
            run(20_000, &repeats(6, 7), 3),
            run(30_000, &repeats(7, 6), 4),
        ];
        let (samples, plays) = _collect_source(runs, &mut DacStats::new(), Some(&[]));
        assert_eq!(samples.len(), 2);
        let mut ordered = plays.clone();
        ordered.sort_by_key(|p| p.onset);
        assert_eq!(
            ordered.iter().map(|p| p.sample).collect::<Vec<_>>(),
            vec![0, 0, 1, 1]
        );
        assert_eq!(
            samples.iter().map(|s| s.c4_rate).collect::<BTreeSet<_>>(),
            BTreeSet::from([11025, 7350])
        );
    }

    #[test]
    fn test_one_short_gap_does_not_raise_the_playback_rate() {
        // A 4-wait read with a single 3-wait command boundary is still
        // 11,025 Hz.  Taking the smallest interval made the whole body play a
        // fourth too high whenever it was reused (Alien Soldier's E-0 drum).
        let mut waits = vec![4i64; 63];
        waits[31] = 3;
        let (samples, plays) =
            _collect_source(vec![run(0, &waits, 1)], &mut DacStats::new(), Some(&[]));
        assert_eq!(samples.len(), 1);
        assert_eq!(plays.len(), 1);
        assert_eq!(samples[0].c4_rate, 11025);
    }
}

#[allow(non_snake_case)]
mod TestSampleMapCleanup {
    use super::*;
    use vgm2fur::furwrite::{_compact_sample_map_notes, _shift_sample_map_instruments, Row};

    #[test]
    fn test_unused_slots_are_compacted_and_later_instruments_shift() {
        let mut rows = vec![
            Row { note: 60, ins: 0, ..Default::default() },
            Row { note: 62, ins: 0, ..Default::default() },
            Row { note: 60, ins: 2, ..Default::default() },
        ];
        let mut patterns = vec![(0i32, 0i32, rows.iter().cloned().map(Some).collect::<Vec<_>>())];
        let samples: Vec<i32> = (0..240).collect();
        let (samples2, count) =
            _compact_sample_map_notes(&mut patterns, 0, 2, &samples);
        assert_eq!(samples2, vec![&0i32, &2i32]);
        assert_eq!(count, 1);
        let out_rows = &patterns[0].2;
        assert_eq!((out_rows[0].as_ref().unwrap().note, out_rows[0].as_ref().unwrap().ins), (60, 0));
        assert_eq!((out_rows[1].as_ref().unwrap().note, out_rows[1].as_ref().unwrap().ins), (61, 0));
        assert_eq!((out_rows[2].as_ref().unwrap().note, out_rows[2].as_ref().unwrap().ins), (60, 2));
        _shift_sample_map_instruments(&mut patterns, 2, -1);
        let out_rows = &patterns[0].2;
        assert_eq!((out_rows[2].as_ref().unwrap().note, out_rows[2].as_ref().unwrap().ins), (60, 1));
        // Keep the unused bindings alive for the compiler's view of the port.
        let _ = rows.pop();
    }
}

// ---------------------------------------------------------------------------
// Pass 2: analysis, writer and round-trip.
// ---------------------------------------------------------------------------

#[allow(non_snake_case)]
mod TestYm2612Analyze {
    use super::*;

    #[test]
    fn test_fm_note_and_patch() {
        let song = analyze(&vgm_default(fm_note_default(4, 1232)), AnalyzeOptions::default())
            .expect("analyze");
        assert_eq!(song.fm_chip, "ym2612");
        assert_eq!(song.ym2612_ids, vec![0]);
        assert!(!song.ym2612_ext);
        assert_eq!(song.fm_patches.len(), 1);
        let p = &song.fm_patches[0];
        assert!(p.opn);
        assert_eq!(p.alg, 7);
        let ev = song.events.iter().find(|e| e.on).expect("note on");
        assert_eq!(ev.ch, 0);
        assert_eq!(
            (ev.note, ev.e5),
            ym2612_note_for(4, 1232, CLOCK, YM2612_FREQ_BASE)
        );
        assert_eq!(ev.ins, 0);
        // The carrier TL moved into the pattern volume column (the test
        // patch's carrier is at TL 0, so the column is wide open).
        assert_eq!(ev.vol, 127);
    }

    #[test]
    fn test_key_off_event_and_end() {
        let song = analyze(
            &vgm_default(fm_note(4, 1232, 100, 60000, 0, 0, None, [0; 4], [0; 4], [0; 4])),
            AnalyzeOptions::default(),
        )
        .expect("analyze");
        let on = song.events.iter().find(|e| e.on).expect("note on");
        assert_eq!(on.end, 60000);
        let off: Vec<i64> = song
            .events
            .iter()
            .filter(|e| !e.on && e.ch == 0)
            .map(|e| e.sample)
            .collect();
        assert_eq!(off, vec![60000]);
    }

    #[test]
    fn test_retrigger_off_is_dropped() {
        let mut ws = fm_note(4, 1232, 100, 0, 0, 0, None, [0; 4], [0; 4], [0; 4]);
        ws.extend([
            ChipWrite::new(5000, "ym2612", 0, 0x28, 0x00),
            ChipWrite::new(5001, "ym2612", 0, 0x28, 0xF0),
        ]);
        let song = analyze(&vgm_default(ws), AnalyzeOptions::default()).expect("analyze");
        let offs: Vec<_> = song
            .events
            .iter()
            .filter(|e| !e.on && e.ch == 0)
            .collect();
        assert!(offs.is_empty());
    }

    #[test]
    fn test_channels_four_to_six_use_bank_one() {
        let mut ws = fm_note_default(4, 1232);
        ws.extend(fm_note(4, 1466, 200, 60000, 0, 3, None, [0; 4], [0; 4], [0; 4]));
        ws.extend(fm_note(4, 1204, 300, 60000, 0, 5, None, [0; 4], [0; 4], [0; 4]));
        let song = analyze(&vgm_default(ws), AnalyzeOptions::default()).expect("analyze");
        let mut chans: Vec<i32> = song.events.iter().filter(|e| e.on).map(|e| e.ch).collect();
        chans.sort();
        assert_eq!(chans, vec![0, 3, 5]);
    }

    #[test]
    fn test_mid_note_fnum_becomes_pitch_step() {
        let mut ws = fm_note(4, 1204, 100, 60000, 0, 0, None, [0; 4], [0; 4], [0; 4]);
        ws.extend([
            ChipWrite::new(30000, "ym2612", 0, 0xA4, (4 << 3) | ((1250 >> 8) & 7)),
            ChipWrite::new(30000, "ym2612", 0, 0xA0, 1250 & 0xFF),
        ]);
        let song = analyze(&vgm_default(ws), AnalyzeOptions::default()).expect("analyze");
        let ev = song.events.iter().find(|e| e.on).expect("note on");
        assert!(ev.fm_pitch_pts.len() >= 2);
        // A single held step is cheaper as a row effect and must not spend an
        // instrument copy.
        assert!(!ev.pitch_steps.is_empty());
        assert_eq!(ev.ins, 0);
    }

    #[test]
    fn test_mid_note_patch_change_splits_the_held_note() {
        // A driver can load a new patch into a still-keyed voice (Time Trax
        // holds a drone through several patch swaps). The chip changes the
        // operators at once, but a tracker note keeps its instrument until the
        // next key-on, so the hold has to become a second note.
        let mut ws = fm_note(4, 1204, 100, 60000, 0, 4, None, [0; 4], [0; 4], [0; 4]);
        let t = 30000i64;
        ws.extend([
            ChipWrite::new(t, "ym2612", 0, 0x1B1, 0x3A), // alg 2, feedback 7
            ChipWrite::new(t, "ym2612", 0, 0x131, 0x08),
            ChipWrite::new(t, "ym2612", 0, 0x135, 0x24),
        ]);
        let song = analyze(&vgm_default(ws), AnalyzeOptions::default()).expect("analyze");
        let on: Vec<_> = song
            .events
            .iter()
            .filter(|e| e.on && e.ch == 4)
            .collect();
        assert_eq!(
            on.iter().map(|e| e.sample).collect::<Vec<_>>(),
            vec![100, t]
        );
        assert_eq!(on[0].end, t);
        assert_ne!(on[0].ins, on[1].ins);
        assert_eq!(song.fm_patches[on[1].ins as usize].alg, 2);
        // The key stayed down: the patch swap is a legato split, not a new
        // attack. EA01 lets the new instrument's macros take over without
        // resetting the voice; the next real key-on writes EA00.
        assert!(on[1].legato);
        let mod_ = parse_fur(&write_fur(
            &song,
            WriteOptions {
                include_pcm: false,
                include_fm: true,
                ..Default::default()
            },
        ))
        .expect("parse fur");
        let mut found = false;
        for (&(ch, _pat), rows) in &mod_.patterns {
            if ch != 4 {
                continue;
            }
            for row in rows {
                if row.note == on[1].note
                    && row.ins == on[1].ins
                    && row.fx.iter().any(|&(cmd, val)| cmd == 0xEA && val == 1)
                {
                    found = true;
                }
            }
        }
        assert!(found, "legato split lost the instrument change");
    }

    #[test]
    fn test_patch_load_before_key_on_does_not_split() {
        // The patch bytes just before a key transition belong to the next
        // note. Splitting the old note would put two notes on one row.
        let mut ws = fm_note(4, 1204, 100, 0, 0, 4, None, [0; 4], [0; 4], [0; 4]);
        ws.extend([
            ChipWrite::new(29900, "ym2612", 0, 0x1B1, 0x3A),
            ChipWrite::new(29900, "ym2612", 0, 0x131, 0x08),
            ChipWrite::new(30000, "ym2612", 0, 0x28, 0x05),
            ChipWrite::new(30001, "ym2612", 0, 0x28, 0xF5),
        ]);
        let song = analyze(&vgm_default(ws), AnalyzeOptions::default()).expect("analyze");
        let on: Vec<_> = song
            .events
            .iter()
            .filter(|e| e.on && e.ch == 4)
            .collect();
        assert_eq!(
            on.iter().map(|e| e.sample).collect::<Vec<_>>(),
            vec![100, 30001]
        );
    }

    #[test]
    fn test_long_pitch_sweep_becomes_pitch_macro() {
        let mut ws = fm_note(4, 1204, 100, 60000, 0, 0, None, [0; 4], [0; 4], [0; 4]);
        for (i, fnum) in [1210, 1260].iter().cycle().take(12).enumerate() {
            let t = 30000 + i as i64 * 1000;
            ws.extend([
                ChipWrite::new(t, "ym2612", 0, 0xA4, (4 << 3) | ((fnum >> 8) & 7)),
                ChipWrite::new(t, "ym2612", 0, 0xA0, fnum & 0xFF),
            ]);
        }
        let song = analyze(&vgm_default(ws), AnalyzeOptions::default()).expect("analyze");
        let ev = song.events.iter().find(|e| e.on).expect("note on");
        assert!(ev.fm_pitch_pts.len() > 4);
        assert_ne!(ev.ins, 0);
        let macro_ = &song.fm_patches[ev.ins as usize].pitch_macro;
        assert!(!macro_.is_empty());
        assert_eq!(macro_[0], 0);
        assert!(*macro_.iter().max().unwrap() > 0);
    }

    #[test]
    fn test_lfo_register_becomes_chip_effect() {
        let mut ws = patch(100, 0, false, 0, [0; 4], [0; 4], [0; 4]);
        ws.extend([
            ChipWrite::new(100, "ym2612", 0, 0x22, 0x0B),   // on, rate 3
            ChipWrite::new(20000, "ym2612", 0, 0x22, 0x02),  // off, rate 2
            ChipWrite::new(100, "ym2612", 0, 0xA4, 0x25),
            ChipWrite::new(100, "ym2612", 0, 0xA0, 0xBA),
            ChipWrite::new(100, "ym2612", 0, 0x28, 0xF0),
            ChipWrite::new(60000, "ym2612", 0, 0x28, 0x00),
        ]);
        let song = analyze(&vgm_default(ws), AnalyzeOptions::default()).expect("analyze");
        let lfo: Vec<(i32, i32)> = song
            .chip_fx
            .iter()
            .filter(|fx| fx.1 == 0x10)
            .map(|fx| (fx.1, fx.2))
            .collect();
        assert_eq!(lfo, vec![(0x10, 0x13), (0x10, 0x02)]);
    }

    #[test]
    fn test_carrier_tl_moves_to_volume_and_shares_the_patch() {
        // Algorithm 0: operator 4 is the only carrier.
        let mut ws = patch(100, 0, false, 0, [0; 4], [0; 4], [0; 4]);
        ws.extend([
            ChipWrite::new(100, "ym2612", 0, 0xB0, 0x00),
            ChipWrite::new(100, "ym2612", 0, 0xA4, 0x25),
            ChipWrite::new(100, "ym2612", 0, 0xA0, 0xBA),
            ChipWrite::new(100, "ym2612", 0, 0x28, 0xF0),
            ChipWrite::new(60000, "ym2612", 0, 0x28, 0x00),
            // Same timbre, carrier TL 0x30 at the next key-on.
            ChipWrite::new(70000, "ym2612", 0, 0x4C, 0x30),
            ChipWrite::new(70000, "ym2612", 0, 0x28, 0xF0),
            ChipWrite::new(80000, "ym2612", 0, 0x28, 0x00),
        ]);
        let song = analyze(&vgm_default(ws), AnalyzeOptions::default()).expect("analyze");
        let on: Vec<_> = song.events.iter().filter(|e| e.on).collect();
        assert_eq!(on.len(), 2);
        assert_eq!(on[0].ins, on[1].ins);
        assert_eq!(on[0].vol, 127);
        assert_eq!(on[1].vol, 127 - 0x30);
        assert_eq!(song.fm_patches.len(), 1);
        // The instrument keeps only the relative carrier level.
        assert_eq!(song.fm_patches[0].ops[3].tl, 0);
    }

    #[test]
    fn test_alg4_carriers_use_register_order() {
        // Algorithm 4: the real carriers are S2 and S4, i.e. registers 0x38
        // and 0x3C (op indices 2 and 3), not S3/S4 (1 and 3).
        let mut ws = patch(100, 0, false, 0, [0; 4], [0; 4], [0; 4]);
        ws.extend([
            ChipWrite::new(100, "ym2612", 0, 0xB0, 0x04),
            ChipWrite::new(100, "ym2612", 0, 0x40, 0x10), // S1 TL (modulator)
            ChipWrite::new(100, "ym2612", 0, 0x44, 0x40), // S3 TL (modulator)
            ChipWrite::new(100, "ym2612", 0, 0x48, 0x20), // S2 TL (carrier)
            ChipWrite::new(100, "ym2612", 0, 0x4C, 0x30), // S4 TL (carrier)
            ChipWrite::new(100, "ym2612", 0, 0xA4, 0x25),
            ChipWrite::new(100, "ym2612", 0, 0xA0, 0xBA),
            ChipWrite::new(100, "ym2612", 0, 0x28, 0xF0),
            ChipWrite::new(60000, "ym2612", 0, 0x28, 0x00),
        ]);
        let song = analyze(&vgm_default(ws), AnalyzeOptions::default()).expect("analyze");
        let ev = song.events.iter().find(|e| e.on).expect("note on");
        let p = &song.fm_patches[ev.ins as usize];
        assert_eq!(
            p.ops.iter().map(|op| op.tl as i32).collect::<Vec<_>>(),
            vec![0x10, 0x40, 0x00, 0x10]
        );
        assert_eq!(p.carrier_tl(), 0);
        assert_eq!(ev.vol, 127 - 0x20);
    }

    #[test]
    fn test_partial_kon_mask_mutes_the_right_operators() {
        // KON mask 0x5 enables datasheet operators S1 and S3, i.e. register
        // indices 0 (0x30) and 1 (0x34).  Register indices 2 (S2, 0x38) and
        // 3 (S4, 0x3C) must be muted.
        let mut ws = patch(100, 0, false, 0, [0; 4], [0; 4], [0; 4]);
        ws.extend([
            ChipWrite::new(100, "ym2612", 0, 0xA4, 0x25),
            ChipWrite::new(100, "ym2612", 0, 0xA0, 0xBA),
            ChipWrite::new(100, "ym2612", 0, 0x28, 0x50),
            ChipWrite::new(60000, "ym2612", 0, 0x28, 0x00),
        ]);
        let song = analyze(&vgm_default(ws), AnalyzeOptions::default()).expect("analyze");
        let ev = song.events.iter().find(|e| e.on).expect("note on");
        let p = &song.fm_patches[ev.ins as usize];
        assert_ne!(p.ops[0].tl, 0x7F);
        assert_ne!(p.ops[1].tl, 0x7F);
        assert_eq!(p.ops[2].tl, 0x7F);
        assert_eq!(p.ops[3].tl, 0x7F);
    }

    #[test]
    fn test_same_sample_writes_after_kon_are_in_the_patch() {
        let mut ws = fm_note(4, 1232, 100, 60000, 0, 0, None, [0; 4], [0; 4], [0; 4]);
        ws.extend([
            ChipWrite::new(100, "ym2612", 0, 0xB0, 0x00), // alg 0 after the KON
            ChipWrite::new(100, "ym2612", 0, 0x4C, 0x30), // carrier TL after it
            ChipWrite::new(100, "ym2612", 0, 0xB4, 0xC7), // PMS 7 after it
        ]);
        let song = analyze(&vgm_default(ws), AnalyzeOptions::default()).expect("analyze");
        let ev = song.events.iter().find(|e| e.on).expect("note on");
        let p = &song.fm_patches[ev.ins as usize];
        assert_eq!(p.alg, 0);
        assert_eq!(p.pms, 7);
        assert_eq!(p.ops[3].tl, 0);
        assert_eq!(ev.vol, 127 - 0x30);
    }

    #[test]
    fn test_pms_ams_change_while_held_becomes_effects() {
        let mut ws = fm_note(4, 1232, 100, 60000, 0, 0, None, [0; 4], [0; 4], [0; 4]);
        ws.push(ChipWrite::new(30000, "ym2612", 0, 0xB4, 0xD3)); // AMS 1, PMS 3
        let song = analyze(&vgm_default(ws), AnalyzeOptions::default()).expect("analyze");
        let fx: BTreeSet<(i32, i32)> = song
            .channel_fx
            .iter()
            .map(|&(_t, _ch, cmd, val)| (cmd, val))
            .collect();
        assert!(fx.contains(&(0x63, 1))); // AMS
        assert!(fx.contains(&(0x62, 3))); // PMS (FMS)
    }

    #[test]
    fn test_pms_ams_are_part_of_the_patch() {
        let mut ws = fm_note(4, 1232, 100, 60000, 0, 0, None, [0; 4], [0; 4], [0; 4]);
        ws.extend([
            // A second note with the same timbre but different LFO depth must
            // not reuse the first instrument.
            ChipWrite::new(70000, "ym2612", 0, 0xB4, 0xD3),
            ChipWrite::new(70000, "ym2612", 0, 0x28, 0xF0),
        ]);
        let song = analyze(&vgm_default(ws), AnalyzeOptions::default()).expect("analyze");
        let on: Vec<_> = song.events.iter().filter(|e| e.on).collect();
        assert_eq!(on.len(), 2);
        assert_ne!(on[0].ins, on[1].ins);
    }
}

#[allow(non_snake_case)]
mod TestYm2612SpecialCases {
    use super::*;

    fn special() -> Vec<ChipWrite> {
        let mut ws = patch(100, 0, false, 0, [0; 4], [0; 4], [0; 4]);
        ws.extend([
            ChipWrite::new(100, "ym2612", 0, 0x27, 0x40), // channel-3 special
            ChipWrite::new(100, "ym2612", 0, 0xAD, 0x22), // group 1 (column 0)
            ChipWrite::new(100, "ym2612", 0, 0xA9, 0x05),
            ChipWrite::new(100, "ym2612", 0, 0xAC, 0x24), // group 0 (column 2)
            ChipWrite::new(100, "ym2612", 0, 0xA8, 0xB9),
            ChipWrite::new(100, "ym2612", 0, 0xAE, 0x1A), // group 2 (column 1)
            ChipWrite::new(100, "ym2612", 0, 0xAA, 0xD3),
            ChipWrite::new(100, "ym2612", 0, 0xA6, 0x01), // normal, column 3
            ChipWrite::new(100, "ym2612", 0, 0xA2, 0x23),
            // Make the channel-3 operators audible in every column.
            ChipWrite::new(100, "ym2612", 0, 0x32, 0x01),
            ChipWrite::new(100, "ym2612", 0, 0x36, 0x01),
            ChipWrite::new(100, "ym2612", 0, 0x3A, 0x01),
            ChipWrite::new(100, "ym2612", 0, 0x3E, 0x01),
            ChipWrite::new(100, "ym2612", 0, 0xB2, 0x07),
            ChipWrite::new(100, "ym2612", 0, 0xB6, 0x40), // channel 3: right only
            ChipWrite::new(100, "ym2612", 0, 0x28, 0xF2),
            ChipWrite::new(2000, "ym2612", 0, 0x28, 0xF2), // same mask: no retrigger
            ChipWrite::new(60000, "ym2612", 0, 0x28, 0x02),
        ]);
        ws
    }

    #[test]
    fn test_special_mode_uses_ext_layout() {
        let song = analyze(&vgm_default(special()), AnalyzeOptions::default()).expect("analyze");
        assert!(song.ym2612_ext);
        let evs: Vec<_> = song.events.iter().filter(|e| e.on).collect();
        assert_eq!(evs.len(), 4);
        let mut chans: Vec<i32> = evs.iter().map(|e| e.ch).collect();
        chans.sort();
        assert_eq!(chans, vec![2, 3, 4, 5]);
        let by_ch: BTreeMap<i32, &_> = evs.iter().map(|e| (e.ch, *e)).collect();
        assert_eq!(
            (by_ch[&2].note, by_ch[&2].e5),
            ym2612_note_for(4, 0x205, CLOCK, YM2612_FREQ_BASE)
        );
        assert_eq!(
            (by_ch[&3].note, by_ch[&3].e5),
            ym2612_note_for(3, 0x2D3, CLOCK, YM2612_FREQ_BASE)
        );
        assert_eq!(
            (by_ch[&4].note, by_ch[&4].e5),
            ym2612_note_for(4, 0x4B9, CLOCK, YM2612_FREQ_BASE)
        );
        assert_eq!(
            (by_ch[&5].note, by_ch[&5].e5),
            ym2612_note_for(0, 0x123, CLOCK, YM2612_FREQ_BASE)
        );
        // The four columns share the channel's pan register.
        let pans: BTreeSet<i32> = (2..6).map(|c| by_ch[&c].pan).collect();
        assert_eq!(pans, BTreeSet::from([0x0F]));
        let mod_ = parse_fur(&write_fur(&song, WriteOptions::default())).expect("parse");
        assert_eq!(mod_.chips, vec![CHIP_YM2612_EXT]);
        assert_eq!(mod_.n_ch, 9);
    }

    #[test]
    fn test_partial_key_on_before_the_first_full_chord_is_kept() {
        // FM Towns MIDI drivers open channel 3 with a two-operator mask before
        // the first full chord. Those are real notes, not operator setup.
        let t = 20_000i64;
        let mut ws = patch(100, 0, false, 2, [0; 4], [0; 4], [0; 4]);
        ws.extend([
            ChipWrite::new(100, "ym2612", 0, 0x27, 0x40),
            ChipWrite::new(100, "ym2612", 0, 0xAD, 0x22), // group 1 (column 0)
            ChipWrite::new(100, "ym2612", 0, 0xA9, 0x6A),
            ChipWrite::new(100, "ym2612", 0, 0xAE, 0x22), // group 2 (column 1)
            ChipWrite::new(100, "ym2612", 0, 0xAA, 0x6A),
            ChipWrite::new(t, "ym2612", 0, 0x28, 0x32), // operators 1 and 2
        ]);
        let song = analyze(&vgm_default(ws), AnalyzeOptions::default()).expect("analyze");
        let mut ons: Vec<i32> = song
            .events
            .iter()
            .filter(|e| e.on && e.sample == t)
            .map(|e| e.ch)
            .collect();
        ons.sort();
        assert_eq!(ons, vec![2, 3]);
    }

    #[test]
    fn test_normal_channel_notes_feed_all_four_columns() {
        let mut ws = patch(100, 0, false, 0, [0; 4], [0; 4], [0; 4]);
        ws.extend([
            ChipWrite::new(100, "ym2612", 0, 0x27, 0x40),
            ChipWrite::new(100, "ym2612", 0, 0x32, 0x01),
            ChipWrite::new(100, "ym2612", 0, 0x36, 0x01),
            ChipWrite::new(100, "ym2612", 0, 0x3A, 0x01),
            ChipWrite::new(100, "ym2612", 0, 0x3E, 0x01),
            ChipWrite::new(100, "ym2612", 0, 0xB2, 0x07),
            ChipWrite::new(100, "ym2612", 0, 0xA6, 0x1A),
            ChipWrite::new(100, "ym2612", 0, 0xA2, 0xD3),
            ChipWrite::new(100, "ym2612", 0, 0x28, 0xF2),
            ChipWrite::new(60000, "ym2612", 0, 0x28, 0x02),
        ]);
        let song = analyze(&vgm_default(ws), AnalyzeOptions::default()).expect("analyze");
        let evs: Vec<_> = song.events.iter().filter(|e| e.on).collect();
        assert_eq!(evs.len(), 4);
        let want = ym2612_note_for(3, 0x2D3, CLOCK, YM2612_FREQ_BASE);
        assert!(evs.iter().all(|e| (e.note, e.e5) == want));
    }

    #[test]
    fn test_csm_mode_warns_but_converts() {
        let mut ws = patch(100, 0, false, 0, [0; 4], [0; 4], [0; 4]);
        ws.extend([
            ChipWrite::new(100, "ym2612", 0, 0x27, 0x80),
            ChipWrite::new(100, "ym2612", 0, 0x28, 0xF2),
            ChipWrite::new(60000, "ym2612", 0, 0x28, 0x02),
        ]);
        let song = analyze(&vgm_default(ws), AnalyzeOptions::default()).expect("analyze");
        assert!(song.ym2612_ext);
        assert!(song.ym2612_csm);
        assert!(song.warnings.iter().any(|w| w.contains("CSM")));
    }

    #[test]
    fn test_dac_key_ons_do_not_become_fm_notes() {
        let ws = vec![
            ChipWrite::new(100, "ym2612", 0, 0x2B, 0x80), // DAC on
            ChipWrite::new(100, "ym2612", 0, 0x28, 0xF5), // FM6 key-on
            ChipWrite::new(200, "ym2612", 0, 0x2A, 0x80),
            ChipWrite::new(300, "ym2612", 0, 0x28, 0x05), // FM6 key-off
        ];
        let song = analyze(&vgm_default(ws), AnalyzeOptions::default()).expect("analyze");
        assert!(song
            .events
            .iter()
            .filter(|e| e.on && e.ch == 5)
            .next()
            .is_none());
        assert!(
            song.warnings.iter().any(|w| w.contains("DAC")),
            "warnings={:?}",
            song.warnings
        );
    }

    #[test]
    fn test_pitch_stream_follows_bank_one() {
        let ws = fm_note(4, 1232, 100, 60000, 0, 4, None, [0; 4], [0; 4], [0; 4]);
        let src = vgm_default(ws);
        let stream = ym2612_pitch_stream(&src, 4, "ym2612");
        assert!(!stream.is_empty());
        assert_almost_eq!(stream.last().unwrap().1, ym2612_pitch_units(4, 1232), 6);
        assert_eq!(
            ym2612_keyed_intervals(&src, 4, "ym2612", true),
            vec![(100, 60000)]
        );
        // A channel outside the chip has no stream.
        assert!(ym2612_pitch_stream(&src, 6, "ym2612").is_empty());
    }
}

#[allow(non_snake_case)]
mod TestYm2612Writer {
    use super::*;

    fn song_of(ws: Vec<ChipWrite>) -> vgm2fur::model::Song {
        analyze(&vgm_default(ws), AnalyzeOptions::default()).expect("analyze")
    }

    #[test]
    fn test_single_chip_layout_and_ids() {
        let data = write_fur(
            &song_of(fm_note_default(4, 1232)),
            WriteOptions {
                include_pcm: false,
                ..Default::default()
            },
        );
        let m = parse_fur(&data).expect("parse");
        assert_eq!(m.chips, vec![CHIP_YM2612]);
        assert_eq!(m.n_ch, 6);
        assert_eq!(
            CHIP_CHANNELS
                .iter()
                .find(|(c, _)| *c == CHIP_YM2612)
                .unwrap()
                .1,
            6
        );
        assert!(contains_subslice(&data, b"FM 1"));
        assert!(contains_subslice(&data, b"FM 6"));
        assert!(contains_subslice(&data, b"customClock=7670453"));
        assert_eq!(ins_types(&data)[0], INS_FM);
    }

    #[test]
    fn test_fm_towns_clock_uses_the_named_setting() {
        // Furnace's Genesis clock list has a 8 MHz FM Towns entry; a real FM
        // Towns rip should select it instead of carrying a custom clock.
        let mut song = song_of(fm_note_default(4, 1232));
        song.vgm.ym2612_clock = 8_000_000;
        let data = write_fur(
            &song,
            WriteOptions {
                include_pcm: false,
                ..Default::default()
            },
        );
        assert!(contains_subslice(&data, b"clockSel=2"));
        assert!(!contains_subslice(&data, b"customClock=8000000"));
    }

    #[test]
    fn test_opn_operator_fields() {
        let sr = [0x12u32, 0x13, 0x14, 0x15];
        let ssg = [0x05u32, 0x06, 0x07, 0x08];
        let ws = fm_note(4, 1232, 100, 60000, 0, 0, None, sr, ssg, [0; 4]);
        let data = write_fur(
            &song_of(ws),
            WriteOptions {
                include_pcm: false,
                ..Default::default()
            },
        );
        let feat = fm_feature(&data);
        assert!(!feat.is_empty());
        assert_eq!(feat[3] & 0x3F, 0x20); // 4-op bit
        for op in 0..4usize {
            let base = 4 + op * 8;
            assert_eq!(feat[base + 4] & 0x1F, (0x12 + op) as u8);
            assert_eq!(feat[base + 6] & 0x0F, (0x05 + op) as u8);
        }
    }

    #[test]
    fn test_pitch_macro_rides_on_an_instrument_copy() {
        let mut ws = fm_note(4, 1204, 100, 60000, 0, 0, None, [0; 4], [0; 4], [0; 4]);
        for (i, fnum) in [1210, 1260].iter().cycle().take(12).enumerate() {
            let t = 30000 + i as i64 * 1000;
            ws.extend([
                ChipWrite::new(t, "ym2612", 0, 0xA4, (4 << 3) | ((fnum >> 8) & 7)),
                ChipWrite::new(t, "ym2612", 0, 0xA0, fnum & 0xFF),
            ]);
        }
        let song = song_of(ws);
        let ev = song.events.iter().find(|e| e.on).expect("note on");
        assert!(!song.fm_patches[ev.ins as usize].pitch_macro.is_empty());
        let data = write_fur(
            &song,
            WriteOptions {
                include_pcm: false,
                ..Default::default()
            },
        );
        assert!(
            data.windows(4).filter(|w| w == b"INS2").count() >= 2
        );
    }

    #[test]
    fn test_dual_chip_layout() {
        let mut ws = fm_note(4, 1232, 100, 60000, 0, 0, None, [0; 4], [0; 4], [0; 4]);
        ws.extend(fm_note(4, 923, 200, 60000, 1, 0, None, [0; 4], [0; 4], [0; 4]));
        let data = write_fur(
            &song_of(ws),
            WriteOptions {
                include_pcm: false,
                ..Default::default()
            },
        );
        let m = parse_fur(&data).expect("parse");
        assert_eq!(m.chips, vec![CHIP_YM2612, CHIP_YM2612]);
        assert_eq!(m.n_ch, 12);
        assert!(contains_subslice(&data, b"FM 1 (2)"));
        assert_eq!(ins_types(&data), vec![INS_FM]);
    }
}

#[allow(non_snake_case)]
mod TestYm2612Furnace {
    use super::*;

    fn export(song: &vgm2fur::model::Song, name: &str) -> VgmFile {
        let fur = tmp_dir().join(format!("{name}.fur"));
        let out = tmp_dir().join(format!("{name}.vgm"));
        std::fs::write(
            &fur,
            write_fur(
                song,
                WriteOptions {
                    include_pcm: false,
                    ..Default::default()
                },
            ),
        )
        .expect("write fur");
        vgm2fur::furnace::export_vgm(&fur, &out).expect("export vgm");
        load_vgm(&out).expect("load export")
    }

    fn pairs(vgm: &VgmFile, ch: i32) -> Vec<(i32, i32)> {
        let mut hi: Option<u32> = None;
        let mut out = Vec::new();
        let bank = ch >= 3;
        let sub = if ch >= 3 { ch - 3 } else { ch };
        for w in &vgm.writes {
            if w.chip != "ym2612" || w.chip_id != 0 {
                continue;
            }
            let r = w.reg & 0xFF;
            if ((w.reg & 0x100) != 0) != bank {
                continue;
            }
            if r == 0xA4 + sub as u32 {
                hi = Some(w.val);
            } else if r == 0xA0 + sub as u32 {
                if let Some(high) = hi {
                    out.push((
                        ((high >> 3) & 7) as i32,
                        (w.val | ((high & 7) << 8)) as i32,
                    ));
                    hi = None;
                }
            }
        }
        out
    }

    #[test]
    fn test_note_pitch_round_trip() {
        skip_without_furnace!();
        for (block, fnum) in [(2, 702), (4, 1232), (3, 923), (3, 1466)] {
            let song = analyze(
                &vgm_default(fm_note(block, fnum, 100, 60000, 0, 0, None, [0; 4], [0; 4], [0; 4])),
                AnalyzeOptions::default(),
            )
            .expect("analyze");
            let vgm = export(&song, &format!("ym2612_note_{block}_{fnum}"));
            assert_eq!(vgm.ym2612_clock, CLOCK);
            let pairs = pairs(&vgm, 0);
            let want = ym2612_pitch_units(block, fnum);
            // The export may choose an equivalent block/f-num octave; the
            // played pitch is what has to match.
            assert!(
                pairs
                    .iter()
                    .any(|&(b, f)| (ym2612_pitch_units(b, f) - want).abs() < 0.5),
                "{:?}",
                pairs
            );
        }
    }

    #[test]
    fn test_bank_one_round_trip() {
        skip_without_furnace!();
        let song = analyze(
            &vgm_default(fm_note(4, 1232, 100, 60000, 0, 4, None, [0; 4], [0; 4], [0; 4])),
            AnalyzeOptions::default(),
        )
        .expect("analyze");
        let vgm = export(&song, "ym2612_bank1");
        let want = ym2612_pitch_units(4, 1232);
        assert!(pairs(&vgm, 4)
            .iter()
            .any(|&(b, f)| (ym2612_pitch_units(b, f) - want).abs() < 0.5));
    }

    #[test]
    fn test_sr_and_ssg_eg_reach_the_chip() {
        skip_without_furnace!();
        let ws = fm_note(
            4,
            1232,
            100,
            60000,
            0,
            0,
            None,
            [0x12, 0x13, 0x14, 0x15],
            [0x05, 0x06, 0x07, 0x08],
            [0; 4],
        );
        let song = analyze(&vgm_default(ws), AnalyzeOptions::default()).expect("analyze");
        let vgm = export(&song, "ym2612_ops");
        let mut reg: BTreeMap<u32, BTreeSet<u32>> = BTreeMap::new();
        for w in &vgm.writes {
            if w.chip == "ym2612" && w.chip_id == 0 && (w.reg & 0x100) == 0 {
                reg.entry(w.reg & 0xFF).or_default().insert(w.val);
            }
        }
        for op in 0..4u32 {
            let sr_vals: BTreeSet<u32> = reg[&(0x70 + op * 4)].iter().map(|v| v & 0x1F).collect();
            assert!(sr_vals.contains(&(0x12 + op)));
            let ssg_vals: BTreeSet<u32> = reg[&(0x90 + op * 4)].iter().map(|v| v & 0x0F).collect();
            assert!(ssg_vals.contains(&(0x05 + op)));
        }
    }

    #[test]
    fn test_dt_four_is_negative_zero() {
        skip_without_furnace!();
        let ws = fm_note(4, 1232, 100, 60000, 0, 0, None, [0; 4], [0; 4], [4, 4, 4, 4]);
        let song = analyze(&vgm_default(ws), AnalyzeOptions::default()).expect("analyze");
        let vgm = export(&song, "ym2612_dt4");
        for op in 0..4u32 {
            let vals: BTreeSet<u32> = vgm
                .writes
                .iter()
                .filter(|w| {
                    w.chip == "ym2612"
                        && w.chip_id == 0
                        && (w.reg & 0x100) == 0
                        && (w.reg & 0xFF) == 0x30 + op * 4
                })
                .map(|w| w.val)
                .collect();
            assert!(!vals.is_empty());
            let dt: BTreeSet<u32> = vals.iter().map(|v| v >> 4).collect();
            assert_eq!(dt, BTreeSet::from([0]));
        }
    }

    #[test]
    fn test_alg4_effective_carrier_levels_reach_the_chip() {
        skip_without_furnace!();
        let mut ws = patch(100, 0, false, 0, [0; 4], [0; 4], [0; 4]);
        ws.extend([
            ChipWrite::new(100, "ym2612", 0, 0xB0, 0x04),
            ChipWrite::new(100, "ym2612", 0, 0x40, 0x10), // S1 modulator
            ChipWrite::new(100, "ym2612", 0, 0x44, 0x40), // S3 modulator
            ChipWrite::new(100, "ym2612", 0, 0x48, 0x20), // S2 carrier
            ChipWrite::new(100, "ym2612", 0, 0x4C, 0x30), // S4 carrier
            ChipWrite::new(100, "ym2612", 0, 0xA4, 0x25),
            ChipWrite::new(100, "ym2612", 0, 0xA0, 0xBA),
            ChipWrite::new(100, "ym2612", 0, 0x28, 0xF0),
            ChipWrite::new(60000, "ym2612", 0, 0x28, 0x00),
        ]);
        let song = analyze(&vgm_default(ws), AnalyzeOptions::default()).expect("analyze");
        let vgm = export(&song, "ym2612_alg4");
        let mut reg: BTreeMap<u32, BTreeSet<u32>> = BTreeMap::new();
        for w in &vgm.writes {
            if w.chip == "ym2612" && w.chip_id == 0 && (w.reg & 0x100) == 0 {
                reg.entry(w.reg & 0xFF).or_default().insert(w.val);
            }
        }
        assert!(reg[&0x40].contains(&0x10));
        assert!(reg[&0x44].contains(&0x40));
        assert!(reg[&0x48].contains(&0x20));
        assert!(reg[&0x4C].contains(&0x30));
    }

    #[test]
    fn test_pms_ams_and_carrier_level_reach_the_chip() {
        skip_without_furnace!();
        let mut ws = patch(100, 0, false, 0, [0; 4], [0; 4], [0; 4]);
        ws.extend([
            ChipWrite::new(100, "ym2612", 0, 0xB4, 0xD3), // L/R, AMS 1, PMS 3
            ChipWrite::new(100, "ym2612", 0, 0xA4, 0x25),
            ChipWrite::new(100, "ym2612", 0, 0xA0, 0xBA),
            ChipWrite::new(100, "ym2612", 0, 0x28, 0xF0),
            ChipWrite::new(60000, "ym2612", 0, 0x28, 0x00),
            // A second note with carrier TL 0x30: the instrument stores the
            // relative level and the volume column carries the absolute one.
            ChipWrite::new(70000, "ym2612", 0, 0x4C, 0x30),
            ChipWrite::new(70000, "ym2612", 0, 0x28, 0xF0),
            ChipWrite::new(80000, "ym2612", 0, 0x28, 0x00),
        ]);
        let song = analyze(&vgm_default(ws), AnalyzeOptions::default()).expect("analyze");
        let vgm = export(&song, "ym2612_pms");
        let mut reg: BTreeMap<u32, BTreeSet<u32>> = BTreeMap::new();
        for w in &vgm.writes {
            if w.chip == "ym2612" && w.chip_id == 0 && (w.reg & 0x100) == 0 {
                reg.entry(w.reg & 0xFF).or_default().insert(w.val);
            }
        }
        assert!(reg[&0xB4].contains(&0xD3));
        // Carrier TL 0 at vol 127, then TL 0x30 at vol 127-0x30.
        assert!(reg[&0x4C].contains(&0x00));
        assert!(reg[&0x4C].contains(&0x30));
    }

    #[test]
    fn test_lfo_effect_sets_register_22() {
        skip_without_furnace!();
        let mut ws = patch(100, 0, false, 0, [0; 4], [0; 4], [0; 4]);
        ws.extend([
            ChipWrite::new(100, "ym2612", 0, 0x22, 0x0B),
            ChipWrite::new(100, "ym2612", 0, 0xA4, 0x25),
            ChipWrite::new(100, "ym2612", 0, 0xA0, 0xBA),
            ChipWrite::new(100, "ym2612", 0, 0x28, 0xF0),
            ChipWrite::new(60000, "ym2612", 0, 0x28, 0x00),
        ]);
        let song = analyze(&vgm_default(ws), AnalyzeOptions::default()).expect("analyze");
        let vgm = export(&song, "ym2612_lfo");
        let vals: BTreeSet<u32> = vgm
            .writes
            .iter()
            .filter(|w| {
                w.chip == "ym2612" && (w.reg & 0x100) == 0 && (w.reg & 0xFF) == 0x22
            })
            .map(|w| w.val)
            .collect();
        assert!(vals.contains(&0x0B));
    }
}

#[allow(non_snake_case)]
mod TestYm2612DacAlienSoldier {
    use super::*;

    const SRC: &str = r"D:\Downloads\Alien Soldier\21 - Soldiers Song.vgz";

    #[test]
    fn test_e0_drum_keeps_its_nominal_rate() {
        if !Path::new(SRC).exists() {
            eprintln!("skipping: Alien Soldier reference rip missing");
            return;
        }
        let song = analyze(
            &load_vgm(Path::new(SRC)).expect("load vgm"),
            AnalyzeOptions::default(),
        )
        .expect("analyze");
        let base = song.fm_patches.len() as i32;
        let notes: Vec<_> = song
            .events
            .iter()
            .filter(|e| e.on && e.dac && e.note == 64)
            .collect();
        assert!(!notes.is_empty());
        let indexes: BTreeSet<usize> = notes
            .iter()
            .map(|e| (((e.ins - base) * 120 + (e.note - 60)) as usize))
            .collect();
        let rates: BTreeSet<i32> = indexes
            .iter()
            .map(|&i| song.dac_samples[i].c4_rate)
            .collect();
        assert_eq!(rates, BTreeSet::from([11025]));
    }
}

#[allow(non_snake_case)]
mod TestYm2612DacLongHolds {
    use super::*;

    fn bad_silence(song: &vgm2fur::model::Song) -> Vec<(usize, usize, usize)> {
        let mut bad = Vec::new();
        for (i, smp) in song.dac_samples.iter().enumerate() {
            if smp.data.len() < DAC_SILENCE_MIN_LEN {
                continue;
            }
            let (run, _start) = _longest_silence(&smp.data);
            if run as f64 > smp.data.len() as f64 * DAC_SILENCE_FRACTION {
                bad.push((i, smp.data.len(), run));
            }
        }
        bad
    }

    /// One pointer run: a kick, a held silence, then a snare.
    fn bank_run() -> (Vec<u8>, Vec<(i64, u8, i64)>) {
        let kick: Vec<u8> = [9i64, 20, 30, 20].iter().map(|&v| (0x80 + v) as u8).collect();
        let snare: Vec<u8> = [40i64, 60, 40, 20].iter().map(|&v| (0x80 + v) as u8).collect();
        let mut bank = kick.clone();
        bank.push(0x80);
        bank.extend_from_slice(&snare);
        let mut events: Vec<(i64, u8, i64)> = vec![(0, 0, 0)];
        events.extend((0..4).map(|i| (i * 4, 1, 4)));
        events.push((1000, 1, 4)); // the silent byte the DAC holds
        events.extend((0..4).map(|i| (5000 + i * 4, 1, 4)));
        // The driver then reads the same bank range again later.
        events.push((20000, 0, 0));
        events.extend((0..4).map(|i| (20000 + i * 4, 1, 4)));
        events.push((21000, 1, 4));
        events.extend((0..4).map(|i| (25000 + i * 4, 1, 4)));
        (bank, events)
    }

    #[test]
    fn test_held_gap_splits_the_source_run() {
        let (bank, events) = bank_run();
        let mut src = vgm_with(vec![ChipWrite::new(0, "ym2612", 0, 0x22, 0)], 50000, CLOCK);
        src.dac_data = bank;
        src.dac_events = events;
        let song = analyze(
            &src,
            AnalyzeOptions {
                speed: Some(6),
                ..Default::default()
            },
        )
        .expect("analyze");
        assert_eq!(song.dac_stats.text("dac_mode", ""), "source");
        assert!(song.dac_stats.int("dac_source_silence_splits") >= 1);
        assert!(bad_silence(&song).is_empty());
        let mut notes: Vec<i64> = song
            .events
            .iter()
            .filter(|e| e.dac && e.on)
            .map(|e| e.sample)
            .collect();
        notes.sort();
        // The snare after the hold is its own note, not a late byte inside
        // the kick's body.
        assert!(notes.iter().any(|&t| t >= 4000), "{:?}", notes);
        assert_eq!(song.dac_stats.int("dac_release_trimmed"), 0);
    }

    #[test]
    fn test_dense_silent_stretch_leaves_the_stored_body() {
        // The reads stay dense through the rest, so the source splitter does
        // not see a gap; the stored body itself has to give the silence back.
        let mut bank: Vec<u8> = (0..1000usize).map(|i| 0x80 + (i % 30 + 1) as u8).collect();
        bank.extend(vec![0x80u8; 5000]);
        bank.extend((0..4500usize).map(|i| 0x80 + (i % 30 + 1) as u8));
        let mut events: Vec<(i64, u8, i64)> = vec![(0, 0, 0)];
        events.extend((0..bank.len()).map(|i| (i as i64 * 4, 1, 4)));
        let bank_len = bank.len();
        let mut src = vgm_with(vec![ChipWrite::new(0, "ym2612", 0, 0x22, 0)], 30000, CLOCK);
        src.dac_data = bank;
        src.dac_events = events;
        let song = analyze(
            &src,
            AnalyzeOptions {
                speed: Some(6),
                ..Default::default()
            },
        )
        .expect("analyze");
        assert!(song.dac_stats.int("dac_silence_split_samples") >= 1);
        assert!(bad_silence(&song).is_empty());
        let total: usize = song.dac_samples.iter().map(|s| s.data.len()).sum();
        assert!(total < bank_len);
    }
}

#[allow(non_snake_case)]
mod TestYm2612DacUnusedSamples {
    use super::*;

    #[test]
    fn test_source_mode_drops_a_sample_no_row_selects() {
        // A DAC note and an FM6 note can land on the same cell. The later FM6
        // key wins the pattern cell, so the body stored for the lost DAC note
        // is never played. The writer must remove that slot (and keep the
        // surviving one) instead of shipping an unreachable sample.
        let mut writes = vec![ChipWrite::new(0, "ym2612", 0, 0x2B, 0x80)]; // DAC on
        writes.extend(patch(0, 0, true, 2, [0; 4], [0; 4], [0; 4]));
        writes.extend([
            ChipWrite::new(0, "ym2612", 0, 0x1A4, (4 << 3)),
            ChipWrite::new(0, "ym2612", 0, 0x1A0, 0x00),
            ChipWrite::new(20, "ym2612", 0, 0x2B, 0x00), // DAC off
            ChipWrite::new(21, "ym2612", 0, 0x28, 0xF6), // FM6 takes the row
            ChipWrite::new(40000, "ym2612", 0, 0x28, 0x06),
            ChipWrite::new(50000, "ym2612", 0, 0x2B, 0x80),
        ]);
        let mut bank: Vec<u8> = [9i64, 20, 30, 20].iter().map(|&v| (0x80 + v) as u8).collect();
        bank.push(0x80);
        bank.extend([40i64, 60, 40, 20].iter().map(|&v| (0x80 + v) as u8));
        let mut events: Vec<(i64, u8, i64)> = vec![(2, 0, 0)];
        events.extend((0..4).map(|i| (2 + i * 2, 1, 2)));
        events.push((1000, 1, 2));
        events.extend((0..4).map(|i| (50002 + i * 4, 1, 4)));
        let mut src = vgm_with(writes, 60000, CLOCK);
        src.dac_data = bank;
        src.dac_events = events;
        let song = analyze(
            &src,
            AnalyzeOptions {
                speed: Some(6),
                ..Default::default()
            },
        )
        .expect("analyze");
        assert_eq!(song.dac_stats.text("dac_mode", ""), "source");
        assert_eq!(song.dac_samples.len(), 2);
        assert_eq!(song.dac_count, 2);

        let mut stats = indexmap::IndexMap::new();
        let data = write_fur(
            &song,
            WriteOptions {
                stats: Some(&mut stats),
                ..Default::default()
            },
        );
        let mod_ = parse_fur(&data).expect("parse");
        let unused = stats.get("dac_samples_unused").copied().unwrap_or(0);
        assert!(unused >= 1);
        assert!(stats.get("note_collision").copied().unwrap_or(0) > 0);
        assert_eq!(
            mod_.samples.len() as i64,
            song.dac_samples.len() as i64 - unused
        );
        let base = song.fm_patches.len() as i32;
        let n_ins = ((song.dac_samples.len() as i32) + 119) / 120;
        let mut used = BTreeSet::new();
        for rows in mod_.patterns.values() {
            for row in rows {
                if (60..180).contains(&row.note) && base <= row.ins && row.ins < base + n_ins {
                    used.insert((row.ins - base) * 120 + (row.note - 60));
                }
            }
        }
        let want: BTreeSet<i32> = (0..mod_.samples.len() as i32).collect();
        assert_eq!(used, want);
    }
}

#[allow(non_snake_case)]
mod TestYm2612PitchRoundTrip {
    use super::*;

    #[test]
    fn test_all_channels_survive_the_round_trip() {
        skip_without_furnace!();
        let notes = [
            (4, 1204, 0u16),
            (3, 923, 1),
            (2, 702, 2),
            (4, 1232, 3),
            (3, 1466, 4),
            (4, 1100, 5),
        ];
        let mut ws: Vec<ChipWrite> = Vec::new();
        for (block, fnum, ch) in notes {
            let t = 100 + ch as i64 * 12000;
            ws.extend(fm_note(
                block,
                fnum,
                t,
                t + 6000,
                0,
                ch,
                None,
                [0; 4],
                [0; 4],
                [0; 4],
            ));
        }
        let src = vgm_with(ws, 80000, CLOCK);
        let song = analyze(&src, AnalyzeOptions::default()).expect("analyze");
        assert!(song.warnings.is_empty(), "{:?}", song.warnings);
        let fur = tmp_dir().join("ym2612_pitch_round_trip.fur");
        std::fs::write(
            &fur,
            write_fur(
                &song,
                WriteOptions {
                    include_pcm: false,
                    ..Default::default()
                },
            ),
        )
        .expect("write fur");
        let out = tmp_dir().join("ym2612_pitch_round_trip.vgm");
        vgm2fur::furnace::export_vgm(&fur, &out).expect("export vgm");
        let back = load_vgm(&out).expect("load export");
        for ch in 0..6i32 {
            assert!(
                pitch_runs(&src, &back, ch, 0.25, 60.0).is_empty(),
                "ch{ch}"
            );
        }
    }
}

#[allow(non_snake_case)]
mod TestOpmWithDacOnlyYm2612 {
    use super::*;

    #[test]
    fn test_dac_only_chip_stays_beside_the_opm() {
        // A log can score the music on a YM2151 and use a second YM2612 only
        // as a DAC stream. The YM2612 keys no FM note, but its PCM is music,
        // so both systems belong in the module.
        let bank: Vec<u8> = (0..300).map(|i| (0x80 + (i % 31) - 15) as u8).collect();
        let mut events: Vec<(i64, u8, i64)> = vec![(0, 0, 0)];
        events.extend((0..300).map(|i| (i * 4, 1, 4)));
        let vgm = VgmFile {
            path: PathBuf::from("synthetic.vgm"),
            buf: Vec::new(),
            version: 0x161,
            total_samples: 400_000,
            ym2151_clock: 3_579_545,
            writes: vec![
                ChipWrite::new(0, "ym2151", 0, 0x08, 0x78),
                ChipWrite::new(0, "ym2612", 0, 0x22, 0x00),
            ],
            ym2612_clock: 7_670_453,
            dac_data: bank,
            dac_events: events,
            ..Default::default()
        };
        let song = analyze(&vgm, AnalyzeOptions::default()).expect("analyze");
        assert_eq!(
            song.fm_systems
                .iter()
                .map(|s| s.kind.as_str())
                .collect::<Vec<_>>(),
            vec!["ym2151", "ym2612"]
        );
        assert_eq!(song.dac_count, 1);
        assert!(song.events.iter().any(|e| e.on && e.dac));
        assert!(!song
            .warnings
            .iter()
            .any(|w| w.contains("YM2612 writes in this log")));
        let mod_ = parse_fur(&write_fur(&song, WriteOptions::default())).expect("parse");
        assert_eq!(mod_.chips, vec![CHIP_YM2151, CHIP_YM2612]);
        assert_eq!(mod_.samples.len(), 1);
    }
}

#[allow(non_snake_case)]
mod TestYm2612PanOff {
    use super::*;

    fn rows(song: &vgm2fur::model::Song, ch: usize) -> BTreeMap<i32, FurRow> {
        let mod_ = parse_fur(&write_fur(
            song,
            WriteOptions {
                include_pcm: false,
                include_fm: true,
                ..Default::default()
            },
        ))
        .expect("parse");
        let mut out = BTreeMap::new();
        for (oi, &pidx) in mod_.orders[ch].iter().enumerate() {
            if let Some(pr) = mod_.patterns.get(&(ch as u16, pidx)) {
                for (ri, row) in pr.iter().enumerate() {
                    out.insert(
                        oi as i32 * mod_.pat_len as i32 + ri as i32,
                        row.clone(),
                    );
                }
            }
        }
        out
    }

    #[test]
    fn test_note_keyed_with_both_sides_off_is_muted() {
        let ws = fm_note(4, 1204, 100, 20000, 0, 0, None, [0; 4], [0; 4], [0; 4]);
        // The patch helper opens both sides; key this note with pan 00.
        let mut ws2: Vec<ChipWrite> = ws
            .into_iter()
            .map(|w| {
                if w.sample == 100 && w.reg == 0xB4 {
                    ChipWrite::new(w.sample, w.chip, w.chip_id, w.reg, 0x00)
                } else {
                    w
                }
            })
            .collect();
        ws2.push(ChipWrite::new(8000, "ym2612", 0, 0xB4, 0xC0));
        let song = analyze(
            &vgm_with(ws2, 30000, CLOCK),
            AnalyzeOptions {
                speed: Some(2),
                ..Default::default()
            },
        )
        .expect("analyze");
        let ev = song.events.iter().find(|e| e.on).expect("note on");
        assert_eq!(ev.pan, 0x00);
        let rows = rows(&song, 0);
        let note_row = &rows[&song.place(100, None).0];
        assert_eq!(note_row.vol, 0);
        assert!(!note_row.fx.contains(&(0x08, 0x00)));
        // When the source opens both sides again the note's level comes back.
        let restore: Vec<&FurRow> = rows
            .values()
            .filter(|row| row.fx.contains(&(0x08, 0xFF)))
            .collect();
        assert!(!restore.is_empty());
        assert!(restore.iter().any(|row| row.vol == ev.vol));
    }

    #[test]
    fn test_mid_note_both_sides_off_is_a_volume_mute() {
        let mut ws = fm_note(4, 1204, 100, 30000, 0, 0, None, [0; 4], [0; 4], [0; 4]);
        ws.extend([
            ChipWrite::new(10000, "ym2612", 0, 0xB4, 0x00),
            ChipWrite::new(20000, "ym2612", 0, 0xB4, 0xC0),
        ]);
        let song = analyze(
            &vgm_with(ws, 40000, CLOCK),
            AnalyzeOptions {
                speed: Some(2),
                ..Default::default()
            },
        )
        .expect("analyze");
        let ev = song.events.iter().find(|e| e.on).expect("note on");
        let rows = rows(&song, 0);
        assert!(!rows
            .values()
            .any(|row| row.fx.contains(&(0x08, 0x00))));
        let muted: Vec<&FurRow> = rows.values().filter(|row| row.vol == 0).collect();
        assert!(!muted.is_empty());
        let restored: Vec<&FurRow> = rows
            .values()
            .filter(|row| row.fx.contains(&(0x08, 0xFF)) && row.vol == ev.vol)
            .collect();
        assert!(!restored.is_empty());
    }
}
