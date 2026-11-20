//! Turn a pasted path into files the commands can open.
//!
//! Windows hands us quotes (Explorer's Copy as path, a drag onto the console),
//! a leading PowerShell ``&``, ``file://`` URIs, and globs that cmd.exe does not
//! expand. One cleaner feeds the menu and the command line.
//!
//! Port of `vgm2151fur/paths.py` (0.11.0).

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use glob::{glob_with, MatchOptions};

use crate::convert::{iter_vgms, VGM_SUFFIXES};
use crate::packs::{find_packs_with, pack_tracks, Pack};

/// The module suffix `fur_targets` looks for.
pub const FUR_SUFFIX: &str = ".fur";

/// Strip the wrapping people paste around a path. The path itself stays.
pub fn clean_text(text: &str) -> String {
    let mut s = py_strip(text).trim_matches('\u{feff}').to_string();
    if let Some(rest) = s.strip_prefix('&') {
        s = py_strip(rest).to_string();
    }
    // while changed and len(s) >= 2: matching quote pairs come off in layers.
    loop {
        let mut chars = s.chars();
        let Some(first) = chars.next() else { break };
        if chars.next().is_none() {
            break;
        }
        let last = s.chars().next_back().expect("non-empty");
        if first == last && (first == '"' || first == '\'') {
            s = py_strip(&s[first.len_utf8()..s.len() - last.len_utf8()]).to_string();
        } else {
            break;
        }
    }
    if has_two_chars(&s) {
        let first = s.chars().next().expect("non-empty");
        let last = s.chars().next_back().expect("non-empty");
        if (last == '"' || last == '\'') && first != '"' && first != '\'' {
            // A trailing backslash ate the closing quote on the way in: C:\dir"
            s = py_rstrip(&s[..s.len() - last.len_utf8()]).to_string();
        }
    }
    if has_two_chars(&s) {
        let first = s.chars().next().expect("non-empty");
        let last = s.chars().next_back().expect("non-empty");
        if first == '<' && last == '>' {
            s = py_strip(&s[1..s.len() - 1]).to_string();
        }
    }
    let is_file_uri = s.get(..5).is_some_and(|prefix| prefix.eq_ignore_ascii_case("file:"));
    if is_file_uri {
        let mut path = unquote(&urlparse_path(&s));
        let bytes = path.as_bytes();
        if bytes.len() >= 3 && bytes[0] == b'/' && bytes[2] == b':' {
            path = path[1..].to_string();
        }
        s = path;
    }
    expanduser(&s)
}

/// One pasted token. A glob with no matches is an empty list.
///
/// A path that does not exist is returned as a single path so the caller
/// can say it was not found. An existing path is returned as-is, not walked.
pub fn expand_token(text: &str) -> Vec<PathBuf> {
    let cleaned = clean_text(text);
    if cleaned.is_empty() {
        return Vec::new();
    }
    let path = PathBuf::from(&cleaned);
    if path.exists() {
        return vec![path];
    }
    if cleaned.chars().any(|ch| matches!(ch, '*' | '?' | '[')) {
        let options = MatchOptions {
            // fnmatch compares with normcase on Windows.
            case_sensitive: !cfg!(windows),
            require_literal_separator: false,
            require_literal_leading_dot: false,
        };
        return match glob_with(&cleaned, options) {
            Ok(paths) => paths.filter_map(Result::ok).collect(),
            // Python's fnmatch treats an unmatched '[' as a literal; glob
            // reports a pattern error instead and finds nothing.
            Err(_) => Vec::new(),
        };
    }
    vec![path]
}

/// Every token expanded, with duplicates removed in first-seen order.
pub fn expand_tokens(texts: &[String]) -> Vec<PathBuf> {
    let mut found: Vec<PathBuf> = Vec::new();
    for text in texts {
        found.extend(expand_token(text));
    }
    dedupe(&found)
}

