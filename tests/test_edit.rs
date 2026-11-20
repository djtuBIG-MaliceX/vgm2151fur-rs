//! Chip volume editing on .fur files: both value copies, and the caps.
//!
//! Port of `tests/test_edit.py` at vgm2151fur a8edb6e. The Python file has no
//! CLI cases; any CLI coverage lives in `test_menu`.

mod common;

use common::*;
use vgm2fur::analyze::analyze;
use vgm2fur::edit::{read_chips, set_chip_volume, MAX_INFO_VOLUME, _read_info};
use vgm2fur::furwrite::{write_fur, WriteOptions, CHIP_YM2151, FUR_MAGIC};

fn one_chip_fur() -> Vec<u8> {
    let song = analyze(
        &note_song(),
        vgm2fur::analyze::AnalyzeOptions {
            pcm: false,
            ..Default::default()
        },
    )
    .expect("analyze note_song");
    write_fur(
        &song,
        WriteOptions {
            include_pcm: false,
            include_fm: true,
            ..Default::default()
        },
    )
}

fn info_parts(data: &[u8]) -> (usize, i64) {
    let (_chips, vol_off, triples) = _read_info(data).expect("read INFO");
    (vol_off, triples)
}

fn read_f32(data: &[u8], off: usize) -> f32 {
    let mut b = [0u8; 4];
    b.copy_from_slice(&data[off..off + 4]);
    f32::from_le_bytes(b)
}

mod TestReadChips {
    use super::*;

    #[test]
    fn test_lists_the_written_chips() {
        let chips = read_chips(&one_chip_fur()).expect("read chips");
        assert_eq!(chips.iter().map(|c| c.id).collect::<Vec<_>>(), vec![CHIP_YM2151 as u32]);
        assert_almost_eq!(chips[0].volume, 1.0, 3);
    }

    #[test]
    fn test_rejects_a_foreign_buffer() {
        // Python asserts FurEditError or ValueError.
        assert!(read_chips(b"not a fur at all").is_err());
    }
}

mod TestSetChipVolume {
    use super::*;

    #[test]
    fn test_factor_scales_the_byte_and_the_float() {
        let data = one_chip_fur();
        let (vol_off, triples) = info_parts(&data);
        let (patched, chips) = set_chip_volume(&data, Some(0.5), None, None).expect("set volume");
        assert_almost_eq!(chips[0].volume, 0.5, 3);
        assert_eq!(patched[vol_off], 32);
        assert_almost_eq!(read_f32(&patched, triples as usize) as f64, 0.5, 3);
    }

    #[test]
    fn test_value_sets_outright() {
        let (_patched, chips) = set_chip_volume(&one_chip_fur(), None, Some(0.25), None)
            .expect("set volume");
        assert_almost_eq!(chips[0].volume, 0.25, 3);
    }

    #[test]
    fn test_info_caps_at_unity() {
        let (_patched, chips) =
            set_chip_volume(&one_chip_fur(), Some(4.0), None, None).expect("set volume");
        assert_almost_eq!(chips[0].volume, MAX_INFO_VOLUME, 3);
    }

    #[test]
    fn test_targets_keep_other_chips() {
        let data = one_chip_fur();
        let (patched, chips) =
            set_chip_volume(&data, Some(0.5), None, Some(&[])).expect("set volume");
        assert_almost_eq!(chips[0].volume, 1.0, 3);
        assert_eq!(patched, data);
    }

    #[test]
    fn test_needs_exactly_one_of_factor_value() {
        let data = one_chip_fur();
        assert!(set_chip_volume(&data, None, None, None).is_err());
        assert!(set_chip_volume(&data, Some(1.0), Some(1.0), None).is_err());
    }

    #[test]
    fn test_round_trip_keeps_the_file_readable() {
        let data = one_chip_fur();
        let (patched, _chips) = set_chip_volume(&data, Some(0.75), None, None).expect("set volume");
        let chips = read_chips(&patched).expect("read chips");
        assert_almost_eq!(chips[0].volume, 0.75, 3);
    }
}

mod TestInf2 {
    use super::*;

    fn inf2(volume: f64) -> Vec<u8> {
        fn s(text: &str, out: &mut Vec<u8>) {
            out.extend_from_slice(text.as_bytes());
            out.push(0);
        }

        let mut payload = Vec::new();
        for text in ["name", "author", "system", "", "", "", "", ""] {
            s(text, &mut payload);
        }
        payload.extend_from_slice(&440.0f32.to_le_bytes());
        payload.push(0);
        payload.extend_from_slice(&1.0f32.to_le_bytes());
        payload.extend_from_slice(&8u16.to_le_bytes());
        payload.extend_from_slice(&1u16.to_le_bytes());
        payload.extend_from_slice(&0x82u16.to_le_bytes());
        payload.extend_from_slice(&8u16.to_le_bytes());
        payload.extend_from_slice(&(volume as f32).to_le_bytes());
        payload.extend_from_slice(&0.0f32.to_le_bytes());
        payload.extend_from_slice(&0.0f32.to_le_bytes());

        let mut buf = vec![0u8; 16];
        buf[0..16].copy_from_slice(FUR_MAGIC);
        buf.extend_from_slice(&157u16.to_le_bytes());
        buf.extend_from_slice(&0u16.to_le_bytes());
        buf.extend_from_slice(&32u32.to_le_bytes());
        buf.extend_from_slice(&[0u8; 8]);
        buf.extend_from_slice(b"INF2");
        buf.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        buf.extend_from_slice(&payload);
        buf
    }

    #[test]
    fn test_reads_and_patches_floats() {
        let data = inf2(0.8);
        let chips = read_chips(&data).expect("read chips");
        assert_eq!(chips.len(), 1);
        assert_almost_eq!(chips[0].volume, 0.8, 3);
        let (_patched, chips) = set_chip_volume(&data, Some(1.25), None, None).expect("set volume");
        assert_almost_eq!(chips[0].volume, 1.0, 3);
    }

    #[test]
    fn test_inf2_allows_more_than_unity() {
        let (_patched, chips) = set_chip_volume(&inf2(1.0), None, Some(2.0), None)
            .expect("set volume");
        assert_almost_eq!(chips[0].volume, 2.0, 3);
    }
}
