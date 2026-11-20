//! Port of `tests/test_vgm2151fur.py` at vgm2151fur a8edb6e.
//!
//! Two adaptations are forced by the Rust API and reported to the parent:
//! `Song` has no all-default constructor here, so the helpers build it from a
//! literal; and `write_fur` does not take `EmitHooks`, so
//! `test_effect_columns_match_content` drives `_build_patterns` +
//! `_fit_fx_cols` through the same `EmitHooks` seam the writer uses.

mod common;

use std::path::{Path, PathBuf};

use common::*;
use indexmap::IndexMap;

use vgm2fur::analyze::{AnalyzeOptions, analyze};
use vgm2fur::convert::{select_tracks, track_number};
use vgm2fur::fm::{FMPatch, OpRegs};
use vgm2fur::furio::{FurModule, FurRow, fx_pan_amps, list_fur_samples, parse_fur};
use vgm2fur::furwrite::{
    CHIP_K007232, CHIP_MSM6258, CHIP_MSM6295, CHIP_YM2151, EmitHooks, Grid, NOTE_OFF, Row,
    WriteOptions, _add_fx, _blank_row, _build_patterns, _emit_sweep, _fit_fx_cols,
    decompress_fur, pack_dt_mul, write_fur, ym_dt_to_furnace,
};
use vgm2fur::model::{
    NoteEvent, Song, furnace_note_name, kc_to_furnace_note, kf_to_e5, pcm_pan_fx, vol15,
};
use vgm2fur::oki::{
    collect_oki, msm6258_best_divider, msm6258_div_fx, oki_flag_text, oki_furnace_vol, oki_rate,
};
use vgm2fur::pcm::{
    PcmHit, PcmSample, collect_pcm_hits, k007232_rate, pcm7_to_s8, scale_pcm7,
};
use vgm2fur::vgm::{ChipWrite, Msm6258Play, RomSlice, VgmFile, _parse_volume_list, load_vgm};

// ---------------------------------------------------------------------------
// Fixtures.

/// `_fake_song` / `_pcm_song` / `_fm_song`: a Song with every defaulted list
/// empty. Rust's `Song` has no `Default`, so the full literal is spelled out
/// with the Python dataclass defaults.
#[allow(clippy::too_many_arguments)]
fn make_song(
    vgm: VgmFile,
    speed: i32,
    hz: f64,
    loop_sample: Option<i64>,
    pcm_samples: Vec<PcmSample>,
    events: Vec<NoteEvent>,
) -> Song {
    Song {
        vgm,
        t0: 0,
        loop_sample,
        speed,
        hz,
        fm_patches: Vec::new(),
        pcm_samples,
        events,
        warnings: Vec::new(),
        oki_samples: Vec::new(),
        msm6258_samples: Vec::new(),
        segapcm_samples: Vec::new(),
        segapcm_ids: Vec::new(),
        rf5c68_samples: Vec::new(),
        rf5c164_samples: Vec::new(),
        c140_samples: Vec::new(),
        c219_samples: Vec::new(),
        c219_ids: Vec::new(),
        c219_count: 0,
        c352_samples: Vec::new(),
        k053260_samples: Vec::new(),
        k054539_samples: Vec::new(),
        k054539_ids: Vec::new(),
        k054539_count: 0,
        ymz280b_samples: Vec::new(),
        ymz280b_ids: Vec::new(),
        ymz280b_count: 0,
        multipcm_samples: Vec::new(),
        multipcm_ids: Vec::new(),
        multipcm_count: 0,
        upd7759_samples: Vec::new(),
        upd7759_ids: Vec::new(),
        upd7759_count: 0,
        pwm_samples: Vec::new(),
        pwm_ids: Vec::new(),
        pwm_count: 0,
        msm5205_samples: Vec::new(),
        msm5205_ids: Vec::new(),
        msm5205_count: 0,
        ga20_samples: Vec::new(),
        ga20_ids: Vec::new(),
        ga20_count: 0,
        msm5232_ids: Vec::new(),
        msm5232_controls: Vec::new(),
        msm5232_count: 0,
        es5503_samples: Vec::new(),
        es5503_ids: Vec::new(),
        es5503_oscs: Vec::new(),
        es5503_rate: 0,
        es5503_count: 0,
        channel_vol_events: Vec::new(),
        mikey_ids: Vec::new(),
        mikey_ins: Vec::new(),
        mikey_samples: Vec::new(),
        mikey_count: 0,
        x1_010_ids: Vec::new(),
        x1_010_ins: Vec::new(),
        x1_010_samples: Vec::new(),
        x1_010_count: 0,
        x1_waves: Vec::new(),
        x1_envs: Vec::new(),
        fds_ids: Vec::new(),
        fds_ins: Vec::new(),
        fds_waves: Vec::new(),
        fds_mod_waves: Vec::new(),
        fds_count: 0,
        wonderswan_ids: Vec::new(),
        wonderswan_ins: Vec::new(),
        wonderswan_waves: Vec::new(),
        wonderswan_pcm_samples: Vec::new(),
        wonderswan_count: 0,
        wonderswan_pcm_count: 0,
        qsound_ids: Vec::new(),
        qsound_ins: Vec::new(),
        qsound_samples: Vec::new(),
        qsound_count: 0,
        qsound_adpcm_count: 0,
        channel_fx: Vec::new(),
        chip_fx: Vec::new(),
        grid_note: String::new(),
        legato_steps: 0,
        legato_ramps: 0,
        row_subdiv: 1.0,
        hilight_a: 4,
        hilight_b: 16,
        fm_chip: "ym2151".to_string(),
        fm_systems: Vec::new(),
        has_fm_source: true,
        ym2151_ids: Vec::new(),
        ym2203_ids: Vec::new(),
        ym2203_ext: false,
        ym2203_ssg_count: 0,
        ym2612_ids: Vec::new(),
        ym2612_ext: false,
        ym2612_csm: false,
        opl_ids: Vec::new(),
        opl_drums: false,
        opll_ids: Vec::new(),
        opll_drums: false,
        opll_vrc7: false,
        fm_vol_max: 127,
        fm_notes: 0,
        drum_notes: 0,
        ym2610_ids: Vec::new(),
        ym2610_b: false,
        ym2610_ssg_count: 0,
        ym2610_a_samples: Vec::new(),
        ym2610_b_samples: Vec::new(),
        ym2610_a_count: 0,
        ym2610_b_count: 0,
        ym2608_ids: Vec::new(),
        ym2608_ext: false,
        ym2608_ssg_count: 0,
        ym2608_rhythm_count: 0,
        ym2608_rhythm_notes: 0,
        ym2608_b_samples: Vec::new(),
        ym2608_b_count: 0,
        y8950_b_samples: Vec::new(),
        y8950_b_count: 0,
        opl4_wave_samples: Vec::new(),
        opl4_wave_count: 0,
        k007232_volume: 0.20,
        loop_end_sample: None,
        loop_start_row: None,
        loop_end_row: None,
        loop_marker_sample: None,
        loop_cut_sample: None,
        c352_quad: false,
        scc_ids: Vec::new(),
        scc_waves: Vec::new(),
        scc_ins: Vec::new(),
        scc_count: 0,
        k005289_ids: Vec::new(),
        k005289_waves: Vec::new(),
        k005289_ins: Vec::new(),
        k005289_count: 0,
        ay_ids: Vec::new(),
        ay_ins: Vec::new(),
        ay_count: 0,
        pokey_ids: Vec::new(),
        pokey_ins: Vec::new(),
        pokey_count: 0,
        ssg_pcm_samples: Vec::new(),
        ssg_pcm_count: 0,
        nes_ids: Vec::new(),
        nes_present: false,
        nes_ins: Vec::new(),
        nes_dpcm_samples: Vec::new(),
        nes_pcm_samples: Vec::new(),
        nes_count: 0,
        nes_pcm_count: 0,
        huc6280_ids: Vec::new(),
        huc6280_waves: Vec::new(),
        huc6280_ins: Vec::new(),
        huc6280_pcm_samples: Vec::new(),
        huc6280_count: 0,
        huc6280_pcm_count: 0,
        gb_ids: Vec::new(),
        gb_waves: Vec::new(),
        gb_ins: Vec::new(),
        gb_count: 0,
        vsu_ids: Vec::new(),
        vsu_waves: Vec::new(),
        vsu_mod_waves: Vec::new(),
        vsu_ins: Vec::new(),
        vsu_count: 0,
        vsu_dynamic: false,
        saa_ids: Vec::new(),
        saa_ins: Vec::new(),
        saa_count: 0,
        psg_ids: Vec::new(),
        psg_ins: Vec::new(),
        psg_count: 0,
        psg_stereo: false,
        psg_t6w28: false,
        psg_dac_samples: Vec::new(),
        psg_dac_plays: Vec::new(),
        psg_dac_voices: Vec::new(),
        psg_dac_rate: vgm2fur::psg_dac::PSG_DAC_RATE,
        psg_dac_count: 0,
        psg_dac_stats: IndexMap::new(),
        dac_samples: Vec::new(),
        dac_count: 0,
        dac_stats: Default::default(),
    }
}