/// Drop paths whose resolved, lowercased spelling was already seen.
pub fn dedupe(paths: &[PathBuf]) -> Vec<PathBuf> {
    let mut seen: HashSet<String> = HashSet::new();
    let mut out: Vec<PathBuf> = Vec::new();
    for path in paths {
        let key = _key(path);
        if seen.insert(key) {
            out.push(path.clone());
        }
    }
    out
}

/// VGMs directly in `folder`, playlist order first when an .m3u names them.
pub fn ordered_vgms(folder: &Path) -> Vec<PathBuf> {
    let direct = iter_vgms(folder);
    if direct.is_empty() {
        return Vec::new();
    }
    let playlists = playlists_in(folder);
    if playlists.is_empty() {
        return direct;
    }
    let pack = Pack {
        path: folder.to_path_buf(),
        tracks: direct,
        playlist: Some(playlists[0].clone()),
        chips: Default::default(),
    };
    match pack_tracks(&pack) {
        Ok((tracks, _from_playlist)) => dedupe(&tracks),
        // Python would let the playlist read raise; the caller has no error
        // channel here, so keep the folder's own order.
        Err(_) => pack.tracks,
    }
}

/// A pack for this folder, or None when it holds no VGMs of its own.
pub fn folder_pack(folder: &Path) -> Option<Pack> {
    let tracks = iter_vgms(folder);
    if tracks.is_empty() {
        return None;
    }
    let playlists = playlists_in(folder);
    Some(Pack {
        path: folder.to_path_buf(),
        tracks,
        playlist: playlists.into_iter().next(),
        chips: Default::default(),
    })
}

/// `(files, nested packs)`.
///
/// A folder that itself holds VGMs is `files` (playlist order when there is
/// an .m3u) and is not also returned as a pack. A folder of packs is
/// `nested packs` and no files. `pack_limit` is passed to the nested scan;
/// None walks the whole tree.
pub fn vgm_targets(path: &Path, pack_limit: Option<usize>) -> (Vec<PathBuf>, Vec<Pack>) {
    if !path.exists() {
        return (Vec::new(), Vec::new());
    }
    if path.is_file() {
        let suffix = suffix_lower(path);
        let files = if VGM_SUFFIXES.iter().any(|known| *known == suffix.as_str()) {
            vec![path.to_path_buf()]
        } else {
            Vec::new()
        };
        return (files, Vec::new());
    }
    let direct = ordered_vgms(path);
    if !direct.is_empty() {
        return (direct, Vec::new());
    }
    let nested = find_packs_with(path, 5, pack_limit)
        .into_iter()
        .filter(|pack| !_same(&pack.path, path))
        .collect();
    (Vec::new(), nested)
}

/// A `.fur` file, the `.fur` files in a folder, or that folder's `fur/` child.
///
/// When the folder itself has none, one level of subfolders is included, so
/// a pack folder works after a convert and so does a folder of those packs.
pub fn fur_targets(path: &Path) -> Vec<PathBuf> {
    if !path.exists() {
        return Vec::new();
    }
    if path.is_file() {
        return if suffix_lower(path) == FUR_SUFFIX {
            vec![path.to_path_buf()]
        } else {
            Vec::new()
        };
    }
    let mut direct = fur_glob(path);
    direct.sort_by(|a, b| name_lower(a).cmp(&name_lower(b)));
    let direct = dedupe(&direct);
    if !direct.is_empty() {
        return direct;
    }
    let child = path.join("fur");
    if child.is_dir() {
        let mut nested = fur_glob(&child);
        nested.sort_by(|a, b| name_lower(a).cmp(&name_lower(b)));
        let nested = dedupe(&nested);
        if !nested.is_empty() {
            return nested;
        }
    }
    let entries = match std::fs::read_dir(path) {
        Ok(entries) => entries,
        Err(_) => return Vec::new(),
    };
    let mut children: Vec<(String, PathBuf)> = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') {
            continue;
        }
        if entry.metadata().map(|meta| meta.is_dir()).unwrap_or(false) {
            children.push((name, entry.path()));
        }
    }
    children.sort_by(|a, b| a.0.to_lowercase().cmp(&b.0.to_lowercase()));
    let mut found: Vec<PathBuf> = Vec::new();
    for (name, sub) in children {
        if name == "__pycache__" || name == "fur" {
            continue;
        }
        found.extend(fur_glob(&sub));
        let fur = sub.join("fur");
        if fur.is_dir() {
            found.extend(fur_glob(&fur));
        }
    }
    found.sort_by(|a, b| path_lower(a).cmp(&path_lower(b)));
    dedupe(&found)
}

