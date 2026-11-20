//! The uPD7759 dump, on synthetic ROMs. No MAME or game ROM needed.

mod common;
use common::*;

use std::collections::BTreeSet;
use std::path::Path;

use vgm2fur::upd7759::{
    decode_phrase, dump_rom, phrase_address, phrase_count, render_levels, render_wav_bytes,
    STANDARD_CLOCK,
};

fn fill(data: &mut [u8], offset: usize, bytes: &[u8]) {
    data[offset..offset + bytes.len()].copy_from_slice(bytes);
}

/// Header + three vectors + data. See the test docstrings for the layout.
fn rom_three_phrases() -> Vec<u8> {
    let mut data = vec![0u8; 0x20];
    fill(&mut data, 0, &[0x02, 0x5A, 0xA5, 0x69, 0x55]);
    fill(&mut data, 5, &6u16.to_be_bytes()); // phrase 0 -> 0x0D
    fill(&mut data, 7, &9u16.to_be_bytes()); // phrase 1 -> 0x13
    fill(&mut data, 9, &12u16.to_be_bytes()); // phrase 2 -> 0x19
    // phrase 0: rate 1, 4 nibbles 1 2 3 4, end
    fill(&mut data, 0x0D, &[0x80, 0x03, 0x12, 0x34, 0x00]);
    // phrase 1: rate 2, 2 nibbles A B, end (preceded by a silence block)
    fill(&mut data, 0x13, &[0x01, 0x81, 0x01, 0xAB, 0x00]);
    // phrase 2: rate 2, 3 nibbles (odd: the last low nibble is dropped)
    fill(&mut data, 0x19, &[0x81, 0x02, 0x12, 0x34, 0x00]);
    data
}

/// One phrase: 0xC1 repeats the following block twice.
fn rom_repeat() -> Vec<u8> {
    let mut data = vec![0u8; 0x12];
    fill(&mut data, 0, &[0x00, 0x5A, 0xA5, 0x69, 0x55]);
    fill(&mut data, 5, &5u16.to_be_bytes()); // -> 0x0B
    fill(&mut data, 0x0B, &[0xC1, 0x80, 0x01, 0xAB, 0x00]);
    data
}

#[allow(non_snake_case)]
mod TestTable {
    use super::*;

    #[test]
    fn test_header_and_vectors() {
        let rom = rom_three_phrases();
        assert_eq!(phrase_count(&rom).unwrap(), 3);
        assert_eq!(phrase_address(&rom, 0).unwrap(), 0x0D);
        assert_eq!(phrase_address(&rom, 1).unwrap(), 0x13);
        assert_eq!(phrase_address(&rom, 2).unwrap(), 0x19);
    }

    #[test]
    fn test_bad_magic() {
        assert!(phrase_count(&[0x00, 0x00, 0x00, 0x00, 0x00]).is_err());
    }

    #[test]
    fn test_index_range() {
        let rom = rom_three_phrases();
        assert!(phrase_address(&rom, 3).is_err());
    }
}

#[allow(non_snake_case)]
mod TestDecode {
    use super::*;

    #[test]
    fn test_nibble_block() {
        // step_table[0] first nibbles: 1 -> 0, 2 -> +1, 3 -> +2, 4 -> +3
        // each block header costs 36 clocks (held level), the count byte 36 more
        let phrase = decode_phrase(&rom_three_phrases(), 0, STANDARD_CLOCK).unwrap();
        assert_eq!(
            phrase.events,
            vec![(0, 36), (0, 36), (0, 4), (1, 4), (3, 4), (6, 4), (0, 1024)]
        );
        assert_eq!(phrase.end, Some(0x12));
        assert_eq!(phrase.rates, BTreeSet::from([1]));
        assert_almost_eq!(phrase.seconds(STANDARD_CLOCK), 1112.0 / 640000.0, 9);
    }

    #[test]
    fn test_silence_and_negative_steps() {
        let phrase = decode_phrase(&rom_three_phrases(), 1, STANDARD_CLOCK).unwrap();
        assert_eq!(
            phrase.events,
            vec![(0, 2048), (0, 36), (0, 36), (-1, 8), (-3, 8), (0, 1024)]
        );
    }