fn fake_song(stem: &str, samples: Vec<PcmSample>) -> Song {
    let mut vgm = VgmFile::default();
    vgm.path = PathBuf::from(stem);
    vgm.buf = Vec::new();
    vgm.version = 0x170;
    vgm.total_samples = 1000;
    vgm.loop_samples = 0;
    vgm.ym2151_clock = 0;
    vgm.k007232_clock = 3579545;
    make_song(vgm, 6, 60.0, None, samples, Vec::new())
}

fn pcm_song(events: Vec<NoteEvent>, loop_sample: Option<i64>) -> Song {
    let mut vgm = VgmFile::default();
    vgm.path = PathBuf::from("synthetic.vgm");
    vgm.version = 0x151;
    vgm.total_samples = 1;
    vgm.ym2151_clock = 3579545;
    vgm.k007232_clock = 3579545;
    // Channel 8 events only exist when the bank had audible samples, so the
    // fixture carries one: the K007232 pair is written from the samples, not
    // from the notes.
    let smp = PcmSample::new(0x10000, vec![0x40; 16]);
    make_song(vgm, 6, 60.0, loop_sample, vec![smp], events)
}

/// A YM2151-only track: FM notes and no sample chip of its own.
fn fm_song(events: Vec<NoteEvent>, speed: i32, hz: f64) -> Song {
    let mut vgm = VgmFile::default();
    vgm.path = PathBuf::from("synthetic.vgm");
    vgm.version = 0x151;
    vgm.total_samples = 1;
    vgm.ym2151_clock = 3579545;
    vgm.k007232_clock = 3579545;
    make_song(vgm, speed, hz, None, Vec::new(), events)
}

fn write_all(song: &Song, include_pcm: bool) -> Vec<u8> {
    write_fur(
        song,
        WriteOptions {
            include_pcm,
            include_fm: true,
            ..Default::default()
        },
    )
}

fn write_pcm(song: &Song) -> Vec<u8> {
    write_all(song, true)
}

fn pcm_notes(song: &Song) -> Vec<(i32, FurRow)> {
    let mod_ = parse_fur(&write_pcm(song)).expect("parse fur");
    let mut found = Vec::new();
    for (oi, pat) in mod_.orders[8].iter().enumerate() {
        for (i, row) in mod_
            .patterns
            .get(&(8, *pat))
            .expect("pattern")
            .iter()
            .enumerate()
        {
            if (0..180).contains(&row.note) {
                found.push(((oi * mod_.pat_len as usize + i) as i32, row.clone()));
            }
        }
    }
    found
}

fn rows_of(mod_: &FurModule, ch: usize) -> Vec<(i32, FurRow)> {
    let mut out = Vec::new();
    for (oi, pat) in mod_.orders[ch].iter().enumerate() {
        for (i, row) in mod_
            .patterns
            .get(&(ch as u16, *pat))
            .expect("pattern")
            .iter()
            .enumerate()
        {
            if row.note >= 0 || !row.fx.is_empty() {
                out.push(((oi * mod_.pat_len as usize + i) as i32, row.clone()));
            }
        }
    }
    out
}

fn find_from(data: &[u8], needle: &[u8], start: usize) -> Option<usize> {
    if start > data.len() || needle.len() > data.len() {
        return None;
    }
    data[start..]
        .windows(needle.len())
        .position(|w| w == needle)
        .map(|p| p + start)
}

/// Chip ids, output gains, and legacy volume bytes from an INFO block.
fn chip_mix(data: &[u8]) -> (Vec<u8>, Vec<f64>, Vec<u8>) {
    let info = find_from(data, b"INFO", 0).expect("INFO");
    let size = u32::from_le_bytes(data[info + 4..info + 8].try_into().unwrap()) as usize;
    let payload = &data[info + 8..info + 8 + size];
    let chips: Vec<u8> = payload[24..56].iter().copied().filter(|&c| c != 0).collect();
    let legacy: Vec<u8> = payload[56..56 + chips.len()].to_vec();
    let tail = 43usize; // patchbay, compat, speed pattern, groove, three ADIR pointers
    let end = payload.len() - tail;
    let start = end - 12 * chips.len();
    let raw = &payload[start..end];
    let gains: Vec<f64> = (0..chips.len())
        .map(|i| {
            let off = i * 12;
            f32::from_le_bytes(raw[off..off + 4].try_into().unwrap()) as f64
        })
        .collect();
    (chips, gains, legacy)
}

fn ins_types(data: &[u8]) -> Vec<i32> {
    let mut types = Vec::new();
    let mut pos = 0usize;
    loop {
        let Some(i) = find_from(data, b"INS2", pos) else {
            break;
        };
        types.push(u16::from_le_bytes(data[i + 10..i + 12].try_into().unwrap()) as i32);
        pos = i + 4;
    }
    types
}

fn note(sample: i64, ch: i32, on: bool, note: i32, ins: i32, vol: i32, pan: i32) -> NoteEvent {
    let mut ev = NoteEvent::new(sample, ch, on, note, ins, vol, 0x80, pan);
    // Channels 8-9 are the sample engines (the reference fixture sets
    // `pcm=True` on every one of them).
    ev.pcm = ch >= 8;
    ev
}

// ---------------------------------------------------------------------------
// Tests.

mod TestVgmExtraVolumeList {
    use super::*;

    #[test]
    fn test_linked_devices_and_relative_values_are_kept() {
        // Bit 7 of the chip byte marks the linked SSG half; bit 15 of the
        // value is the relative-volume flag. Rejecting either silently
        // dropped the SSG mix from every OPNA rip.
        let buf = [
            2, 0x87, 0x00, 0xA0, 0x80, // OPNA SSG, relative 0xA0
            0x03, 0x00, 0x00, 0x01, // YM2151, absolute 0x0100
        ];
        let got = _parse_volume_list(&buf, 0);
        let mut want: IndexMap<u8, (u8, u16)> = IndexMap::new();
        want.insert(0x87, (0, 0x80A0));
        want.insert(0x03, (0, 0x0100));
        assert_eq!(got, want);
    }
}

mod TestKC {
    use super::*;

    #[test]
    fn test_a4() {
        // Official table: octave 4, nibble 10 (A) = A-4 = furnace 117
        assert_eq!(kc_to_furnace_note(0x4A), 117);
        assert_eq!(furnace_note_name(117), "A-4");
        // nibble 8 is G, not A
        assert_eq!(kc_to_furnace_note(0x48), 115);
        assert_eq!(furnace_note_name(115), "G-4");
    }