/// `str(path.resolve()).lower()`, with the lowercased path as the fallback.
fn _key(path: &Path) -> String {
    match std::fs::canonicalize(path) {
        Ok(resolved) => resolved.to_string_lossy().to_lowercase(),
        Err(_) => path_lower(path),
    }
}

/// `a.resolve().samefile(b.resolve())` for the identities this module needs.
fn _same(a: &Path, b: &Path) -> bool {
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(left), Ok(right)) => left == right,
        _ => {
            let left = _key(a);
            let right = _key(b);
            left.trim_end_matches(['\\', '/']) == right.trim_end_matches(['\\', '/'])
        }
    }
}

/// Python `Path.suffix.lower()`, dot included; empty when there is none.
fn suffix_lower(path: &Path) -> String {
    match path.extension() {
        Some(ext) => format!(".{}", ext.to_string_lossy()).to_lowercase(),
        None => String::new(),
    }
}

/// `p.name.lower()` used by the two `sorted(..., key=...)` calls.
fn name_lower(path: &Path) -> String {
    path.file_name().map(|name| name.to_string_lossy().to_lowercase()).unwrap_or_default()
}

/// `str(p).lower()` used by the final `fur_targets` sort.
fn path_lower(path: &Path) -> String {
    path.to_string_lossy().to_lowercase()
}

/// `.m3u` files directly in `folder`, sorted like `sorted(Path.glob(...))`.
fn playlists_in(folder: &Path) -> Vec<PathBuf> {
    let mut playlists: Vec<PathBuf> = Vec::new();
    let Ok(entries) = std::fs::read_dir(folder) else { return playlists };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if star_dot_matches(&name, ".m3u")
            && entry.metadata().map(|meta| meta.is_file()).unwrap_or(false)
        {
            playlists.push(entry.path());
        }
    }
    playlists.sort_by(|a, b| path_lower(a).cmp(&path_lower(b)));
    playlists
}

/// One `folder.glob("*.fur")`. Entries are not filtered by type, like pathlib.
fn fur_glob(folder: &Path) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    let Ok(entries) = std::fs::read_dir(folder) else { return out };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if star_dot_matches(&name, FUR_SUFFIX) {
            out.push(entry.path());
        }
    }
    out
}

/// Does `name` match `*<suffix>`? Windows fnmatch compares with normcase.
fn star_dot_matches(name: &str, suffix: &str) -> bool {
    let name = name.as_bytes();
    let suffix = suffix.as_bytes();
    if name.len() < suffix.len() {
        return false;
    }
    let tail = &name[name.len() - suffix.len()..];
    if cfg!(windows) { tail.eq_ignore_ascii_case(suffix) } else { tail == suffix }
}

/// Python `str.strip()` with no arguments.
fn py_strip(text: &str) -> &str {
    text.trim_matches(is_py_space)
}

/// Python `str.rstrip()` with no arguments.
fn py_rstrip(text: &str) -> &str {
    text.trim_end_matches(is_py_space)
}

/// Python's `str.isspace()`: Unicode whitespace plus `\x1c`..`\x1f`.
fn is_py_space(ch: char) -> bool {
    ch.is_whitespace() || matches!(ch, '\u{1c}'..='\u{1f}')
}

/// Python length check `len(s) >= 2`.
fn has_two_chars(s: &str) -> bool {
    s.chars().nth(1).is_some()
}

