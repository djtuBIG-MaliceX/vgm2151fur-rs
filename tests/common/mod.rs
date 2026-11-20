//! Shared test helpers: the port of `tests/_synth.py` plus the path and
//! environment shims the Python tests use.

#![allow(dead_code)]

use std::path::{Path, PathBuf};

use vgm2fur::vgm::ChipWrite;

/// The YM2151 patch every synthetic song loads.
pub const PATCH: [(u16, u16); 24] = [
    (0x20, 0xC7),
    (0x28, 0x3E),
    (0x30, 0x00),
    (0x38, 0x00),
    (0x60, 0x00),
    (0x68, 0x00),
    (0x70, 0x00),
    (0x78, 0x00),
    (0x80, 0x1F),
    (0x88, 0x1F),
    (0x90, 0x1F),
    (0x98, 0x1F),
    (0xA0, 0x00),
    (0xA8, 0x00),
    (0xB0, 0x00),
    (0xB8, 0x00),
    (0xC0, 0x00),
    (0xC8, 0x00),
    (0xD0, 0x00),
    (0xD8, 0x00),
    (0xE0, 0x0F),
    (0xE8, 0x0F),
    (0xF0, 0x0F),
    (0xF8, 0x0F),
];

/// A heartbeat of key-ons: the analyzer measures the row grid from key-on
/// spacing, so every test song needs one.
pub fn lattice(n: usize, spacing: i64) -> Vec<ChipWrite> {
    (0..n)
        .map(|k| ChipWrite::new(k as i64 * spacing, "ym2151", 0, 0x08, 0x78))
        .collect()
}

/// A loadable VGM file: header, the given command bytes, end-of-data.
pub fn raw_vgm(cmds: &[u8], ym_clock: u32) -> Vec<u8> {
    let mut buf = vec![0u8; 0x40];
    buf[0..4].copy_from_slice(b"Vgm ");
    buf[8..12].copy_from_slice(&0x161u32.to_le_bytes());
    buf[0x18..0x1C].copy_from_slice(&44100u32.to_le_bytes());
    buf[0x30..0x34].copy_from_slice(&ym_clock.to_le_bytes());
    buf[0x34..0x38].copy_from_slice(&(0x40u32 - 0x34).to_le_bytes());
    buf.extend_from_slice(cmds);
    buf.push(0x66);
    let len = (buf.len() - 4) as u32;
    buf[4..8].copy_from_slice(&len.to_le_bytes());
    buf
}

/// Corpus root: `VGM2FUR_SAMPLES`, else `D:/Downloads/moreVGMs`.
pub fn samples_root() -> Option<PathBuf> {
    for candidate in [std::env::var_os("VGM2FUR_SAMPLES"), Some("D:/Downloads/moreVGMs".into())] {
        if let Some(v) = candidate {
            let p = PathBuf::from(v);
            if p.is_dir() {
                return Some(p);
            }
        }
    }
    let fallback = PathBuf::from("D:/Downloads");
    fallback.is_dir().then_some(fallback)
}

/// A corpus file by relative path, or None when the corpus is absent.
pub fn sample_path(relative: &str) -> Option<PathBuf> {
    let root = samples_root()?;
    let p = root.join(relative);
    p.is_file().then_some(p)
}

/// `self.assertAlmostEqual(a, b, places=7)`.
#[macro_export]
macro_rules! assert_almost_eq {
    ($a:expr, $b:expr) => {
        assert_almost_eq!($a, $b, 7)
    };
    ($a:expr, $b:expr, $places:expr) => {
        assert!(
            vgm2fur::parity::py_almost_eq($a as f64, $b as f64, $places),
            "{} != {} within {} places",
            $a,
            $b,
            $places
        )
    };
}

/// `@unittest.skipUnless(_furnace_available(), ...)`.
#[macro_export]
macro_rules! skip_without_furnace {
    () => {
        if vgm2fur::furnace::find_furnace().is_none() {
            eprintln!("skipping: furnace.exe not found");
            return;
        }
    };
}

/// Write bytes to a temp file and return the path; the directory is kept
/// alive by the returned handle.
pub fn temp_file(dir: &Path, name: &str, bytes: &[u8]) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, bytes).expect("write temp file");
    path
}

/// The synthetic `VgmFile` every probe starts from.
pub fn vgm(writes: Vec<ChipWrite>) -> vgm2fur::vgm::VgmFile {
    let mut v = vgm2fur::vgm::VgmFile::default();
    v.path = PathBuf::from("synthetic.vgm");
    v.buf = Vec::new();
    v.version = 0x161;
    v.total_samples = 400_000;
    v.loop_samples = 0;
    v.loop_file_pos = None;
    v.loop_sample = None;
    v.ym2151_clock = 3_579_545;
    v.k007232_clock = 0;
    v.gd3 = Default::default();
    v.roms = Vec::new();
    v.writes = writes;
    v
}

