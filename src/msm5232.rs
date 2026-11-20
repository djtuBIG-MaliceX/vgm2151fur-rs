//! OKI MSM5232 tone/noise register model.
//!
//! The MSM5232 is an eight-voice square-wave/additive tone generator, not a
//! sample player. Registers 0x00-0x07 hold one pitch/gate byte per voice:
//! bit 7 is key-on, and values 0xD8-0xFF select noise. Registers 0x08/0x09 and
//! 0x0A/0x0B set the attack and decay of the two four-channel groups, and
//! 0x0C/0x0D hold the group output enables and sustain bit.
//!
//! VGM command 0x43 writes (register, data); older libvgm builds used 0x2D.
//! The module uses Furnace's native MSM5232 system, so this collector only
//! records notes and group state changes.

use std::collections::HashMap;

use crate::parity;
use crate::vgm::ChipWrite;

pub use crate::vgm::MSM5232_DEFAULT_CLOCK;

pub const MSM5232_VOICES: usize = 8;
pub const MSM5232_NOTE_BASE: i32 = 72;

// Furnace's MSM5232 platform maps the attack/decay effect value through these
// tables before writing the chip (platform/msm5232.cpp).  The tables are not
// the identity: effect 5 is hardware decay 9, for example.  The collector has
// to invert them so the chip receives the source register value.
pub const FURNACE_MSM5232_ATTACK_MAP: [i32; 8] = [0, 1, 2, 3, 4, 5, 5, 5];
pub const FURNACE_MSM5232_DECAY_MAP: [i32; 16] = [
    0, 1, 2, 3, 8, 9, 4, 10, 5, 11, 12, 13, 13, 13, 13, 13,
];
// Inverse of the decay table, keyed by the hardware register value.  Values
// the platform cannot reach (6, 7, 14 and 15) use the nearest hardware rate.
pub const FURNACE_MSM5232_DECAY_FX: [i32; 16] = [
    0, 1, 2, 3, 6, 8, 9, 10, 4, 5, 7, 9, 10, 11, 11, 11,
];

/// Port of the fields of `vgm.py::ChipWrite` that this collector reads.
///
/// The VGM container module is ported separately; keeping the collector over
/// this local trait lets it compile and be tested before `crate::vgm` exists.
/// Once `ChipWrite` lands, implement this trait for it in this module.
pub trait Msm5232Write {
    fn sample_time(&self) -> i64;
    fn chip_id(&self) -> u8;
    fn chip(&self) -> &str;
    fn reg(&self) -> u32;
    fn val(&self) -> u32;
}

impl Msm5232Write for ChipWrite {
    fn sample_time(&self) -> i64 {
        self.sample
    }
    fn chip_id(&self) -> u8 {
        self.chip_id
    }
    fn chip(&self) -> &str {
        self.chip
    }
    fn reg(&self) -> u32 {
        self.reg
    }
    fn val(&self) -> u32 {
        self.val
    }
}

/// Effect value that makes Furnace write hardware attack `register`.
pub fn msm5232_attack_fx(register: i32) -> i32 {
    (register & 0x07).min(5)
}

/// Effect value that makes Furnace write hardware decay `register`.
pub fn msm5232_decay_fx(register: i32) -> i32 {
    FURNACE_MSM5232_DECAY_FX[(register & 0x0F) as usize]
}

/// TA7630 group volume (0-15) as a Furnace 0-127 channel level.
pub fn msm5232_volume_fx(value: i32) -> i32 {
    let mut db = 0.0;
    let mut step = 1.5;
    let inc = 0.125;
    let mut amp = 0.0;
    let value = value.clamp(0, 15);
    for index in 0..16 {
        if 15 - index == value {
            amp = 1.0 / 10f64.powf(db / 20.0);
            break;
        }
        db += step;
        step += inc;
    }
    (parity::py_int(parity::py_round(amp * 127.0)) as i32).clamp(0, 127)
}

#[derive(Clone, Debug, PartialEq)]
pub struct Msm5232Hit {
    pub sample_time: i64,
    pub chip_id: u8,
    pub voice: i32,
    pub pitch: i32,
    pub noise: bool,
    pub volume: i32,
    pub control: i32,
    pub off_sample: i64,
}

