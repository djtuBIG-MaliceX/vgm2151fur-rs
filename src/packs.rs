//! Find VGM packs and read their playlists.
//!
//! A pack is a folder with .vgm/.vgz files directly inside. Many packs ship an
//! .m3u playlist that fixes the track order, sometimes different from the
//! alphabetical order. The browser lists packs, and a pack view lists its tracks
//! in playlist order when there is one.
//!
//! Port of `vgm2151fur/packs.py` (0.11.0).

use std::collections::{BTreeSet, HashSet};
use std::path::{Path, PathBuf};

use crate::error::{Error, Result};
use crate::vgm::{chips_in_head, head_bytes};

/// `.vgm`/`.vgz` suffixes, lowercased for comparison.
pub const VGM_SUFFIXES: &[&str] = &[".vgm", ".vgz"];

/// Directories the pack scan never enters.
pub const SKIP_DIRS: &[&str] = &[
    ".git",
    "__pycache__",
    "node_modules",
    "output",
    "third_party",
    "soundcheck",
    "fur",
    "docs",
    "tests",
];

/// Display order and names for the header-visible chips. A K052539 rip reports
/// both "k051649" (the clock field it shares) and "scc_plus"; the label shows
/// the more specific one only.
pub const CHIP_LABELS: &[(&str, &str)] = &[
    ("ym2151", "YM2151"),
    ("ym2203", "YM2203"),
    ("ym2612", "YM2612"),
    ("ym2610", "YM2610"),
    ("ym2610_b", "YM2610B"),
    ("ym2608", "YM2608"),
    // The OPL family: an AdLib-style log names its chip by command byte, and
    // rhythm mode is reported apart because it changes the channel layout.
    ("opl", "OPL"),
    ("opl2", "OPL2"),
    ("opl3", "OPL3"),
    ("opll", "YM2413"),
    ("k007232", "K007232"),
    ("segapcm", "SegaPCM"),
    ("c140", "C140"),
    ("c352", "C352"),
    ("k053260", "K053260"),
    ("c219", "C219"),
    ("k054539", "K054539"),
    ("ga20", "GA20"),
    ("es5503", "ES5503"),
    ("msm5205", "MSM5205"),
    ("msm5232", "MSM5232"),
    ("msm6295", "MSM6295"),
    ("msm6258", "MSM6258"),
    ("scc_plus", "SCC+"),
    ("k051649", "SCC"),
    ("ay8910", "AY-8910"),
    ("saa1099", "SAA1099"),
    ("pokey", "POKEY"),
    ("fds", "FDS"),
    ("wonderswan", "WonderSwan"),
    ("qsound", "QSound"),
];

/// One folder holding VGMs, its tracks, and its optional playlist.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Pack {
    pub path: PathBuf,
    pub tracks: Vec<PathBuf>,
    pub playlist: Option<PathBuf>,
    pub chips: BTreeSet<String>,
}

impl Pack {
    /// `Pack.name`: the folder's own name.
    pub fn name(&self) -> String {
        self.path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
    }

    /// `Pack.chip_label`: the header chips in display order, joined by ", ".
    pub fn chip_label(&self) -> String {
        let mut kinds: BTreeSet<&str> = self.chips.iter().map(String::as_str).collect();
        if kinds.contains("scc_plus") {
            kinds.remove("k051649");
        }
        if kinds.contains("ym2610_b") {
            kinds.remove("ym2610");
        }
        let names: Vec<&str> = CHIP_LABELS
            .iter()
            .filter(|(kind, _label)| kinds.contains(kind))
            .map(|(_kind, label)| *label)
            .collect();
        names.join(", ")
    }
}

/// Folders under `root` that hold .vgm/.vgz files directly, with Python's
/// `max_depth=5, limit=80` defaults.
pub fn find_packs(root: &Path) -> Vec<Pack> {
    find_packs_with(root, 5, Some(80))
}

/// Folders under `root` that hold .vgm/.vgz files directly.
///
/// `limit` caps the list so a scan from a huge root still returns. `None`
/// walks the whole tree. The scan is the literal `os.walk` loop: directories
/// are filtered and sorted, the depth limit cuts the children of a directory
/// at `max_depth`, and a pack is appended once its tracks are collected.
pub fn find_packs_with(root: &Path, max_depth: usize, limit: Option<usize>) -> Vec<Pack> {
    let mut packs: Vec<Pack> = Vec::new();
    walk_dir(root, 0, max_depth, limit, &mut packs);
    packs
}

