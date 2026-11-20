//! Stable line dump of a parsed VGM file, matching `tools/dump_vgm.py`.
//!
//! The two dumps must be byte-identical: it is the Stage 1 gate for the
//! container port.

use std::io::Write;
use std::path::Path;

use vgm2fur::error::Error;
use vgm2fur::parity::fmt_float_repr;
use vgm2fur::vgm::load_vgm;

macro_rules! hdr_int {
    ($out:expr, $v:expr, $name:ident) => {
        $out.push(format!("hdr {} {}", stringify!($name), $v.$name));
    };
}
macro_rules! hdr_bool {
    ($out:expr, $v:expr, $name:ident) => {
        $out.push(format!("hdr {} {}", stringify!($name), $v.$name as u8));
    };
}
macro_rules! hdr_opt {
    ($out:expr, $v:expr, $name:ident) => {
        $out.push(format!(
            "hdr {} {}",
            stringify!($name),
            match $v.$name {
                Some(x) => x.to_string(),
                None => "None".to_string(),
            }
        ));
    };
}
macro_rules! hdr_float {
    ($out:expr, $v:expr, $name:ident) => {
        $out.push(format!("hdr {} {}", stringify!($name), fmt_float_repr($v.$name)));
    };
}

/// CRC-32 (IEEE), the same value as Python's `zlib.crc32`.
fn crc32(data: &[u8]) -> u32 {
    let mut crc: u32 = 0xFFFF_FFFF;
    for &byte in data {
        crc ^= byte as u32;
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
        }
    }
    !crc
}

fn tag(data: &[u8]) -> String {
    format!("{:08x}", crc32(data))
}

