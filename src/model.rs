//! Note events, instrument descriptions and the song container.
//!
//! `NoteEvent` is the analyzer's only currency: every chip collector produces
//! them, the pattern builder consumes them, and the field set is the union of
//! everything any chip needs to describe one sounding note.

use crate::parity::{py_floor_div, py_mod, py_round};

/// A driver may key an OPN voice while its carriers are still muted and write
/// the patch's real TL a few samples later. A level write this close to the
/// key-on is the note's starting level, not a mid-note step.
pub const OPN_TL_FOLD: i64 = 512;
/// A driver can load a new patch into a still-keyed OPN voice. Split the note
/// when a pitch/timbre register changes, after giving the patch block time to
/// settle.
pub const OPN_PATCH_SETTLE: i64 = 512;

/// One sample-chip trigger is often a key-on plus its own pitch and volume
/// writes. A real drum interval is hundreds of samples, never tens.
pub const ONSET_MERGE_SAMPLES: i64 = 100;

/// A driver that keys on with its carriers parked at TL 7F loads the voice's
/// real levels inside the same burst. A burst no longer than a few writes is
/// still the same command.
pub const OPM_BURST_LOAD_SAMPLES: i64 = 16;

/// Official YM2151 KC note nibble to semitone from C of this octave.
pub const KC_SEMI: [i32; 16] = [
    1, 2, 3, 4, 4, 5, 6, 7, 7, 8, 9, 10, 10, 11, 12, 12,
];

/// Furnace's default YM2151 clock.
pub const OPM_DEFAULT_CLOCK: u32 = 3_579_545;

/// YM2151 KC to Furnace note (0 = C-(-5), 108 = C-4, 179 = B-9).
pub fn kc_to_furnace_note(kc: i32) -> i32 {
    let octave = (kc >> 4) & 7;
    let semi = KC_SEMI[(kc & 0x0F) as usize];
    ((octave + 5) * 12 + semi).clamp(0, 179)
}

pub fn furnace_note_name(n: i32) -> String {
    if n == 180 {
        return "OFF".to_string();
    }
    let names = [
        "C-", "C#", "D-", "D#", "E-", "F-", "F#", "G-", "G#", "A-", "A#", "B-",
    ];
    let octv = py_floor_div(n as i64, 12) - 5;
    format!("{}{}", names[py_mod(n as i64, 12) as usize], octv)
}

/// YM2151 KF register to Furnace E5xx fine tune.
pub fn kf_to_e5(kf: i32) -> i32 {
    (0x80 + (kf >> 2)).clamp(0, 255)
}

/// Absolute pitch code in 1/64 semitone units (fraction = KF >> 2).
pub fn kc_kf_units(kc: i32, kf: i32) -> i32 {
    ((((kc >> 4) & 7) * 12 + KC_SEMI[(kc & 0x0F) as usize]) * 64) + (kf >> 2)
}

/// Clock to note transposition in 1/64 semitone units.
pub fn opm_pitch_offset(clock: Option<u32>) -> i32 {
    match clock {
        None => 0,
        Some(0) => 0,
        Some(c) if c == OPM_DEFAULT_CLOCK => 0,
        Some(c) => {
            py_round(768.0 * (c as f64 / OPM_DEFAULT_CLOCK as f64).log2()) as i32
        }
    }
}

/// Shift a note/E5 pair by `offset` 1/64-semitone units.
pub fn shift_note_e5(note: i32, e5: i32, offset: i32) -> (i32, i32) {
    let total = note * 64 + (e5 - 0x80) + offset;
    let shifted = py_floor_div(total as i64, 64) as i32;
    let frac = py_mod(total as i64, 64) as i32;
    (shifted.clamp(0, 179), 0x80 + frac)
}

/// Move FM note/E5 pairs for a rip whose OPM clock is not the default.
pub fn transpose_opm_events(events: &mut [NoteEvent], clock: Option<u32>, channels: i32) {
    let offset = opm_pitch_offset(clock);
    if offset == 0 {
        return;
    }
    for ev in events.iter_mut() {
        // DAC notes select a sample-map slot, not a musical pitch.
        if ev.on && !ev.dac && ev.ch < channels && ev.note >= 0 && ev.note < 180 {
            let (note, e5) = shift_note_e5(ev.note, ev.e5, offset);
            ev.note = note;
            ev.e5 = e5;
        }
    }
}