/// One `os.walk` directory step. Returns true when `limit` ended the scan.
fn walk_dir(
    dir: &Path,
    depth: usize,
    max_depth: usize,
    limit: Option<usize>,
    packs: &mut Vec<Pack>,
) -> bool {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        // os.walk ignores a directory it cannot open.
        Err(_) => return false,
    };
    let mut dirnames: Vec<(String, PathBuf)> = Vec::new();
    let mut filenames: Vec<PathBuf> = Vec::new();
    let mut playlists: Vec<PathBuf> = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        // pathlib.Path.glob("*.m3u") matches entries regardless of type; on
        // Windows fnmatch is case-insensitive.
        if name.to_lowercase().ends_with(".m3u") {
            playlists.push(entry.path());
        }
        // os.walk classifies with entry.is_dir(), which follows symlinks.
        let is_dir = entry.metadata().map(|m| m.is_dir()).unwrap_or(false);
        if is_dir {
            dirnames.push((name, entry.path()));
        } else {
            filenames.push(entry.path());
        }
    }

    dirnames.retain(|(name, _path)| {
        !SKIP_DIRS.contains(&name.as_str()) && !name.starts_with('.')
    });
    dirnames.sort_by(|a, b| a.0.cmp(&b.0));
    if depth >= max_depth {
        dirnames.clear();
    }

    let mut tracks: Vec<PathBuf> = filenames
        .into_iter()
        .filter(|path| {
            path.file_name()
                .map(|name| {
                    let name = name.to_string_lossy().to_lowercase();
                    VGM_SUFFIXES.iter().any(|suffix| name.ends_with(suffix))
                })
                .unwrap_or(false)
        })
        .collect();
    tracks.sort_by(|a, b| {
        let left = a.file_name().map(|n| n.to_string_lossy().to_lowercase()).unwrap_or_default();
        let right = b.file_name().map(|n| n.to_string_lossy().to_lowercase()).unwrap_or_default();
        left.cmp(&right)
    });

    if !tracks.is_empty() {
        playlists.sort_by(|a, b| {
            a.to_string_lossy().to_lowercase().cmp(&b.to_string_lossy().to_lowercase())
        });
        packs.push(Pack {
            path: dir.to_path_buf(),
            tracks,
            playlist: playlists.into_iter().next(),
            chips: BTreeSet::new(),
        });
        if let Some(limit) = limit {
            if packs.len() >= limit {
                return true;
            }
        }
    }

    // os.walk does not follow symlinked directories.
    for (_name, path) in dirnames {
        let is_link =
            std::fs::symlink_metadata(&path).map(|m| m.file_type().is_symlink()).unwrap_or(false);
        if is_link {
            continue;
        }
        if walk_dir(&path, depth + 1, max_depth, limit, packs) {
            return true;
        }
    }
    false
}

/// `path.resolve()`, lowercased, with the plain path as the fallback.
fn path_key(path: &Path) -> String {
    match std::fs::canonicalize(path) {
        Ok(resolved) => resolved.to_string_lossy().to_lowercase(),
        Err(_) => path.to_string_lossy().to_lowercase(),
    }
}

/// Python `str.splitlines()`: split on the characters `splitlines` treats as
/// line boundaries; a trailing boundary does not add an empty line.
fn py_splitlines(text: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = 0usize;
    let mut iter = text.char_indices().peekable();
    while let Some((idx, ch)) = iter.next() {
        let boundary = matches!(
            ch,
            '\n' | '\r'
                | '\u{0b}'
                | '\u{0c}'
                | '\u{1c}'
                | '\u{1d}'
                | '\u{1e}'
                | '\u{85}'
                | '\u{2028}'
                | '\u{2029}'
        );
        if !boundary {
            continue;
        }
        out.push(&text[start..idx]);
        if ch == '\r' {
            if let Some((next_idx, '\n')) = iter.peek().copied() {
                iter.next();
                start = next_idx + 1;
                continue;
            }
        }
        start = idx + ch.len_utf8();
    }
    if start < text.len() {
        out.push(&text[start..]);
    }
    out
}

/// Python `repr()` for a `str`, as used by the `!r` field in the errors.
fn py_repr_str(text: &str) -> String {
    let quote = if !text.contains('\'') {
        '\''
    } else if !text.contains('"') {
        '"'
    } else {
        '\''
    };
    let mut out = String::with_capacity(text.len() + 2);
    out.push(quote);
    for ch in text.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c == quote => {
                out.push('\\');
                out.push(c);
            }
            c if (c as u32) < 0x20 || (c as u32) == 0x7f => {
                out.push_str(&format!("\\x{:02x}", c as u32));
            }
            c => out.push(c),
        }
    }
    out.push(quote);
    out
}

/// Python `str.strip()` with no arguments: Unicode whitespace plus the four
/// file separators `str.isspace()` also accepts.
fn py_strip(text: &str) -> &str {
    text.trim_matches(|c: char| c.is_whitespace() || matches!(c, '\u{1c}'..='\u{1f}'))
}

/// Existing tracks named by an .m3u, in playlist order.
///
/// A line that points into a missing subdirectory still matches a file of
/// the same name sitting in the pack folder.
pub fn read_playlist(path: &Path) -> Result<Vec<PathBuf>> {
    let raw = std::fs::read(path).map_err(|e| Error::io(path.to_path_buf(), e))?;
    // utf-8-sig: decode with replacement, then drop a leading BOM.
    let mut text = String::from_utf8_lossy(&raw).into_owned();
    if let Some(stripped) = text.strip_prefix('\u{feff}') {
        text = stripped.to_string();
    }
    let folder = path.parent().unwrap_or(Path::new(""));
    let mut out: Vec<PathBuf> = Vec::new();
    for line in py_splitlines(&text) {
        let line = py_strip(line).trim_matches('"').trim_matches('\'');
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut candidate = folder.join(line);
        if !candidate.is_file() {
            let base = Path::new(line)
                .file_name()
                .unwrap_or_else(|| std::ffi::OsStr::new(""));
            candidate = folder.join(base);
        }
        if candidate.is_file() {
            out.push(candidate);
        }
    }
    Ok(out)
}