    #[test]
    fn test_c5_from_note14() {
        // Konami / Yamaha: nibble 14 = C of the next octave. 0x4E = C-5.
        assert_eq!(kc_to_furnace_note(0x4E), 120);
        assert_eq!(furnace_note_name(120), "C-5");
        assert_eq!(kc_to_furnace_note(0x3E), 108); // C-4
        assert_eq!(furnace_note_name(108), "C-4");
    }

    #[test]
    fn test_cs4() {
        assert_eq!(kc_to_furnace_note(0x40), 109);
        assert_eq!(furnace_note_name(109), "C#4");
    }
}

mod TestK007232Rate {
    use super::*;

    #[test]
    fn test_typical_drum_pitch() {
        let hz = k007232_rate(3579545, 4051);
        assert!(hz > 15000.0);
        assert!(hz < 25000.0);
    }

    #[test]
    fn test_s8_center() {
        assert_eq!(pcm7_to_s8(b"\x40")[0], 0);
    }

    #[test]
    fn test_scale_pcm7_preserves_center() {
        assert_eq!(scale_pcm7(b"\x40\x7f", 1.0), b"\x40\x7f");
        let scaled = scale_pcm7(b"\x40\x7f", 0.72);
        assert_eq!(scaled[0], 0x40);
        assert!(scaled[1] < 0x7F);
        assert!(scaled[1] > 0x40);
    }
}

mod TestDTRemap {
    use super::*;

    #[test]
    fn test_furnace_dt_table_roundtrip() {
        // arcade.cpp: rWrite(DT, dtTable[op.dt]<<4) with dtTable = 7,6,5,0,1,2,3,4
        let dt_table = [7, 6, 5, 0, 1, 2, 3, 4];
        for ym in 0..8 {
            let fur = ym_dt_to_furnace(ym);
            assert_eq!(dt_table[fur as usize], ym, "YM DT {ym}");
        }
    }

    #[test]
    fn test_pack_keeps_mult() {
        assert_eq!(pack_dt_mul(0x01, false) & 0x0F, 1);
        assert_eq!(pack_dt_mul(0x73, false) & 0x0F, 3);
    }
}

mod TestK007232ReadTriggers {
    use super::*;

    fn hits(writes: Vec<ChipWrite>) -> Vec<PcmHit> {
        let mut vgm = VgmFile::default();
        vgm.path = PathBuf::from("synthetic.vgm");
        vgm.version = 0x171;
        vgm.total_samples = 10_000;
        vgm.loop_samples = 0;
        vgm.ym2151_clock = 0;
        vgm.k007232_clock = 3579545;
        vgm.writes = writes;
        collect_pcm_hits(&vgm)
    }

    fn setup(t0: i64, triggers: Vec<ChipWrite>) -> Vec<ChipWrite> {
        let mut writes = vec![
            ChipWrite::new(t0, "k007232", 0, 0x00, 0xD3),
            ChipWrite::new(t0, "k007232", 0, 0x01, 0x0F),
            ChipWrite::new(t0, "k007232", 0, 0x02, 0x00),
            ChipWrite::new(t0, "k007232", 0, 0x03, 0x01),
            ChipWrite::new(t0, "k007232", 0, 0x04, 0x00),
            ChipWrite::new(t0, "k007232", 0, 0x10, 0x55),
            ChipWrite::new(t0, "k007232", 0, 0x11, 0x55),
        ];
        writes.extend(triggers);
        writes
    }

    #[test]
    fn test_two_trigger_reads_are_one_keyon() {
        for gap in [0, 1, 2] {
            let got = hits(setup(
                877,
                vec![
                    ChipWrite::new(877, "k007232", 0, 0x1F, 0x05),
                    ChipWrite::new(877 + gap, "k007232", 0, 0x1F, 0x05),
                ],
            ));
            assert_eq!(got.len(), 1, "gap={gap}");
            assert_eq!(got[0].pitch, 0xFD3, "gap={gap}");
        }
    }

    #[test]
    fn test_later_keyon_beyond_the_window_is_kept() {
        let got = hits(setup(
            877,
            vec![
                ChipWrite::new(877, "k007232", 0, 0x1F, 0x05),
                ChipWrite::new(1000, "k007232", 0, 0x1F, 0x05),
            ],
        ));
        assert_eq!(got.len(), 2);
    }

    #[test]
    fn test_reads_of_other_offsets_do_not_trigger() {
        let got = hits(setup(
            877,
            vec![
                ChipWrite::new(877, "k007232", 0, 0x1F, 0x00),
                ChipWrite::new(878, "k007232", 0, 0x1F, 0x0A),
            ],
        ));
        assert_eq!(got, Vec::new());
    }

    #[test]
    fn test_zero_volume_keyon_starts_silent() {
        // The driver keys voice 0 with both volume registers at zero and only
        // opens them later.  Writing the note at full scale would put an
        // audible blip on the first row that the source does not have.
        let mut body: Vec<u8> = (0x40..0x80).collect();
        body.push(0x80);
        let writes = vec![
            ChipWrite::new(0, "k007232", 0, 0x00, 0xD3),
            ChipWrite::new(0, "k007232", 0, 0x01, 0x0F),
            ChipWrite::new(0, "k007232", 0, 0x02, 0x00),
            ChipWrite::new(0, "k007232", 0, 0x03, 0x00),
            ChipWrite::new(0, "k007232", 0, 0x04, 0x00),
            ChipWrite::new(0, "k007232", 0, 0x05, 0x00),
            ChipWrite::new(3000, "k007232", 0, 0x10, 0x55),
            ChipWrite::new(3000, "k007232", 0, 0x11, 0x55),
        ];
        let mut vgm = VgmFile::default();
        vgm.path = PathBuf::from("synthetic.vgm");
        vgm.version = 0x171;
        vgm.total_samples = 10_000;
        vgm.loop_samples = 0;
        vgm.ym2151_clock = 0;
        vgm.k007232_clock = 3579545;
        vgm.roms = vec![RomSlice {
            chip: 0,
            rom_size: 0x1000,
            start: 0,
            data: body,
            kind: "k007232",
            time: 0,
        }];
        vgm.writes = writes;
        let song = analyze(&vgm, AnalyzeOptions::default()).expect("analyze");
        let hit = song.events.iter().find(|e| e.pcm && e.on).expect("pcm note");
        assert_eq!(hit.vol, 0);
        assert_eq!(hit.pan, 0);
    }
}

mod TestManualTickRate {
    use super::*;

    fn vgm() -> VgmFile {
        let mut body: Vec<u8> = (0x40..0x80).collect();
        body.push(0x80);
        let writes = vec![
            ChipWrite::new(0, "k007232", 0, 0x00, 0xD3),
            ChipWrite::new(0, "k007232", 0, 0x01, 0x0F),
            ChipWrite::new(0, "k007232", 0, 0x05, 0x00),
        ];
        let mut vgm = VgmFile::default();
        vgm.path = PathBuf::from("synthetic.vgm");
        vgm.version = 0x171;
        vgm.total_samples = 10_000;
        vgm.loop_samples = 0;
        vgm.ym2151_clock = 0;
        vgm.k007232_clock = 3579545;
        vgm.roms = vec![RomSlice {
            chip: 0,
            rom_size: 0x1000,
            start: 0,
            data: body,
            kind: "k007232",
            time: 0,
        }];
        vgm.writes = writes;
        vgm
    }

    #[test]
    fn test_manual_speed_keeps_60_hz_by_default() {
        let song = analyze(
            &vgm(),
            AnalyzeOptions {
                speed: Some(4),
                ..Default::default()
            },
        )
        .expect("analyze");
        assert_eq!(song.speed, 4);
        assert_almost_eq!(song.hz, 60.0);
    }

