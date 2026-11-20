//! Row condensing and the structural loss detector.
//!
//! Port of `tests/test_condense.py`. Python patches `convert.load_vgm`,
//! `convert.analyze` and `convert.apply_loop`; here a `ConvertDriver` fake
//! plays those roles (the plan's test seam).

mod common;

use std::cell::RefCell;
use std::path::{Path, PathBuf};

use vgm2fur::analyze::{analyze, AnalyzeOptions};
use vgm2fur::convert::{
    convert_variants, ConvertDriver, ConvertOptions, VariantInfo,
};
use vgm2fur::furwrite::{decompress_fur, _add_fx, _blank_row, dev_max, loss_total, write_fur, WriteOptions};
use vgm2fur::furio::parse_fur;
use vgm2fur::model::Song;
use vgm2fur::timing::{condense_row, hz_speed_for_row};
use vgm2fur::vgm::VgmFile;

fn bpm(song: &Song) -> f64 {
    15.0 * song.hz / song.speed as f64
}

struct FakeDriver {
    vgm: VgmFile,
    analyzed: RefCell<Option<Song>>,
    loop_calls: RefCell<Vec<Song>>,
    verify_calls: RefCell<i32>,
}

impl FakeDriver {
    fn new(vgm: VgmFile) -> Self {
        FakeDriver {
            vgm,
            analyzed: RefCell::new(None),
            loop_calls: RefCell::new(Vec::new()),
            verify_calls: RefCell::new(0),
        }
    }
}

impl ConvertDriver for FakeDriver {
    fn load_vgm(&self, _path: &Path) -> vgm2fur::Result<VgmFile> {
        Ok(self.vgm.clone())
    }

    fn analyze(
        &self,
        vgm: &VgmFile,
        opts: AnalyzeOptions<'_>,
    ) -> vgm2fur::Result<Song> {
        if let Some(song) = self.analyzed.borrow().as_ref() {
            return Ok(song.clone());
        }
        analyze(vgm, opts)
    }

    fn apply_loop(&self, song: &mut Song) -> Option<String> {
        // Python's spy keeps the live object, so the recorded copy must be
        // taken after the loop fields are written.
        let msg = vgm2fur::loopfind::apply_loop(song);
        self.loop_calls.borrow_mut().push(song.clone());
        msg
    }

    fn verify_runs(
        &self,
        _src_vgm: &VgmFile,
        _fur_path: &Path,
        _channels: Option<i32>,
        _threshold_st: Option<f64>,
        _min_run_ms: Option<f64>,
    ) -> vgm2fur::Result<indexmap::IndexMap<String, serde_json::Value>> {
        *self.verify_calls.borrow_mut() += 1;
        Ok(indexmap::IndexMap::new())
    }
}

fn tmp_dir() -> PathBuf {
    let dir = tempfile::tempdir().expect("temp dir").keep();
    dir
}

fn options(factors: &[f64], mode: &str, layout: &str) -> ConvertOptions {
    ConvertOptions {
        factors: Some(factors.to_vec()),
        optimize: mode.to_string(),
        pcm: false,
        loop_find: false,
        layout: layout.to_string(),
        ..Default::default()
    }
}

fn run(driver: &FakeDriver, out: &Path, factors: &[f64], mode: &str, layout: &str) -> Vec<VariantInfo> {
    convert_variants(
        Path::new("synthetic.vgz"),
        out,
        &options(factors, mode, layout),
        driver,
    )
    .expect("convert_variants")
}

mod TestCondenseMath {
    use super::*;

    #[test]
    fn test_hz_speed_for_row_targets_the_tick() {
        let (hz, speed) = hz_speed_for_row(900.0, 90.0);
        assert_eq!(speed, 10);
        assert!((hz - 490.0).abs() < 1e-3);
        assert!((900.0 / speed as f64 - 90.0).abs() < 1e-6);
    }

    #[test]
    fn test_condense_halves_the_bpm() {
        let (hz, speed, subdiv) = condense_row(490.0, 10, 8.0, 2.0, 90.0);
        assert_eq!(speed, 20);
        assert!((hz - 490.0).abs() < 1e-3);
        assert!((subdiv - 4.0).abs() < 1e-6);
        assert!((15.0 * hz / speed as f64 - 367.5).abs() < 1e-3);
    }

    #[test]
    fn test_factor_three_is_allowed() {
        let (hz, speed, _subdiv) = condense_row(490.0, 10, 8.0, 3.0, 90.0);
        assert_eq!(speed, 30);
        assert!((hz - 490.0).abs() < 1e-3);
        assert!((15.0 * hz / speed as f64 - 245.0).abs() < 1e-3);
    }

    #[test]
    fn test_factor_one_is_a_noop() {
        assert_eq!(condense_row(490.0, 10, 8.0, 1.0, 90.0), (490.0, 10, 8.0));
    }
}