/// `(tracks, from_playlist)`. Playlist order, then files the playlist skipped.
pub fn pack_tracks(pack: &Pack) -> Result<(Vec<PathBuf>, bool)> {
    if let Some(playlist) = &pack.playlist {
        let listed = read_playlist(playlist)?;
        if !listed.is_empty() {
            let seen: HashSet<String> = listed.iter().map(|p| path_key(p)).collect();
            let rest: Vec<PathBuf> =
                pack.tracks.iter().filter(|p| !seen.contains(&path_key(p))).cloned().collect();
            let mut out = listed;
            out.extend(rest);
            return Ok((out, true));
        }
    }
    Ok((pack.tracks.clone(), false))
}

/// Chip kinds seen in the headers of up to `sample` tracks (default 6).
pub fn scan_pack_chips(pack: &Pack) -> BTreeSet<String> {
    scan_pack_chips_with(pack, 6)
}

/// Chip kinds seen in the headers of up to `sample` tracks.
pub fn scan_pack_chips_with(pack: &Pack, sample: usize) -> BTreeSet<String> {
    let mut kinds: BTreeSet<String> = BTreeSet::new();
    for track in pack.tracks.iter().take(sample) {
        match head_bytes(track, 0x200) {
            Ok(buf) => {
                for kind in chips_in_head(&buf) {
                    kinds.insert(kind.to_string());
                }
            }
            // Python catches OSError; head_bytes only fails on I/O.
            Err(_) => continue,
        }
    }
    kinds
}

/// `'all'`, `'3'`, `'2-6'`, `'1,3-5'` -> sorted 0-based track indexes.
///
/// Returns `Error::Msg` when a token is not a track or lands outside
/// `1..count`.
pub fn parse_selection(spec: &str, count: usize) -> Result<Vec<usize>> {
    let spec = py_strip(spec).to_lowercase();
    if spec.is_empty() || spec == "all" || spec == "*" {
        return Ok((0..count).collect());
    }
    let mut chosen: BTreeSet<usize> = BTreeSet::new();
    for token in spec.split(',') {
        let token = py_strip(token);
        if token.is_empty() {
            continue;
        }
        if token.contains('-') {
            let (low, high) = token.split_once('-').expect("contains '-'");
            let low = py_strip(low);
            let high = py_strip(high);
            if !is_digits(low) || !is_digits(high) {
                return Err(Error::Msg(format!(
                    "cannot read {} as a track range",
                    py_repr_str(token)
                )));
            }
            let range_error = || {
                Error::Msg(format!("range {} is outside 1..{}", py_repr_str(token), count))
            };
            let first: u64 = low.parse().map_err(|_| range_error())?;
            let last: u64 = high.parse().map_err(|_| range_error())?;
            if !(1 <= first && first <= last && last <= count as u64) {
                return Err(range_error());
            }
            chosen.extend((first as usize - 1)..(last as usize));
        } else if is_digits(token) {
            let value: u64 = match token.parse() {
                Ok(value) => value,
                // Python's int() takes arbitrarily long values; only a token
                // that parsed would reach the bounds message there.
                Err(_) => {
                    return Err(Error::Msg(format!(
                        "track {} is outside 1..{}",
                        token, count
                    )));
                }
            };
            if !(1 <= value && value <= count as u64) {
                return Err(Error::Msg(format!(
                    "track {} is outside 1..{}",
                    value, count
                )));
            }
            chosen.insert(value as usize - 1);
        } else {
            return Err(Error::Msg(format!(
                "cannot read {} as a track number",
                py_repr_str(token)
            )));
        }
    }
    Ok(chosen.into_iter().collect())
}

/// Python `str.isdigit()` for the ASCII digit runs track specs use. Empty is
/// false, like `"".isdigit()`.
fn is_digits(text: &str) -> bool {
    !text.is_empty() && text.bytes().all(|b| b.is_ascii_digit())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn digits() {
        assert!(is_digits("1"));
        assert!(!is_digits(""));
        assert!(!is_digits("x"));
    }

    #[test]
    fn splitlines_matches_python() {
        assert_eq!(py_splitlines(""), Vec::<&str>::new());
        assert_eq!(py_splitlines("a\n"), vec!["a"]);
        assert_eq!(py_splitlines("a\r\nb"), vec!["a", "b"]);
        assert_eq!(py_splitlines("a\rb\x0cc"), vec!["a", "b", "c"]);
    }

    #[test]
    fn repr_uses_python_quotes() {
        assert_eq!(py_repr_str("x"), "'x'");
        assert_eq!(py_repr_str("it's"), "\"it's\"");
        assert_eq!(py_repr_str("a'b\"c"), "'a\\'b\"c'");
    }

    #[test]
    fn digits_check() {
        digits();
    }
}