    #[test]
    fn test_manual_speed_with_tick_rate() {
        let song = analyze(
            &vgm(),
            AnalyzeOptions {
                speed: Some(4),
                tick_hz: Some(120.0),
                ..Default::default()
            },
        )
        .expect("analyze");
        assert_eq!(song.speed, 4);
        assert_almost_eq!(song.hz, 120.0);
    }

    #[test]
    fn test_tick_rate_without_speed_is_rejected() {
        let got = analyze(
            &vgm(),
            AnalyzeOptions {
                tick_hz: Some(120.0),
                ..Default::default()
            },
        );
        assert!(got.is_err());
    }
}

mod TestVolAndKF {
    use super::*;

    #[test]
    fn test_k007232_volume_is_linear_amplitude() {
        // MAME mixes a voice as sample * (vol * 2) / 32768, so 255 is full
        // scale and the column has to be linear in the register.
        assert_eq!(vol15(0, 255.0), 0);
        assert_eq!(vol15(255, 255.0), 15);
        assert_eq!(vol15(136, 255.0), 8); // 0x88, the common Twin16 step
        assert_eq!(vol15(34, 255.0), 2); // 0x22
        assert_eq!(vol15(17, 255.0), 1); // 0x11
        assert_eq!(vol15(153, 255.0), 9); // 0x99 is not full scale
        assert!(vol15(34, 255.0) < vol15(136, 255.0));
        assert!(vol15(136, 255.0) < vol15(255, 255.0));
    }

    #[test]
    fn test_k007232_volume_honours_a_0_15_preset_scale() {
        // Some drivers write a 0-15 preset instead of the register; the file
        // is scaled by its loudest write (see analyze).
        assert_eq!(vol15(15, 15.0), 15);
        assert_eq!(vol15(7, 15.0), 7);
        assert_eq!(vol15(0, 15.0), 0);
        assert_eq!(vol15(255, 255.0), vol15(255, 255.0));
    }

    #[test]
    fn test_pcm_pan_is_furnace_08xy_nibbles() {
        // 08xy: x = left, y = right. Loud side stays at F so the volume
        // column is not halved. 0x80 is left-at-8 / right-off, not center.
        assert_eq!(pcm_pan_fx(15, 15), 0xFF);
        assert_eq!(pcm_pan_fx(15, 0), 0xF0);
        assert_eq!(pcm_pan_fx(0, 15), 0x0F);
        assert_eq!(pcm_pan_fx(0, 0), 0x00);
        assert_eq!(pcm_pan_fx(10, 5), 0xF8);
        assert_ne!(pcm_pan_fx(15, 15), 0x80);
    }

    #[test]
    fn test_kf_fraction_is_top_six_bits() {
        // ymfm: block_freq = (KC << 6) | (KF >> 2): the low 2 bits are ignored.
        // Furnace 0.6.8.3 maps E5 0x80+f to KF fraction f (one unit per step).
        assert_eq!(kf_to_e5(0x00), 0x80);
        assert_eq!(kf_to_e5(0x03), 0x80); // low two bits do not count
        assert_eq!(kf_to_e5(0x04), 0x81);
        assert_eq!(kf_to_e5(0x80), 0xA0); // 32/64 semitone
        assert_eq!(kf_to_e5(0xFC), 0xBF); // 63/64 semitone
    }
}

mod TestK007232FurRows {
    use super::*;

    #[test]
    fn test_chip_volume_follows_the_sources_mix() {
        let hit = PcmHit {
            sample_time: 0,
            ch: 0,
            start: 0x10000,
            pitch: 4000,
            bank: 0,
            vol_l: 0x88,
            vol_r: 0x88,
            ..Default::default()
        };
        let mut smp = PcmSample::new(0x10000, vec![0x40; 16]);
        smp.hits = vec![hit];
        let mut song = fake_song("track.vgm", vec![smp]);
        song.k007232_volume = 0.5;
        let data = write_pcm(&song);
        // chip output triple (vol, panL, panR) in the .fur
        let mut triple = Vec::new();
        triple.extend(0.5f32.to_le_bytes());
        triple.extend(0.0f32.to_le_bytes());
        triple.extend(0.0f32.to_le_bytes());
        assert!(data.windows(12).any(|w| w == triple.as_slice()));
    }

    #[test]
    fn test_chip_volume_reads_the_extra_header() {
        let mut vgm = fake_song("x.vgz", Vec::new()).vgm;
        vgm.extra_volumes.insert(0x2A, (0, 0x004D));
        vgm.extra_volumes.insert(0x03, (1, 0x0100));
        assert_almost_eq!(vgm.chip_volume(0x2A).unwrap(), 0x4D as f64 / 256.0);
        assert_eq!(vgm.chip_volume(0x03), None); // flags set: keep the default
        assert_eq!(vgm.chip_volume(0x66), None); // absent
    }
}

mod TestTrackSelect {
    use super::*;

    #[test]
    fn test_number_and_substring() {
        let files = vec![
            PathBuf::from("01 First Track.vgm"),
            PathBuf::from("15 Last Track (BGM 3).vgm"),
            PathBuf::from("04 Middle Track.vgm"),
        ];
        assert_eq!(track_number(&files[1]), Some(15));
        let hit = select_tracks(&files, Some("15")).expect("track 15");
        assert_eq!(hit[0].file_name().unwrap(), files[1].file_name().unwrap());
        let hit = select_tracks(&files, Some("middle")).expect("middle");
        assert_eq!(hit.len(), 1);
        assert!(hit[0].file_name().unwrap().to_string_lossy().contains("Middle"));
        assert!(select_tracks(&files, Some("99")).is_err());
    }
}

mod TestPcmRowCollisions {
    use super::*;

    #[test]
    fn test_distinct_times_on_one_row_both_play() {
        // 4410 samples/row. 100 and 400 share row 0 and used to overwrite.
        let song = pcm_song(
            vec![
                note(100, 8, true, 100, 0, 12, 0xFF),
                note(400, 8, true, 110, 0, 12, 0xFF),
                note(20000, 8, true, 120, 0, 12, 0xFF),
            ],
            None,
        );
        let notes: Vec<i32> = pcm_notes(&song)
            .into_iter()
            .map(|(_r, row)| row.note)
            .collect();
        assert!(notes.contains(&100));
        assert!(notes.contains(&110));
        assert_eq!(notes.len(), 3);
    }

    #[test]
    fn test_tight_pair_keeps_both_triggers() {
        // 4410 samples/row. 100 and 200 share row 0. The later trigger moves
        // to row 1 and the next trigger follows it, so no sample is dropped.
        let song = pcm_song(
            vec![
                note(100, 8, true, 100, 0, 12, 0xFF),
                note(200, 8, true, 110, 0, 12, 0xFF),
                note(4500, 8, true, 120, 0, 12, 0xFF),
            ],
            None,
        );
        let found: Vec<(i32, i32)> = pcm_notes(&song)
            .into_iter()
            .map(|(row_i, row)| (row_i, row.note))
            .collect();
        assert_eq!(found, vec![(0, 100), (1, 110), (2, 120)]);
    }

    #[test]
    fn test_same_timestamp_keeps_the_later_trigger() {
        let song = pcm_song(
            vec![
                note(100, 8, true, 100, 0, 12, 0xFF),
                note(100, 8, true, 110, 0, 8, 0xF0),
                note(20000, 8, true, 120, 0, 12, 0xFF),
            ],
            None,
        );
        let early: Vec<FurRow> = pcm_notes(&song)
            .into_iter()
            .filter(|(row_i, _row)| *row_i == 0)
            .map(|(_row_i, row)| row)
            .collect();
        assert_eq!(early.len(), 1);
        assert_eq!(early[0].note, 110);
        assert_eq!(early[0].vol, 8);
        assert!(early[0].fx.contains(&(0x08, 0xF0)));
    }