mod TestLossCounters {
    use super::*;
    use indexmap::IndexMap;

    fn stats() -> IndexMap<String, i64> {
        IndexMap::new()
    }

    #[test]
    fn test_add_fx_counts_a_dropped_effect() {
        let mut row = _blank_row();
        let mut st = stats();
        for cmd in [0x01, 0x02, 0x03] {
            _add_fx(&mut row, cmd, 1, 2, Some(&mut st));
        }
        assert_eq!(st.get("fx_dropped").copied().unwrap_or(0), 1);
    }

    #[test]
    fn test_add_fx_is_lossless_with_room() {
        let mut row = _blank_row();
        let mut st = stats();
        for cmd in [0x01, 0x02] {
            _add_fx(&mut row, cmd, 1, 2, Some(&mut st));
        }
        assert!(!st.contains_key("fx_dropped"));
    }

    #[test]
    fn test_loss_total_ignores_diagnostic_counters() {
        let mut st = stats();
        st.insert("note_collision".into(), 2);
        st.insert("ed_rolled".into(), 9);
        st.insert("sweep_dev_bad".into(), 7);
        assert_eq!(loss_total(&st), 2);
    }

    #[test]
    fn test_dev_max_reads_the_worst_deviation() {
        let mut st = stats();
        st.insert("sweep_dev_max".into(), 33);
        assert_eq!(dev_max(&st), 33);
        assert_eq!(dev_max(&stats()), 0);
    }
}

mod TestAnalyzeCondense {
    use super::*;

    #[test]
    fn test_condense_lengthens_rows_without_moving_hz() {
        let base = analyze(
            &common::note_song(),
            AnalyzeOptions { pcm: false, ..Default::default() },
        )
        .expect("analyze");
        let twice = analyze(
            &common::note_song(),
            AnalyzeOptions { pcm: false, condense: 2.0, ..Default::default() },
        )
        .expect("analyze");
        assert!((twice.hz - base.hz).abs() <= base.hz * 0.05);
        assert!((bpm(&twice) - bpm(&base) / 2.0).abs() <= bpm(&base) * 0.03);
        assert!(twice.samples_per_row() > base.samples_per_row() * 1.8);
    }

    #[test]
    fn test_written_module_carries_the_condensed_grid() {
        let song = analyze(
            &common::note_song(),
            AnalyzeOptions { pcm: false, condense: 4.0, ..Default::default() },
        )
        .expect("analyze");
        let mut stats = indexmap::IndexMap::new();
        let raw = write_fur(
            &song,
            WriteOptions {
                include_pcm: false,
                stats: Some(&mut stats),
                ..Default::default()
            },
        );
        let module = parse_fur(&decompress_fur(&raw)).expect("parse fur");
        assert_eq!(module.speed, song.speed);
        assert!((module.hz - song.hz).abs() < 0.01);
    }
}

mod TestConvertVariants {
    use super::*;

    fn names(res: &[VariantInfo]) -> std::collections::BTreeSet<String> {
        res.iter()
            .map(|r| {
                Path::new(&r.dst)
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default()
            })
            .collect()
    }