impl Default for Msm5232Hit {
    fn default() -> Self {
        Self {
            sample_time: 0,
            chip_id: 0,
            voice: 0,
            pitch: 0,
            noise: false,
            volume: 15,
            control: 0x0F,
            off_sample: 0,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Msm5232Vol {
    pub sample_time: i64,
    pub chip_id: u8,
    pub group: i32,
    pub value: i32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Msm5232Off {
    pub sample_time: i64,
    pub chip_id: u8,
    pub voice: i32,
}

/// A group-level register write, carried as a pattern effect.
#[derive(Clone, Debug, PartialEq)]
pub struct Msm5232Fx {
    pub sample_time: i64,
    pub chip_id: u8,
    pub group: i32,
    pub code: i32,
    pub value: i32,
}

/// Record a group attack/decay change once, mapped for Furnace.
fn env_fx<W: Msm5232Write>(
    fx: &mut Vec<Msm5232Fx>,
    last_env: &mut HashMap<(u8, i32, i32), i32>,
    write: &W,
    group: i32,
    code: i32,
    register: i32,
    mapper: fn(i32) -> i32,
) {
    let key = (write.chip_id(), group, code);
    if last_env.get(&key) == Some(&register) {
        return;
    }
    last_env.insert(key, register);
    fx.push(Msm5232Fx {
        sample_time: write.sample_time(),
        chip_id: write.chip_id(),
        group,
        code,
        value: mapper(register),
    });
}

/// Collect MSM5232 key-on/key-off, group and volume writes.
pub fn collect_msm5232<W: Msm5232Write>(
    writes: &[W],
) -> (
    Vec<Msm5232Hit>,
    Vec<Msm5232Off>,
    Vec<Msm5232Fx>,
    Vec<Msm5232Vol>,
    Vec<String>,
) {
    let mut hits: Vec<Msm5232Hit> = Vec::new();
    let mut offs: Vec<Msm5232Off> = Vec::new();
    let mut fx: Vec<Msm5232Fx> = Vec::new();
    let mut vols: Vec<Msm5232Vol> = Vec::new();
    let mut warnings: Vec<String> = Vec::new();
    let mut keyed = [false; MSM5232_VOICES];
    // Python keeps references to the emitted hits and mutates `off_sample`
    // through them; indices into `hits` are the exact equivalent.
    let mut active: [Option<usize>; MSM5232_VOICES] = [None; MSM5232_VOICES];
    // libvgm starts both TA7630 group volumes at maximum; the group control
    // output-enable mask resets to all four parts on.
    let mut ext_vol: HashMap<u8, [i32; 2]> = HashMap::new();
    let mut group_ctrl: HashMap<u8, [i32; 2]> = HashMap::new();
    // Repeated group writes with the same value do nothing on the chip.  Keep
    // the last source register per group so 60 Hz refresh writes don't flood
    // the pattern with identical 12xx/13xx effects.
    let mut last_env: HashMap<(u8, i32, i32), i32> = HashMap::new();
    for w in writes {
        if w.chip() != "msm5232" {
            continue;
        }
        let reg = (w.reg() & 0x7F) as i32;
        let val = (w.val() & 0xFF) as i32;
        let group_vol = ext_vol.entry(w.chip_id()).or_insert([15, 15]);
        let ctrl = group_ctrl.entry(w.chip_id()).or_insert([0x0F, 0x0F]);
        if reg < 0x08 {
            let voice = reg as usize;
            if val & 0x80 != 0 {
                if let Some(index) = active[voice] {
                    hits[index].off_sample = w.sample_time();
                }
                let pitch = val & 0x7F;
                let hit = Msm5232Hit {
                    sample_time: w.sample_time(),
                    chip_id: w.chip_id(),
                    voice: voice as i32,
                    pitch,
                    noise: pitch >= 0x58,
                    volume: group_vol[voice >> 2],
                    control: ctrl[voice >> 2] & 0x1F,
                    off_sample: 0,
                };
                hits.push(hit);
                active[voice] = Some(hits.len() - 1);
                keyed[voice] = true;
            } else {
                if keyed[voice] {
                    offs.push(Msm5232Off {
                        sample_time: w.sample_time(),
                        chip_id: w.chip_id(),
                        voice: voice as i32,
                    });
                    if let Some(index) = active[voice] {
                        hits[index].off_sample = w.sample_time();
                        active[voice] = None;
                    }
                }
                keyed[voice] = false;
            }
            continue;
        }
        if reg == 0x08 {
            env_fx(
                &mut fx,
                &mut last_env,
                w,
                0,
                0x12,
                val & 0x07,
                msm5232_attack_fx,
            );
        } else if reg == 0x09 {
            env_fx(
                &mut fx,
                &mut last_env,
                w,
                1,
                0x12,
                val & 0x07,
                msm5232_attack_fx,
            );
        } else if reg == 0x0A {
            env_fx(
                &mut fx,
                &mut last_env,
                w,
                0,
                0x13,
                val & 0x0F,
                msm5232_decay_fx,
            );
        } else if reg == 0x0B {
            env_fx(
                &mut fx,
                &mut last_env,
                w,
                1,
                0x13,
                val & 0x0F,
                msm5232_decay_fx,
            );
        } else if reg == 0x0C {
            ctrl[0] = val & 0x1F;
        } else if reg == 0x0D {
            ctrl[1] = val & 0x1F;
        } else if reg == 0x1E {
            let value = val & 0x0F;
            if group_vol[0] != value {
                group_vol[0] = value;
                vols.push(Msm5232Vol {
                    sample_time: w.sample_time(),
                    chip_id: w.chip_id(),
                    group: 0,
                    value,
                });
            }
        } else if reg == 0x1F {
            let value = val & 0x0F;
            if group_vol[1] != value {
                group_vol[1] = value;
                vols.push(Msm5232Vol {
                    sample_time: w.sample_time(),
                    chip_id: w.chip_id(),
                    group: 1,
                    value,
                });
            }
        }
    }
    if hits.is_empty() && !fx.is_empty() {
        warnings.push("MSM5232 group writes but no key-on; the voices stay silent".to_string());
    }
    (hits, offs, fx, vols, warnings)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Clone, Debug)]
    struct Write {
        sample_time: i64,
        chip_id: u8,
        chip: &'static str,
        reg: u32,
        val: u32,
    }

    impl Msm5232Write for Write {
        fn sample_time(&self) -> i64 {
            self.sample_time
        }
        fn chip_id(&self) -> u8 {
            self.chip_id
        }
        fn chip(&self) -> &str {
            self.chip
        }
        fn reg(&self) -> u32 {
            self.reg
        }
        fn val(&self) -> u32 {
            self.val
        }
    }

    fn write(sample_time: i64, chip_id: u8, reg: u32, val: u32) -> Write {
        Write {
            sample_time,
            chip_id,
            chip: "msm5232",
            reg,
            val,
        }
    }

    #[test]
    fn decodes_key_on_voice_register() {
        // Voice 3, key-on bit set, pitch 0x2A (below the noise threshold).
        let (hits, offs, fx, vols, warnings) =
            collect_msm5232(&[write(100, 0, 3, 0xAA)]);
        assert_eq!(
            hits,
            vec![Msm5232Hit {
                sample_time: 100,
                chip_id: 0,
                voice: 3,
                pitch: 0x2A,
                noise: false,
                volume: 15,
                control: 0x0F,
                off_sample: 0,
            }]
        );
        assert!(offs.is_empty());
        assert!(fx.is_empty());
        assert!(vols.is_empty());
        assert!(warnings.is_empty());
    }

    #[test]
    fn uses_clock_default_and_note_base() {
        assert_eq!(MSM5232_DEFAULT_CLOCK, 2_119_040);
        assert_eq!(MSM5232_VOICES, 8);
        assert_eq!(MSM5232_NOTE_BASE, 72);
    }

    #[test]
    fn maps_pitch_noise_and_volume_helpers() {
        // 0x58 is the first noise value, 0x57 the last pitched one.
        let mut recs = vec![write(1, 0, 0, 0xD7), write(2, 0, 0, 0xD8)];
        recs.push(write(3, 0, 1, 0x57)); // key-off, ignored for noise
        let (hits, _, _, _, _) = collect_msm5232(&recs);
        assert!(!hits[0].noise);
        assert_eq!(hits[0].pitch, 0x57);
        assert!(hits[1].noise);
        assert_eq!(hits[1].pitch, 0x58);

        assert_eq!(msm5232_attack_fx(7), 5);
        assert_eq!(msm5232_decay_fx(15), 11);
        assert_eq!(msm5232_volume_fx(0), 2);
        assert_eq!(msm5232_volume_fx(14), 107);
        assert_eq!(msm5232_volume_fx(15), 127);
        assert_eq!(msm5232_volume_fx(99), 127);
        assert_eq!(msm5232_volume_fx(-4), 2);
    }

    #[test]
    fn handles_key_off_group_writes_boundaries() {
        let recs = [
            write(10, 1, 7, 0x90),  // key-on voice 7
            write(20, 1, 0x0D, 0x05), // group 1 control
            write(30, 1, 7, 0x91),  // retrigger: previous hit off_sample 30
            write(40, 1, 7, 0x00),  // key-off
            write(50, 1, 7, 0x00),  // already off: no second off
            write(60, 1, 0x1F, 0x02), // group 1 volume
            // Register byte 0x8B masks to 0x0B (group 1 decay), value 0xFF
            // masks to hardware decay 15 -> effect 11; repeat is deduped.
            write(65, 1, 0x8B, 0xFF),
            write(70, 1, 0x8B, 0xFF),
            // A non-MSM5232 write is ignored entirely.
            Write {
                sample_time: 80,
                chip_id: 1,
                chip: "ym2151",
                reg: 0x1E,
                val: 0x01,
            },
        ];
        let (hits, offs, fx, vols, warnings) = collect_msm5232(&recs);
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].off_sample, 30);
        assert_eq!(hits[1].off_sample, 40);
        assert_eq!(hits[1].control, 0x05);
        assert_eq!(
            offs,
            vec![Msm5232Off {
                sample_time: 40,
                chip_id: 1,
                voice: 7,
            }]
        );
        assert_eq!(
            fx,
            vec![Msm5232Fx {
                sample_time: 65,
                chip_id: 1,
                group: 1,
                code: 0x13,
                value: 11,
            }]
        );
        assert_eq!(
            vols,
            vec![Msm5232Vol {
                sample_time: 60,
                chip_id: 1,
                group: 1,
                value: 2,
            }]
        );
        assert!(warnings.is_empty());
    }
}