/// Sample-playback note/E5 for a DAC-stream byte rate.
pub fn pcm_rate_note(rate: i64, reference: i64) -> (i32, i32) {
    if rate <= 0 || reference <= 0 {
        return (108, crate::pitch::E5_CENTER);
    }
    let semis = 12.0 * (rate as f64 / reference as f64).log2();
    let base = semis.floor() as i32;
    let fine = py_round((semis - base as f64) * 128.0) as i32;
    let mut note = 108 + base;
    let mut fine = fine;
    while note < 0 {
        note += 1;
        fine -= 128;
    }
    while note > crate::pitch::NOTE_MAX {
        note -= 1;
        fine += 128;
    }
    (note, (crate::pitch::E5_CENTER + fine).clamp(0, 255))
}

/// Furnace 08xy that reproduces a MultiPCM pan-register nibble.
pub fn multipcm_pan_fx(pan: i32) -> i32 {
    let pan = pan & 0xF;
    if pan == 8 {
        return 0;
    }
    if pan == 0 {
        return 0xFF;
    }
    if pan < 8 {
        let left = 16 - 2 * pan;
        return ((left & 0xF) << 4) | 0x0F;
    }
    let right = 2 * (pan - 9);
    0xF0 | (right & 0xF)
}

/// Furnace 08xy from K007232 L/R volumes (0-15).
pub fn pcm_pan_fx(vl: i32, vr: i32) -> i32 {
    let vl = vl.clamp(0, 15);
    let vr = vr.clamp(0, 15);
    let peak = vl.max(vr);
    if peak <= 0 {
        return 0x00;
    }
    let left = (vl * 15 + peak / 2) / peak;
    let right = (vr * 15 + peak / 2) / peak;
    ((left & 15) << 4) | (right & 15)
}

/// K007232 volume register to Furnace 0-15.
pub fn vol15(raw: i32, scale: f64) -> i32 {
    if raw <= 0 {
        return 0;
    }
    (py_round(raw as f64 * 15.0 / scale) as i32).clamp(1, 15)
}