    #[test]
    fn test_retrigger_off_does_not_cut_the_new_note() {
        // The old sample's key-off lands on the same sample as the next
        // trigger. The trigger wins and the off must not become a row.
        let song = pcm_song(
            vec![
                note(100, 8, true, 100, 0, 12, 0xFF),
                note(5000, 8, true, 110, 0, 12, 0xFF),
                note(5000, 8, false, 180, -1, -1, 0),
                note(15000, 8, true, 120, 0, 12, 0xFF),
                note(15000, 8, false, 180, -1, -1, 0),
                note(25000, 8, true, 130, 0, 12, 0xFF),
                note(35000, 8, false, 180, -1, -1, 0),
            ],
            None,
        );
        let mod_ = parse_fur(&write_pcm(&song)).expect("parse fur");
        let mut offs = Vec::new();
        for pat in &mod_.orders[8] {
            for (i, row) in mod_.patterns.get(&(8, *pat)).expect("pattern").iter().enumerate() {
                if row.note == NOTE_OFF {
                    offs.push(i as i32);
                }
            }
        }
        assert_eq!(offs, vec![song.place(35000, None).0]);
    }

    #[test]
    fn test_equal_volumes_write_both_speakers() {
        let song = pcm_song(
            vec![note(0, 8, true, 108, 0, 13, 0xFF)],
            None,
        );
        let rows = pcm_notes(&song);
        assert_eq!(rows.len(), 1);
        assert!(rows[0].1.fx.contains(&(0x08, 0xFF)));
    }
}

mod TestMelodyOnlyModule {
    use super::*;

    #[test]
    fn test_no_sample_chip_means_no_pcm_pair() {
        let song = fm_song(
            vec![note(0, 0, true, 108, -1, -1, 0x80)],
            6,
            60.0,
        );
        let data = write_all(&song, true);
        let (chips, _gains, _legacy) = chip_mix(&data);
        assert_eq!(chips, vec![CHIP_YM2151]);
        let mod_ = parse_fur(&data).expect("parse fur");
        assert_eq!(mod_.n_ch, 8);
        assert_eq!(mod_.pcm_channels, Vec::<usize>::new());
    }

    #[test]
    fn test_k007232_samples_keep_the_pair() {
        let song = pcm_song(
            vec![note(0, 8, true, 108, 0, 12, 0xFF)],
            None,
        );
        let mod_ = parse_fur(&write_pcm(&song)).expect("parse fur");
        assert_eq!(mod_.chips, vec![CHIP_YM2151, CHIP_K007232]);
        assert_eq!(mod_.n_ch, 10);
        assert_eq!(mod_.pcm_channels, vec![8, 9]);
    }
}

mod TestCompression {
    use super::*;

    #[test]
    fn test_compressed_module_round_trip() {
        let song = analyze(
            &note_song(),
            AnalyzeOptions {
                pcm: false,
                ..Default::default()
            },
        )
        .expect("analyze");
        let raw = write_all(&song, false);
        let comp = write_fur(
            &song,
            WriteOptions {
                include_pcm: false,
                include_fm: true,
                compress: true,
                ..Default::default()
            },
        );
        assert_eq!(&comp[..1], b"x");
        assert!(comp.len() < raw.len());
        assert_eq!(decompress_fur(&comp), raw);
        assert_eq!(
            parse_fur(&comp).expect("compressed fur").n_ch,
            parse_fur(&raw).expect("raw fur").n_ch
        );
    }
}

mod TestPatternWindow {
    use super::*;

    fn events(rows: &[i64]) -> Vec<NoteEvent> {
        rows.iter()
            .map(|&row| {
                let mut ev = note(row * 735, 0, true, 108, -1, -1, 0x80);
                ev.e5 = 0x80;
                ev
            })
            .collect()
    }

    #[test]
    fn test_short_track_gets_a_fitted_window() {
        // 735 samples/row at speed 1, 60 Hz. The last note sits on row 20.
        let song = fm_song(events(&[0, 4, 20]), 1, 60.0);
        let mod_ = parse_fur(&write_fur(&song, WriteOptions::default())).expect("parse fur");
        assert_eq!(mod_.n_ord, 1);
        assert_eq!(mod_.pat_len, 20 + 1 + 10);
    }

    #[test]
    fn test_long_track_uses_a_measure_window() {
        // One row per 16th: 8/4 is 32 rows. A 300-row track becomes ten
        // phrase patterns instead of one 256-row chunk.
        let song = fm_song(events(&[0, 300]), 1, 60.0);
        let mod_ = parse_fur(&write_fur(&song, WriteOptions::default())).expect("parse fur");
        assert_eq!(mod_.pat_len, 32);
        assert_eq!(mod_.n_ord, 10);
    }

    #[test]
    fn test_a_dense_grid_still_reaches_the_ceiling() {
        // Eight rows per 16th makes 8/4 the full 256 rows. A longer track
        // cannot grow past Furnace's ceiling.
        let mut song = fm_song(events(&[0, 300]), 1, 60.0);
        song.row_subdiv = 8.0;
        let mod_ = parse_fur(&write_fur(&song, WriteOptions::default())).expect("parse fur");
        assert_eq!(mod_.pat_len, 256);
        assert_eq!(mod_.n_ord, 2);
    }
}

mod TestFurPanAmps {
    use super::*;

    #[test]
    fn test_08xy_nibbles() {
        assert_eq!(fx_pan_amps(0xFF), (1.0, 1.0));
        assert_eq!(fx_pan_amps(0xF0), (1.0, 0.0));
        assert_eq!(fx_pan_amps(0x0F), (0.0, 1.0));
        let (l, r) = fx_pan_amps(0x80);
        assert_almost_eq!(l, 8.0 / 15.0, 5);
        assert_eq!(r, 0.0);
        let (l, r) = fx_pan_amps(0x88);
        assert_almost_eq!(l, 8.0 / 15.0, 5);
        assert_almost_eq!(r, 8.0 / 15.0, 5);
    }
}

mod TestFMOperatorOrder {
    use super::*;

    #[test]
    fn test_ops_in_register_order() {
        let mut vgm = VgmFile::default();
        vgm.path = PathBuf::from("synthetic.vgm");
        vgm.version = 0x151;
        vgm.total_samples = 1;
        vgm.ym2151_clock = 3579545;
        vgm.k007232_clock = 3579545;
        let ops: Vec<OpRegs> = (0..4i32)
            .map(|i| OpRegs {
                dt_mul: ((((i + 1) & 7) << 4) | (i & 0x0F)) as u8,
                tl: (0x11 + i * 0x11) as u8,
                ks_ar: (((i & 3) << 6) | (0x1F - i)) as u8,
                am_d1r: ((if i & 1 != 0 { 0x80 } else { 0 }) | (i + 3)) as u8,
                dt2_d2r: (((i & 3) << 6) | (i + 5)) as u8,
                d1l_rr: (((i + 1) << 4) | i) as u8,
                ..Default::default()
            })
            .collect();
        let patch = FMPatch {
            alg: 4,
            fb: 5,
            ams: 2,
            pms: 3,
            ops: [ops[0].clone(), ops[1].clone(), ops[2].clone(), ops[3].clone()],
            ..Default::default()
        };
        let song = make_song(
            vgm,
            1,
            60.0,
            None,
            Vec::new(),
            vec![note(0, 0, true, 108, 0, -1, 0x80)],
        );
        let mut song = song;
        song.fm_patches = vec![patch];
        let data = write_all(&song, false);
        let ins2 = find_from(&data, b"INS2", 0).expect("INS2");
        let fm = find_from(&data, b"FM", ins2).expect("FM");
        assert!(fm > 0);
        let body = &data[fm + 4..];
        assert_eq!(body[0] & 0xF0, 0xF0); // all four operators enabled
        assert_eq!(body[1], (4 << 4) | 5); // alg/fb
        assert_eq!(body[2], (2 << 3) | 3); // ams/fms
        for i in 0..4usize {
            let op = &body[4 + i * 8..4 + i * 8 + 8];
            assert_eq!(
                op[0],
                ((ym_dt_to_furnace(((ops[i].dt_mul >> 4) & 7) as i32) << 4)
                    | (ops[i].dt_mul & 0x0F) as i32) as u8,
                "op{i} dt/mul"
            );
            assert_eq!(op[1], ops[i].tl & 0x7F, "op{i} tl");
            assert_eq!(op[2], ops[i].ks_ar & 0xDF, "op{i} ks/ar");
            assert_eq!(op[3], ops[i].am_d1r, "op{i} am/d1r");
            assert_eq!(
                op[7],
                (((ops[i].dt2_d2r >> 6) & 3) << 3) as u8,
                "op{i} dt2"
            );
        }
        // A swap regression would land op2's TL in slot 1.
        assert_eq!(body[4 + 1 * 8 + 1], ops[1].tl);
        assert_eq!(body[4 + 2 * 8 + 1], ops[2].tl);
    }
}

