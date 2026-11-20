//! Port of `tests/test_opm_operator_macro.py`: YM2151 per-operator DT1/MULT
//! macros inside very short notes.

mod common;

use common::*;
use vgm2fur::analyze::{analyze, AnalyzeOptions};
use vgm2fur::furio::{iter_blocks, parse_fur};
use vgm2fur::furwrite::{decompress_fur, write_fur, WriteOptions};

const CH: i32 = 2;
const KEY_ON: i64 = 60_000;
const KEY_OFF: i64 = 63_000;

fn _song(writes: &[(i64, u16, u16)]) -> vgm2fur::vgm::VgmFile {
    note_song_with(KEY_ON, KEY_OFF, 0x00, 0x00, writes, CH as u16)
}

/// `-> [(operator tag, macro code, speed, values)]` from a raw .fur.
fn _op_macros(data: &[u8]) -> Vec<(String, u8, u8, Vec<u8>)> {
    let decompressed = decompress_fur(data);
    let mut out = Vec::new();
    for block in iter_blocks(&decompressed).expect("blocks") {
        if block.magic != *b"INS2" {
            continue;
        }
        let blk = block.payload;
        let mut pos = 4usize;
        while pos + 4 <= blk.len() {
            let tag = &blk[pos..pos + 2];
            if tag == b"EN" {
                break;
            }
            let size = u16::from_le_bytes([blk[pos + 2], blk[pos + 3]]) as usize;
            let body_end = (pos + 4 + size).min(blk.len());
            let body = &blk[pos + 4..body_end];
            if tag.first() == Some(&b'O') && body.len() > 10 {
                let code = body[2];
                let count = body[3] as usize;
                let speed = body[9];
                let values = body[10..(10 + count).min(body.len())].to_vec();
                out.push((
                    String::from_utf8_lossy(tag).into_owned(),
                    code,
                    speed,
                    values,
                ));
            }
            pos += 4 + size;
        }
    }
    out
}

fn _find_on(song: &vgm2fur::model::Song) -> vgm2fur::model::NoteEvent {
    song.events
        .iter()
        .find(|e| e.on && e.ch == CH)
        .cloned()
        .expect("note event")
}

mod short_note_operator_tracking {
    use super::*;

    fn _setup() -> (vgm2fur::model::Song, vgm2fur::model::NoteEvent) {
        let song = analyze(
            &_song(&[
                (KEY_ON + 100, 0x50 | CH as u16, 0),
                (KEY_ON + 900, 0x50 | CH as u16, 1),
                (KEY_ON + 1700, 0x50 | CH as u16, 2),
                (KEY_ON + 2500, 0x50 | CH as u16, 3),
            ]),
            AnalyzeOptions { pcm: false, ..Default::default() },
        )
        .expect("song");
        let ev = _find_on(&song);
        (song, ev)
    }

    #[test]
    fn test_the_note_records_the_operator_staircase() {
        let (_song, ev) = _setup();
        assert!(!ev.op_dtmul_pts.is_empty());
        assert_eq!(
            ev.op_dtmul_pts
                .iter()
                .map(|&(_s, op, value)| (op, value))
                .collect::<Vec<_>>(),
            vec![(2, 0), (2, 1), (2, 2), (2, 3)]
        );
    }

    #[test]
    fn test_the_staircase_becomes_row_effects() {
        // The steps land on separate rows, so the pattern carries them.
        let (song, ev) = _setup();
        let patch = &song.fm_patches[ev.ins as usize];
        assert!(!ev.op_mul_fx.is_empty());
        assert_eq!(
            ev.op_mul_fx
                .iter()
                .map(|&(_s, _op, value)| value)
                .collect::<Vec<_>>(),
            vec![0, 1, 2, 3]
        );
        assert_eq!(patch.op_mul_macro, Vec::new());
    }

    #[test]
    fn test_the_file_stores_16xy_rows() {
        let (song, _ev) = _setup();
        let data = write_fur(
            &song,
            WriteOptions { include_pcm: false, include_fm: true, ..Default::default() },
        );
        let m = parse_fur(&data).expect("module");
        let fx: Vec<(u8, u8)> = m
            .patterns
            .values()
            .flat_map(|rows| rows.iter())
            .flat_map(|row| row.fx.iter().copied())
            .collect();
        // Operator index 2 is effect x=2 on the OPM (orderedOps 0,2,1,3).
        assert_eq!(
            fx.iter()
                .filter(|(code, _)| *code == 0x16)
                .map(|(_, value)| *value)
                .collect::<Vec<_>>(),
            vec![0x20, 0x21, 0x22, 0x23]
        );
        assert!(
            _op_macros(&data)
                .iter()
                .filter(|f| f.1 == 3)
                .collect::<Vec<_>>()
                .is_empty(),
            "row-safe steps must not create an operator macro"
        );
    }

    #[test]
    fn test_two_steps_inside_one_row_stay_a_macro() {
        // Both changes are shorter than a row, so a row effect would lose one.
        let song = analyze(
            &_song(&[
                (KEY_ON + 100, 0x50 | CH as u16, 0),
                (KEY_ON + 200, 0x50 | CH as u16, 1),
                (KEY_ON + 300, 0x50 | CH as u16, 2),
                (KEY_ON + 400, 0x50 | CH as u16, 3),
            ]),
            AnalyzeOptions { pcm: false, ..Default::default() },
        )
        .expect("song");
        let ev = _find_on(&song);
        let patch = &song.fm_patches[ev.ins as usize];
        assert!(ev.op_mul_fx.is_empty());
        assert!(!patch.op_mul_macro.is_empty());
    }