#[derive(Clone, Debug, PartialEq)]
pub struct NoteEvent {
    pub sample: i64,
    /// 0-7 FM, 8-9 PCM.
    pub ch: i32,
    pub on: bool,
    /// Furnace 0-179, or 180 off.
    pub note: i32,
    pub ins: i32,
    /// 0-127 FM, 0-15 PCM; -1 = omit.
    pub vol: i32,
    /// 0x80 = none.
    pub e5: i32,
    /// 0xLR, 0 means omit.
    pub pan: i32,
    pub pcm: bool,
    pub legato: bool,
    pub dac: bool,
    /// Key-off sample (0 = still playing at the end / unknown).
    pub end: i64,
    /// Mid-note pitch writes: (sample, 1/64-semitone delta from the note pitch).
    pub sweep: Vec<(i64, i32)>,
    /// OPM PMS/AMS writes while the note is held: (sample, pms, ams).
    pub opm_pms_pts: Vec<(i64, i32, i32)>,
    /// Mid-note volume steps: (sample, chip level, 08xy balance or None).
    pub vol_pts: Vec<(i64, i32, Option<i32>)>,
    /// Mid-note carrier TL steps: (sample, absolute YM2151 carrier TL).
    pub tl_pts: Vec<(i64, i32)>,
    /// Mid-note DT1/MULT writes, (sample, operator, raw 0x40 register byte).
    pub op_dtmul_pts: Vec<(i64, i32, i32)>,
    pub op_mul_fx: Vec<(i64, i32, i32)>,
    pub op_dt_fx: Vec<(i64, i32, i32)>,
    pub op_dt2_fx: Vec<(i64, i32, i32)>,
    pub op_dt2_pts: Vec<(i64, i32, i32)>,
    pub vol_tl_base: Option<i32>,
    pub no_pan: bool,
    pub cut_before: bool,
    pub c219_noise: bool,
    pub c219_mode: i32,
    /// C352 quad output: (front L/R, rear L/R), 0-255.
    pub quad: Option<(i32, i32, i32, i32)>,
    pub quad_pts: Vec<(i64, i32, i32, i32, i32)>,
    pub drum: bool,
    pub tone_pitch_pts: Vec<(i64, i32)>,
    /// Tone chips only: (sample, wavetable image index) whenever the image
    /// changed. The byte bodies live in the song's wave lists.
    pub tone_wave_pts: Vec<(i64, i32)>,
    pub tone_wave: i32,
    pub tone_sample: i32,
    pub tone_sample_kind: String,
    pub tone_fds_mod: i32,
    pub tone_fds_depth: i32,
    pub tone_fds_speed: i32,
    pub tone_fds_pos: i32,
    pub tone_pan_l: i32,
    pub tone_pan_r: i32,
    pub tone_period_offset: i32,
    pub tone_period: i32,
    /// 0 = not a tone-chip note, 1 = SCC, 2 = AY, 8 = SAA1099, 16 = WSG.
    pub tone_chip: i32,
    pub tone_amp_pts: Vec<(i64, i32, i32)>,
    pub tone_duty_pts: Vec<(i64, i32)>,
    pub tone_lfsr_pts: Vec<(i64, i32)>,
    pub tone_duty: i32,
    pub tone_lfsr: i32,
    pub x1_freq: i32,
    pub x1_freq_pts: Vec<(i64, i32)>,
    pub x1_wave_pts: Vec<(i64, i32)>,
    pub x1_env_shape_pts: Vec<(i64, i32)>,
    pub x1_env_period_pts: Vec<(i64, i32)>,
    pub x1_env_mode_pts: Vec<(i64, i32)>,
    pub tone_env_shape: i32,
    pub tone_env_period: i32,
    pub tone_env_mode: i32,
    pub tone_noise: bool,
    pub tone_noise_end: i64,
    pub tone_noise_modes: Vec<(i64, i32)>,
    pub tone_clock: u32,
    pub tone_div: i32,
    pub tone_mod: i32,
    pub tone_gb_wave: bool,
    pub gb_soft_env: bool,
    pub gb_env_vol: i32,
    pub gb_env_dir: i32,
    pub gb_env_len: i32,
    pub gb_sound_len: i32,
    pub gb_sound_len_inf: bool,
    /// AY only: (sample, effect, value), including the state at the note start.
    pub tone_fx_pts: Vec<(i64, i32, i32)>,
    /// 0 = volume macro or column, 1 = FA01 from the note, 2 = FA00 on the note.
    pub vol_slide: i32,
    pub sweep_div: f64,
    pub e5_lo: i32,
    pub e5_hi: i32,
    /// (sample, block, fnum) writes while the note is held.
    pub fm_pitch_pts: Vec<(i64, i32, i32)>,
    pub pitch_steps: Vec<(i64, i32)>,
    pub opl_op_tl: Vec<(i32, Vec<(i64, i32)>)>,
    pub op_tl_fx: Vec<(i64, i32, i32)>,
    pub opll_preset_pts: Vec<(i64, i32)>,
    pub opll_phase_pts: Vec<i64>,
}

impl NoteEvent {
    /// `NoteEvent(sample, ch, on, note, ins, vol, e5, pan, **kw)` with the
    /// dataclass defaults for everything after `pan`.
    pub fn new(
        sample: i64,
        ch: i32,
        on: bool,
        note: i32,
        ins: i32,
        vol: i32,
        e5: i32,
        pan: i32,
    ) -> Self {
        NoteEvent { sample, ch, on, note, ins, vol, e5, pan, ..Self::default() }
    }
}

