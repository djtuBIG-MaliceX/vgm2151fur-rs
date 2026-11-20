//! Ricoh RF5C68 / RF5C164 sample RAM → 8-bit signed samples → Furnace.
//!
//! Both chips share one register file (`$B0`/`$B1` register writes, `$C1`/`$C2`
//! memory writes, 16-bit LE RAM offsets).  Furnace hosts them as a single
//! system (`0x95`) with a `chipType` flag: 0 is the RF5C68 (FM Towns, 8 MHz)
//! and 1 the RF5C164 (Sega CD, 12.5 MHz).
//!
//! Register model (MAME rf5c68.cpp, which Furnace's core copies):
//!
//! ```text
//! 0x00 envelope     0x01 pan (low nibble left, high nibble right)
//! 0x02/0x03 step    0x04/0x05 loop address
//! 0x06 start (8 bits, units of 256 bytes)
//! 0x07 control: bit 7 chip enable, bit 6 select channel (bits 2-0) else
//!      select the 4 KB write bank (bits 3-0 → address bits 12-15)
//! 0x08 channel enable bitmap: a clear bit keys the voice, a set bit disables
//!      it and rewinds it to the start address
//! ```
//!
//! A voice addresses sample RAM as `start << 8`; it advances `step` per output
//! sample and reads `(addr >> 11) & 0xFFFF`.  A read of 0xFF is the end
//! marker: the voice jumps to `loop` and keeps playing if that byte is real
//! audio, or stops when the loop byte is also 0xFF.  The stored byte is
//! sign-magnitude (bit 7 = sign, bits 0-6 = magnitude), silence is 0x80.
//!
//! The playback rate is `clock * step / 786432` bytes/second.  Furnace's
//! `CHIP_FREQBASE` for the core is the same 786432, so a note's C-4 rate is the
//! rate of the sample's usual step and the note/E5 pair is the ratio to it.
//!
//! The `$C1`/`$C2` memory writes and the `0xC0`/`0xC1` RAM blocks are folded
//! into one flat 64 KB buffer here; playback only addresses the low 64 KB
//! window.

use indexmap::IndexMap;

use crate::parity;
use crate::vgm::{ChipWrite, VgmFile};

pub const RF5C_VOICES: usize = 8;
pub const RF5C_RAM_SIZE: usize = 0x10000; // one 64 KB window, the width the core reads
pub const RF5C_END_MARK: u8 = 0xFF; // end-of-sample byte
pub const RF5C_RATE_DIV: f64 = 786432.0; // Furnace CHIP_FREQBASE; 384 * 2048
pub const RF5C68_DEFAULT_CLOCK: u32 = 8_000_000;
pub const RF5C164_DEFAULT_CLOCK: u32 = 12_500_000;
// A driver retriggers by disabling the voice, writing start/loop, then
// enabling it in the same burst. A disable that is followed by an enable this
// soon is that burst, not a real note-off.
pub const RF5C_RETRIGGER: i64 = 64;

/// Bytes per second for a 16-bit frequency step.
pub fn rf5c_rate(clock: u32, step: i32) -> f64 {
    let default = RF5C68_DEFAULT_CLOCK;
    (if clock == 0 { default } else { clock }) as f64 * (step & 0xFFFF) as f64
        / RF5C_RATE_DIV
}

#[derive(Clone, Debug, PartialEq)]
pub struct Rf5cHit {
    pub sample_time: i64,
    pub voice: i32,
    pub start: i32, // byte offset in sample RAM
    pub r#loop: i32, // byte offset in sample RAM
    pub step: i32,  // 16-bit frequency step
    pub env: i32,   // 8-bit envelope
    pub pan: i32,   // low nibble left, high nibble right
    pub chip_id: i32,
    // Step-register writes while the voice stays enabled. The chip uses the
    // new step from that sample on, so a microsample at a fixed loop address
    // is also how these drivers play a pitch: the key-on step is the note and
    // these are its bends/slides. (sample, 16-bit step)
    pub step_pts: Vec<(i64, i32)>,
}