fn oki_vgm() -> VgmFile {
    // Banked phrase 0 (8 bytes, voice 0) and phrase 1 (400 bytes, voice 3).
    let mut rom = vec![0u8; 0x20800];
    rom[0x20000..0x20008].copy_from_slice(&[0x00, 0x05, 0x00, 0x00, 0x05, 0x07, 0x00, 0x00]);
    rom[0x20008..0x20010].copy_from_slice(&[0x00, 0x06, 0x00, 0x00, 0x07, 0x8F, 0x00, 0x00]);
    rom[0x20500..0x20508].copy_from_slice(&[0x11; 8]);
    rom[0x20600..0x20790].copy_from_slice(&[0x22; 400]);
    let writes = vec![
        ChipWrite::new(0, "msm6295", 0, 0x0E, 0x81),
        ChipWrite::new(10, "msm6295", 0, 0x10, 0x02),
        ChipWrite::new(100, "msm6295", 0, 0x00, 0x80),
        ChipWrite::new(110, "msm6295", 0, 0x00, 0x10),
        ChipWrite::new(200, "msm6295", 0, 0x00, 0x81),
        ChipWrite::new(210, "msm6295", 0, 0x00, 0x80),
        ChipWrite::new(2000, "msm6295", 0, 0x00, 0x40),
    ];
    let mut vgm = VgmFile::default();
    vgm.path = PathBuf::from("oki.vgm");
    vgm.version = 0x161;
    vgm.total_samples = 3000;
    vgm.ym2151_clock = 4_000_000;
    vgm.k007232_clock = 4_000_000;
    vgm.gd3.insert("track".to_string(), "oki".to_string());
    vgm.roms = vec![RomSlice {
        chip: 0,
        rom_size: rom.len() as i64,
        start: 0,
        data: rom,
        kind: "msm6295",
        time: 0,
    }];
    vgm.writes = writes;
    vgm.msm6295_clock = 2_000_000;
    vgm.msm6295_pin7 = true;
    vgm
}

fn msm6258_vgm() -> VgmFile {
    let writes = vec![
        ChipWrite::new(0, "msm6258", 0, 0, 0x02),
        ChipWrite::new(10, "msm6258", 0, 1, 0x12),
        ChipWrite::new(20, "msm6258", 0, 1, 0x34),
        ChipWrite::new(30, "msm6258", 0, 1, 0x56),
        ChipWrite::new(40, "msm6258", 0, 1, 0x78),
        ChipWrite::new(800, "msm6258", 0, 0, 0x01),
    ];
    let mut vgm = VgmFile::default();
    vgm.path = PathBuf::from("adpcm.vgm");
    vgm.version = 0x161;
    vgm.total_samples = 1000;
    vgm.ym2151_clock = 4_000_000;
    vgm.k007232_clock = 4_000_000;
    vgm.gd3.insert("track".to_string(), "adpcm".to_string());
    vgm.writes = writes;
    vgm.msm6258_clock = 4_000_000;
    vgm.msm6258_flags = 0;
    vgm
}

mod TestOkiVolumeAndFlags {
    use super::*;

    #[test]
    fn test_nibble_and_pin7() {
        assert_eq!(oki_furnace_vol(0), Some(8));
        assert_eq!(oki_furnace_vol(2), Some(6));
        assert_eq!(oki_furnace_vol(8), Some(0));
        assert_eq!(oki_furnace_vol(9), None);
        assert_eq!(oki_rate(2_000_000, true), 15152);
        assert_eq!(msm6258_div_fx(1024), Some(0));
        assert_eq!(msm6258_div_fx(768), Some(1));
        assert_eq!(msm6258_div_fx(512), None);
        assert_eq!(msm6258_best_divider(8_000_000, 6, 7813), 512);
        assert_eq!(msm6258_best_divider(8_000_000, 0, 0), 1024);
        let high = oki_flag_text(2_000_000, true, false);
        assert!(high.contains("clockSel=8"));
        assert!(!high.contains("rateSel"));
        let low = oki_flag_text(2_000_000, false, true);
        assert!(low.contains("rateSel=true"));
        assert!(low.contains("isBanked=true"));
    }
}

mod TestOkiPhrase {
    use super::*;

    #[test]
    fn test_0f_bank_selects_the_second_256k_half() {
        // Some drivers use $0F as a single 256 KB bank select instead of the
        // NMK112 map. The phrase book and sample data are both relative to
        // that bank; reading them from offset 0 saw an empty book and dropped
        // every MSM6295 channel.
        let mut rom = vec![0u8; 0x80000];
        rom[0x40000..0x40008].copy_from_slice(&[0x00, 0x01, 0x00, 0x00, 0x01, 0x07, 0x00, 0x00]);
        rom[0x40100..0x40108].copy_from_slice(&[0x11; 8]);
        let mut v = VgmFile::default();
        v.path = PathBuf::from("oki_bank.vgm");
        v.version = 0x161;
        v.total_samples = 1000;
        v.ym2151_clock = 4_000_000;
        v.k007232_clock = 0;
        v.roms = vec![RomSlice {
            chip: 0,
            rom_size: 0x80000,
            start: 0,
            data: rom,
            kind: "msm6295",
            time: 0,
        }];
        v.writes = vec![
            ChipWrite::new(0, "msm6295", 0, 0x0F, 0x00),
            ChipWrite::new(10, "msm6295", 0, 0x0F, 0x01),
            ChipWrite::new(100, "msm6295", 0, 0x00, 0x80),
            ChipWrite::new(110, "msm6295", 0, 0x00, 0x10),
        ];
        v.msm6295_clock = 2_000_000;
        v.msm6295_pin7 = true;
        let (samples, warn) = collect_oki(&v);
        assert!(warn.is_empty());
        assert_eq!(samples.len(), 1);
        assert_eq!(samples[0].data, vec![0x11; 8]);
        let hits: Vec<(i32, i32)> = samples[0].hits.iter().map(|h| (h.voice, h.vol)).collect();
        assert_eq!(hits, vec![(0, 8)]);
    }

