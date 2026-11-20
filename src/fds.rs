//! Nintendo Famicom Disk System: one 64-step wavetable voice.
//!
//! VGM carries FDS writes through the NES command ``0xB4``.  libvgm remaps
//! offsets ``0x20-0x3F`` to the FDS register file (``$4080-$408A`` plus the
//! ``$4023`` I/O enable) and leaves ``0x40-0x7F`` as the 64-byte wave table.
//! The parser already splits those into the ``fds`` chip stream.

use std::collections::{BTreeMap, BTreeSet};

use crate::vgm::VgmFile;

pub const FDS_DEFAULT_CLOCK: u32 = 1_789_773;
// Furnace's FDS platform is a frequency-mode pitch table with CHIP_FREQBASE.
pub const FDS_FREQ_DIV: i32 = 262_144;
pub const FDS_MAX_FREQ: i32 = 0xFFFF;
pub const FDS_WAVE_LEN: usize = 64;
pub const FDS_MOD_LEN: usize = 32;
// A raw frequency-register jump this large starts a new note; a smaller
// change is treated as a bend.
pub const FDS_NOTE_JUMP: i32 = 64;
// Shorter than this is a click or a re-attack of the same note.
pub const FDS_MIN_NOTE: i64 = 64;
// A volume write that lands this soon after the frequency write belongs to
// the same key-on.
pub const FDS_SETTLE: i64 = 96;

/// One FDS note with its starting state and live changes.
#[derive(Clone, Debug, PartialEq)]
pub struct FdsHit {
    pub sample_time: i64,
    pub chip_id: i32,
    pub freq: i32,
    pub wave: Vec<u8>,
    pub mod_table: Vec<u8>,
    pub mod_depth: i32,
    pub mod_speed: i32,
    pub mod_pos: i32,
    pub volume: i32,
    pub vol_pts: Vec<(i64, i32)>,
    pub freq_pts: Vec<(i64, i32)>,
    pub wave_pts: Vec<(i64, Vec<u8>)>,
    pub mod_pts: Vec<(i64, i32, i32)>,
    pub off_sample: i64,
}

impl Default for FdsHit {
    fn default() -> Self {
        FdsHit {
            sample_time: 0,
            chip_id: 0,
            freq: 0,
            wave: Vec::new(),
            mod_table: Vec::new(),
            mod_depth: 0,
            mod_speed: 0,
            mod_pos: 0,
            volume: 0,
            vol_pts: Vec::new(),
            freq_pts: Vec::new(),
            wave_pts: Vec::new(),
            mod_pts: Vec::new(),
            off_sample: 0,
        }
    }
}

fn _freq_value(low: i32, high: i32) -> i32 {
    ((high & 0x0F) << 8) | (low & 0xFF)
}

#[derive(Clone, Debug, PartialEq)]
struct State {
    wave: [u8; FDS_WAVE_LEN],
    mod_: [u8; FDS_MOD_LEN],
    mod_pos: i32,
    low: i32,
    high: i32,
    volume: i32,
    #[allow(dead_code)]
    env_mode: i32,
    mod_depth: i32,
    mod_speed: i32,
    mod_position: i32,
    active: Option<usize>,
    last_note_t: i64,
    wave_burst: bool,
    wave_dirty: bool,
    #[allow(dead_code)]
    mod_burst: bool,
    #[allow(dead_code)]
    pending_off: i64,
}

impl State {
    fn new() -> State {
        State {
            wave: [0; FDS_WAVE_LEN],
            mod_: [0; FDS_MOD_LEN],
            mod_pos: 0,
            low: 0,
            high: 0,
            volume: 0,
            env_mode: 0,
            mod_depth: 0,
            mod_speed: 0,
            mod_position: 0,
            active: None,
            last_note_t: -1,
            wave_burst: false,
            wave_dirty: false,
            mod_burst: false,
            pending_off: 0,
        }
    }

    fn current_freq(&self) -> i32 {
        _freq_value(self.low, self.high)
    }
}

struct Collector {
    state: BTreeMap<i32, State>,
    hits: Vec<FdsHit>,
}