impl Default for NoteEvent {
    fn default() -> Self {
        NoteEvent {
            sample: 0,
            ch: 0,
            on: false,
            note: 0,
            ins: 0,
            vol: 0,
            e5: 0,
            pan: 0,
            pcm: false,
            legato: false,
            dac: false,
            end: 0,
            sweep: Vec::new(),
            opm_pms_pts: Vec::new(),
            vol_pts: Vec::new(),
            tl_pts: Vec::new(),
            op_dtmul_pts: Vec::new(),
            op_mul_fx: Vec::new(),
            op_dt_fx: Vec::new(),
            op_dt2_fx: Vec::new(),
            op_dt2_pts: Vec::new(),
            vol_tl_base: None,
            no_pan: false,
            cut_before: false,
            c219_noise: false,
            c219_mode: 0,
            quad: None,
            quad_pts: Vec::new(),
            drum: false,
            tone_pitch_pts: Vec::new(),
            tone_wave_pts: Vec::new(),
            tone_wave: -1,
            tone_sample: -1,
            tone_sample_kind: String::new(),
            tone_fds_mod: 0,
            tone_fds_depth: 0,
            tone_fds_speed: 0,
            tone_fds_pos: 0,
            tone_pan_l: 15,
            tone_pan_r: 15,
            tone_period_offset: 0,
            tone_period: 0,
            tone_chip: 0,
            tone_amp_pts: Vec::new(),
            tone_duty_pts: Vec::new(),
            tone_lfsr_pts: Vec::new(),
            tone_duty: 0,
            tone_lfsr: 0,
            x1_freq: 0,
            x1_freq_pts: Vec::new(),
            x1_wave_pts: Vec::new(),
            x1_env_shape_pts: Vec::new(),
            x1_env_period_pts: Vec::new(),
            x1_env_mode_pts: Vec::new(),
            tone_env_shape: 0,
            tone_env_period: 0,
            tone_env_mode: 1,
            tone_noise: false,
            tone_noise_end: 0,
            tone_noise_modes: Vec::new(),
            tone_clock: 0,
            tone_div: 8,
            tone_mod: 0,
            tone_gb_wave: false,
            gb_soft_env: false,
            gb_env_vol: 0,
            gb_env_dir: 0,
            gb_env_len: 0,
            gb_sound_len: 255,
            gb_sound_len_inf: true,
            tone_fx_pts: Vec::new(),
            vol_slide: 0,
            sweep_div: 1.0,
            e5_lo: 0x40,
            e5_hi: 0xBF,
            fm_pitch_pts: Vec::new(),
            pitch_steps: Vec::new(),
            opl_op_tl: Vec::new(),
            op_tl_fx: Vec::new(),
            opll_preset_pts: Vec::new(),
            opll_phase_pts: Vec::new(),
        }
    }
}

/// One SCC/AY/WSG instrument: a starting wave plus per-tick macro values.
#[derive(Clone, Debug, PartialEq)]
pub struct ToneIns {
    pub wave: i32,
    pub pitch: Vec<i32>,
    pub pitch_speed: i32,
    pub pitch_loop: i32,
    pub pitch_delay: i32,
    pub waves: Vec<i32>,
    pub wave_speed: i32,
    pub wave_loop: i32,
    pub vol: Vec<i32>,
    pub vol_speed: i32,
    pub vol_loop: i32,
    pub vol_delay: i32,
    pub noise: Vec<i32>,
    pub noise_speed: i32,
    pub noise_loop: i32,
    pub mode: Vec<i32>,
    pub mode_speed: i32,
    pub mode_loop: i32,
    pub env_shape: Vec<i32>,
    pub env_shape_speed: i32,
    pub env_shape_loop: i32,
    pub env_hw: bool,
    pub env_period: Vec<i32>,
    pub env_period_speed: i32,
    pub env_period_loop: i32,
    pub duty: Vec<i32>,
    pub duty_speed: i32,
    pub duty_loop: i32,
    pub lfsr: Vec<i32>,
    pub lfsr_speed: i32,
    pub lfsr_loop: i32,
    pub panl: Vec<i32>,
    pub panl_speed: i32,
    pub panl_loop: i32,
    pub panl_delay: i32,
    pub panr: Vec<i32>,
    pub panr_speed: i32,
    pub panr_loop: i32,
    pub panr_delay: i32,
    pub env: Vec<i32>,
    pub env_speed: i32,
    pub env_loop: i32,
    pub env_delay: i32,
    pub gb_soft_env: bool,
    pub gb_env_vol: i32,
    pub gb_env_dir: i32,
    pub gb_env_len: i32,
    pub gb_sound_len: i32,
    pub gb_sound_len_inf: bool,
    pub gb_wave: bool,
    pub vsu_mod: i32,
    pub sample: i32,
    pub sample_kind: String,
    pub fds_mod: i32,
    pub fds_mod_depth: i32,
    pub fds_mod_speed: i32,
    pub fds_mod_pos: i32,
}