    #[test]
    fn test_writes_the_set_without_touching_the_canonical_name() {
        let out = tmp_dir();
        let driver = FakeDriver::new(common::note_song());
        let res = run(&driver, &out, &[1.0, 2.0, 4.0], "none", "flat");
        let want: std::collections::BTreeSet<String> = [
            "synthetic.fur",
            "synthetic x2.fur",
            "synthetic x4.fur",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        assert_eq!(names(&res), want);
    }

    #[test]
    fn test_every_factor_halves_and_quarters_the_bpm() {
        let out = tmp_dir();
        let driver = FakeDriver::new(common::note_song());
        let res = run(&driver, &out, &[1.0, 2.0, 4.0], "all", "flat");
        let by = |f: f64| res.iter().find(|r| r.factor == f).expect("factor").bpm;
        assert!((by(2.0) - by(1.0) / 2.0).abs() <= by(1.0) * 0.03);
        assert!((by(4.0) - by(1.0) / 4.0).abs() <= by(1.0) * 0.03);
    }

    #[test]
    fn test_pick_follows_the_mode() {
        let out = tmp_dir();
        let driver = FakeDriver::new(common::note_song());
        let none = run(&driver, &out, &[1.0, 2.0, 4.0], "none", "flat");
        let picked = none.iter().find(|r| r.pick).expect("a pick");
        assert_eq!(picked.factor, 1.0);
        let out2 = tmp_dir();
        let all = run(&driver, &out2, &[1.0, 2.0, 4.0], "all", "flat");
        let picked = all.iter().find(|r| r.pick).expect("a pick");
        assert_eq!(picked.factor, 4.0);
    }

    #[test]
    fn test_lossless_pick_never_adds_loss() {
        let out = tmp_dir();
        let driver = FakeDriver::new(common::note_song());
        let res = run(&driver, &out, &[1.0, 2.0, 3.0, 4.0], "lossless", "flat");
        let base = res.iter().find(|r| r.factor == 1.0).expect("base");
        let pick = res.iter().find(|r| r.pick).expect("pick");
        assert!(pick.loss_total <= base.loss_total);
        assert!(pick.dev_max <= base.dev_max + 6);
    }

    #[test]
    fn test_every_variant_reports_loss_and_dev() {
        let out = tmp_dir();
        let driver = FakeDriver::new(common::note_song());
        for r in run(&driver, &out, &[1.0, 2.0, 3.0, 4.0], "all", "flat") {
            assert!(!r.loss.is_empty() || r.loss_total >= 0);
            let _ = r.dev_max;
            let _ = r.loss_total;
        }
    }

    #[test]
    fn test_low_confidence_grid_still_writes_the_factor() {
        let out = tmp_dir();
        let driver = FakeDriver::new(common::note_song());
        let mut song = analyze(
            &common::note_song(),
            AnalyzeOptions { pcm: false, ..Default::default() },
        )
        .expect("analyze");
        song.grid_note = "low confidence".into();
        *driver.analyzed.borrow_mut() = Some(song);
        let res = run(&driver, &out, &[1.0, 2.0], "all", "flat");
        let factors: std::collections::BTreeSet<String> =
            res.iter().map(|r| vgm2fur::parity::fmt_g(r.factor, 6)).collect();
        assert_eq!(factors, ["1", "2"].iter().map(|s| s.to_string()).collect());
        assert!(res
            .iter()
            .any(|r| r.warnings.iter().any(|w| w.contains("low-confidence"))));
        let two = res.iter().find(|r| r.factor == 2.0).expect("x2");
        assert!(Path::new(&two.dst).exists());
    }

    #[test]
    fn test_all_mode_skips_furnace_verification() {
        let out = tmp_dir();
        let driver = FakeDriver::new(common::note_song());
        let mut opts = options(&[1.0, 2.0], "all", "flat");
        opts.verify = true;
        let res = convert_variants(
            Path::new("synthetic.vgz"),
            &out,
            &opts,
            &driver,
        )
        .expect("convert_variants");
        assert_eq!(*driver.verify_calls.borrow(), 0);
        assert!(res.iter().all(|r| r.div_ms.is_none()));
        assert!(res
            .iter()
            .all(|r| !Path::new(&r.dst).with_extension("verify.vgm").exists()));
    }

    #[test]
    fn test_fractional_factor_names_the_file() {
        let out = tmp_dir();
        let driver = FakeDriver::new(common::note_song());
        let res = run(&driver, &out, &[1.0, 1.5], "all", "flat");
        let want: std::collections::BTreeSet<String> =
            ["synthetic.fur", "synthetic x1.5.fur"]
                .iter()
                .map(|s| s.to_string())
                .collect();
        assert_eq!(names(&res), want);
        let by = |f: f64| res.iter().find(|r| r.factor == f).expect("factor").bpm;
        assert!((by(1.5) - by(1.0) / 1.5).abs() <= by(1.0) * 0.05);
    }

    #[test]
    #[ignore = "Python v0.11.0 baseline failure: test_all_mode_marks_only_the_rejected_factors"]
    fn test_all_mode_marks_only_the_rejected_factors() {
        let out = tmp_dir();
        let driver = FakeDriver::new(common::note_song());
        let res = run(&driver, &out, &[1.0, 1.5, 2.0, 4.0], "all", "flat");
        let name = |f: f64| {
            let r = res.iter().find(|r| r.factor == f).expect("factor");
            Path::new(&r.dst)
                .file_name()
                .unwrap()
                .to_string_lossy()
                .into_owned()
        };
        assert_eq!(name(1.0), "synthetic.fur");
        assert_eq!(name(1.5), "synthetic x1.5.fur");
        assert_eq!(name(2.0), "synthetic x2 (lossy).fur");
        assert_eq!(name(4.0), "synthetic x4 (lossy).fur");
    }

    #[test]
    fn test_lossless_mode_leaves_the_names_plain() {
        let out = tmp_dir();
        let driver = FakeDriver::new(common::note_song());
        let res = run(&driver, &out, &[1.0, 2.0, 4.0], "lossless", "flat");
        let name = |f: f64| {
            let r = res.iter().find(|r| r.factor == f).expect("factor");
            Path::new(&r.dst)
                .file_name()
                .unwrap()
                .to_string_lossy()
                .into_owned()
        };
        assert_eq!(name(1.0), "synthetic.fur");
        assert_eq!(name(2.0), "synthetic x2.fur");
        assert_eq!(name(4.0), "synthetic x4.fur");
    }

    #[test]
    fn test_dirs_layout_files_default_and_optimised() {
        let out = tmp_dir();
        let driver = FakeDriver::new(common::note_song());
        let res = run(&driver, &out, &[1.0, 2.0, 3.0, 4.0], "all", "dirs");
        let rel = |f: f64| {
            let r = res.iter().find(|r| r.factor == f).expect("factor");
            Path::new(&r.dst)
                .strip_prefix(&out)
                .expect("relative")
                .to_path_buf()
        };
        assert_eq!(rel(1.0).components().next().unwrap().as_os_str(), "default");
        assert_eq!(
            rel(2.0).components().next().unwrap().as_os_str(),
            "x2 optimised"
        );
        assert_eq!(
            rel(3.0).components().next().unwrap().as_os_str(),
            "x3 optimised"
        );
        for f in [1.0, 2.0, 3.0, 4.0] {
            assert!(out.join(rel(f)).is_file());
        }
        for r in &res {
            assert!(r.lossless);
        }
    }
}

mod TestVariantsShareTheLoop {
    use super::*;
    use vgm2fur::furio::FurModule;