impl Collector {
    fn close(&mut self, cid: i32, when: i64, off: bool) {
        let st = self.state.get_mut(&cid).expect("chip state exists");
        let Some(idx) = st.active else {
            return;
        };
        if off && when > self.hits[idx].sample_time {
            self.hits[idx].off_sample = when;
        }
        st.active = None;
    }

    fn open_hit(&mut self, cid: i32, when: i64, freq: i32) -> usize {
        let hit = {
            let st = self.state.get(&cid).expect("chip state exists");
            FdsHit {
                sample_time: when,
                chip_id: cid,
                freq,
                wave: st.wave.to_vec(),
                mod_table: st.mod_.to_vec(),
                mod_depth: st.mod_depth,
                mod_speed: st.mod_speed,
                mod_pos: st.mod_position,
                volume: st.volume.clamp(0, 32),
                ..FdsHit::default()
            }
        };
        self.hits.push(hit);
        let idx = self.hits.len() - 1;
        self.state.get_mut(&cid).expect("chip state exists").active = Some(idx);
        idx
    }
}

/// Walk the FDS register stream and return its notes.
pub fn collect_fds(vgm: &VgmFile) -> (Vec<FdsHit>, Vec<String>) {
    let warnings: Vec<String> = Vec::new();
    let chips: BTreeSet<i32> = vgm
        .writes
        .iter()
        .filter(|w| w.chip == "fds")
        .map(|w| w.chip_id as i32)
        .collect();
    if chips.is_empty() {
        return (Vec::new(), warnings);
    }

    let total = vgm.total_samples;
    let mut col = Collector {
        state: BTreeMap::new(),
        hits: Vec::new(),
    };
    for cid in &chips {
        col.state.insert(*cid, State::new());
    }

    for w in &vgm.writes {
        if w.chip != "fds" {
            continue;
        }
        let cid = w.chip_id as i32;
        let t = w.sample;
        let reg = w.reg;
        let val = (w.val & 0xFF) as i32;
        let st = col.state.get_mut(&cid).expect("chip state exists");

        // Wave table writes (0x40-0x7F) and the 0x89 write-enable toggle.
        if (0x40..=0x7F).contains(&reg) {
            st.wave[((reg - 0x40) & 0x3F) as usize] = (val & 0x3F) as u8;
            st.wave_burst = true;
            st.wave_dirty = true;
            continue;
        }

        // A non-wave register write ends a wave upload burst.
        if st.wave_burst {
            if let Some(idx) = st.active {
                if st.wave_dirty {
                    let body = st.wave.to_vec();
                    let hit = &mut col.hits[idx];
                    if body != hit.wave {
                        hit.wave_pts.push((t, body.clone()));
                        hit.wave = body;
                    }
                }
            }
            st.wave_burst = false;
            st.wave_dirty = false;
        }

        if reg == 0x80 {
            st.volume = val & 0x3F;
            st.env_mode = (val >> 6) & 0x03;
            if let Some(idx) = st.active {
                let hit = &mut col.hits[idx];
                let level = st.volume.clamp(0, 32);
                if hit.vol_pts.last().map(|p| p.1) != Some(level) {
                    hit.vol_pts.push((t, level));
                }
                if level == 0 {
                    col.close(cid, t, true);
                }
            }
            continue;
        }
        if reg == 0x82 {
            st.low = val;
            if let Some(idx) = st.active {
                let hit = &mut col.hits[idx];
                if hit.freq_pts.is_empty() && t - hit.sample_time <= FDS_SETTLE {
                    // Some drivers write $4083 (high) before $4082 (low).  The
                    // key-on therefore opened with the previous low byte; fold
                    // the paired low write into the note before it becomes
                    // audible.
                    hit.freq = st.current_freq();
                }
            }
            continue;
        }
        if reg == 0x83 {
            let old_high = st.high;
            st.high = val;
            let freq = st.current_freq();
            let silenced = (val & 0x80) != 0;
            let hit_old = st.active;
            let volume = st.volume;
            let last_note_t = st.last_note_t;
            if silenced || freq == 0 {
                if hit_old.is_some() {
                    col.close(cid, t, true);
                }
                continue;
            }
            // A new note unless this is a small pitch change on a live note.
            let jump = match hit_old {
                Some(idx) => (freq - col.hits[idx].freq).abs() >= FDS_NOTE_JUMP,
                None => true,
            };
            let reattack = hit_old.is_some() && volume > 0 && (old_high & 0x80) != 0;
            let fresh = hit_old.is_none()
                || jump
                || reattack
                || (t - last_note_t >= FDS_MIN_NOTE && (old_high & 0x80) == 0);
            if let Some(idx) = hit_old {
                if !fresh {
                    let hit = &mut col.hits[idx];
                    hit.freq_pts.push((t, freq));
                    hit.freq = freq;
                    continue;
                }
            }
            col.close(cid, t, hit_old.is_some());
            if volume > 0 || !silenced {
                col.open_hit(cid, t, freq);
                col.state.get_mut(&cid).expect("chip state exists").last_note_t = t;
            }
            continue;
        }
        if reg == 0x84 {
            // $4084: bit 6 = increase, bit 7 = disable envelope, low six
            // bits = depth.  Furnace's platform uses a fixed depth.
            st.mod_depth = val & 0x3F;
            st.mod_position = 0;
            if let Some(idx) = st.active {
                let hit = &mut col.hits[idx];
                hit.mod_pts.push((t, 0x11, st.mod_depth));
                hit.mod_depth = st.mod_depth;
            }
            continue;
        }
        if reg == 0x85 {
            st.mod_position = val & 0x7F;
            if let Some(idx) = st.active {
                let hit = &mut col.hits[idx];
                hit.mod_pts.push((t, 0x14, st.mod_position));
                hit.mod_pos = st.mod_position;
            }
            continue;
        }
        if reg == 0x86 {
            st.mod_speed = (st.mod_speed & 0xF00) | val;
            if let Some(idx) = st.active {
                let hit = &mut col.hits[idx];
                hit.mod_pts.push((t, 0x13, val));
                hit.mod_pts
                    .push((t, 0x12, (st.mod_speed >> 8) & 0x0F));
            }
            continue;
        }
        if reg == 0x87 {
            st.mod_speed = (st.mod_speed & 0x0FF) | ((val & 0x0F) << 8);
            if (val & 0x80) != 0 {
                // Modulator halted: the next 0x88 writes upload the table.
                st.mod_pos = 0;
                st.mod_burst = true;
            }
            if let Some(idx) = st.active {
                let hit = &mut col.hits[idx];
                hit.mod_pts
                    .push((t, 0x12, (st.mod_speed >> 8) & 0x0F));
                hit.mod_pts.push((t, 0x11, st.mod_depth));
            }
            continue;
        }
        if reg == 0x88 {
            st.mod_[(st.mod_pos & 0x1F) as usize] = (val & 0x07) as u8;
            st.mod_pos = (st.mod_pos + 1) & 0x1F;
            continue;
        }
        if reg == 0x89 {
            // Bit 7 enables wave writes.  The bit itself is not audible.
            continue;
        }
        if reg == 0x8A {
            st.mod_speed = val;
            if let Some(idx) = st.active {
                col.hits[idx]
                    .mod_pts
                    .push((t, 0x12, (val >> 4) & 0x0F));
            }
            continue;
        }
        // $4023 and anything else is setup, not a note.
    }

    for cid in &chips {
        col.close(*cid, total, false);
    }

    // Drop empty or degenerate hits and sort for the caller.
    let mut hits: Vec<FdsHit> = col
        .hits
        .into_iter()
        .filter(|h| h.freq > 0 && (h.off_sample == 0 || h.off_sample > h.sample_time))
        .collect();
    hits.sort_by_key(|h| (h.sample_time, h.chip_id));
    (hits, warnings)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vgm::ChipWrite;

    fn vgm(writes: Vec<ChipWrite>) -> VgmFile {
        let mut v = VgmFile::default();
        v.writes = writes;
        v
    }

    fn fds(sample: i64, reg: u32, val: u32) -> ChipWrite {
        ChipWrite::new(sample, "fds", 0, reg, val)
    }

    fn note(sample: i64, low: u32, high: u32, volume: u32) -> Vec<ChipWrite> {
        vec![
            fds(sample, 0x82, low),
            fds(sample, 0x80, volume),
            fds(sample, 0x83, high),
        ]
    }

    #[test]
    fn note_starts_on_the_high_write_and_closes_at_silence() {
        let mut writes = note(1000, 0x34, 0x01, 0x10);
        writes.push(fds(5000, 0x80, 0x00));
        let (hits, warnings) = collect_fds(&vgm(writes));
        assert!(warnings.is_empty());
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].sample_time, 1000);
        assert_eq!(hits[0].freq, 0x134);
        assert_eq!(hits[0].volume, 16);
        assert_eq!(hits[0].off_sample, 5000);
        assert_eq!(hits[0].vol_pts, vec![(5000, 0)]);
    }

    #[test]
    fn paired_low_write_after_high_is_folded_into_the_note() {
        // The driver writes $4083 before $4082 at the same sample; the note
        // opens with the stale low byte and must pick up the new one.
        let writes = vec![
            fds(1000, 0x83, 0x02),
            fds(1000, 0x82, 0x55),
            fds(1000, 0x80, 0x1F),
        ];
        let (hits, _) = collect_fds(&vgm(writes));
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].freq, 0x255);
        assert!(hits[0].freq_pts.is_empty());
    }

    #[test]
    fn small_pitch_change_is_a_bend_and_a_large_jump_splits() {
        let mut writes = note(1000, 0x00, 0x01, 0x20);
        // Within FDS_MIN_NOTE of the key-on a low/high pair is a bend.
        writes.push(fds(1000, 0x82, 0x10));
        writes.push(fds(1000, 0x83, 0x01));
        // A jump of 0x200 starts a new note.
        writes.push(fds(4000, 0x83, 0x03));
        let (hits, _) = collect_fds(&vgm(writes));
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].freq_pts, vec![(1000, 0x110)]);
        assert_eq!(hits[0].off_sample, 4000);
        assert_eq!(hits[1].sample_time, 4000);
        assert_eq!(hits[1].freq, 0x310);
    }

    #[test]
    fn wave_upload_during_a_note_becomes_a_wave_point() {
        let mut writes = note(1000, 0x00, 0x01, 0x10);
        writes.push(fds(2000, 0x89, 0x80));
        for i in 0..64u32 {
            writes.push(fds(2000, 0x40 + i, (i + 1) & 0x3F));
        }
        writes.push(fds(2500, 0x80, 0x10));
        let (hits, _) = collect_fds(&vgm(writes));
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].wave_pts.len(), 1);
        let (t, ref body) = hits[0].wave_pts[0];
        assert_eq!(t, 2500);
        assert_eq!(body.len(), 64);
        assert_eq!(body[0], 1);
        assert_eq!(body[63], 64 & 0x3F);
        assert_eq!(hits[0].wave, *body);
    }

    #[test]
    fn mod_table_and_mod_registers_are_captured() {
        let mut writes = vec![
            fds(0, 0x87, 0x80), // halt modulator, upload follows
            fds(0, 0x88, 0x05),
            fds(0, 0x88, 0x02),
            fds(0, 0x84, 0x22), // depth 0x22
            fds(0, 0x85, 0x03), // position 3
            fds(0, 0x86, 0x07), // speed low
            fds(0, 0x87, 0x01), // speed high nibble
        ];
        writes.extend(note(1000, 0x00, 0x01, 0x20));
        writes.push(fds(1200, 0x8A, 0x34));
        let (hits, _) = collect_fds(&vgm(writes));
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].mod_depth, 0x22);
        assert_eq!(hits[0].mod_speed, 0x107);
        assert_eq!(hits[0].mod_pos, 3);
        let mut table = vec![0u8; 32];
        table[0] = 5;
        table[1] = 2;
        assert_eq!(hits[0].mod_table, table);
        assert_eq!(hits[0].mod_pts, vec![(1200, 0x12, 3)]);
    }
}