/// `os.path.expanduser` for the string paths this module handles.
fn expanduser(path: &str) -> String {
    if !path.starts_with('~') {
        return path.to_string();
    }
    // i is the first separator after the tilde, or len(path).
    let mut i = path.len();
    for (idx, ch) in path.char_indices().skip(1) {
        if ch == '\\' || ch == '/' {
            i = idx;
            break;
        }
    }
    let userhome = match std::env::var("USERPROFILE") {
        Ok(home) => home,
        Err(_) => match std::env::var("HOMEPATH") {
            Ok(homepath) => {
                let drive = std::env::var("HOMEDRIVE").unwrap_or_default();
                nt_join(&drive, &homepath)
            }
            Err(_) => return path.to_string(),
        },
    };
    let mut userhome = userhome;
    if i != 1 {
        // ~user
        let target_user = &path[1..i];
        let current_user = std::env::var("USERNAME").ok();
        if current_user.as_deref() != Some(target_user) {
            if current_user.as_deref() != Some(nt_basename(&userhome)) {
                return path.to_string();
            }
            userhome = nt_join(&nt_dirname(&userhome), target_user);
        }
    }
    format!("{userhome}{}", &path[i..])
}

/// `ntpath.join` for two paths.
fn nt_join(first: &str, second: &str) -> String {
    let (first_drive, first_root, first_path) = splitroot(first);
    let mut result_drive = first_drive.to_string();
    let mut result_root = first_root.to_string();
    let mut result_path = first_path.to_string();
    let (p_drive, p_root, p_path) = splitroot(second);
    let mut append = true;
    if !p_root.is_empty() {
        // Second path is absolute.
        if !p_drive.is_empty() || result_drive.is_empty() {
            result_drive = p_drive.to_string();
        }
        result_root = p_root.to_string();
        result_path = p_path.to_string();
    } else if !p_drive.is_empty() && p_drive != result_drive.as_str() {
        if !p_drive.eq_ignore_ascii_case(&result_drive) {
            // Different drives: ignore the first path entirely.
            result_drive = p_drive.to_string();
            result_root = p_root.to_string();
            result_path = p_path.to_string();
            append = false;
        } else {
            // Same drive in different case.
            result_drive = p_drive.to_string();
        }
    }
    if append {
        if !result_path.is_empty() && !ends_with_sep(&result_path) {
            result_path.push('\\');
        }
        result_path.push_str(&p_path);
    }
    if !result_path.is_empty()
        && result_root.is_empty()
        && !result_drive.is_empty()
        && !matches!(result_drive.chars().next_back(), Some(':') | Some('\\') | Some('/'))
    {
        return format!("{result_drive}\\{result_path}");
    }
    format!("{result_drive}{result_root}{result_path}")
}

/// `ntpath.dirname`.
fn nt_dirname(path: &str) -> String {
    nt_split(path).0
}

/// `ntpath.basename`.
fn nt_basename(path: &str) -> &str {
    nt_split(path).1
}

/// `ntpath.split`: `(head, tail)`.
fn nt_split(path: &str) -> (String, &str) {
    let (drive, root, tail) = splitroot(path);
    let bytes = tail.as_bytes();
    let mut i = tail.len();
    while i > 0 && !is_sep(bytes[i - 1]) {
        i -= 1;
    }
    let head = tail[..i].trim_end_matches(['\\', '/']);
    (format!("{drive}{root}{head}"), &tail[i..])
}