impl Default for ToneIns {
    fn default() -> Self {
        ToneIns {
            wave: 0,
            pitch: Vec::new(),
            pitch_speed: 1,
            pitch_loop: 255,
            pitch_delay: 0,
            waves: Vec::new(),
            wave_speed: 1,
            wave_loop: 255,
            vol: Vec::new(),
            vol_speed: 1,
            vol_loop: 255,
            vol_delay: 0,
            noise: Vec::new(),
            noise_speed: 1,
            noise_loop: 255,
            mode: Vec::new(),
            mode_speed: 1,
            mode_loop: 255,
            env_shape: Vec::new(),
            env_shape_speed: 1,
            env_shape_loop: 255,
            env_hw: false,
            env_period: Vec::new(),
            env_period_speed: 1,
            env_period_loop: 255,
            duty: Vec::new(),
            duty_speed: 1,
            duty_loop: 255,
            lfsr: Vec::new(),
            lfsr_speed: 1,
            lfsr_loop: 255,
            panl: Vec::new(),
            panl_speed: 1,
            panl_loop: 255,
            panl_delay: 0,
            panr: Vec::new(),
            panr_speed: 1,
            panr_loop: 255,
            panr_delay: 0,
            env: Vec::new(),
            env_speed: 1,
            env_loop: 255,
            env_delay: 0,
            gb_soft_env: false,
            gb_env_vol: 0,
            gb_env_dir: 0,
            gb_env_len: 0,
            gb_sound_len: 255,
            gb_sound_len_inf: true,
            gb_wave: false,
            vsu_mod: 0,
            sample: -1,
            sample_kind: String::new(),
            fds_mod: 0,
            fds_mod_depth: 0,
            fds_mod_speed: 0,
            fds_mod_pos: 0,
        }
    }
}

/// One FM system in module order, for the .fur chip list.
#[derive(Clone, Debug, PartialEq)]
pub struct FmSystem {
    pub kind: String,
    pub chip_id: u8,
    pub fm_channels: i32,
    pub ext: bool,
    pub csm: bool,
    pub drums: bool,
    pub clock: u32,
    /// The YM2413 core asked to run as a Konami VRC7.
    pub vrc7: bool,
}

impl Default for FmSystem {
    fn default() -> Self {
        FmSystem {
            kind: String::new(),
            chip_id: 0,
            fm_channels: 0,
            ext: false,
            csm: false,
            drums: false,
            clock: 0,
            vrc7: false,
        }
    }
}

impl FmSystem {
    /// Module columns this system occupies, its sample engines included.
    pub fn channels(&self) -> i32 {
        match self.kind.as_str() {
            "opl" | "opl2" | "opl3" => {
                crate::opl::channel_count(&self.kind, self.drums)
            }
            "y8950" => crate::opl::channel_count(&self.kind, self.drums) + 1,
            "opl4" => crate::opl::channel_count(&self.kind, self.drums) + 24,
            "opll" => {
                if self.vrc7 {
                    6
                } else if self.drums {
                    crate::ym2413::OPLL_DRUMS_CHANNELS as i32
                } else {
                    crate::ym2413::OPLL_CHANNELS as i32
                }
            }
            "ym2610" | "ym2608" => self.fm_channels + 3 + 6 + 1,
            "ym2203" => self.fm_channels + 3,
            _ => {
                if self.fm_channels != 0 {
                    self.fm_channels
                } else {
                    8
                }
            }
        }
    }
}