fn dump(path: &Path) -> Result<Vec<String>, Error> {
    let v = load_vgm(path)?;
    let mut out: Vec<String> = Vec::new();
    out.push(format!(
        "path {}",
        v.path
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default()
    ));
    hdr_int!(out, v, ay8910_clock);
    hdr_bool!(out, v, ay8910_dual);
    hdr_int!(out, v, ay8910_flags);
    hdr_int!(out, v, ay8910_type);
    hdr_int!(out, v, c140_clock);
    hdr_bool!(out, v, c140_dual);
    hdr_int!(out, v, c140_type);
    hdr_int!(out, v, c352_clock);
    hdr_int!(out, v, c352_divider);
    hdr_bool!(out, v, c352_mute_rear);
    hdr_int!(out, v, es5503_channels);
    hdr_int!(out, v, es5503_clock);
    hdr_int!(out, v, ga20_clock);
    hdr_bool!(out, v, ga20_dual);
    hdr_int!(out, v, gb_dmg_clock);
    hdr_bool!(out, v, gb_dmg_dual);
    hdr_int!(out, v, huc6280_clock);
    hdr_bool!(out, v, huc6280_dual);
    hdr_int!(out, v, k005289_clock);
    hdr_int!(out, v, k007232_clock);
    hdr_bool!(out, v, k007232_dual);
    hdr_int!(out, v, k051649_clock);
    hdr_bool!(out, v, k051649_dual);
    hdr_bool!(out, v, k051649_plus);
    hdr_int!(out, v, k053260_clock);
    hdr_bool!(out, v, k053260_dual);
    hdr_int!(out, v, k054539_clock);
    hdr_int!(out, v, k054539_clock_2);
    hdr_bool!(out, v, k054539_dual);
    hdr_int!(out, v, k054539_flags);
    hdr_opt!(out, v, loop_file_pos);
    hdr_opt!(out, v, loop_sample);
    hdr_int!(out, v, loop_samples);
    hdr_float!(out, v, main_volume);
    hdr_int!(out, v, mikey_clock);
    hdr_bool!(out, v, mikey_dual);
    hdr_int!(out, v, msm5205_clock);
    hdr_bool!(out, v, msm5205_dual);
    hdr_int!(out, v, msm5205_flags);
    hdr_int!(out, v, msm5232_clock);
    hdr_bool!(out, v, msm5232_dual);
    hdr_int!(out, v, msm6258_clock);
    hdr_bool!(out, v, msm6258_dual);
    hdr_int!(out, v, msm6258_flags);
    hdr_int!(out, v, msm6295_clock);
    hdr_bool!(out, v, msm6295_dual);
    hdr_bool!(out, v, msm6295_pin7);
    hdr_int!(out, v, multipcm_clock);
    hdr_int!(out, v, multipcm_clock_2);
    hdr_bool!(out, v, multipcm_dual);
    hdr_int!(out, v, nes_clock);
    hdr_bool!(out, v, nes_dual);
    hdr_bool!(out, v, nes_fds);
    hdr_int!(out, v, opl3_clock);
    hdr_bool!(out, v, opl3_dual);
    hdr_int!(out, v, opl2_clock);
    hdr_bool!(out, v, opl2_dual);
    hdr_bool!(out, v, opl2_pan);
    hdr_int!(out, v, opl_clock);
    hdr_bool!(out, v, opl_dual);
    hdr_int!(out, v, ym2413_clock);
    hdr_bool!(out, v, ym2413_dual);
    hdr_bool!(out, v, ym2413_vrc7);
    hdr_int!(out, v, pokey_clock);
    hdr_int!(out, v, pokey_clock_2);
    hdr_bool!(out, v, pokey_dual);
    hdr_int!(out, v, pwm_clock);
    hdr_int!(out, v, pwm_clock_2);
    hdr_bool!(out, v, pwm_dual);
    hdr_int!(out, v, qsound_clock);
    hdr_int!(out, v, rf5c164_clock);
    hdr_bool!(out, v, rf5c164_dual);
    hdr_int!(out, v, rf5c68_clock);
    hdr_bool!(out, v, rf5c68_dual);
    hdr_int!(out, v, saa1099_clock);
    hdr_bool!(out, v, saa1099_dual);
    hdr_int!(out, v, segapcm_bankmask);
    hdr_int!(out, v, segapcm_bankshift);
    hdr_int!(out, v, segapcm_clock);
    hdr_bool!(out, v, segapcm_dual);
    hdr_int!(out, v, sn76489_clock);
    hdr_int!(out, v, sn76489_clock_2);
    hdr_bool!(out, v, sn76489_dual);
    hdr_bool!(out, v, sn76489_stereo);
    hdr_bool!(out, v, sn76489_t6w28);
    hdr_int!(out, v, stream_setups);
    hdr_int!(out, v, total_samples);
    hdr_int!(out, v, upd7759_clock);
    hdr_int!(out, v, upd7759_clock_2);
    hdr_bool!(out, v, upd7759_dual);
    hdr_bool!(out, v, upd7759_slave);
    hdr_int!(out, v, version);
    hdr_int!(out, v, vsu_clock);
    hdr_int!(out, v, vsu_clock_2);
    hdr_bool!(out, v, vsu_dual);
    hdr_int!(out, v, wonderswan_clock);
    hdr_bool!(out, v, wonderswan_dual);
    hdr_int!(out, v, x1_010_clock);
    hdr_bool!(out, v, x1_010_dual);
    hdr_int!(out, v, y8950_clock);
    hdr_bool!(out, v, y8950_dual);
    hdr_int!(out, v, ym2151_clock);
    hdr_bool!(out, v, ym2151_dual);
    hdr_int!(out, v, ym2203_clock);
    hdr_bool!(out, v, ym2203_dual);
    hdr_int!(out, v, ym2413_clock);
    hdr_bool!(out, v, ym2413_dual);
    hdr_bool!(out, v, ym2413_vrc7);
    hdr_int!(out, v, ym2608_clock);
    hdr_bool!(out, v, ym2608_dual);
    hdr_bool!(out, v, ym2610_b);
    hdr_int!(out, v, ym2610_clock);
    hdr_bool!(out, v, ym2610_dual);
    hdr_int!(out, v, ym2612_clock);
    hdr_bool!(out, v, ym2612_dual);
    hdr_int!(out, v, ymf278b_clock);
    hdr_bool!(out, v, ymf278b_dual);
    hdr_int!(out, v, ymz280b_clock);
    hdr_int!(out, v, ymz280b_clock_2);
    hdr_bool!(out, v, ymz280b_dual);
    let mut extra_clocks: Vec<_> = v.extra_clocks.iter().collect();
    extra_clocks.sort_by_key(|(k, _)| **k);
    for (chip, clock) in extra_clocks {
        out.push(format!("extraclk {} {}", chip, clock));
    }
    let mut extra_volumes: Vec<_> = v.extra_volumes.iter().collect();
    extra_volumes.sort_by_key(|(k, _)| **k);
    for (chip, entry) in extra_volumes {
        out.push(format!("extravol {} {} {}", chip, entry.0, entry.1));
    }
    for (key, value) in v.gd3.iter() {
        out.push(format!("gd3 {} {}", key, value));
    }
    for warning in v.warnings.iter() {
        out.push(format!("warn {}", warning));
    }
    for w in v.writes.iter() {
        out.push(format!(
            "write {} {} {} {} {}",
            w.sample, w.chip, w.chip_id, w.reg, w.val
        ));
    }
    for r in v.roms.iter() {
        out.push(format!(
            "rom {} {} {} {} {} {} {}",
            r.chip,
            r.kind,
            r.rom_size,
            r.start,
            r.data.len(),
            r.time,
            tag(&r.data)
        ));
    }
    for (sample, kind, value) in v.dac_events.iter() {
        out.push(format!("dacev {} {} {}", sample, kind, value));
    }
    for play in v.dac_stream_plays.iter() {
        out.push(format!(
            "stream {} {} {} {} {} {} {} {} {} {} {} {} {} {} {} {} {}",
            play.sample,
            play.chip_type,
            play.chip_id,
            play.stream,
            play.port,
            play.reg,
            play.offset,
            play.length,
            play.mode,
            play.rate,
            play.loop_ as u8,
            play.reverse as u8,
            play.step,
            play.step_base,
            play.bank,
            play.block,
            play.stop_sample
        ));
    }
    for play in v.msm6258_plays.iter() {
        out.push(format!(
            "msm6258 {} {} {} {} {} {} {} {} {} {} {} {}",
            play.sample,
            play.chip_id,
            play.block,
            play.rate,
            play.loop_ as u8,
            play.reverse as u8,
            play.offset,
            play.nbytes,
            play.end,
            play.step,
            play.step_base,
            play.length_mode
        ));
    }
    for w in v.pcm_ram_writes.iter() {
        out.push(format!(
            "pcmram {} {} {} {} {} {}",
            w.sample, w.type_, w.chip_id, w.src, w.dest, w.length
        ));
    }
    for d in v.dac_blocks.iter() {
        out.push(format!("dacblock {} {}", d.len(), tag(d)));
    }
    for d in v.adpcm_blocks.iter() {
        out.push(format!("adpcmblock {} {}", d.len(), tag(d)));
    }
    out.push(format!("dacdata {} {}", v.dac_data.len(), tag(&v.dac_data)));
    Ok(out)
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        eprintln!("usage: parity_dump FILE [FILE...]");
        std::process::exit(2);
    }
    let stdout = std::io::stdout();
    let mut lock = stdout.lock();
    for name in args {
        match dump(Path::new(&name)) {
            Ok(lines) => {
                for line in lines {
                    let _ = writeln!(lock, "{}", line);
                }
            }
            Err(err) => {
                let _ = writeln!(lock, "error {}", err);
            }
        }
    }
}