/// `ntpath.splitroot`: `(drive, root, tail)`.
fn splitroot(path: &str) -> (&str, &str, &str) {
    let bytes = path.as_bytes();
    if bytes.first() == Some(&b'\\') || bytes.first() == Some(&b'/') {
        if bytes.get(1) == Some(&b'\\') || bytes.get(1) == Some(&b'/') {
            let start = if path.len() >= 8
                && path.is_char_boundary(8)
                && path[..8].eq_ignore_ascii_case("\\\\?\\UNC\\")
            {
                8
            } else {
                2
            };
            let Some(index) = find_sep(bytes, start) else {
                return (path, "", "");
            };
            let Some(index2) = find_sep(bytes, index + 1) else {
                return (path, "", "");
            };
            return (&path[..index2], &path[index2..index2 + 1], &path[index2 + 1..]);
        }
        // Relative path with root, e.g. \Windows
        return ("", &path[..1], &path[1..]);
    }
    if bytes.get(1) == Some(&b':') {
        if bytes.get(2) == Some(&b'\\') || bytes.get(2) == Some(&b'/') {
            // Absolute drive-letter path, e.g. X:\Windows
            return (&path[..2], &path[2..3], &path[3..]);
        }
        // Relative path with drive, e.g. X:Windows
        return (&path[..2], "", &path[2..]);
    }
    ("", "", path)
}

fn find_sep(bytes: &[u8], start: usize) -> Option<usize> {
    if start >= bytes.len() {
        return None;
    }
    bytes[start..].iter().position(|byte| is_sep(*byte)).map(|pos| pos + start)
}

fn is_sep(byte: u8) -> bool {
    byte == b'\\' || byte == b'/'
}

fn ends_with_sep(text: &str) -> bool {
    matches!(text.chars().next_back(), Some('\\') | Some('/'))
}

/// `urllib.parse.urlparse(s).path`, including its control-character cleanup.
fn urlparse_path(url: &str) -> String {
    // Only lstrip: the WHATWG C0 control-or-space set.
    let url = url.trim_start_matches(|ch: char| ch <= ' ');
    let url: String =
        url.chars().filter(|ch| !matches!(ch, '\t' | '\r' | '\n')).collect();
    let mut rest: &str = &url;
    if let Some(colon) = url.find(':') {
        if colon > 0 {
            let first = url.chars().next().expect("colon implies non-empty");
            let scheme_ok = first.is_ascii_alphabetic()
                && url[..colon].chars().all(|ch| {
                    ch.is_ascii_alphanumeric() || matches!(ch, '+' | '-' | '.')
                });
            if scheme_ok {
                rest = &url[colon + 1..];
            }
        }
    }
    if let Some(after) = rest.strip_prefix("//") {
        // _splitnetloc(url, 2): the earliest of '/', '?' and '#'.
        let mut delim = after.len();
        for needle in ['/', '?', '#'] {
            if let Some(pos) = after.find(needle) {
                delim = delim.min(pos);
            }
        }
        rest = &after[delim..];
    }
    if let Some(pos) = rest.find('#') {
        rest = &rest[..pos];
    }
    if let Some(pos) = rest.find('?') {
        rest = &rest[..pos];
    }
    rest.to_string()
}

/// `urllib.parse.unquote` with the default UTF-8/replace decoding.
fn unquote(text: &str) -> String {
    if !text.contains('%') {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len());
    let mut run_start: Option<usize> = None;
    for (idx, ch) in text.char_indices() {
        if ch.is_ascii() {
            if run_start.is_none() {
                run_start = Some(idx);
            }
        } else {
            if let Some(start) = run_start.take() {
                out.push_str(&unquote_run(&text[start..idx]));
            }
            out.push(ch);
        }
    }
    if let Some(start) = run_start {
        out.push_str(&unquote_run(&text[start..]));
    }
    out
}