    #[test]
    fn test_detune_steps_use_53xy_on_rows() {
        let song = analyze(
            &_song(&[
                (KEY_ON + 100, 0x50 | CH as u16, 0x31), // DT1 raw 3 (engine 6)
                (KEY_ON + 900, 0x50 | CH as u16, 0x71), // DT1 raw 7 (engine 0)
            ]),
            AnalyzeOptions { pcm: false, ..Default::default() },
        )
        .expect("song");
        let ev = _find_on(&song);
        assert_eq!(
            ev.op_dt_fx
                .iter()
                .map(|&(_s, _op, value)| value)
                .collect::<Vec<_>>(),
            vec![6, 0]
        );
        let data = write_fur(
            &song,
            WriteOptions { include_pcm: false, include_fm: true, ..Default::default() },
        );
        let m = parse_fur(&data).expect("module");
        let fx: Vec<u8> = m
            .patterns
            .values()
            .flat_map(|rows| rows.iter())
            .flat_map(|row| row.fx.iter())
            .filter(|(code, _)| *code == 0x53)
            .map(|(_, value)| *value)
            .collect();
        assert_eq!(fx, vec![0x26, 0x20]);
    }

    #[test]
    fn test_detune2_steps_use_55xy_on_rows() {
        let reg = 0xD0 | CH; // operator 3's DT2/D2R
        let song = analyze(
            &_song(&[
                (KEY_ON + 100, reg as u16, 0x40),
                (KEY_ON + 900, reg as u16, 0x80),
            ]),
            AnalyzeOptions { pcm: false, ..Default::default() },
        )
        .expect("song");
        let ev = _find_on(&song);
        assert_eq!(
            ev.op_dt2_fx
                .iter()
                .map(|&(_s, _op, value)| value)
                .collect::<Vec<_>>(),
            vec![1, 2]
        );
        let data = write_fur(
            &song,
            WriteOptions { include_pcm: false, include_fm: true, ..Default::default() },
        );
        let m = parse_fur(&data).expect("module");
        let fx: Vec<u8> = m
            .patterns
            .values()
            .flat_map(|rows| rows.iter())
            .flat_map(|row| row.fx.iter())
            .filter(|(code, _)| *code == 0x55)
            .map(|(_, value)| *value)
            .collect();
        assert_eq!(fx, vec![0x21, 0x22]);
    }

    #[test]
    fn test_a_key_on_burst_mult_is_the_instrument_not_a_macro() {
        let song = analyze(
            &_song(&[(KEY_ON, 0x50 | CH as u16, 0)]),
            AnalyzeOptions { pcm: false, ..Default::default() },
        )
        .expect("song");
        let ev = _find_on(&song);
        let patch = &song.fm_patches[ev.ins as usize];
        assert_eq!(ev.op_dtmul_pts, Vec::new());
        assert_eq!(patch.ops[2].dt_mul, 0);
        assert_eq!(patch.op_mul_macro, Vec::new());
    }
}

mod dense_operator_batch {
    use super::*;

    #[test]
    fn test_four_operator_mult_and_dt2_batch_becomes_macros() {
        // A driver that retunes all four operators' MULT and DT2 on one tick
        // would need eight effect columns (plus ED) for the row form. The
        // analyzer must move the batch to the per-operator O1..O4 macros
        // instead of letting _add_fx overwrite three operators per row.
        let mut writes: Vec<(i64, u16, u16)> = Vec::new();
        for sample in (61000..=65000).step_by(2000) {
            for op in 0..4i64 {
                // OpRegs defaults to MUL 1 / DT2 0, so use values that are
                // real changes for every operator.
                writes.push((
                    sample,
                    (0x40 | (op << 3) | CH as i64) as u16,
                    (op + 2) as u16,
                ));
                writes.push((
                    sample,
                    (0xC0 | (op << 3) | CH as i64) as u16,
                    (((op % 3) + 1) << 6) as u16,
                ));
            }
        }
        let song = analyze(
            &_song(&writes),
            AnalyzeOptions { pcm: false, ..Default::default() },
        )
        .expect("song");
        let ev = _find_on(&song);
        let patch = &song.fm_patches[ev.ins as usize];
        assert!(ev.op_mul_fx.is_empty());
        assert!(ev.op_dt2_fx.is_empty());
        assert_eq!(
            patch.op_mul_macro.iter().map(|e| e.0).collect::<Vec<_>>(),
            vec![0, 1, 2, 3]
        );
        assert_eq!(
            patch.op_dt2_macro.iter().map(|e| e.0).collect::<Vec<_>>(),
            vec![0, 1, 2, 3]
        );
        let data = write_fur(
            &song,
            WriteOptions { include_pcm: false, include_fm: true, ..Default::default() },
        );
        let m = parse_fur(&data).expect("module");
        let row_fx: std::collections::BTreeSet<u8> = m
            .patterns
            .values()
            .flat_map(|rows| rows.iter())
            .flat_map(|row| row.fx.iter())
            .map(|(code, _value)| *code)
            .collect();
        assert!(!row_fx.contains(&0x16));
        assert!(!row_fx.contains(&0x55));
    }
}