    #[test]
    fn test_odd_nibble_count_drops_low_nibble() {
        let phrase = decode_phrase(&rom_three_phrases(), 2, STANDARD_CLOCK).unwrap();
        assert_eq!(
            phrase.events,
            vec![(0, 36), (0, 36), (0, 8), (1, 8), (3, 8), (0, 1024)]
        );
        assert_eq!(phrase.rates, BTreeSet::from([2]));
    }

    #[test]
    fn test_repeat_block_plays_twice() {
        let phrase = decode_phrase(&rom_repeat(), 0, STANDARD_CLOCK).unwrap();
        assert_eq!(
            phrase.events,
            vec![
                (0, 36),
                (0, 36),
                (0, 36),
                (-1, 4),
                (-3, 4),
                (-3, 36),
                (-3, 36),
                (-4, 4),
                (-6, 4),
                (0, 1024)
            ]
        );
        assert_eq!(phrase.end, Some(0x10));
    }
}

#[allow(non_snake_case)]
mod TestRender {
    use super::*;

    #[test]
    fn test_levels_interpolate_between_event_points() {
        let phrase = decode_phrase(&rom_three_phrases(), 0, STANDARD_CLOCK).unwrap();
        let levels = render_levels(&phrase, 160000, STANDARD_CLOCK); // one output frame per clock/4
        assert_eq!(levels.len(), 278);
        // event points hold levels 0 (x3), 1, 3, 6, then the final silence
        assert_almost_eq!(levels[0], 0.0, 6);
        assert_almost_eq!(levels[18], 0.0, 6);
        assert_almost_eq!(levels[19], 0.5, 6);
        assert_almost_eq!(levels[20], 2.0, 6);
        assert_almost_eq!(levels[21], 4.5, 6);
        assert!(levels.iter().copied().fold(f64::NEG_INFINITY, f64::max) > 5.9); // peak sits between output samples
        assert!(levels[levels.len() - 1] < 0.1); // ramps to silence, no click
    }

    #[test]
    fn test_wav_bytes_are_sixteen_bit() {
        let phrase = decode_phrase(&rom_three_phrases(), 0, STANDARD_CLOCK).unwrap();
        let pcm = render_wav_bytes(&phrase, 44100, STANDARD_CLOCK, 1.0);
        assert_eq!(pcm.len() % 2, 0);
        assert!(pcm.len() > 0);
    }
}

#[allow(non_snake_case)]
mod TestDump {
    use super::*;

    fn wav_format(path: &Path) -> (u16, u16, u32) {
        let bytes = std::fs::read(path).unwrap();
        assert_eq!(&bytes[0..4], b"RIFF");
        assert_eq!(&bytes[8..12], b"WAVE");
        let channels = u16::from_le_bytes([bytes[22], bytes[23]]);
        let rate = u32::from_le_bytes([bytes[24], bytes[25], bytes[26], bytes[27]]);
        let bits = u16::from_le_bytes([bytes[34], bytes[35]]);
        (channels, bits / 8, rate)
    }

    #[test]
    fn test_dump_writes_wavs_and_index() {
        let rom = rom_three_phrases();
        let td = tempfile::TempDir::new().unwrap();
        let out = td.path().join("upd");
        let index = dump_rom(&rom, &out, "synthetic", 44100, STANDARD_CLOCK, Some(0x70), None)
            .unwrap();
        assert_eq!(index["items"].as_array().unwrap().len(), 3);
        assert_eq!(index["items"][0]["code"], "0x70");
        assert_eq!(index["gain"].as_f64().unwrap(), 1.0);
        let (channels, sampwidth, rate) = wav_format(&out.join("voice_00_code70.wav"));
        assert_eq!(rate, 44100);
        assert_eq!(channels, 1);
        assert_eq!(sampwidth, 2);
        let written_text = std::fs::read_to_string(out.join("upd7759_index.json")).unwrap();
        let written: serde_json::Value = serde_json::from_str(&written_text).unwrap();
        assert_eq!(written["phrases"].as_u64().unwrap(), 3);
        let indices: Vec<u64> = written["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|item| item["index"].as_u64().unwrap())
            .collect();
        assert_eq!(indices, vec![0, 1, 2]);
    }
}
