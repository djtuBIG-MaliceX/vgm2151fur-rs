//! Compressed-data-block tests for the VGM loader's 0x40-0x7E decoder and
//! the type-0x7F compression table.
//!
//! Mirrors the Python `_decompress_data_block` / `_parse_compression_table`
//! behavior, including the table case from
//! `test_vgm2151fur_ym2612.TestYm2612Parsing.test_compressed_dac_block_uses_the_7f_table`.

use vgm2fur::vgm::{_decompress_data_block, _parse_compression_table};

#[test]
fn uncompressed_passthrough() {
    // comp 0, sub 0: n-Bit values copied verbatim, 8-bit depth.
    let payload = [
        0x00, 0x04, 0x00, 0x00, 0x00, // comp, size = 4
        0x08, 0x08, // bits_dec = 8, bits_cmp = 8
        0x00, // sub 0
        0x00, 0x00, // add
        0xDE, 0xAD, 0xBE, 0xEF,
    ];
    assert_eq!(
        _decompress_data_block(0x40, &payload, None),
        Some(vec![0xDE, 0xAD, 0xBE, 0xEF]),
    );
}

#[test]
fn nibble_run_shifts_into_byte_depth() {
    // comp 0, sub 1: four 4-bit values shifted up to 8 bits.
    let payload = [
        0x00, 0x04, 0x00, 0x00, 0x00, // comp, size = 4
        0x08, 0x04, // bits_dec = 8, bits_cmp = 4
        0x01, // sub 1
        0x00, 0x00, // add
        0x12, 0x34,
    ];
    assert_eq!(
        _decompress_data_block(0x40, &payload, None),
        Some(vec![0x10, 0x20, 0x30, 0x40]),
    );
}

#[test]
fn delta_pcm_uses_the_7f_table() {
    // Four 8-bit outputs from 0x80 with deltas +1, +2, -1, 0.
    let table_payload = [
        0x01, 0x00, 0x08, 0x04, // comp, sub, bits_dec, bits_cmp
        0x04, 0x00, // count = 4
        0x01, 0x02, 0xFF, 0x00,
    ];
    let table = _parse_compression_table(&table_payload);
    assert_eq!(table, Some((1, 0, 8, 4, vec![1, 2, 255, 0])));
    let block = [
        0x01, 0x04, 0x00, 0x00, 0x00, // comp 1, size = 4
        0x08, 0x04, // bits_dec = 8, bits_cmp = 4
        0x00, // sub (unused for comp 1)
        0x80, 0x00, // initial value 0x80
        0x01, 0x23, // indexes 0, 1, 2, 3
    ];
    assert_eq!(
        _decompress_data_block(0x40, &block, table.as_ref()),
        Some(vec![0x81, 0x83, 0x82, 0x82]),
    );
}

#[test]
fn truncated_payload_returns_none() {
    // Fewer than ten header bytes: the reference returns None.
    let payload = [0x00, 0x04, 0x00, 0x00, 0x00, 0x08, 0x08, 0x00, 0x00];
    assert_eq!(_decompress_data_block(0x40, &payload, None), None);
    // Out-of-range types are rejected too.
    let full = [0u8; 10];
    assert_eq!(_decompress_data_block(0x3F, &full, None), None);
    assert_eq!(_decompress_data_block(0x7F, &full, None), None);
    // Invalid bit depths.
    let mut bad = full;
    bad[5] = 0;
    assert_eq!(_decompress_data_block(0x40, &bad, None), None);
    let mut bad = full;
    bad[6] = 17;
    assert_eq!(_decompress_data_block(0x40, &bad, None), None);
}

#[test]
fn parse_table_six_byte_header() {
    let payload = [0x01, 0x00, 0x08, 0x04, 0x00, 0x00];
    assert_eq!(
        _parse_compression_table(&payload),
        Some((1, 0, 8, 4, vec![])),
    );
}

#[test]
fn parse_table_short_payload() {
    // Not even the six-byte header.
    assert_eq!(
        _parse_compression_table(&[0x01, 0x00, 0x08, 0x04, 0x04]),
        None,
    );
    // Header promises four 8-bit entries but the payload stops early.
    assert_eq!(
        _parse_compression_table(&[0x01, 0x00, 0x08, 0x04, 0x04, 0x00, 0x01, 0x02]),
        None,
    );
    // Invalid bit depth and bit count.
    assert_eq!(
        _parse_compression_table(&[0x01, 0x00, 0x00, 0x04, 0x00, 0x00]),
        None,
    );
    assert_eq!(
        _parse_compression_table(&[0x01, 0x00, 0x08, 0x11, 0x00, 0x00]),
        None,
    );
}
