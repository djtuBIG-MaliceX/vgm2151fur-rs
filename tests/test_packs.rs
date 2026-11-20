//! Pack discovery and playlist tests, ported from `test_menu.py`'s
//! `TestPacks`.

mod common;

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use common::raw_vgm;
use vgm2fur::packs::{find_packs, pack_tracks, parse_selection, scan_pack_chips};

fn music_vgm() -> Vec<u8> {
    // The pack tests only read the header, so clock + terminator suffice.
    raw_vgm(&[], 3_579_545)
}

/// `_make_pack`: three tracks and a playlist that reverses them.
fn make_pack(root: &Path, tracks: usize) -> PathBuf {
    let pack = root.join("Test Pack (Arcade)");
    std::fs::create_dir_all(&pack).unwrap();
    let names: Vec<String> =
        (1..=tracks).map(|i| format!("0{i} Song {i}.vgz")).collect();
    for name in &names {
        std::fs::write(pack.join(name), music_vgm()).unwrap();
    }
    let mut playlist = names.iter().rev().cloned().collect::<Vec<_>>().join("\n");
    playlist.push('\n');
    std::fs::write(pack.join("Test Pack.m3u"), playlist).unwrap();
    pack
}

fn file_names(tracks: &[PathBuf]) -> BTreeSet<String> {
    tracks
        .iter()
        .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
        .collect()
}

#[test]
fn parse_selection_sorts_and_rejects() {
    assert_eq!(parse_selection("", 4).unwrap(), vec![0, 1, 2, 3]);
    assert_eq!(parse_selection("all", 4).unwrap(), vec![0, 1, 2, 3]);
    assert_eq!(parse_selection("2", 4).unwrap(), vec![1]);
    assert_eq!(parse_selection("2-3", 4).unwrap(), vec![1, 2]);
    assert_eq!(parse_selection("1,3-4", 4).unwrap(), vec![0, 2, 3]);
    assert_eq!(parse_selection("4,2", 4).unwrap(), vec![1, 3]);
    for bad in ["0", "5", "3-2", "x", "1-"] {
        assert!(parse_selection(bad, 4).is_err(), "{bad}");
    }
}

#[test]
fn find_packs_and_playlist_order() {
    let tmp = tempfile::tempdir().unwrap();
    let pack = make_pack(tmp.path(), 3);
    let found = find_packs(tmp.path());
    assert_eq!(found.iter().map(|p| p.path.clone()).collect::<Vec<_>>(), vec![pack]);
    let (tracks, from_playlist) = pack_tracks(&found[0]).unwrap();
    assert!(from_playlist);
    assert_eq!(tracks[0].file_name().unwrap().to_string_lossy(), "03 Song 3.vgz");
    assert_eq!(tracks.len(), 3);
}

#[test]
fn pack_chips_from_headers() {
    let tmp = tempfile::tempdir().unwrap();
    make_pack(tmp.path(), 3);
    let mut pack = find_packs(tmp.path()).remove(0);
    assert!(scan_pack_chips(&pack).contains("ym2151"));
    pack.chips = scan_pack_chips(&pack);
    assert!(pack.chip_label().contains("YM2151"));
}

#[test]
fn playlist_keeps_a_track_it_names_by_basename() {
    let tmp = tempfile::tempdir().unwrap();
    let pack = make_pack(tmp.path(), 3);
    // The line's directory is not there. The file in the pack still counts,
    // and the two tracks the playlist never names stay on the list.
    std::fs::write(pack.join("Test Pack.m3u"), "nested/03 Song 3.vgz\n").unwrap();
    let found = find_packs(tmp.path());
    let (tracks, from_playlist) = pack_tracks(&found[0]).unwrap();
    assert!(from_playlist);
    assert_eq!(tracks[0].file_name().unwrap().to_string_lossy(), "03 Song 3.vgz");
    assert_eq!(
        file_names(&tracks),
        BTreeSet::from([
            "01 Song 1.vgz".to_string(),
            "02 Song 2.vgz".to_string(),
            "03 Song 3.vgz".to_string(),
        ])
    );
}