/// (system, first module channel) in module order.
pub fn system_channel_ranges(systems: &[FmSystem]) -> Vec<(FmSystem, i32)> {
    let mut out = Vec::new();
    let mut ch = 0;
    for system in systems {
        out.push((system.clone(), ch));
        ch += system.channels();
    }
    out
}

/// Keep a header-declared second chip even when it never sounds.
pub fn dual_ids(ids: &[i32], dual: bool) -> Vec<i32> {
    let mut values: Vec<i32> = ids.to_vec();
    values.sort();
    values.dedup();
    if dual && !values.is_empty() {
        values.push(0);
        values.push(1);
        values.sort();
        values.dedup();
    }
    values
}

/// The analyzer's output: every note event plus the chip inventories the
/// writer and the report need.
#[derive(Clone, Debug, PartialEq)]
pub struct Song {
    pub vgm: crate::vgm::VgmFile,
    pub t0: i64,
    pub loop_sample: Option<i64>,
    pub speed: i32,
    pub hz: f64,
    pub fm_patches: Vec<crate::fm::FMPatch>,
    pub pcm_samples: Vec<crate::pcm::PcmSample>,
    pub events: Vec<NoteEvent>,
    pub warnings: Vec<String>,
    pub oki_samples: Vec<crate::oki::OkiSample>,
    pub msm6258_samples: Vec<crate::oki::AdpcmSample>,
    pub segapcm_samples: Vec<crate::segapcm::SegapcmSample>,
    pub segapcm_ids: Vec<i32>,
    pub rf5c68_samples: Vec<crate::rf5c::Rf5cSample>,
    pub rf5c164_samples: Vec<crate::rf5c::Rf5cSample>,
    pub c140_samples: Vec<crate::c140::C140Sample>,
    pub c219_samples: Vec<crate::c219::C219Sample>,
    pub c219_ids: Vec<i32>,
    pub c219_count: i32,
    pub c352_samples: Vec<crate::c352::C352Sample>,
    pub k053260_samples: Vec<crate::k053260::K053260Sample>,
    pub k054539_samples: Vec<crate::k054539::K054539Sample>,
    pub k054539_ids: Vec<i32>,
    pub k054539_count: i32,
    pub ymz280b_samples: Vec<crate::ymz280b::YmzSample>,
    pub ymz280b_ids: Vec<i32>,
    pub ymz280b_count: i32,
    pub multipcm_samples: Vec<crate::multipcm::MultipcmSample>,
    pub multipcm_ids: Vec<i32>,
    pub multipcm_count: i32,
    pub upd7759_samples: Vec<crate::upd7759::Upd7759Sample>,
    pub upd7759_ids: Vec<i32>,
    pub upd7759_count: i32,
    pub pwm_samples: Vec<crate::pwm::PwmSample>,
    pub pwm_ids: Vec<i32>,
    pub pwm_count: i32,
    pub msm5205_samples: Vec<crate::msm5205::Msm5205Sample>,
    pub msm5205_ids: Vec<i32>,
    pub msm5205_count: i32,
    pub ga20_samples: Vec<crate::ga20::Ga20Sample>,
    pub ga20_ids: Vec<i32>,
    pub ga20_count: i32,
    pub msm5232_ids: Vec<i32>,
    pub msm5232_controls: Vec<i32>,
    pub msm5232_count: i32,
    pub es5503_samples: Vec<crate::es5503::Es5503Sample>,
    pub es5503_ids: Vec<i32>,
    pub es5503_oscs: Vec<(i32, i32)>,
    pub es5503_rate: i32,
    pub es5503_count: i32,
    /// Standalone channel volume changes (MSM5232 group volume).
    pub channel_vol_events: Vec<(i64, i32, i32)>,
    pub mikey_ids: Vec<i32>,
    pub mikey_ins: Vec<ToneIns>,
    pub mikey_samples: Vec<crate::mikey::MikeySample>,
    pub mikey_count: i32,
    pub x1_010_ids: Vec<i32>,
    pub x1_010_ins: Vec<ToneIns>,
    pub x1_010_samples: Vec<crate::x1_010::X1PcmHit>,
    pub x1_010_count: i32,
    pub x1_waves: Vec<Vec<u8>>,
    pub x1_envs: Vec<Vec<u8>>,
    pub fds_ids: Vec<i32>,
    pub fds_ins: Vec<ToneIns>,
    pub fds_waves: Vec<Vec<u8>>,
    pub fds_mod_waves: Vec<Vec<u8>>,
    pub fds_count: i32,
    pub wonderswan_ids: Vec<i32>,
    pub wonderswan_ins: Vec<ToneIns>,
    pub wonderswan_waves: Vec<Vec<u8>>,
    pub wonderswan_pcm_samples: Vec<crate::wonderswan::WsPcmSample>,
    pub wonderswan_count: i32,
    pub wonderswan_pcm_count: i32,
    pub qsound_ids: Vec<i32>,
    pub qsound_ins: Vec<ToneIns>,
    pub qsound_samples: Vec<crate::qsound::QSoundSample>,
    pub qsound_count: i32,
    pub qsound_adpcm_count: i32,
    /// (sample, channel, effect, value) placed before notes.
    pub channel_fx: Vec<(i64, i32, i32, i32)>,
    /// Chip-wide LFO effects: (sample, effect, value, tick offset, channel).
    /// Entries with a zero target are the historical 3-tuples on channel 0,
    /// which is what the Python list's optional components decode to.
    pub chip_fx: Vec<(i64, i32, i32, i32, i32)>,
    pub grid_note: String,
    pub legato_steps: i32,
    pub legato_ramps: i32,
    pub row_subdiv: f64,
    pub hilight_a: i32,
    pub hilight_b: i32,
    pub fm_chip: String,
    pub fm_systems: Vec<FmSystem>,
    pub has_fm_source: bool,
    pub ym2151_ids: Vec<i32>,
    pub ym2203_ids: Vec<i32>,
    pub ym2203_ext: bool,
    pub ym2203_ssg_count: i32,
    pub ym2612_ids: Vec<i32>,
    pub ym2612_ext: bool,
    pub ym2612_csm: bool,
    pub opl_ids: Vec<i32>,
    pub opl_drums: bool,
    pub opll_ids: Vec<i32>,
    pub opll_drums: bool,
    pub opll_vrc7: bool,
    pub fm_vol_max: i32,
    pub fm_notes: i32,
    pub drum_notes: i32,
    pub ym2610_ids: Vec<i32>,
    pub ym2610_b: bool,
    pub ym2610_ssg_count: i32,
    pub ym2610_a_samples: Vec<crate::ym2610::AdpcmASample>,
    pub ym2610_b_samples: Vec<crate::ym2610::AdpcmBSample>,
    pub ym2610_a_count: i32,
    pub ym2610_b_count: i32,
    pub ym2608_ids: Vec<i32>,
    pub ym2608_ext: bool,
    pub ym2608_ssg_count: i32,
    pub ym2608_rhythm_count: i32,
    pub ym2608_rhythm_notes: i32,
    pub ym2608_b_samples: Vec<crate::ym2610::AdpcmBSample>,
    pub ym2608_b_count: i32,
    pub y8950_b_samples: Vec<crate::ym2610::AdpcmBSample>,
    pub y8950_b_count: i32,
    pub opl4_wave_samples: Vec<crate::ymf278b::Opl4WaveSample>,
    pub opl4_wave_count: i32,
    pub k007232_volume: f64,
    pub loop_end_sample: Option<i64>,
    pub loop_start_row: Option<i32>,
    pub loop_end_row: Option<i32>,
    pub loop_marker_sample: Option<i64>,
    pub loop_cut_sample: Option<i64>,
    pub c352_quad: bool,
    pub scc_ids: Vec<i32>,
    pub scc_waves: Vec<Vec<u8>>,
    pub scc_ins: Vec<ToneIns>,
    pub scc_count: i32,
    pub k005289_ids: Vec<i32>,
    pub k005289_waves: Vec<Vec<u8>>,
    pub k005289_ins: Vec<ToneIns>,
    pub k005289_count: i32,
    pub ay_ids: Vec<i32>,
    pub ay_ins: Vec<ToneIns>,
    pub ay_count: i32,
    pub pokey_ids: Vec<i32>,
    pub pokey_ins: Vec<ToneIns>,
    pub pokey_count: i32,
    pub ssg_pcm_samples: Vec<crate::ssgpcm::SsgPcmSample>,
    pub ssg_pcm_count: i32,
    pub nes_ids: Vec<i32>,
    pub nes_present: bool,
    pub nes_ins: Vec<ToneIns>,
    pub nes_dpcm_samples: Vec<crate::nes::NesDpcmSample>,
    pub nes_pcm_samples: Vec<crate::nes::NesPcmSample>,
    pub nes_count: i32,
    pub nes_pcm_count: i32,
    pub huc6280_ids: Vec<i32>,
    pub huc6280_waves: Vec<Vec<u8>>,
    pub huc6280_ins: Vec<ToneIns>,
    pub huc6280_pcm_samples: Vec<crate::huc6280::Huc6280PcmSample>,
    pub huc6280_count: i32,
    pub huc6280_pcm_count: i32,
    pub gb_ids: Vec<i32>,
    pub gb_waves: Vec<Vec<u8>>,
    pub gb_ins: Vec<ToneIns>,
    pub gb_count: i32,
    pub vsu_ids: Vec<i32>,
    pub vsu_waves: Vec<Vec<u8>>,
    pub vsu_mod_waves: Vec<Vec<u8>>,
    pub vsu_ins: Vec<ToneIns>,
    pub vsu_count: i32,
    pub vsu_dynamic: bool,
    pub saa_ids: Vec<i32>,
    pub saa_ins: Vec<ToneIns>,
    pub saa_count: i32,
    pub psg_ids: Vec<i32>,
    pub psg_ins: Vec<ToneIns>,
    pub psg_count: i32,
    pub psg_stereo: bool,
    pub psg_t6w28: bool,
    pub psg_dac_samples: Vec<crate::psg_dac::PsgDacSample>,
    pub psg_dac_plays: Vec<crate::psg_dac::PsgDacPlay>,
    pub psg_dac_voices: Vec<(i32, i32)>,
    pub psg_dac_rate: i32,
    pub psg_dac_count: i32,
    pub psg_dac_stats: indexmap::IndexMap<String, i64>,
    pub dac_samples: Vec<crate::ym2612dac::DacSample>,
    pub dac_count: i32,
    /// `collect_dac` counters: ints plus the `dac_mode` text and the
    /// `dac_pan_points` list, so the Python dict is a typed enum map here.
    pub dac_stats: crate::ym2612dac::DacStats,
}

impl Song {
    pub fn samples_per_row(&self) -> f64 {
        (crate::vgm::VGM_RATE as f64 / self.hz) * self.speed as f64
    }

    /// Quarter-note tempo implied by the measured 16th note.
    pub fn bpm(&self) -> f64 {
        (self.hz / self.speed as f64 / self.row_subdiv.max(1.0)) * 15.0
    }

    /// Nearest row (round half up).
    pub fn row_of(&self, sample: i64) -> i32 {
        let x = (sample - self.t0) as f64 / self.samples_per_row() + 0.5;
        (x.floor() as i32).max(0)
    }

    /// (row, delay_ticks): the closest playable position.
    pub fn place(
        &self,
        sample: i64,
        stats: Option<&mut indexmap::IndexMap<String, i64>>,
    ) -> (i32, i32) {
        crate::timing::place_tick(
            sample,
            self.t0,
            self.samples_per_row(),
            self.speed,
            crate::vgm::VGM_RATE as f64 / self.hz,
            stats,
        )
    }

    /// Ticks after the row start; 0 if on-grid (compat wrapper).
    pub fn delay_ticks(&self, sample: i64, _row: i32) -> i32 {
        self.place(sample, None).1
    }
}