/// One ASCII run through `_unquote_impl`, decoded as UTF-8 with replacement.
fn unquote_run(run: &str) -> String {
    let mut parts = run.split('%');
    let mut bytes: Vec<u8> = Vec::with_capacity(run.len());
    if let Some(first) = parts.next() {
        bytes.extend_from_slice(first.as_bytes());
    }
    for item in parts {
        let item = item.as_bytes();
        if item.len() >= 2 {
            if let (Some(high), Some(low)) = (hex_value(item[0]), hex_value(item[1])) {
                bytes.push(high * 16 + low);
                bytes.extend_from_slice(&item[2..]);
                continue;
            }
        }
        bytes.push(b'%');
        bytes.extend_from_slice(item);
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(paths: &[PathBuf]) -> Vec<String> {
        paths.iter().map(|p| p.file_name().unwrap().to_string_lossy().into_owned()).collect()
    }

    #[test]
    fn clean_text_strips_paste_wrapping() {
        assert_eq!(clean_text("\"C:\\Music\\Song.vgz\""), "C:\\Music\\Song.vgz");
        assert_eq!(clean_text("& 'C:\\Music\\Song.vgz'"), "C:\\Music\\Song.vgz");
        assert_eq!(clean_text("file:///C:/Music/My%20Song.vgz"), "C:/Music/My Song.vgz");
        assert_eq!(clean_text("  \u{feff}<C:\\Music\\Song.vgm>  "), "C:\\Music\\Song.vgm");
        // A trailing backslash ate the closing quote.
        assert_eq!(clean_text("C:\\Music\\Song.vgz\""), "C:\\Music\\Song.vgz");
        assert_eq!(clean_text("~"), std::env::var("USERPROFILE").unwrap());
    }

    #[test]
    fn expand_token_takes_existing_paths_and_globs() {
        let dir = tempfile::tempdir().unwrap();
        let song = dir.path().join("Song.vgm");
        std::fs::write(&song, b"x").unwrap();
        let missing = dir.path().join("missing.vgm");
        assert_eq!(expand_token(missing.to_str().unwrap()), vec![missing.clone()]);
        let pattern = format!("{}{}*.vgm", dir.path().display(), std::path::MAIN_SEPARATOR);
        assert_eq!(expand_token(&pattern), vec![song]);
        assert!(expand_token("   ").is_empty());
    }

    #[test]
    fn dedupe_keeps_the_first_spelling() {
        let dir = tempfile::tempdir().unwrap();
        let song = dir.path().join("Song.vgm");
        std::fs::write(&song, b"x").unwrap();
        let spelled = dir.path().join(".").join("Song.vgm");
        let out = dedupe(&[song.clone(), song.clone(), spelled]);
        assert_eq!(out, vec![song]);
    }

    #[test]
    fn fur_targets_prefers_the_folder_then_its_fur_child() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("b.fur"), b"x").unwrap();
        std::fs::write(dir.path().join("A.FUR"), b"x").unwrap();
        let expected =
            if cfg!(windows) { vec!["A.FUR", "b.fur"] } else { vec!["b.fur"] };
        assert_eq!(names(&fur_targets(dir.path())), expected);

        let nested = tempfile::tempdir().unwrap();
        std::fs::create_dir(nested.path().join("fur")).unwrap();
        std::fs::write(nested.path().join("fur").join("x.fur"), b"x").unwrap();
        assert_eq!(names(&fur_targets(nested.path())), vec!["x.fur"]);

        let file = nested.path().join("only.fur");
        std::fs::write(&file, b"x").unwrap();
        assert_eq!(fur_targets(&file), vec![file]);
    }

    #[test]
    fn vgm_targets_orders_a_folder_by_its_playlist() {
        let dir = tempfile::tempdir().unwrap();
        for name in ["01 a.vgm", "02 b.vgm", "03 c.vgz"] {
            std::fs::write(dir.path().join(name), b"x").unwrap();
        }
        std::fs::write(dir.path().join("list.m3u"), "03 c.vgz\n01 a.vgm\n").unwrap();
        let (files, packs) = vgm_targets(dir.path(), Some(10));
        assert!(packs.is_empty());
        assert_eq!(names(&files), vec!["03 c.vgz", "01 a.vgm", "02 b.vgm"]);

        let pack = folder_pack(dir.path()).unwrap();
        assert_eq!(pack.tracks.len(), 3);
        assert_eq!(
            pack.playlist.unwrap().file_name().unwrap().to_string_lossy().as_ref(),
            "list.m3u"
        );

        let empty = tempfile::tempdir().unwrap();
        assert!(folder_pack(empty.path()).is_none());
        assert_eq!(
            vgm_targets(empty.path(), None),
            (Vec::<PathBuf>::new(), Vec::<Pack>::new())
        );
    }
}