    #[test]
    fn test_bank_voice3_and_early_stop() {
        let song = analyze(
            &oki_vgm(),
            AnalyzeOptions {
                speed: Some(1),
                pcm: true,
                ..Default::default()
            },
        )
        .expect("analyze");
        assert_eq!(
            song.oki_samples.iter().map(|s| s.length()).collect::<Vec<_>>(),
            vec![8, 400]
        );
        assert_eq!(song.oki_samples[0].phrase, 0);
        assert_eq!(song.oki_samples[1].phrase, 1);
        assert_eq!(song.oki_samples[0].rate, 15152);
        let short = &song.oki_samples[0].hits;
        let long = &song.oki_samples[1].hits;
        assert_eq!(
            short.iter().map(|h| (h.voice, h.vol)).collect::<Vec<_>>(),
            vec![(0, 8)]
        );
        assert_eq!(
            long.iter().map(|h| (h.voice, h.vol, h.end)).collect::<Vec<_>>(),
            vec![(3, 8, 2000)]
        );
        let mut ons: Vec<(i32, i32, i32)> = song
            .events
            .iter()
            .filter(|e| e.on && e.pcm)
            .map(|e| (e.ch, e.note, e.vol))
            .collect();
        ons.sort();
        assert_eq!(ons, vec![(8, 108, 8), (11, 108, 8)]);
        assert!(song.events.iter().filter(|e| e.on && e.pcm).all(|e| e.no_pan));
        let offs: Vec<(i32, i64)> = song
            .events
            .iter()
            .filter(|e| !e.on && e.pcm)
            .map(|e| (e.ch, e.sample))
            .collect();
        assert_eq!(offs, vec![(11, 2000)]);

        let data = write_pcm(&song);
        let (chips, gains, legacy) = chip_mix(&data);
        assert_eq!(chips, vec![CHIP_YM2151, CHIP_MSM6295]);
        assert_eq!(legacy, vec![64, 64]);
        assert_almost_eq!(gains[0], 1.0, 5);
        assert_almost_eq!(gains[1], 1.0, 5);
        assert!(!chips.contains(&CHIP_K007232));
        assert!(find_from(&data, b"clockSel=8", 0).is_some());
        assert!(find_from(&data, b"rateSel=true", 0).is_none());
        assert_eq!(ins_types(&data), vec![36, 36]);
        let samples = list_fur_samples(&data).expect("samples");
        assert_eq!(samples.iter().map(|s| s.depth).collect::<Vec<_>>(), vec![10, 10]);
        // A VOX sample's stored length is a NIBBLE count. DivSample::initInternal
        // allocates `lengthVOX = (count + 1) / 2` bytes on every chip, and a byte
        // count plays half the sample. Eight payload bytes are sixteen frames.
        assert_eq!(samples.iter().map(|s| s.n_frames).collect::<Vec<_>>(), vec![16, 800]);
        assert_eq!(samples[0].c4_rate, 15152);
        assert_eq!(samples[0].data, vec![0x11; 8]);
        assert_ne!(samples[0].presence, vec![0x00; 16]);

        let mod_ = parse_fur(&data).expect("parse fur");
        assert_eq!(mod_.n_ch, 12);
        let mut played: Vec<(usize, i32, i32, Vec<(u8, u8)>)> = Vec::new();
        for ch in 8..12usize {
            for (_row_i, row) in rows_of(&mod_, ch) {
                if (0..180).contains(&row.note) {
                    played.push((ch, row.note, row.vol, row.fx.clone()));
                }
                assert!(!row.fx.iter().any(|(c, _v)| *c == 0x08));
            }
        }
        assert_eq!(
            played
                .iter()
                .map(|(c, n, v, _fx)| (*c, *n, *v))
                .collect::<Vec<_>>(),
            vec![(8, 108, 8), (11, 108, 8)]
        );
        let offs_ch: Vec<i32> = rows_of(&mod_, 11)
            .into_iter()
            .filter(|(_i, row)| row.note == NOTE_OFF)
            .map(|(_i, row)| row.note)
            .collect();
        assert_eq!(offs_ch, vec![NOTE_OFF]);
    }
}

mod TestMsm6258Burst {
    use super::*;

    #[test]
    fn test_one_burst_and_divider_effect() {
        let song = analyze(
            &msm6258_vgm(),
            AnalyzeOptions {
                speed: Some(1),
                pcm: true,
                ..Default::default()
            },
        )
        .expect("analyze");
        assert_eq!(song.msm6258_samples.len(), 1);
        assert_eq!(song.msm6258_samples[0].data, vec![0x12, 0x34, 0x56, 0x78]);
        assert_eq!(song.channel_fx, vec![(0, 8, 0x20, 0)]);
        let ons: Vec<(i32, i32, i32)> = song
            .events
            .iter()
            .filter(|e| e.on && e.pcm)
            .map(|e| (e.ch, e.note, e.vol))
            .collect();
        assert_eq!(ons, vec![(8, 60, 127)]);
        let data = write_pcm(&song);
        let (chips, gains, legacy) = chip_mix(&data);
        assert_eq!(chips, vec![CHIP_YM2151, CHIP_MSM6258]);
        assert_eq!(legacy[1], 44);
        assert_almost_eq!(gains[1], 0.6815, 3);
        assert_eq!(ins_types(&data), vec![35]);
        let samples = list_fur_samples(&data).expect("samples");
        assert_eq!(samples.len(), 1);
        assert_eq!(samples[0].depth, 10);
        assert_eq!(samples[0].n_frames, 8);
        assert_eq!(samples[0].data, vec![0x21, 0x43, 0x65, 0x87]);
        let mod_ = parse_fur(&data).expect("parse fur");
        assert_eq!(mod_.n_ch, 9);
        let rows = rows_of(&mod_, 8);
        let notes: Vec<FurRow> = rows
            .into_iter()
            .filter(|(_i, row)| (0..180).contains(&row.note))
            .map(|(_i, row)| row)
            .collect();
        assert_eq!(notes.len(), 1);
        assert_eq!(notes[0].note, 60);
        assert_eq!(notes[0].vol, 127);
        assert!(notes[0].fx.contains(&(0x20, 0)));
        assert!(!notes[0].fx.iter().any(|(c, _v)| *c == 0x08));
        assert!(rows_of(&mod_, 8).iter().any(|(_i, row)| row.note == NOTE_OFF));
    }

    #[test]
    fn test_short_direct_burst_is_dropped() {
        let mut vgm = msm6258_vgm();
        vgm.writes = vec![ChipWrite::new(0, "msm6258", 0, 1, 0x02)];
        let song = analyze(
            &vgm,
            AnalyzeOptions {
                speed: Some(1),
                pcm: true,
                ..Default::default()
            },
        )
        .expect("analyze");
        assert_eq!(song.msm6258_samples, Vec::new());
        assert!(song.warnings.iter().any(|w| w.contains("shorter than")));
    }
}

fn raw_adpcm(cmds: &[u8], blocks: &[Vec<u8>], clock: u32, flags: u8, total: u32) -> Vec<u8> {
    let mut buf = vec![0u8; 0x100];
    buf[0..4].copy_from_slice(b"Vgm ");
    buf[8..12].copy_from_slice(&0x161u32.to_le_bytes());
    buf[0x18..0x1C].copy_from_slice(&total.to_le_bytes());
    buf[0x30..0x34].copy_from_slice(&4_000_000u32.to_le_bytes());
    buf[0x34..0x38].copy_from_slice(&(0x100u32 - 0x34).to_le_bytes());
    buf[0x90..0x94].copy_from_slice(&clock.to_le_bytes());
    buf[0x94] = flags;
    let mut body = Vec::new();
    for data in blocks {
        body.push(0x67);
        body.push(0x66);
        body.push(0x04);
        body.extend_from_slice(&(data.len() as u32).to_le_bytes());
        body.extend_from_slice(data);
    }
    body.extend_from_slice(cmds);
    body.push(0x66);
    let mut raw = buf;
    raw.extend_from_slice(&body);
    let len = (raw.len() - 4) as u32;
    raw[4..8].copy_from_slice(&len.to_le_bytes());
    raw
}

mod TestMsm6258Stream {
    use super::*;

    fn wait(n: u16) -> Vec<u8> {
        let mut v = vec![0x61];
        v.extend_from_slice(&n.to_le_bytes());
        v
    }

    fn play(blk: u16) -> Vec<u8> {
        let mut v = vec![0x95, 0x00];
        v.extend_from_slice(&blk.to_le_bytes());
        v.push(0x00);
        v
    }