/// A heartbeat plus a note, optionally with mid-note KC/KF writes.
pub fn note_song_with(
    key_on: i64,
    key_off: i64,
    kc: u16,
    kf: u16,
    pitch_writes: &[(i64, u16, u16)],
    ch: u16,
) -> vgm2fur::vgm::VgmFile {
    let mut writes = lattice((key_off / 5000 + 6) as usize, 5000);
    for &(reg, val) in PATCH.iter() {
        let reg = if ch != 0 && (reg == 0x28 || reg == 0x30) { reg | ch } else { reg };
        writes.push(ChipWrite::new(key_on - 1000, "ym2151", 0, reg as u32, val as u32));
    }
    writes.push(ChipWrite::new(key_on, "ym2151", 0, (0x28 | ch) as u32, kc as u32));
    writes.push(ChipWrite::new(key_on, "ym2151", 0, (0x30 | ch) as u32, kf as u32));
    writes.push(ChipWrite::new(key_on, "ym2151", 0, 0x08, (0x78 | ch) as u32));
    for &(sample, reg, val) in pitch_writes {
        writes.push(ChipWrite::new(sample, "ym2151", 0, reg as u32, val as u32));
    }
    writes.push(ChipWrite::new(key_off, "ym2151", 0, 0x08, ch as u32));
    writes.sort_by_key(|w| w.sample);
    vgm(writes)
}

/// `note_song()` with the Python defaults.
pub fn note_song() -> vgm2fur::vgm::VgmFile {
    note_song_with(60_000, 160_000, 0x00, 0x00, &[], 0)
}

/// KC/KF writes of one 0.25 st step each.
pub fn staircase(start: i64, steps: usize, step_samples: i64, kc: u16, kf: u16, ch: u16) -> Vec<(i64, u16, u16)> {
    let mut out = Vec::new();
    let mut kc_v = kc as i32;
    let mut kf_v = kf as i32;
    for i in 0..steps {
        kf_v += 0x40;
        if kf_v > 0xFF {
            kf_v -= 0x100;
            kc_v += 1;
        }
        let t = start + i as i64 * step_samples;
        out.push((t, 0x28 | ch, kc_v as u16));
        out.push((t + 10, 0x30 | ch, kf_v as u16));
    }
    out
}

/// KC writes climbing whole semitones a few samples apart.
pub fn climb(start: i64, semitones: usize, step_samples: i64, kc: u16, kf: u16, ch: u16) -> Vec<(i64, u16, u16)> {
    let base = vgm2fur::model::kc_kf_units(kc as i32, kf as i32);
    let mut out = Vec::new();
    for i in 1..=semitones {
        let target = base + 64 * i as i32;
        let mut code = None;
        for cand in 0..256i32 {
            if vgm2fur::model::kc_kf_units(cand, kf as i32) == target {
                code = Some(cand);
                break;
            }
        }
        let Some(code) = code else { break };
        out.push((start + i as i64 * step_samples, 0x28 | ch, code as u16));
    }
    out
}

/// `_synth.inject_rows`: run `body` with the writer's sweep emitter wrapped so
/// extra effect columns land on rows offset from the note's own row.
///
/// Python swaps `furwrite._emit_sweep` for the duration of the block; the Rust
/// writer takes the same override through `EmitHooks`, and the hook calls the
/// real emitter first exactly like the reference's `original(...)`.
pub fn inject_rows<R>(
    song: &mut vgm2fur::model::Song,
    plan: &[(i32, i32, i32)],
    key_on: i64,
    body: impl FnOnce(&mut vgm2fur::model::Song, i32, &mut vgm2fur::furwrite::EmitHooks<'_>) -> R,
) -> R {
    for ev in song.events.iter_mut() {
        if ev.on && ev.sample == key_on && ev.sweep.is_empty() {
            ev.sweep = vec![(key_on + 500, 0)];
        }
    }
    let row0 = song.place(key_on, None).0;
    let plan: Vec<(i32, i32, i32)> = plan.to_vec();
    let mut hook = move |grid: &mut vgm2fur::furwrite::Grid,
                         song_arg: &vgm2fur::model::Song,
                         ev: &vgm2fur::model::NoteEvent,
                         fx_cols: usize,
                         total_rows: i32,
                         sweep_e5: &mut indexmap::IndexMap<(i32, i32), i32>,
                         note_rows: &indexmap::IndexMap<i32, Vec<i32>>,
                         chan: Option<i32>,
                         mut stats: Option<&mut indexmap::IndexMap<String, i64>>| {
        vgm2fur::furwrite::_emit_sweep(
            grid,
            song_arg,
            ev,
            fx_cols,
            total_rows,
            sweep_e5,
            note_rows,
            chan,
            stats.as_deref_mut(),
        );
        if !ev.on || ev.ch >= 8 {
            return;
        }
        let ch = chan.unwrap_or(ev.ch) as usize;
        for &(k, cmd, val) in plan.iter() {
            let idx = row0 + k;
            if grid[ch].get(&idx).is_none() {
                grid[ch].insert(idx, vgm2fur::furwrite::_blank_row());
            }
            if let Some(cell) = grid[ch].get_mut(&idx) {
                vgm2fur::furwrite::_add_fx(cell, cmd, val, fx_cols, stats.as_deref_mut());
            }
        }
    };
    let mut hooks = vgm2fur::furwrite::EmitHooks { sweep_hook: Some(&mut hook) };
    body(song, row0, &mut hooks)
}