impl Default for Rf5cHit {
    fn default() -> Self {
        Self {
            sample_time: 0,
            voice: 0,
            start: 0,
            r#loop: 0,
            step: 0,
            env: 0,
            pan: 0,
            chip_id: 0,
            step_pts: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Rf5cOff {
    pub sample_time: i64,
    pub voice: i32,
    pub chip_id: i32,
}

impl Default for Rf5cOff {
    fn default() -> Self {
        Self {
            sample_time: 0,
            voice: 0,
            chip_id: 0,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Rf5cSample {
    pub start: i32,
    pub data: Vec<u8>, // signed 8-bit
    pub loop_start: Option<i32>,
    pub loop_end: Option<i32>,
    pub hits: Vec<Rf5cHit>,
    pub chip_id: i32,
}

impl Default for Rf5cSample {
    fn default() -> Self {
        Self {
            start: 0,
            data: Vec::new(),
            loop_start: None,
            loop_end: None,
            hits: Vec::new(),
            chip_id: 0,
        }
    }
}

impl Rf5cSample {
    pub fn length(&self) -> usize {
        self.data.len()
    }

    /// Most common nonzero key-on step (0 when unknown).
    pub fn mode_step(&self) -> i32 {
        // Counter.most_common(1) keeps the first key seen on a tie.
        let mut steps: IndexMap<i32, i32> = IndexMap::new();
        for hit in &self.hits {
            if hit.step != 0 {
                *steps.entry(hit.step).or_insert(0) += 1;
            }
        }
        let mut best: Option<(i32, i32)> = None;
        for (&step, &count) in &steps {
            if best.is_none_or(|(_, best_count)| count > best_count) {
                best = Some((step, count));
            }
        }
        best.map(|(step, _)| step).unwrap_or(0)
    }

    pub fn c4_rate(&self, clock: u32) -> i32 {
        std::cmp::max(
            1000,
            parity::py_int(parity::py_round(rf5c_rate(clock, self.mode_step()))) as i32,
        )
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
struct _Voice {
    env: i32,
    pan: i32,
    step: i32,
    r#loop: i32,
    start: i32,
    enabled: bool,
}

/// Core byte → signed 8-bit. Bit 7 is the sign, bits 0-6 the magnitude.
fn sign_magnitude(val: i32) -> i32 {
    if val & 0x80 != 0 {
        val & 0x7F
    } else {
        -(val & 0x7F)
    }
}

fn apply_reg(voice: &mut _Voice, reg: u8, val: u8) {
    let val = val as i32;
    match reg {
        0x00 => voice.env = val & 0xFF,
        0x01 => voice.pan = val & 0xFF,
        0x02 => voice.step = (voice.step & 0xFF00) | (val & 0xFF),
        0x03 => voice.step = (voice.step & 0x00FF) | ((val & 0xFF) << 8),
        0x04 => voice.r#loop = (voice.r#loop & 0xFF00) | (val & 0xFF),
        0x05 => voice.r#loop = (voice.r#loop & 0x00FF) | ((val & 0xFF) << 8),
        0x06 => voice.start = val & 0xFF,
        _ => {}
    }
}

/// Reconstruct a chip's sample RAM from its blocks and memory writes.
pub fn build_rf5c_ram(vgm: &VgmFile, chip: &str, chip_id: i32) -> Vec<u8> {
    let mut ram = vec![0u8; RF5C_RAM_SIZE];
    for slice in &vgm.roms {
        if slice.kind != chip || slice.chip as i32 != chip_id {
            continue;
        }
        let start = (slice.start as i64 & 0xFFFF) as usize;
        let end = std::cmp::min(start + slice.data.len(), RF5C_RAM_SIZE);
        if end > start {
            ram[start..end].copy_from_slice(&slice.data[..end - start]);
        }
    }
    let mem_chip = format!("{chip}_mem");
    for write in &vgm.writes {
        if write.chip != mem_chip || write.chip_id as i32 != chip_id {
            continue;
        }
        let addr = (write.reg as i64 & 0xFFFF) as usize;
        ram[addr] = (write.val as i64 & 0xFF) as u8;
    }
    ram
}

/// First end-marker byte at or after start (or the buffer end).
fn scan_end(ram: &[u8], start: usize) -> usize {
    let mut end = start;
    let n = ram.len();
    while end < n && ram[end] != RF5C_END_MARK {
        end += 1;
    }
    end
}

fn flush_off(
    offs: &mut Vec<Rf5cOff>,
    pending_off: &mut [Option<i64>],
    i: usize,
    chip_id: i32,
) {
    if let Some(time) = pending_off[i] {
        offs.push(Rf5cOff {
            sample_time: time,
            voice: i as i32,
            chip_id,
        });
        pending_off[i] = None;
    }
}

/// Read one RF5C chip's notes and the samples they play.
///
/// `chip` is "rf5c68" or "rf5c164"; they share the register model and differ
/// only in clock, RAM contents and which write commands feed them.
pub fn collect_rf5c(
    vgm: &VgmFile,
    chip: &str,
    clock: Option<u32>,
    chip_id: i32,
) -> (Vec<u8>, Vec<Rf5cSample>, Vec<Rf5cOff>, Vec<String>) {
    let mut warnings: Vec<String> = Vec::new();
    let ram = build_rf5c_ram(vgm, chip, chip_id);
    let writes: Vec<&ChipWrite> = vgm
        .writes
        .iter()
        .filter(|w| w.chip == chip && w.chip_id as i32 == chip_id)
        .collect();
    if writes.is_empty() {
        return (ram, Vec::new(), Vec::new(), warnings);
    }
    let clock = match clock {
        Some(clock) => clock,
        None => {
            if chip == "rf5c68" {
                if vgm.rf5c68_clock != 0 {
                    vgm.rf5c68_clock
                } else {
                    RF5C68_DEFAULT_CLOCK
                }
            } else if vgm.rf5c164_clock != 0 {
                vgm.rf5c164_clock
            } else {
                RF5C164_DEFAULT_CLOCK
            }
        }
    };
    // The reference computes the clock even though the scan below does not
    // read it; callers use it for rate conversion.
    let _ = clock;

    let mut voices = vec![_Voice::default(); RF5C_VOICES];
    let mut current: usize = 0;
    let mut hits: Vec<Rf5cHit> = Vec::new();
    let mut offs: Vec<Rf5cOff> = Vec::new();
    // A disable is held here until the next event on the voice proves whether
    // it was a real note-off or the first half of a retrigger burst.
    let mut pending_off: Vec<Option<i64>> = vec![None; RF5C_VOICES];
    // The hit currently sounding on each voice, so later step writes land on
    // it. A disable clears it: any writes before the next enable are that
    // note's setup, not a bend on the old one.
    let mut active_hit: Vec<Option<usize>> = vec![None; RF5C_VOICES];

    for write in &writes {
        let reg = (write.reg as i64 & 0xFF) as u8;
        let val = (write.val as i64 & 0xFF) as u8;
        let sample_time = write.sample as i64;
        if reg == 0x07 {
            if val & 0x40 != 0 {
                current = (val & 0x07) as usize;
            }
            // A clear bit 6 selects the RAM write bank, already folded into
            // the memory-write addresses by vgm.py.
            continue;
        }
        if reg == 0x08 {
            // The enable register is a whole-chip bitmap. A clear bit keys a
            // voice; a set bit disables and rewinds it.
            let was: Vec<bool> = voices.iter().map(|v| v.enabled).collect();
            for i in 0..RF5C_VOICES {
                voices[i].enabled = (val >> i) & 1 == 0;
            }
            for i in 0..RF5C_VOICES {
                if voices[i].enabled && !was[i] {
                    if pending_off[i].is_some_and(|time| sample_time - time <= RF5C_RETRIGGER) {
                        // Retrigger: the disable belonged to this same note.
                        pending_off[i] = None;
                    } else {
                        flush_off(&mut offs, &mut pending_off, i, chip_id);
                    }
                    let voice = &voices[i];
                    let hit = Rf5cHit {
                        sample_time,
                        voice: i as i32,
                        start: (voice.start << 8) & 0xFFFF,
                        r#loop: voice.r#loop & 0xFFFF,
                        step: voice.step & 0xFFFF,
                        env: voice.env & 0xFF,
                        pan: voice.pan & 0xFF,
                        chip_id,
                        step_pts: Vec::new(),
                    };
                    hits.push(hit);
                    active_hit[i] = Some(hits.len() - 1);
                } else if was[i] && !voices[i].enabled {
                    flush_off(&mut offs, &mut pending_off, i, chip_id);
                    pending_off[i] = Some(sample_time);
                    active_hit[i] = None;
                }
            }
            continue;
        }
        if reg <= 0x06 {
            let voice = &mut voices[current];
            apply_reg(voice, reg, val);
            if reg == 0x02 || reg == 0x03 {
                let enabled = voice.enabled;
                let step = voice.step & 0xFFFF;
                if let Some(idx) = active_hit[current] {
                    if enabled {
                        let hit = &mut hits[idx];
                        if sample_time - hit.sample_time <= RF5C_RETRIGGER {
                            // A pitch write right after the key is the note's
                            // own step (drivers often set it after the enable).
                            if hit.step_pts.is_empty() {
                                hit.step = step;
                            }
                        } else {
                            let last = match hit.step_pts.last() {
                                Some(&(_, step)) => step,
                                None => hit.step,
                            };
                            if step != last {
                                hit.step_pts.push((sample_time, step));
                            }
                        }
                    }
                }
            }
        }
    }

    for i in 0..RF5C_VOICES {
        flush_off(&mut offs, &mut pending_off, i, chip_id);
    }

    if hits.is_empty() {
        return (ram, Vec::new(), offs, warnings);
    }

    let mut by_key: IndexMap<(i32, Option<i32>), Rf5cSample> = IndexMap::new();
    for hit in &hits {
        let start = hit.start;
        if start >= RF5C_RAM_SIZE as i32 {
            continue;
        }
        let start = start as usize;
        let end = scan_end(&ram, start);
        if end <= start + 2 {
            continue;
        }
        let mut loop_off: Option<i32> = None;
        if start < hit.r#loop as usize && (hit.r#loop as usize) < end {
            // The chip has no loop-disable bit. Drivers that want a one-shot
            // point the loop at the trailing silence just before the end
            // marker; that is a hold, not a musical loop.
            let candidate = hit.r#loop - start as i32;
            if candidate < (end - start) as i32 - 2 {
                loop_off = Some(candidate);
            }
        }
        let key = (hit.start, loop_off);
        let sample = by_key.entry(key).or_insert_with(|| {
            let body: Vec<u8> = ram[start..end]
                .iter()
                .map(|&b| (sign_magnitude(b as i32) & 0xFF) as u8)
                .collect();
            let loop_end = loop_off.map(|_| (end - start) as i32);
            Rf5cSample {
                start: hit.start,
                data: body,
                loop_start: loop_off,
                loop_end,
                hits: Vec::new(),
                chip_id,
            }
        });
        sample.hits.push(hit.clone());
    }

    if !by_key.values().any(|s| s.length() >= 8) {
        // Notes exist but the RAM behind them is empty: a rip that lost its
        // sample block. Say so instead of writing silent channels.
        warnings.push(format!(
            "{} notes but no sample RAM data; the PCM channels will be silent until samples are added",
            chip.to_uppercase()
        ));
        return (ram, Vec::new(), offs, warnings);
    }

    let mut samples: Vec<Rf5cSample> = by_key.into_values().collect();
    samples.sort_by_key(|s| {
        (
            s.start,
            match s.loop_start {
                Some(v) if v != 0 => v,
                _ => -1,
            },
        )
    });
    (ram, samples, offs, warnings)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rf5c_rate_uses_step_and_default_clock() {
        assert_eq!(rf5c_rate(8_000_000, 0x100), 8_000_000.0 * 256.0 / 786_432.0);
        // A zero clock falls back to the RF5C68 default.
        assert_eq!(rf5c_rate(0, 0x100), rf5c_rate(8_000_000, 0x100));
        // Only the low 16 bits of the step count.
        assert_eq!(rf5c_rate(8_000_000, 0x1_0100), rf5c_rate(8_000_000, 0x0100));
    }

    use crate::vgm::RomSlice;

    fn write(sample: i64, reg: u32, val: u32) -> ChipWrite {
        ChipWrite::new(sample, "rf5c68", 0, reg, val)
    }

    fn body_rom() -> RomSlice {
        RomSlice {
            chip: 0,
            rom_size: 0x100,
            start: 0x100,
            data: vec![0x80, 0x81, 0x82, 0x83, 0x84, 0x85, 0x86, 0x87, 0xFF],
            kind: "rf5c68".into(),
            time: 0,
        }
    }

    fn keyed_vgm() -> VgmFile {
        let mut vgm = VgmFile::default();
        vgm.roms.push(body_rom());
        vgm.writes = vec![
            write(0, 0x02, 0x00),
            write(1, 0x03, 0x01), // step 0x0100
            write(2, 0x06, 0x01), // start 0x100
            write(3, 0x01, 0x2F),
            write(4, 0x00, 0xE0),
            write(5, 0x07, 0x40), // select voice 0
            write(10, 0x08, 0xFE), // key voice 0
        ];
        vgm
    }

    #[test]
    fn collect_decodes_voice_and_sample() {
        let mut vgm = keyed_vgm();
        vgm.writes.push(write(100, 0x08, 0xFF)); // disable voice 0
        let (ram, samples, offs, warnings) = collect_rf5c(&vgm, "rf5c68", Some(8_000_000), 0);
        assert_eq!(ram.len(), RF5C_RAM_SIZE);
        assert_eq!(ram[0x100], 0x80);
        assert_eq!(samples.len(), 1);
        let sample = &samples[0];
        assert_eq!(sample.start, 0x100);
        assert_eq!(sample.data, vec![0, 1, 2, 3, 4, 5, 6, 7]);
        assert_eq!(sample.loop_start, None);
        assert_eq!(sample.loop_end, None);
        assert_eq!(sample.length(), 8);
        assert_eq!(sample.hits.len(), 1);
        let hit = &sample.hits[0];
        assert_eq!(hit.sample_time, 10);
        assert_eq!(hit.step, 0x100);
        assert_eq!(hit.env, 0xE0);
        assert_eq!(hit.pan, 0x2F);
        assert_eq!(hit.start, 0x100);
        assert_eq!(hit.r#loop, 0);
        assert_eq!(sample.mode_step(), 0x100);
        assert_eq!(sample.c4_rate(8_000_000), 2604);
        assert_eq!(
            offs,
            vec![Rf5cOff {
                sample_time: 100,
                voice: 0,
                chip_id: 0,
            }]
        );
        assert!(warnings.is_empty());
    }

    #[test]
    fn retrigger_within_64_samples_is_not_a_note_off() {
        let mut vgm = keyed_vgm();
        vgm.writes.push(write(100, 0x08, 0xFF));
        vgm.writes.push(write(164, 0x08, 0xFE)); // exactly RF5C_RETRIGGER later
        vgm.writes.push(write(300, 0x08, 0xFF));
        vgm.writes.push(write(400, 0x08, 0xFE));
        let (_, samples, offs, warnings) = collect_rf5c(&vgm, "rf5c68", None, 0);
        assert_eq!(samples.len(), 1);
        assert_eq!(samples[0].hits.len(), 3);
        assert_eq!(
            offs,
            vec![Rf5cOff {
                sample_time: 300,
                voice: 0,
                chip_id: 0,
            }]
        );
        assert!(warnings.is_empty());
    }

    #[test]
    fn mode_step_picks_most_common_and_first_on_tie() {
        let mut sample = Rf5cSample {
            start: 0x100,
            ..Rf5cSample::default()
        };
        for (step, time) in [(0, 0), (5, 10), (7, 20), (5, 30)] {
            sample.hits.push(Rf5cHit {
                sample_time: time,
                step,
                ..Rf5cHit::default()
            });
        }
        assert_eq!(sample.mode_step(), 5);
        assert_eq!(sample.c4_rate(8_000_000), {
            let rate = rf5c_rate(8_000_000, 5);
            std::cmp::max(1000, parity::py_int(parity::py_round(rate)) as i32)
        });

        sample.hits.clear();
        for step in [5, 7] {
            sample.hits.push(Rf5cHit {
                step,
                ..Rf5cHit::default()
            });
        }
        assert_eq!(sample.mode_step(), 5);
    }

    #[test]
    fn empty_input_and_missing_ram() {
        let (ram, samples, offs, warnings) =
            collect_rf5c(&VgmFile::default(), "rf5c68", None, 0);
        assert_eq!(ram.len(), RF5C_RAM_SIZE);
        assert!(ram.iter().all(|&b| b == 0));
        assert!(samples.is_empty());
        assert!(offs.is_empty());
        assert!(warnings.is_empty());

        // A key with no playable body in RAM reports the lost sample block.
        let mut vgm = VgmFile::default();
        vgm.roms.push(RomSlice {
            chip: 0,
            rom_size: 1,
            start: 0x100,
            data: vec![RF5C_END_MARK],
            kind: "rf5c68".into(),
            time: 0,
        });
        vgm.writes = vec![write(0, 0x06, 0x01), write(10, 0x08, 0xFE)];
        let (_, samples, _, warnings) = collect_rf5c(&vgm, "rf5c68", None, 0);
        assert!(samples.is_empty());
        assert_eq!(
            warnings,
            vec![
                "RF5C68 notes but no sample RAM data; the PCM channels will be \
                 silent until samples are added"
                    .to_string()
            ]
        );
    }
}