    #[test]
    fn test_block_plays_share_a_sample_and_ignore_dummy_bytes() {
        let block_a = vec![0x12, 0x34, 0x56, 0x78, 0x9A, 0xBC, 0xDE, 0xF0];
        let block_b = vec![0xAB, 0xCD, 0xEF, 0x01, 0x23, 0x45, 0x67, 0x89];
        let mut cmds = Vec::new();
        cmds.extend_from_slice(&[0x90, 0x00, 0x17, 0x00, 0x01]);
        cmds.extend_from_slice(&[0x91, 0x00, 0x04, 0x01, 0x00]);
        cmds.extend_from_slice(&[0x92, 0x00]);
        cmds.extend_from_slice(&7813u32.to_le_bytes());
        cmds.extend_from_slice(&[0xB7, 0x01, 0x02]);
        cmds.extend(wait(100));
        cmds.extend(play(0));
        cmds.extend(wait(5000));
        cmds.extend_from_slice(&[0x94, 0x00]);
        cmds.extend(wait(100));
        cmds.extend(play(0));
        cmds.extend(wait(100));
        cmds.extend(play(1));
        let raw = raw_adpcm(&cmds, &[block_a.clone(), block_b.clone()], 8_000_000, 6, 20000);
        let loaded = {
            let tmp = tempfile::tempdir().expect("tempdir");
            let path = tmp.path().join("stream.vgm");
            std::fs::write(&path, &raw).expect("write vgm");
            load_vgm(&path).expect("load vgm")
        };
        assert_eq!(loaded.adpcm_blocks, vec![block_a.clone(), block_b.clone()]);
        assert_eq!(loaded.msm6258_plays.len(), 3);
        assert_eq!(loaded.dac_block_plays, Vec::new());
        assert_eq!(loaded.msm6258_plays[0].end, 5100);
        assert_eq!(loaded.msm6258_plays[2].block, 1);
        let song = analyze(
            &loaded,
            AnalyzeOptions {
                speed: Some(1),
                pcm: true,
                ..Default::default()
            },
        )
        .expect("analyze");
        assert_eq!(song.msm6258_samples.len(), 2);
        assert_eq!(song.msm6258_samples[0].data, block_a);
        assert_eq!(song.msm6258_samples[0].hits.len(), 2);
        assert_eq!(song.msm6258_samples[0].divider, 512);
        assert_eq!(song.channel_fx, Vec::new());
        let ons: Vec<(i32, i32)> = song
            .events
            .iter()
            .filter(|e| e.on && e.pcm)
            .map(|e| (e.note, e.ins))
            .collect();
        assert_eq!(ons, vec![(60, 0), (60, 0), (61, 0)]);
        let data = write_pcm(&song);
        assert_eq!(ins_types(&data), vec![35]);
        let samples = list_fur_samples(&data).expect("samples");
        assert_eq!(samples[0].n_frames, 16);
        assert_eq!(
            samples[0].data,
            vec![0x21, 0x43, 0x65, 0x87, 0xA9, 0xCB, 0xED, 0x0F]
        );
        let info = find_from(&data, b"INFO", 0).expect("INFO");
        assert_eq!(
            u16::from_le_bytes(data[info + 22..info + 24].try_into().unwrap()),
            1
        );
    }

    #[test]
    fn test_bare_95_stays_on_the_ym2612_path() {
        let raw = raw_adpcm(&[0x95, 0x00, 0x00, 0x00, 0x00], &[], 8_000_000, 6, 20000);
        let loaded = {
            let tmp = tempfile::tempdir().expect("tempdir");
            let path = tmp.path().join("dac.vgm");
            std::fs::write(&path, &raw).expect("write vgm");
            load_vgm(&path).expect("load vgm")
        };
        assert_eq!(loaded.msm6258_plays, Vec::new());
        assert_eq!(loaded.dac_block_plays.len(), 1);
    }

    #[test]
    fn test_note_map_keeps_a_long_bank_under_the_instrument_cap() {
        let blocks: Vec<Vec<u8>> = (0..130)
            .map(|i| {
                vec![
                    (i & 0xFF) as u8,
                    0x10,
                    0x20,
                    0x30,
                    0x40,
                    0x50,
                    0x60,
                    0x71,
                ]
            })
            .collect();
        let plays: Vec<Msm6258Play> = (0..130)
            .map(|i| Msm6258Play {
                sample: i * 100,
                chip_id: 0,
                block: i as i32,
                rate: 7813,
                ..Default::default()
            })
            .collect();
        let mut src = msm6258_vgm();
        src.writes = Vec::new();
        src.adpcm_blocks = blocks;
        src.msm6258_plays = plays;
        src.msm6258_clock = 8_000_000;
        src.msm6258_flags = 6;
        let song = analyze(
            &src,
            AnalyzeOptions {
                speed: Some(1),
                pcm: true,
                ..Default::default()
            },
        )
        .expect("analyze");
        assert_eq!(song.msm6258_samples.len(), 130);
        assert!(!song.warnings.iter().any(|w| w.contains("250-slot")));
        let ons: Vec<&NoteEvent> = song.events.iter().filter(|e| e.on && e.pcm).collect();
        assert_eq!((ons[0].note, ons[0].ins), (60, 0));
        assert_eq!((ons[120].note, ons[120].ins), (60, 1));
        let data = write_pcm(&song);
        assert_eq!(ins_types(&data), vec![35, 35]);
        let info = find_from(&data, b"INFO", 0).expect("INFO");
        assert_eq!(
            u16::from_le_bytes(data[info + 22..info + 24].try_into().unwrap()),
            2
        );
        assert_eq!(list_fur_samples(&data).expect("samples").len(), 130);
    }
}

mod TestColumnFit {
    use super::*;

    #[test]
    fn test_fit_fx_cols_floor_and_ceiling() {
        let mut rows = vec![Row::default(), Row::default(), Row::default(), Row::default()];
        rows[0].fx = Some(vec![(0x08, 0x80), (0xE5, 0x90), (0xED, 0x10)]);
        let mut full = _blank_row();
        for cmd in 1..=8 {
            _add_fx(&mut full, cmd, 0x11, 8, None);
        }
        let patterns = vec![
            (0i32, 0i32, rows.into_iter().map(Some).collect::<Vec<Option<Row>>>()),
            (1, 0, vec![Some(full)]),
            (2, 0, vec![Some(_blank_row())]),
        ];
        let cols = _fit_fx_cols(&patterns, 4);
        assert_eq!(cols, vec![3, 8, 1, 1]);
    }

    #[test]
    fn test_effect_columns_match_content() {
        // `inject_rows` installs the sweep hook, so the writer emits the
        // injected effect columns exactly like the Python test's patch.
        let mut song = analyze(
            &note_song_with(60000, 160000, 0, 0, &[], 0),
            AnalyzeOptions {
                pcm: false,
                ..Default::default()
            },
        )
        .expect("analyze");
        let plan: Vec<(i32, i32, i32)> = vec![(2, 0x08, 0x80), (2, 0xE5, 0x90), (2, 0xED, 0x10)];
        let data = common::inject_rows(&mut song, &plan, 60_000, |song, _row0, hooks| {
            vgm2fur::furwrite::write_fur_with_hooks(
                song,
                WriteOptions { include_pcm: false, include_fm: true, ..Default::default() },
                hooks,
            )
        });
        let module = parse_fur(&data).expect("parse fur");
        // Python compares the module's `effect_cols` with the number of
        // effects each parsed row carries (the decoder drops EMPTY pairs).
        let mut max_fx = vec![0u8; module.n_ch as usize];
        for ((ch, _idx), rows) in module.patterns.iter() {
            for row in rows {
                let n = row.fx.len() as u8;
                max_fx[*ch as usize] = max_fx[*ch as usize].max(n);
            }
        }
        let want: Vec<u8> = max_fx.iter().map(|&n| n.max(1)).collect();
        assert_eq!(module.effect_cols, want);
        assert!(module.effect_cols[0] >= 3);
        assert!(module
            .effect_cols
            .iter()
            .all(|&c| (1..=8).contains(&c)));
    }
}