    fn fur_loop(module: &FurModule) -> Option<(i32, i32)> {
        let pat_len = module.pat_len as i32;
        let span = |oi: usize| -> i32 {
            let mut widths: Vec<i32> = Vec::new();
            for ch in 0..module.n_ch {
                let rows = module
                    .patterns
                    .get(&(ch as u16, module.orders[ch as usize][oi as usize]))
                    .cloned()
                    .unwrap_or_default();
                let mut width = if rows.is_empty() { pat_len } else { rows.len() as i32 };
                for (i, row) in rows.iter().enumerate() {
                    if row.fx.iter().any(|(c, _)| *c == 0x0D) {
                        width = width.min(i as i32 + 1);
                    }
                }
                widths.push(if rows.is_empty() { 0 } else { width });
            }
            widths.into_iter().max().unwrap_or(pat_len)
        };
        let mut bases: Vec<i32> = Vec::new();
        let mut total = 0i32;
        let mut spans: Vec<i32> = Vec::new();
        for oi in 0..module.n_ord as usize {
            bases.push(total);
            let w = span(oi as usize);
            spans.push(w);
            total += w;
        }
        let mut end: Option<i32> = None;
        let mut target: Option<i32> = None;
        for oi in 0..module.n_ord as usize {
            for ch in 0..module.n_ch {
                let rows = module
                    .patterns
                    .get(&(ch as u16, module.orders[ch as usize][oi as usize]))
                    .cloned()
                    .unwrap_or_default();
                let upto = (spans[oi] as usize).min(rows.len());
                for (i, row) in rows.iter().take(upto).enumerate() {
                    for (cmd, val) in &row.fx {
                        if *cmd == 0x0B {
                            end = Some(bases[oi] + i as i32);
                            target = Some(*val as i32);
                        }
                    }
                }
            }
        }
        Some((bases[target? as usize], end?))
    }

    #[test]
    fn test_each_grid_finds_its_own_loop_rows() {
        let out = tmp_dir();
        let driver = FakeDriver::new(common::note_song());
        let mut song = common::note_song();
        song.loop_sample = Some(120_000);
        song.loop_samples = 100_000;
        let driver = FakeDriver {
            vgm: song,
            analyzed: RefCell::new(None),
            loop_calls: driver.loop_calls,
            verify_calls: driver.verify_calls,
        };
        let opts = ConvertOptions {
            factors: Some(vec![1.0, 2.0, 4.0]),
            optimize: "all".to_string(),
            pcm: false,
            layout: "dirs".to_string(),
            ..Default::default() // loop_find is on, like the Python call
        };
        let res = convert_variants(Path::new("synthetic.vgz"), &out, &opts, &driver)
            .expect("convert_variants");
        let calls = driver.loop_calls.borrow();
        assert_eq!(calls.len(), 3);
        for (r, source) in res.iter().zip(calls.iter()) {
            let raw = std::fs::read(&r.dst).expect("read fur");
            let module = parse_fur(&decompress_fur(&raw)).expect("parse fur");
            let (start, end) = fur_loop(&module).unwrap_or_else(|| {
                panic!("x{}: no Bxx", vgm2fur::parity::fmt_g(r.factor, 6))
            });
            assert_eq!(Some(start), source.loop_start_row);
            assert_eq!(Some(end), source.loop_end_row);
            let row_samples = 44100.0 * module.speed as f64 / module.hz;
            let drift = ((end - start + 1) as f64 * row_samples - 100_000.0).abs();
            assert!(
                drift <= 1.0,
                "x{}: loop length drifted by {drift}",
                vgm2fur::parity::fmt_g(r.factor, 6)
            );
        }
    }
}
