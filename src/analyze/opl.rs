//! OPL-family register walkers: melodic voices, the rhythm kit and OPLL.
//!
//! Literal port of `_collect_opl`, `_collect_opll`, `_opl_rhythm_plan`,
//! `_fm_vol_max`, `_family_keyons` and `_family_sounds` from `analyze.py`.
//! The two collectors turn a register stream into `NoteEvent`s and instrument
//! patches; the small helpers decide a layout and a family before that.

use std::collections::HashMap;

use indexmap::IndexMap;

use crate::fm::{carriers_for, FMPatch};
use crate::model::{FmSystem, NoteEvent};
use crate::opl::{
    drum_channel, opl3_like, opl_note_for, OPLState, DRUM_BITS, KIND_OPL3, N, ORDERED_OPS_L,
    SLOT_ADDR,
};
use crate::vgm::{ChipWrite, VgmFile};
use crate::opl::opll_note_for;
use crate::ym2413::{OPLL_DRUMS, OPLL_SETTLE, OPLLState};

use super::state::{ChipFx, PatchKey};

/// Python `patches` keys are arbitrary tuples. This module's shapes are
/// flattened into one preorder `Vec<i64>` and wrapped in `PatchKey::Plain`;
/// tag words are negative so they can never collide with an identity (whose
/// first word is a non-negative field).
pub type Patches = IndexMap<PatchKey, FMPatch>;

const TAG_OPL_SHAPE: i64 = i64::MIN + 1;
const TAG_OPL_DRUM: i64 = i64::MIN + 2;
const TAG_OPLL_DRUM: i64 = i64::MIN + 3;

/// One channel effect row write: (sample, channel, effect, value).
pub type ChannelFx = (i64, i32, i32, i32);

/// Membership of a chip-family set. `state` passes `BTreeSet` and `HashSet`
/// interchangeably, exactly where Python writes a `set[str]`.
pub trait ChipSet {
    fn has(&self, chip: &str) -> bool;
}

impl<S: std::borrow::Borrow<str> + Eq + std::hash::Hash> ChipSet
    for std::collections::HashSet<S>
{
    fn has(&self, chip: &str) -> bool {
        self.iter().any(|c| c.borrow() == chip)
    }
}

impl<S: std::borrow::Borrow<str> + Ord> ChipSet for std::collections::BTreeSet<S> {
    fn has(&self, chip: &str) -> bool {
        self.iter().any(|c| c.borrow() == chip)
    }
}

fn bump(stats: &mut IndexMap<String, i64>, key: &str, n: i64) {
    *stats.get_mut(key).expect("Counter key exists") += n;
}

// ---------------------------------------------------------------------------
// _collect_opl
// ---------------------------------------------------------------------------

struct OplCollector<'a> {
    kind: &'a str,
    clock: f64,
    drums: bool,
    opl3: bool,
    freq_base: Option<i32>,
    states: IndexMap<i32, OPLState>,
    freq_owners: IndexMap<i32, HashMap<i32, Vec<i32>>>,
    op_owners: IndexMap<i32, HashMap<i32, Vec<i32>>>,
    op_index: IndexMap<i32, HashMap<(i32, i32), i32>>,
    chan_ops: IndexMap<i32, HashMap<i32, Vec<(i32, u16)>>>,
    // Python iterates `held` (channel order of first key-on), so insertion
    // order is observable in the order the off events are appended.
    held: IndexMap<(i32, i32), usize>,
    pan_last: HashMap<(i32, i32), u8>,
    pending_reset: IndexMap<(i32, i32), (i32, IndexMap<(i32, i32), i32>)>,
    pitch_pts: HashMap<(i32, i32), Vec<(i64, i32, i32)>>,
    pitch_base: HashMap<(i32, i32), (i32, i32)>,
    pitch_last: HashMap<(i32, i32), (i32, i32)>,
    tl_pts: HashMap<(i32, i32), Vec<(i64, i32)>>,
    tl_last: HashMap<(i32, i32), i32>,
    tl_open: HashMap<(i32, i32), i32>,
    op_tl_pts: HashMap<(i32, i32), HashMap<i32, Vec<(i64, i32)>>>,
    op_tl_last: HashMap<(i32, i32, i32), i32>,
    off_idx: Vec<usize>,
    ins_cap: usize,
    reuse_by_shape: HashMap<(i32, Vec<i32>), PatchKey>,
    last_amd: i32,
    last_vib: i32,
    // borrowed shards
    events: &'a mut Vec<NoteEvent>,
    patches: &'a mut Patches,
    patch_order: &'a mut Vec<PatchKey>,
    keyon_times: &'a mut Vec<i64>,
    channel_fx: &'a mut Vec<ChannelFx>,
    chip_fx: &'a mut Vec<ChipFx>,
    base: &'a IndexMap<i32, i32>,
    stats: IndexMap<String, i64>,
    warnings: Vec<String>,
}

/// Instrument's own value for one live operator field, encoded for the row.
fn opl_base_effect(patch: &FMPatch, cmd: i32, e: i32) -> Option<i32> {
    if cmd == 0x11 {
        return Some(patch.fb & 7);
    }
    let op = if patch.four { ORDERED_OPS_L[e as usize] } else { e };
    if !(0 <= op && (op as usize) < patch.ops.len()) {
        return None;
    }
    let o = &patch.ops[op as usize];
    if cmd == 0x16 {
        return Some(((e + 1) << 4) | (i32::from(o.mult) & 15));
    }
    if (0x1A..=0x1D).contains(&cmd) {
        return Some(i32::from(o.ar) & 15);
    }
    if (0x57..=0x5A).contains(&cmd) {
        return Some(i32::from(o.dr) & 15);
    }
    if cmd == 0x50 {
        return Some(((e + 1) << 4) | (i32::from(o.am) & 1));
    }
    if cmd == 0x51 {
        return Some(((e + 1) << 4) | (i32::from(o.sl) & 15));
    }
    if cmd == 0x52 {
        return Some(((e + 1) << 4) | (i32::from(o.rr) & 15));
    }
    if cmd == 0x53 {
        return Some(((e + 1) << 4) | (i32::from(o.vib) & 1));
    }
    if cmd == 0x55 {
        return Some(((e + 1) << 4) | (i32::from(o.sus) & 1));
    }
    if cmd == 0x5B {
        return Some(((e + 1) << 4) | (i32::from(o.ksr) & 1));
    }
    if cmd == 0x2A {
        return Some(((e + 1) << 4) | (i32::from(o.ws) & 7));
    }
    None
}

/// Timbre with the modulator level folded out.
fn shape_key(snap: &FMPatch) -> Vec<i64> {
    let carriers = snap.carriers();
    let mut key = vec![
        TAG_OPL_SHAPE,
        i64::from(snap.alg & 3),
        i64::from(snap.fb & 7),
        snap.opl as i64,
        snap.four as i64,
        snap.drum as i64,
    ];
    for i in 0..4 {
        key.push(i as i64);
        key.extend(snap.ops[i].as_tuple(carriers.contains(&(i as i32))));
    }
    key
}

/// Carrier level out of the instrument and into the volume column.
fn normalized_opl(patch: &FMPatch) -> (FMPatch, i32) {
    let carriers = patch.carriers();
    let mut ops = patch.ops.clone();
    let floor = if carriers.is_empty() {
        0
    } else {
        carriers
            .iter()
            .map(|&i| i32::from(ops[i as usize].tl & 0x3F))
            .min()
            .unwrap()
    };
    for &i in &carriers {
        ops[i as usize].tl = (i32::from(ops[i as usize].tl & 0x3F) - floor) as u8;
    }
    let mut out = patch.clone();
    out.ops = ops;
    (out, 63 - floor.min(63))
}

/// Keep the deepest modulator level seen for a shared shape.
fn loudest(base: &FMPatch, snap: &FMPatch) -> FMPatch {
    let carriers = base.carriers();
    let mut ops = base.ops.clone();
    let mut changed = false;
    for i in 0..4 {
        if carriers.contains(&(i as i32)) {
            continue;
        }
        let here = snap.ops[i].tl & 0x3F;
        if here < (ops[i].tl & 0x3F) {
            ops[i].tl = here;
            changed = true;
        }
    }
    if changed {
        let mut out = base.clone();
        out.ops = ops;
        out
    } else {
        base.clone()
    }
}

impl<'a> OplCollector<'a> {
    fn op_is_carrier(&self, cid: i32, fch: i32, oi: i32) -> bool {
        let st = &self.states[&cid];
        if st.drums && fch > st.melodic {
            return true;
        }
        carriers_for(st.algorithm(fch), false, true, st.is_four_op(fch)).contains(&oi)
    }

    fn live_op_fx(
        &mut self,
        key: (i32, i32),
        ch: i32,
        cmd: i32,
        e: i32,
        value: i32,
        sample: i64,
    ) {
        self.channel_fx.push((sample, ch, cmd, value));
        let idx = match self.held.get(&key) {
            Some(&i) => i,
            None => return,
        };
        let old_ins = self.events[idx].ins;
        if !(0 <= old_ins && (old_ins as usize) < self.patch_order.len()) {
            return;
        }
        let patch = match self.patches.get(&self.patch_order[old_ins as usize]) {
            Some(p) => p,
            None => return,
        };
        let base = match opl_base_effect(patch, cmd, e) {
            Some(v) => v,
            None => return,
        };
        let entry = self
            .pending_reset
            .entry(key)
            .or_insert_with(|| (old_ins, IndexMap::new()));
        entry.1.insert((cmd, e), base);
    }

    /// An existing patch with the same carrier shape, else the first.
    fn reuse(&self, snap: &FMPatch) -> PatchKey {
        let shape = (snap.alg & 7, snap.carriers());
        match self.reuse_by_shape.get(&shape) {
            Some(k) if self.patches.contains_key(k) => k.clone(),
            _ => self.patch_order[0].clone(),
        }
    }

    /// Instrument index and volume column for the voice a channel holds.
    fn intern(&mut self, cid: i32, fch: i32) -> (i32, i32) {
        if self.drums && fch > self.states[&cid].melodic {
            let snap = self.states[&cid].drum_snapshot();
            let mut words = vec![TAG_OPL_DRUM];
            words.extend(snap.identity().0);
            let mut key = PatchKey::Plain(words);
            if !self.patches.contains_key(&key) {
                if self.patch_order.len() >= self.ins_cap {
                    key = self.reuse(&snap);
                    bump(&mut self.stats, "ins_reused", 1);
                } else {
                    let mut snap = snap;
                    snap.name = format!("OPL drum {:02}", self.patch_order.len());
                    self.patches.insert(key.clone(), snap);
                    self.patch_order.push(key.clone());
                }
            }
            if let Some(p) = self.patches.get_mut(&key) {
                p.uses += 1;
            }
            let idx = self.patch_order.iter().position(|k| k == &key).unwrap() as i32;
            return (idx, 63);
        }
        let (snap, vol) = normalized_opl(&self.states[&cid].snapshot(fch));
        let four = self.states[&cid].is_four_op(fch);
        if four {
            bump(&mut self.stats, "four_op", 1);
        }
        let mut key = PatchKey::Plain(shape_key(&snap));
        if !self.patches.contains_key(&key) {
            if self.patch_order.len() >= self.ins_cap {
                key = self.reuse(&snap);
                bump(&mut self.stats, "ins_reused", 1);
            } else {
                let mut snap = snap;
                let fam = if four {
                    "OPL3 4op".to_string()
                } else {
                    self.kind.to_uppercase()
                };
                snap.name = format!("{} {:02}", fam, self.patch_order.len());
                let shape = (snap.alg & 7, snap.carriers());
                self.patches.insert(key.clone(), snap);
                self.patch_order.push(key.clone());
                self.reuse_by_shape.entry(shape).or_insert(key.clone());
            }
        } else {
            let merged = loudest(&self.patches[&key], &snap);
            self.patches.insert(key.clone(), merged);
        }
        if let Some(p) = self.patches.get_mut(&key) {
            p.uses += 1;
        }
        let idx = self.patch_order.iter().position(|k| k == &key).unwrap() as i32;
        (idx, vol)
    }

    fn close_note(&mut self, cid: i32, fch: i32, sample: i64, off: bool) {
        let key = (cid, fch);
        let idx = match self.held.shift_remove(&key) {
            Some(i) => i,
            None => return,
        };
        let ev_sample = self.events[idx].sample;
        let ev_ch = self.events[idx].ch;
        let ev_drum = self.events[idx].drum;

        let pts = self.pitch_pts.remove(&key).unwrap_or_default();
        if !pts.is_empty() {
            let mut merged: Vec<(i64, i32, i32)> = Vec::new();
            for p in &pts {
                match merged.last_mut() {
                    Some(last) if p.0 - last.0 <= 16 => *last = *p,
                    _ => merged.push(*p),
                }
            }
            let first_tail = (merged[0].1, merged[0].2);
            let pbase = self.pitch_base[&key];
            if merged[0].0 - ev_sample <= 120 && first_tail != pbase {
                let (note, e5) = opl_note_for(
                    merged[0].1,
                    merged[0].2,
                    self.clock,
                    self.opl3,
                    self.freq_base,
                );
                self.events[idx].note = note;
                self.events[idx].e5 = e5;
                merged[0] = (ev_sample, merged[0].1, merged[0].2);
                self.pitch_base.insert(key, (merged[0].1, merged[0].2));
            }
            let mut distinct: Vec<(i64, i32, i32)> = Vec::new();
            for p in &merged {
                let same = match distinct.last() {
                    Some(last) => (p.1, p.2) == (last.1, last.2),
                    None => false,
                };
                if !same {
                    distinct.push(*p);
                }
            }
            if distinct.len() > 1 {
                self.events[idx].fm_pitch_pts = distinct;
            }
        }

        let tl = self.tl_pts.remove(&key).unwrap_or_default();
        if !tl.is_empty() {
            if ev_drum {
                let shift = self.tl_open.remove(&key).unwrap_or(tl[0].1);
                self.events[idx].vol_pts = tl
                    .iter()
                    .map(|&(s, value)| (s, (63 + shift - value).clamp(0, 63), None))
                    .collect();
            } else {
                self.events[idx].tl_pts = tl;
            }
        }

        let curves = self.op_tl_pts.remove(&key).unwrap_or_default();
        let mut keys: Vec<i32> = curves.keys().copied().collect();
        keys.sort();
        let op_tl: Vec<(i32, Vec<(i64, i32)>)> = keys
            .into_iter()
            .filter_map(|oi| {
                let pts = &curves[&oi];
                if pts.is_empty() {
                    None
                } else {
                    Some((oi, pts.clone()))
                }
            })
            .collect();
        self.events[idx].opl_op_tl = op_tl;

        for oi in 0..4 {
            self.op_tl_last.remove(&(cid, fch, oi));
        }
        self.tl_open.remove(&key);
        self.pan_last.remove(&key);
        if off {
            self.events[idx].end = sample;
            self.events.push(NoteEvent::new(
                sample, ev_ch, false, 180, -1, -1, 0x80, 0,
            ));
            self.off_idx.push(self.events.len() - 1);
        }
    }

    fn open_note(&mut self, cid: i32, fch: i32, sample: i64) {
        let kit = self.drums && fch > self.states[&cid].melodic;
        self.close_note(cid, fch, sample, false);
        let (block, fnum) = self.states[&cid].frequency(fch);
        let (note, e5) =
            opl_note_for(block, fnum, self.clock, self.opl3, self.freq_base);
        let (ins, vol) = self.intern(cid, fch);
        let key = (cid, fch);
        if let Some((old_ins, fields)) = self.pending_reset.shift_remove(&key) {
            if old_ins == ins {
                for ((cmd, e), base_val) in &fields {
                    let _ = e;
                    let ch = self.base[&cid] + fch;
                    self.channel_fx.push((sample, ch, *cmd, *base_val));
                }
            }
        }
        let ev_pan = if kit {
            0
        } else {
            i32::from(self.states[&cid].pan_byte(fch))
        };
        let mut ev = NoteEvent::new(sample, self.base[&cid] + fch, true, note, ins, vol, e5, ev_pan);
        ev.sweep_div = 2.0;
        ev.e5_lo = 0x00;
        ev.e5_hi = 0xFF;
        ev.drum = kit;
        self.events.push(ev);
        self.held.insert(key, self.events.len() - 1);
        if !kit {
            self.pan_last.insert(key, ev_pan as u8);
        }
        self.pitch_base.insert(key, (block, fnum));
        self.pitch_last.insert(key, (block, fnum));
        self.pitch_pts.insert(key, vec![(sample, block, fnum)]);
        self.tl_pts.insert(key, Vec::new());
        let channel_tl = self.states[&cid].channel_tl(fch);
        self.tl_last.insert(key, channel_tl);
        self.tl_open.insert(key, channel_tl);
        self.op_tl_pts.insert(key, HashMap::new());
        let mut owned: Vec<i32> = Vec::new();
        let chan_ops = self.chan_ops[&cid].get(&fch).cloned().unwrap_or_default();
        for (oi, addr) in chan_ops {
            let level = i32::from(self.states[&cid].regs[0x40 | addr as usize] & 0x3F);
            self.op_tl_last.insert((cid, fch, oi), level);
            if !owned.contains(&oi) {
                owned.push(oi);
            }
            if !self.op_is_carrier(cid, fch, oi) {
                self.op_tl_pts
                    .get_mut(&key)
                    .unwrap()
                    .insert(oi, vec![(sample, level)]);
            }
        }
        let stale: Vec<(i32, i32, i32)> = self
            .op_tl_last
            .keys()
            .filter(|k| k.0 == cid && k.1 == fch && !owned.contains(&k.2))
            .copied()
            .collect();
        for k in stale {
            self.op_tl_last.remove(&k);
        }
        self.keyon_times.push(sample);
        if kit {
            bump(&mut self.stats, "drums", 1);
        } else {
            bump(&mut self.stats, "notes", 1);
        }
    }

    fn release_reinterpreted(&mut self, cid: i32, sample: i64) {
        let melodic = self.states[&cid].melodic;
        let chans: Vec<i32> = self
            .held
            .keys()
            .filter(|k| k.0 == cid && k.1 > melodic)
            .map(|k| k.1)
            .collect();
        for fch in chans {
            self.close_note(cid, fch, sample, true);
            bump(&mut self.stats, "mode_midnote", 1);
        }
    }

    fn keyed_here(w: &ChipWrite) -> bool {
        let r = w.reg & 0xFF;
        if (0xB0..=0xB8).contains(&r) {
            return w.val & 0x20 != 0;
        }
        if r == 0xBD {
            return w.val & 0x3F != 0;
        }
        false
    }

    /// Same-sample key bits last, so the byte beside them is already loaded.
    fn ordered(writes: &[ChipWrite]) -> Vec<&ChipWrite> {
        let mut out: Vec<&ChipWrite> = Vec::with_capacity(writes.len());
        let mut i = 0usize;
        let n = writes.len();
        while i < n {
            let mut j = i + 1;
            let s = writes[i].sample;
            while j < n && writes[j].sample == s {
                j += 1;
            }
            let group = &writes[i..j];
            out.extend(group.iter().filter(|w| !Self::keyed_here(w)));
            out.extend(group.iter().filter(|w| Self::keyed_here(w)));
            i = j;
        }
        out
    }

    fn run(&mut self, vgm: &VgmFile, chip_name: Option<&str>) {
        let source = chip_name.unwrap_or(self.kind);
        for w in Self::ordered(&vgm.writes) {
            if w.chip != source {
                continue;
            }
            let cid = i32::from(w.chip_id);
            if !self.states.contains_key(&cid) {
                continue;
            }
            // The OPL4's port 2 is its wave half, not a third FM bank.
            if (w.reg >> 8) > 1 {
                continue;
            }
            let port = if self.opl3 {
                ((w.reg >> 8) & 1) as i32
            } else {
                0
            };
            let r = (w.reg & 0xFF) as i32;
            let val = w.val as i32;
            let addr = (port << 8) | r;

            if r == 0xBD && port == 0 {
                let old = i32::from(self.states[&cid].regs[addr as usize]);
                let was_rhythm = (old & 0x20) != 0;
                let old_trig = if was_rhythm { old & 0x1F } else { 0 };
                self.states.get_mut(&cid).unwrap().write(port, r, val);
                let is_rhythm = (val & 0x20) != 0;
                let trig = if is_rhythm { val & 0x1F } else { 0 };
                let amd = (val & 0x80 != 0) as i32;
                let vib = (val & 0x40 != 0) as i32;
                if amd != self.last_amd {
                    self.chip_fx.push(ChipFx::Plain(w.sample, 0x10, amd));
                    self.last_amd = amd;
                }
                if vib != self.last_vib {
                    self.chip_fx.push(ChipFx::Plain(w.sample, 0x17, vib));
                    self.last_vib = vib;
                }
                if is_rhythm && !was_rhythm {
                    self.release_reinterpreted(cid, w.sample);
                }
                for (bit, _name) in DRUM_BITS {
                    let bit = i32::from(bit);
                    if trig & bit != 0 && old_trig & bit == 0 {
                        let fch = drum_channel(bit as u8, self.states[&cid].melodic);
                        if !is_rhythm {
                            continue;
                        }
                        self.open_note(cid, fch, w.sample);
                    } else if old_trig & bit != 0 && trig & bit == 0 {
                        let fch = drum_channel(bit as u8, self.states[&cid].melodic);
                        self.close_note(cid, fch, w.sample, true);
                    }
                }
                if is_rhythm && old_trig != 0 && trig == old_trig {
                    bump(&mut self.stats, "repeat_keyon", 1);
                }
                continue;
            }

            if (0xB0..=0xB8).contains(&r) {
                let old = i32::from(self.states[&cid].regs[addr as usize]);
                self.states.get_mut(&cid).unwrap().write(port, r, val);
                let was_on = (old & 0x20) != 0;
                let is_on = (val & 0x20) != 0;
                let chan_off = (port << 8) | (r - 0xB0);
                let owners = self.freq_owners[&cid]
                    .get(&chan_off)
                    .cloned()
                    .unwrap_or_default();
                for fch in owners {
                    let key = (cid, fch);
                    if self.drums && fch >= self.states[&cid].melodic {
                        if is_on && !was_on && fch > self.states[&cid].melodic {
                            bump(&mut self.stats, "lost_melodic", 1);
                        }
                    } else if is_on && !was_on {
                        if fch < 12
                            && (fch & 1) != 0
                            && self.opl3
                            && self.states[&cid].is_four_op(fch - 1)
                        {
                            // the pair's first channel keys the whole voice
                        } else {
                            self.open_note(cid, fch, w.sample);
                        }
                    } else if was_on && !is_on {
                        self.close_note(cid, fch, w.sample, true);
                    }
                    if self.held.contains_key(&key) {
                        let (block, fnum) = self.states[&cid].frequency(fch);
                        if (block, fnum) != self.pitch_last[&key] {
                            self.pitch_pts
                                .get_mut(&key)
                                .unwrap()
                                .push((w.sample, block, fnum));
                            self.pitch_last.insert(key, (block, fnum));
                        }
                    }
                }
                continue;
            }

            let prev_val = i32::from(self.states[&cid].regs[addr as usize]);
            self.states.get_mut(&cid).unwrap().write(port, r, val);

            if (0xC0..=0xC8).contains(&r) {
                let chan_off = (port << 8) | (r - 0xC0);
                let owners = self.freq_owners[&cid]
                    .get(&chan_off)
                    .cloned()
                    .unwrap_or_default();
                for fch in owners {
                    let key = (cid, fch);
                    if !self.held.contains_key(&key)
                        || (self.drums && fch > self.states[&cid].melodic)
                    {
                        continue;
                    }
                    let old_fb = (prev_val >> 1) & 7;
                    let new_fb = (val >> 1) & 7;
                    if new_fb != old_fb {
                        let ch = self.base[&cid] + fch;
                        self.live_op_fx(key, ch, 0x11, 0, new_fb, w.sample);
                    }
                    let new_pan = self.states[&cid].pan_byte(fch);
                    if new_pan != 0 && Some(&new_pan) != self.pan_last.get(&key) {
                        let ch = self.base[&cid] + fch;
                        self.channel_fx
                            .push((w.sample, ch, 0x08, i32::from(new_pan)));
                        self.pan_last.insert(key, new_pan);
                    }
                }
                continue;
            }

            if (0xA0..=0xA8).contains(&r) {
                let chan_off = (port << 8) | (r - 0xA0);
                let owners = self.freq_owners[&cid]
                    .get(&chan_off)
                    .cloned()
                    .unwrap_or_default();
                for fch in owners {
                    let key = (cid, fch);
                    if !self.held.contains_key(&key) {
                        continue;
                    }
                    let (block, fnum) = self.states[&cid].frequency(fch);
                    if (block, fnum) != self.pitch_last[&key] {
                        self.pitch_pts
                            .get_mut(&key)
                            .unwrap()
                            .push((w.sample, block, fnum));
                        self.pitch_last.insert(key, (block, fnum));
                    }
                }
                continue;
            }

            let slot = r & 0x1F;
            let page = r - slot;
            if matches!(page, 0x20 | 0x40 | 0x60 | 0x80 | 0xE0) && slot <= 0x15 {
                let slot_key = (port << 8) | slot;
                let owners = self.op_owners[&cid]
                    .get(&slot_key)
                    .cloned()
                    .unwrap_or_default();
                if owners.is_empty() {
                    continue;
                }
                if page != 0x40 {
                    if prev_val == val {
                        continue; // the driver re-wrote the same byte; no change
                    }
                    for fch in owners {
                        if !self.held.contains_key(&(cid, fch)) {
                            continue;
                        }
                        if self.drums && fch > self.states[&cid].melodic {
                            continue;
                        }
                        let oi = match self.op_index[&cid].get(&(slot_key, fch)) {
                            Some(&oi) => oi,
                            None => continue,
                        };
                        let four = self.states[&cid].is_four_op(fch);
                        let fx_op = if four { ORDERED_OPS_L[oi as usize] } else { oi };
                        let ch = self.base[&cid] + fch;
                        if page == 0x20 {
                            for (bit, cmd) in
                                [(7, 0x50), (6, 0x53), (5, 0x55), (4, 0x5B)]
                            {
                                if ((prev_val >> bit) & 1) != ((val >> bit) & 1) {
                                    self.live_op_fx(
                                        (cid, fch),
                                        ch,
                                        cmd,
                                        fx_op,
                                        ((fx_op + 1) << 4) | ((val >> bit) & 1),
                                        w.sample,
                                    );
                                }
                            }
                            if (prev_val & 15) != (val & 15) {
                                self.live_op_fx(
                                    (cid, fch),
                                    ch,
                                    0x16,
                                    fx_op,
                                    ((fx_op + 1) << 4) | (val & 15),
                                    w.sample,
                                );
                            }
                        } else if page == 0x60 {
                            if ((prev_val >> 4) & 15) != ((val >> 4) & 15) {
                                self.live_op_fx(
                                    (cid, fch),
                                    ch,
                                    0x1A + fx_op,
                                    fx_op,
                                    (val >> 4) & 15,
                                    w.sample,
                                );
                            }
                            if (prev_val & 15) != (val & 15) {
                                self.live_op_fx(
                                    (cid, fch),
                                    ch,
                                    0x57 + fx_op,
                                    fx_op,
                                    val & 15,
                                    w.sample,
                                );
                            }
                        } else if page == 0x80 {
                            if ((prev_val >> 4) & 15) != ((val >> 4) & 15) {
                                self.live_op_fx(
                                    (cid, fch),
                                    ch,
                                    0x51,
                                    fx_op,
                                    ((fx_op + 1) << 4) | ((val >> 4) & 15),
                                    w.sample,
                                );
                            }
                            if (prev_val & 15) != (val & 15) {
                                self.live_op_fx(
                                    (cid, fch),
                                    ch,
                                    0x52,
                                    fx_op,
                                    ((fx_op + 1) << 4) | (val & 15),
                                    w.sample,
                                );
                            }
                        } else if page == 0xE0 {
                            let mask = if self.states[&cid].opl3 { 7 } else { 3 };
                            if (prev_val & mask) != (val & mask) {
                                self.live_op_fx(
                                    (cid, fch),
                                    ch,
                                    0x2A,
                                    fx_op,
                                    ((fx_op + 1) << 4) | (val & mask),
                                    w.sample,
                                );
                            }
                        }
                    }
                    continue;
                }
                for fch in owners {
                    let key = (cid, fch);
                    if !self.held.contains_key(&key) {
                        continue;
                    }
                    let oi = self.op_index[&cid].get(&(slot_key, fch)).copied();
                    if let Some(oi) = oi {
                        let here = val & 0x3F;
                        if self.op_tl_last.get(&(cid, fch, oi)) != Some(&here) {
                            self.op_tl_pts
                                .entry(key)
                                .or_default()
                                .entry(oi)
                                .or_default()
                                .push((w.sample, here));
                            self.op_tl_last.insert((cid, fch, oi), here);
                        }
                        if !self.op_is_carrier(cid, fch, oi) {
                            continue;
                        }
                    }
                    let cur = self.states[&cid].channel_tl(fch);
                    if cur != self.tl_last[&key] {
                        self.tl_pts.get_mut(&key).unwrap().push((w.sample, cur));
                        self.tl_last.insert(key, cur);
                    }
                }
            }
        }

        let held_keys: Vec<(i32, i32)> = self.held.keys().copied().collect();
        for (cid, fch) in held_keys {
            let end = if vgm.total_samples != 0 {
                vgm.total_samples
            } else {
                0
            };
            self.close_note(cid, fch, end, false);
        }

        if self.drums {
            let lost = self.stats["lost_melodic"];
            let cut = self.stats["mode_midnote"];
            let mut text =
                String::from("rhythm mode is on, so bank A's last FM channels are the kit");
            if lost != 0 || cut != 0 {
                text.push_str(&format!(
                    "; {lost} melodic key-on(s) there had no column and \
                     {cut} held note(s) stopped at a mode switch"
                ));
            }
            self.warnings.push(text);
        }
        if self.stats["repeat_keyon"] != 0 {
            self.warnings.push(format!(
                "{} repeated drum trigger byte(s) cannot retrigger a pattern cell; one hit is written",
                self.stats["repeat_keyon"]
            ));
        }
        drop_same_row_offs(self.events, &self.off_idx);
    }
}

fn drop_same_row_offs(events: &mut Vec<NoteEvent>, off_idx: &[usize]) {
    let mut ons_by_ch: HashMap<i32, Vec<i64>> = HashMap::new();
    for e in events.iter() {
        if e.on {
            ons_by_ch.entry(e.ch).or_default().push(e.sample);
        }
    }
    let drop: std::collections::HashSet<usize> = off_idx
        .iter()
        .copied()
        .filter(|&idx| {
            ons_by_ch
                .get(&events[idx].ch)
                .is_some_and(|ts| ts.iter().any(|&t| (t - events[idx].sample).abs() <= 2))
        })
        .collect();
    if !drop.is_empty() {
        let mut i = 0usize;
        events.retain(|_| {
            let keep = !drop.contains(&i);
            i += 1;
            keep
        });
    }
}

#[allow(clippy::too_many_arguments)]
pub fn _collect_opl(
    vgm: &VgmFile,
    events: &mut Vec<NoteEvent>,
    patches: &mut Patches,
    patch_order: &mut Vec<PatchKey>,
    keyon_times: &mut Vec<i64>,
    channel_fx: &mut Vec<ChannelFx>,
    chip_fx: &mut Vec<ChipFx>,
    kind: &str,
    ids: &[i32],
    base: &IndexMap<i32, i32>,
    clock: f64,
    drums: bool,
    chip_name: Option<&str>,
    freq_base: Option<i32>,
) -> (IndexMap<String, i64>, Vec<String>) {
    let opl3 = opl3_like(kind);
    let mut states: IndexMap<i32, OPLState> = IndexMap::new();
    for &cid in ids {
        states.insert(cid, OPLState::new(kind, drums));
    }
    let mut freq_owners: IndexMap<i32, HashMap<i32, Vec<i32>>> = IndexMap::new();
    let mut op_owners: IndexMap<i32, HashMap<i32, Vec<i32>>> = IndexMap::new();
    let mut op_index: IndexMap<i32, HashMap<(i32, i32), i32>> = IndexMap::new();
    let mut chan_ops: IndexMap<i32, HashMap<i32, Vec<(i32, u16)>>> = IndexMap::new();
    for &cid in ids {
        let st = &states[&cid];
        let mut fmap: HashMap<i32, Vec<i32>> = HashMap::new();
        let mut omap: HashMap<i32, Vec<i32>> = HashMap::new();
        let mut imap: HashMap<(i32, i32), i32> = HashMap::new();
        let mut ops_of: HashMap<i32, Vec<(i32, u16)>> = HashMap::new();
        for (fch, cm) in st.chan_map.iter().enumerate() {
            fmap.entry(i32::from(*cm)).or_default().push(fch as i32);
        }
        for row in 0..st.slots.len() {
            for (fch, slot) in st.slots[row].iter().enumerate() {
                if *slot == N {
                    continue;
                }
                let fch = fch as i32;
                let head = if !(row > 1 && (fch & 1) != 0) {
                    fch
                } else {
                    fch - 1
                };
                let four = st.is_four_op(head);
                // Rows 2/3 are the second bank's operators for the even
                // channels that *can* head a 4-op pair.
                if row > 1 && !four {
                    continue;
                }
                let addr = SLOT_ADDR[*slot as usize];
                let owners = omap.entry(i32::from(addr)).or_default();
                if !owners.contains(&fch) {
                    owners.push(fch);
                }
                let oi = if four {
                    ORDERED_OPS_L[row]
                } else {
                    row as i32
                };
                imap.entry((i32::from(addr), fch)).or_insert(oi);
                ops_of.entry(fch).or_default().push((oi, addr));
            }
        }
        freq_owners.insert(cid, fmap);
        op_owners.insert(cid, omap);
        op_index.insert(cid, imap);
        chan_ops.insert(cid, ops_of);
    }

    let mut stats: IndexMap<String, i64> = IndexMap::new();
    for key in [
        "notes",
        "drums",
        "four_op",
        "mode_midnote",
        "lost_melodic",
        "repeat_keyon",
        "ins_reused",
    ] {
        stats.insert(key.to_string(), 0);
    }

    let mut collector = OplCollector {
        kind,
        clock,
        drums,
        opl3,
        freq_base,
        states,
        freq_owners,
        op_owners,
        op_index,
        chan_ops,
        held: IndexMap::new(),
        pan_last: HashMap::new(),
        pending_reset: IndexMap::new(),
        pitch_pts: HashMap::new(),
        pitch_base: HashMap::new(),
        pitch_last: HashMap::new(),
        tl_pts: HashMap::new(),
        tl_last: HashMap::new(),
        tl_open: HashMap::new(),
        op_tl_pts: HashMap::new(),
        op_tl_last: HashMap::new(),
        off_idx: Vec::new(),
        ins_cap: 200,
        reuse_by_shape: HashMap::new(),
        last_amd: -1,
        last_vib: -1,
        events,
        patches,
        patch_order,
        keyon_times,
        channel_fx,
        chip_fx,
        base,
        stats,
        warnings: Vec::new(),
    };
    collector.run(vgm, chip_name);
    (collector.stats, collector.warnings)
}

// ---------------------------------------------------------------------------
// _collect_opll
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
struct PendingNote {
    t: i64,
    snap: FMPatch,
    vol: i32,
    volpts: Vec<(i64, i32)>,
    vol_last: i32,
    preset: Vec<(i64, i32)>,
    phase: Vec<i64>,
    preset_last: i32,
    fnum: (i32, i32),
}

struct OpllCollector<'a> {
    clock: f64,
    drums: bool,
    states: IndexMap<i32, OPLLState>,
    held: IndexMap<(i32, i32), usize>,
    released: HashMap<(i32, i32), i64>,
    pitch_pts: HashMap<(i32, i32), Vec<(i64, i32, i32)>>,
    pitch_base: HashMap<(i32, i32), (i32, i32)>,
    pitch_last: HashMap<(i32, i32), (i32, i32)>,
    vol_pts: HashMap<(i32, i32), Vec<(i64, i32)>>,
    vol_last: HashMap<(i32, i32), i32>,
    held_preset: HashMap<(i32, i32), i32>,
    preset_pts: HashMap<(i32, i32), Vec<(i64, i32)>>,
    phase_pts: HashMap<(i32, i32), Vec<i64>>,
    pending: IndexMap<(i32, i32), PendingNote>,
    off_idx: Vec<usize>,
    events: &'a mut Vec<NoteEvent>,
    patches: &'a mut Patches,
    patch_order: &'a mut Vec<PatchKey>,
    keyon_times: &'a mut Vec<i64>,
    channel_fx: &'a mut Vec<ChannelFx>,
    base: &'a IndexMap<i32, i32>,
    stats: IndexMap<String, i64>,
    warnings: Vec<String>,
}

impl<'a> OpllCollector<'a> {
    /// Furnace columns a chip channel's frequency registers drive.
    fn hw_channels(&self, hw: i32) -> Vec<i32> {
        if !self.drums {
            return if hw < 9 { vec![hw] } else { Vec::new() };
        }
        if hw < 6 {
            return vec![hw];
        }
        match hw {
            6 => vec![6],
            7 => vec![7, 10],
            8 => vec![8, 9],
            _ => Vec::new(),
        }
    }

    fn column_level(&self, cid: i32, fch: i32) -> i32 {
        let st = &self.states[&cid];
        if self.drums && fch >= 6 {
            for entry in OPLL_DRUMS {
                if entry.0 == fch {
                    return 15 - st.drum_volume(entry.3, entry.4);
                }
            }
            return 15;
        }
        15 - st.volume(fch)
    }

    fn intern_patch(&mut self, snap: FMPatch, cid: i32, fch: i32) -> i32 {
        let key;
        if self.drums && fch >= 6 {
            let mut words = vec![TAG_OPLL_DRUM];
            words.extend(snap.identity().0);
            key = PatchKey::Plain(words);
        } else {
            key = PatchKey::Plain(snap.identity().0);
        }
        if !self.patches.contains_key(&key) {
            let mut snap = snap;
            if self.drums && fch >= 6 {
                snap.name = "OPLL drums".to_string();
            } else if snap.preset != 0 {
                snap.name = format!("OPLL preset {:02}", snap.preset);
            } else {
                snap.name = format!("OPLL {:02}", self.patch_order.len());
            }
            self.patches.insert(key.clone(), snap);
            self.patch_order.push(key.clone());
        }
        let _ = cid;
        if let Some(p) = self.patches.get_mut(&key) {
            p.uses += 1;
        }
        self.patch_order.iter().position(|k| k == &key).unwrap() as i32
    }

    fn snapshot_for(&self, cid: i32, fch: i32) -> FMPatch {
        let st = &self.states[&cid];
        if self.drums && fch >= 6 {
            st.drum_patch()
        } else {
            st.snapshot(fch)
        }
    }

    fn close_note(&mut self, cid: i32, fch: i32, sample: i64, off: bool) {
        let key = (cid, fch);
        let idx = match self.held.shift_remove(&key) {
            Some(i) => i,
            None => return,
        };
        let ev_sample = self.events[idx].sample;
        let ev_ch = self.events[idx].ch;
        let rel = self.released.remove(&key);

        let mut pts = self.pitch_pts.remove(&key).unwrap_or_default();
        if rel.is_some() {
            pts.retain(|p| p.0 < sample);
        }
        if !pts.is_empty() {
            let mut merged: Vec<(i64, i32, i32)> = Vec::new();
            for p in &pts {
                match merged.last_mut() {
                    Some(last) if p.0 - last.0 <= 16 => *last = *p,
                    _ => merged.push(*p),
                }
            }
            let first_tail = (merged[0].1, merged[0].2);
            let pbase = self.pitch_base[&key];
            if merged[0].0 - ev_sample <= 120 && first_tail != pbase {
                let (note, e5) = opll_note_for(merged[0].1, merged[0].2, self.clock);
                self.events[idx].note = note;
                self.events[idx].e5 = e5;
                merged[0] = (ev_sample, merged[0].1, merged[0].2);
                self.pitch_base.insert(key, (merged[0].1, merged[0].2));
            }
            let mut distinct: Vec<(i64, i32, i32)> = Vec::new();
            for p in &merged {
                let same = match distinct.last() {
                    Some(last) => (p.1, p.2) == (last.1, last.2),
                    None => false,
                };
                if !same {
                    distinct.push(*p);
                }
            }
            if distinct.len() > 1 {
                self.events[idx].fm_pitch_pts = distinct;
            }
        }

        let mut vp = self.vol_pts.remove(&key).unwrap_or_default();
        if rel.is_some() {
            vp.retain(|p| p.0 < sample);
        }
        if !vp.is_empty() {
            self.events[idx].vol_pts = vp
                .iter()
                .map(|&(s, v)| (s, v.clamp(0, 15), None))
                .collect();
        }

        let mut pp = self.preset_pts.remove(&key).unwrap_or_default();
        if rel.is_some() {
            pp.retain(|p| p.0 < sample);
        }
        if !pp.is_empty() {
            self.events[idx].opll_preset_pts = pp;
        }

        let mut ph = self.phase_pts.remove(&key).unwrap_or_default();
        if rel.is_some() {
            ph.retain(|&t| t < sample);
        }
        if !ph.is_empty() {
            self.events[idx].opll_phase_pts = ph;
        }

        self.pitch_pts.remove(&key);
        self.pitch_base.remove(&key);
        self.pitch_last.remove(&key);
        self.vol_pts.remove(&key);
        self.vol_last.remove(&key);
        self.held_preset.remove(&key);
        self.preset_pts.remove(&key);
        self.phase_pts.remove(&key);

        if let Some(rel) = rel {
            self.events[idx].end = rel;
            self.events.push(NoteEvent::new(rel, ev_ch, false, 180, -1, -1, 0x80, 0));
            self.off_idx.push(self.events.len() - 1);
        } else if off {
            self.events[idx].end = sample;
            self.events.push(NoteEvent::new(
                sample, ev_ch, false, 180, -1, -1, 0x80, 0,
            ));
            self.off_idx.push(self.events.len() - 1);
        }
    }

    fn open_note(
        &mut self,
        cid: i32,
        fch: i32,
        sample: i64,
        snap: Option<FMPatch>,
        vol: Option<i32>,
        pend: Option<&PendingNote>,
    ) {
        self.close_note(cid, fch, sample, false);
        let mut hw = fch;
        if self.drums && fch >= 6 {
            for entry in OPLL_DRUMS {
                if entry.0 == fch {
                    hw = entry.2;
                    break;
                }
            }
        }
        let (block, fnum) = match pend {
            Some(p) => p.fnum,
            None => self.states[&cid].block_fnum(hw),
        };
        let (note, e5) = opll_note_for(block, fnum, self.clock);
        let snap = match snap {
            Some(s) => s,
            None => self.snapshot_for(cid, fch),
        };
        let preset_from_snap = snap.preset;
        let ins = self.intern_patch(snap, cid, fch);
        let vol = match vol {
            Some(v) => v,
            None => self.column_level(cid, fch),
        };
        let key = (cid, fch);
        let mut ev = NoteEvent::new(
            sample,
            self.base[&cid] + fch,
            true,
            note,
            ins,
            vol,
            e5,
            0,
        );
        ev.sweep_div = 2.0;
        ev.e5_lo = 0x00;
        ev.e5_hi = 0xFF;
        ev.drum = self.drums && fch >= 6;
        self.events.push(ev);
        self.held.insert(key, self.events.len() - 1);
        self.pitch_base.insert(key, (block, fnum));
        self.pitch_last.insert(key, (block, fnum));
        self.pitch_pts.insert(key, vec![(sample, block, fnum)]);
        self.vol_pts
            .insert(key, pend.map(|p| p.volpts.clone()).unwrap_or_default());
        self.vol_last.insert(key, pend.map(|p| p.vol_last).unwrap_or(vol));
        let held_preset = match pend {
            Some(p) if !p.preset.is_empty() => p.preset[p.preset.len() - 1].1,
            _ => {
                if !self.drums && hw < 9 {
                    preset_from_snap
                } else {
                    0
                }
            }
        };
        self.held_preset.insert(key, held_preset);
        self.preset_pts
            .insert(key, pend.map(|p| p.preset.clone()).unwrap_or_default());
        self.phase_pts
            .insert(key, pend.map(|p| p.phase.clone()).unwrap_or_default());
        self.keyon_times.push(sample);
        if self.drums && fch >= 6 {
            bump(&mut self.stats, "drums", 1);
        } else {
            bump(&mut self.stats, "notes", 1);
        }
    }

    fn touch_freq(&mut self, cid: i32, hw: i32, sample: i64) {
        let (block, fnum) = self.states[&cid].block_fnum(hw);
        for fch in self.hw_channels(hw) {
            let key = (cid, fch);
            if let Some(item) = self.pending.get_mut(&key) {
                item.fnum = (block, fnum);
            }
            if !self.pending.contains_key(&key)
                && self.held.contains_key(&key)
                && (block, fnum) != self.pitch_last[&key]
            {
                self.pitch_pts
                    .get_mut(&key)
                    .unwrap()
                    .push((sample, block, fnum));
                self.pitch_last.insert(key, (block, fnum));
            }
        }
    }

    fn update_level(&mut self, cid: i32, fch: i32, sample: i64) {
        let key = (cid, fch);
        let level = self.column_level(cid, fch);
        if let Some(item) = self.pending.get_mut(&key) {
            if level != item.vol_last {
                item.volpts.push((sample, level));
                item.vol_last = level;
            }
            return;
        }
        if self.held.contains_key(&key) && level != self.vol_last[&key] {
            self.vol_pts.get_mut(&key).unwrap().push((sample, level));
            self.vol_last.insert(key, level);
        }
    }

    fn keyed_here(w: &ChipWrite) -> bool {
        let r = w.reg & 0xFF;
        if (0x20..=0x28).contains(&r) {
            return w.val & 0x10 != 0;
        }
        if r == 0x0E {
            return w.val & 0x1F != 0;
        }
        false
    }

    fn ordered(writes: &[ChipWrite]) -> Vec<&ChipWrite> {
        let mut out: Vec<&ChipWrite> = Vec::with_capacity(writes.len());
        let mut i = 0usize;
        let n = writes.len();
        while i < n {
            let mut j = i + 1;
            let s = writes[i].sample;
            while j < n && writes[j].sample == s {
                j += 1;
            }
            let group = &writes[i..j];
            out.extend(group.iter().filter(|w| !Self::keyed_here(w)));
            out.extend(group.iter().filter(|w| Self::keyed_here(w)));
            i = j;
        }
        out
    }

    fn key_on_edge(&mut self, cid: i32, fch: i32, sample: i64) {
        self.flush_one(cid, fch);
        let snap = self.snapshot_for(cid, fch);
        let vol = self.column_level(cid, fch);
        let mut hw = fch;
        if self.drums && fch >= 6 {
            for entry in OPLL_DRUMS {
                if entry.0 == fch {
                    hw = entry.2;
                    break;
                }
            }
        }
        let preset_last = if !(self.drums && fch >= 6) {
            snap.preset
        } else {
            0
        };
        let fnum = self.states[&cid].block_fnum(hw);
        self.pending.insert(
            (cid, fch),
            PendingNote {
                t: sample,
                snap,
                vol,
                volpts: Vec::new(),
                vol_last: vol,
                preset: Vec::new(),
                phase: Vec::new(),
                preset_last,
                fnum,
            },
        );
    }

    fn flush_one(&mut self, cid: i32, fch: i32) {
        if let Some(item) = self.pending.shift_remove(&(cid, fch)) {
            self.open_note(
                cid,
                fch,
                item.t,
                Some(item.snap.clone()),
                Some(item.vol),
                Some(&item),
            );
        }
    }

    fn flush_due(&mut self, sample: i64) {
        let keys: Vec<(i32, i32)> = self.pending.keys().copied().collect();
        for key in keys {
            let due = self.pending.get(&key).is_some_and(|it| it.t + OPLL_SETTLE < sample);
            if due {
                let item = self.pending.shift_remove(&key).unwrap();
                self.open_note(
                    key.0,
                    key.1,
                    item.t,
                    Some(item.snap.clone()),
                    Some(item.vol),
                    Some(&item),
                );
            }
        }
    }

    fn release_note(&mut self, cid: i32, fch: i32, sample: i64) {
        let key = (cid, fch);
        if !self.held.contains_key(&key) {
            return;
        }
        self.released.insert(key, sample);
    }

    fn run(&mut self, vgm: &VgmFile) {
        let stream = Self::ordered(&vgm.writes);
        for w in stream {
            // Settle first, but only once the window is fully behind this
            // write: a register landing exactly at the window's end belongs
            // to the key.
            self.flush_due(w.sample);
            if w.chip != "opll" {
                continue;
            }
            let cid = i32::from(w.chip_id);
            if !self.states.contains_key(&cid) {
                continue;
            }
            let r = w.reg & 0xFF;
            let val = w.val & 0xFF;
            let prev = i32::from(self.states[&cid].regs[r as usize]);

            if r == 0x0E {
                self.states.get_mut(&cid).unwrap().write(r, val);
                let was_rhythm = (prev & 0x20) != 0;
                let is_rhythm = (val & 0x20) != 0;
                let old_trig = if was_rhythm { prev & 0x1F } else { 0 };
                let trig = if is_rhythm { (val & 0x1F) as i32 } else { 0 };
                if is_rhythm && !was_rhythm && !self.drums {
                    // The kit takes channels 6-8 away from the melodic section.
                    for hw in [6, 7, 8] {
                        for fch in self.hw_channels(hw) {
                            self.flush_one(cid, fch);
                            self.close_note(cid, fch, w.sample, true);
                        }
                    }
                }
                if self.drums {
                    for entry in OPLL_DRUMS {
                        let (fch, _name, _hw, _reg, _shift, bit) = entry;
                        let bit = bit as i32;
                        if (trig & bit) != 0 && (old_trig & bit) == 0 && is_rhythm {
                            self.key_on_edge(cid, fch, w.sample);
                        } else if (old_trig & bit) != 0 && (trig & bit) == 0 {
                            self.flush_one(cid, fch);
                            self.close_note(cid, fch, w.sample, true);
                        }
                    }
                }
                continue;
            }

            if (0x10..=0x18).contains(&r) {
                self.states.get_mut(&cid).unwrap().write(r, val);
                self.touch_freq(cid, (r - 0x10) as i32, w.sample);
                continue;
            }

            if (0x20..=0x28).contains(&r) {
                self.states.get_mut(&cid).unwrap().write(r, val);
                let hw = (r - 0x20) as i32;
                let was_on = (prev & 0x10) != 0;
                let is_on = (val & 0x10) != 0;
                if self.drums {
                    if hw >= 6 {
                        if is_on && !was_on {
                            bump(&mut self.stats, "lost_melodic", 1);
                        }
                    } else if is_on && !was_on {
                        self.key_on_edge(cid, hw, w.sample);
                    } else if was_on && !is_on {
                        self.flush_one(cid, hw);
                        self.release_note(cid, hw, w.sample);
                    }
                } else if is_on && !was_on {
                    self.key_on_edge(cid, hw, w.sample);
                } else if was_on && !is_on {
                    self.flush_one(cid, hw);
                    self.release_note(cid, hw, w.sample);
                }
                self.touch_freq(cid, hw, w.sample);
                continue;
            }

            if (0x30..=0x38).contains(&r) {
                self.states.get_mut(&cid).unwrap().write(r, val);
                let hw = (r - 0x30) as i32;
                if self.drums && hw >= 6 {
                    for entry in OPLL_DRUMS {
                        if entry.3 == r {
                            self.update_level(cid, entry.0, w.sample);
                        }
                    }
                } else {
                    let key = (cid, hw);
                    let preset = self.states[&cid].preset(hw);
                    if self.pending.contains_key(&key)
                        && preset != self.pending[&key].preset_last
                    {
                        let item = self.pending.get_mut(&key).unwrap();
                        item.preset.push((w.sample, preset));
                        item.preset_last = preset;
                    }
                    if self.held.contains_key(&key) && preset != self.held_preset[&key] {
                        let ev_ins = self.events[self.held[&key]].ins;
                        let bank = if 0 <= ev_ins && (ev_ins as usize) < self.patch_order.len()
                        {
                            self.patches[&self.patch_order[ev_ins as usize]].preset == 0
                        } else {
                            false
                        };
                        self.preset_pts
                            .get_mut(&key)
                            .unwrap()
                            .push((w.sample, preset));
                        if preset != 0 || bank {
                            let ch = self.base[&cid] + hw;
                            self.channel_fx.push((w.sample, ch, 0x10, preset));
                            bump(&mut self.stats, "inst_fx", 1);
                        } else if !self.released.contains_key(&key) {
                            bump(&mut self.stats, "midnote_inst", 1);
                        }
                        self.held_preset.insert(key, preset);
                    }
                    self.update_level(cid, hw, w.sample);
                }
                continue;
            }

            // $00-$07 hold the single user instrument.
            self.states.get_mut(&cid).unwrap().write(r, val);
            if r <= 0x07 {
                let keys: Vec<(i32, i32)> = self.held.keys().copied().collect();
                for key in keys {
                    if self.released.contains_key(&key) {
                        continue;
                    }
                    if key.0 == cid && self.held_preset.get(&key).copied().unwrap_or(0) == 0 {
                        bump(&mut self.stats, "midnote_inst", 1);
                        break;
                    }
                }
            }
        }

        let pending_keys: Vec<(i32, i32)> = self.pending.keys().copied().collect();
        for (cid, fch) in pending_keys {
            self.flush_one(cid, fch);
        }
        let held_keys: Vec<(i32, i32)> = self.held.keys().copied().collect();
        for (cid, fch) in held_keys {
            let end = if vgm.total_samples != 0 {
                vgm.total_samples
            } else {
                0
            };
            self.close_note(cid, fch, end, false);
        }

        if self.stats["midnote_inst"] != 0 {
            self.warnings.push(format!(
                "{} user-patch change(s) landed inside a held OPLL note; the next key-on picks them up",
                self.stats["midnote_inst"]
            ));
        }
        if self.drums && self.stats["lost_melodic"] != 0 {
            self.warnings.push(format!(
                "rhythm mode is on, so OPLL channels 7-9 are the kit; \
                 {} melodic key-on(s) there had no column",
                self.stats["lost_melodic"]
            ));
        }
        drop_same_row_offs(self.events, &self.off_idx);
    }
}

#[allow(clippy::too_many_arguments)]
pub fn _collect_opll(
    vgm: &VgmFile,
    events: &mut Vec<NoteEvent>,
    patches: &mut Patches,
    patch_order: &mut Vec<PatchKey>,
    keyon_times: &mut Vec<i64>,
    channel_fx: &mut Vec<ChannelFx>,
    _chip_fx: &mut Vec<ChipFx>,
    ids: &[i32],
    base: &IndexMap<i32, i32>,
    clock: f64,
    drums: bool,
) -> (IndexMap<String, i64>, Vec<String>) {
    let mut states: IndexMap<i32, OPLLState> = IndexMap::new();
    for &cid in ids {
        states.insert(cid, OPLLState::new(drums));
    }
    let mut stats: IndexMap<String, i64> = IndexMap::new();
    for key in ["notes", "drums", "midnote_inst", "inst_fx", "lost_melodic"] {
        stats.insert(key.to_string(), 0);
    }
    let mut collector = OpllCollector {
        clock,
        drums,
        states,
        held: IndexMap::new(),
        released: HashMap::new(),
        pitch_pts: HashMap::new(),
        pitch_base: HashMap::new(),
        pitch_last: HashMap::new(),
        vol_pts: HashMap::new(),
        vol_last: HashMap::new(),
        held_preset: HashMap::new(),
        preset_pts: HashMap::new(),
        phase_pts: HashMap::new(),
        pending: IndexMap::new(),
        off_idx: Vec::new(),
        events,
        patches,
        patch_order,
        keyon_times,
        channel_fx,
        base,
        stats,
        warnings: Vec::new(),
    };
    collector.run(vgm);
    (collector.stats, collector.warnings)
}

// ---------------------------------------------------------------------------
// Layout and family helpers
// ---------------------------------------------------------------------------

/// Decide which OPL layout a log should become, and count what each costs.
pub fn _opl_rhythm_plan(
    vgm: &VgmFile,
    kind: &str,
    chip_name: Option<&str>,
) -> (bool, IndexMap<String, i64>) {
    // The OPL4's FM half is OPL3-shaped.
    let st = OPLState::new(if kind == "opl4" { KIND_OPL3 } else { kind }, false);
    let kit_regs: std::collections::HashSet<i32> = st
        .chan_map
        .iter()
        .enumerate()
        .filter(|(_, cm)| ((**cm >> 8) & 1) == 0 && (**cm & 0xFF) >= 6)
        .map(|(fch, _)| fch as i32)
        .collect();
    let mut chan_of: HashMap<(i32, u32), Vec<i32>> = HashMap::new();
    for (fch, cm) in st.chan_map.iter().enumerate() {
        let port = if st.opl3 { i32::from((*cm >> 8) & 1) } else { 0 };
        chan_of
            .entry((port, u32::from(*cm & 0xFF)))
            .or_default()
            .push(fch as i32);
    }

    let mut stats: IndexMap<String, i64> = IndexMap::new();
    for key in ["latches", "mode_samples", "melodic_on_kit"] {
        stats.insert(key.to_string(), 0);
    }
    let mut per: IndexMap<u8, Vec<&ChipWrite>> = IndexMap::new();
    let source = chip_name.unwrap_or(kind);
    for w in &vgm.writes {
        if w.chip == source {
            per.entry(w.chip_id).or_default().push(w);
        }
    }
    let end = if vgm.total_samples != 0 {
        vgm.total_samples
    } else {
        0
    };
    for ws in per.values() {
        let mut mode = false;
        let mut since: i64 = 0;
        let mut trig: u32 = 0;
        let mut keyed: HashMap<(i32, u32), bool> = HashMap::new();
        for w in ws.iter() {
            let port = if st.opl3 {
                ((w.reg >> 8) & 1) as i32
            } else {
                0
            };
            let r = w.reg & 0xFF;
            if r == 0xBD && port == 0 {
                let now = w.val & 0x20 != 0;
                if now != mode {
                    *stats
                        .entry("segments".to_string())
                        .or_insert(0) += 1;
                    if mode {
                        *stats.get_mut("mode_samples").unwrap() += w.sample - since;
                    }
                    mode = now;
                    since = w.sample;
                }
                let bits = if now { w.val & 0x1F } else { 0 };
                for k in 0..5 {
                    let bit = 1u32 << k;
                    if bits & bit != 0 && trig & bit == 0 {
                        *stats.get_mut("latches").unwrap() += 1;
                    }
                }
                trig = bits;
                continue;
            }
            if (0xB0..=0xB8).contains(&r) {
                let on = w.val & 0x20 != 0;
                let was = keyed.get(&(port, r)).copied().unwrap_or(false);
                keyed.insert((port, r), on);
                if on && !was && !mode {
                    let count = chan_of
                        .get(&(port, r - 0xB0))
                        .map(|fchs| fchs.iter().filter(|fch| kit_regs.contains(*fch)).count())
                        .unwrap_or(0);
                    *stats.get_mut("melodic_on_kit").unwrap() += count as i64;
                }
            }
        }
        if mode && end > since {
            *stats.get_mut("mode_samples").unwrap() += end - since;
        }
    }
    (stats["latches"] != 0, stats)
}

/// The volume column's ceiling for a module's main FM family.
pub fn _fm_vol_max(systems: &[FmSystem], is_opll: bool, is_opl: bool) -> i32 {
    let kind: &str = if !systems.is_empty() {
        &systems[0].kind
    } else if is_opll {
        "opll"
    } else if is_opl {
        "opl"
    } else {
        "ym2151"
    };
    if kind == "opll" {
        return 15;
    }
    if kind == "opl" || kind == "opl2" || kind == "opl3" {
        return 63;
    }
    127
}

/// Rising key-on edges of the named chip families.
pub fn _family_keyons(vgm: &VgmFile, chips: &impl ChipSet) -> i64 {
    let mut count: i64 = 0;
    let mut fm_mask: HashMap<(&'static str, i32, i32), i32> = HashMap::new();
    let mut opl_on: HashMap<(i32, i32, i32), bool> = HashMap::new();
    let mut opll_on: HashMap<(i32, i32), bool> = HashMap::new();
    let mut rhythm: HashMap<i32, u32> = HashMap::new();
    let mut opll_rhythm: HashMap<i32, u32> = HashMap::new();
    let mut opll_mode: HashMap<i32, bool> = HashMap::new();
    for w in &vgm.writes {
        if !chips.has(w.chip) {
            continue;
        }
        let cid = i32::from(w.chip_id);
        if w.chip == "ym2151" {
            if w.reg == 0x08 {
                let ch = (w.val & 7) as i32;
                let mask = ((w.val & 0x78) >> 3) as i32;
                let key = ("ym2151", cid, ch);
                let prev = fm_mask.get(&key).copied().unwrap_or(0);
                if mask != 0 && prev == 0 {
                    count += 1;
                }
                fm_mask.insert(key, mask);
            }
        } else if w.chip == "ym2203" {
            if (w.reg & 0xFF) == 0x28 {
                let ch = (w.val & 3) as i32;
                let mask = ((w.val >> 4) & 0x0F) as i32;
                let key = ("ym2203", cid, ch);
                let prev = fm_mask.get(&key).copied().unwrap_or(0);
                if mask != 0 && prev == 0 {
                    count += 1;
                }
                fm_mask.insert(key, mask);
            }
        } else if w.chip == "ym2612" {
            if (w.reg & 0x100) == 0 && (w.reg & 0xFF) == 0x28 {
                let raw = (w.val & 7) as i32;
                if raw == 3 || raw == 7 {
                    continue;
                }
                let ch = if raw < 3 { raw } else { raw - 1 };
                let mask = ((w.val >> 4) & 0x0F) as i32;
                let key = ("ym2612", cid, ch);
                let prev = fm_mask.get(&key).copied().unwrap_or(0);
                if mask != 0 && prev == 0 {
                    count += 1;
                }
                fm_mask.insert(key, mask);
            }
        } else if w.chip == "ym2608" || w.chip == "ym2610" {
            if (w.reg & 0x100) == 0 && (w.reg & 0xFF) == 0x28 {
                let raw = (w.val & 7) as i32;
                if raw == 3 || raw == 7 {
                    continue;
                }
                let ch = if raw < 3 { raw } else { raw - 1 };
                if w.chip == "ym2610" && ch > 2 {
                    continue;
                }
                let mask = ((w.val >> 4) & 0x0F) as i32;
                let key = (w.chip, cid, ch);
                let prev = fm_mask.get(&key).copied().unwrap_or(0);
                if mask != 0 && prev == 0 {
                    count += 1;
                }
                fm_mask.insert(key, mask);
            }
        } else if w.chip == "opll" {
            let r = w.reg & 0xFF;
            let mode = opll_mode.get(&cid).copied().unwrap_or(false);
            if (0x20..=0x28).contains(&r) {
                let key = (cid, r as i32);
                let on = (w.val & 0x10) != 0 && !(mode && r >= 0x26);
                if on && !opll_on.get(&key).copied().unwrap_or(false) {
                    count += 1;
                }
                opll_on.insert(key, on);
            } else if r == 0x0E {
                let now = w.val & 0x20 != 0;
                opll_mode.insert(cid, now);
                let sel = if now { w.val & 0x1F } else { 0 };
                let prev = opll_rhythm.get(&cid).copied().unwrap_or(0);
                count += i64::from((sel & !prev).count_ones());
                opll_rhythm.insert(cid, sel);
            }
        } else if w.chip == "opl"
            || w.chip == "opl2"
            || w.chip == "opl3"
            || w.chip == crate::opl::KIND_Y8950
            || w.chip == "ymf278b"
        {
            let r = w.reg & 0xFF;
            let port = ((w.reg >> 8) & 1) as i32;
            if (0xB0..=0xB8).contains(&r) {
                let key = (cid, port, r as i32);
                let on = w.val & 0x20 != 0;
                if on && !opl_on.get(&key).copied().unwrap_or(false) {
                    count += 1;
                }
                opl_on.insert(key, on);
            } else if r == 0xBD && port == 0 {
                let trig = if w.val & 0x20 != 0 { w.val & 0x1F } else { 0 };
                let prev = rhythm.get(&cid).copied().unwrap_or(0);
                count += i64::from((trig & !prev).count_ones());
                rhythm.insert(cid, trig);
            }
        }
    }
    count
}

/// Whether a chip family has anything a module can play.
pub fn _family_sounds(vgm: &VgmFile, names: &impl ChipSet) -> bool {
    if _family_keyons(vgm, names) != 0 {
        return true;
    }
    for w in &vgm.writes {
        if !names.has(w.chip) {
            continue;
        }
        let reg = w.reg & 0xFF;
        if w.chip.ends_with("_ssg") {
            // $0-$5 are the coarse period bytes: a tuned voice can sound.
            if reg <= 5 && w.val != 0 {
                return true;
            }
        } else if w.chip == "ym2608" && (w.reg & 0x100) == 0 {
            // Port 0's $10 latches the six kit triggers.
            if reg == 0x10 && (w.val & 0x3F) != 0 {
                return true;
            }
        } else if w.chip == "ym2610" && (w.reg & 0x100) != 0 {
            // Port 1 of the YM2610 is ADPCM-A; $10 carries its triggers.
            if reg == 0x10 && (w.val & 0xF0) != 0 {
                return true;
            }
        } else if w.chip == crate::opl::KIND_Y8950 {
            // $07 bit 7 starts the delta-T voice.
            if reg == 0x07 && (w.val & 0x80) != 0 && (w.val & 0x01) == 0 {
                return true;
            }
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vgm::ChipWrite;

    fn write(vgm: &mut VgmFile, sample: i64, chip: &'static str, reg: u32, val: u32) {
        vgm.writes.push(ChipWrite::new(sample, chip, 0, reg, val));
    }

    fn empty_shards() -> (
        Vec<NoteEvent>,
        Patches,
        Vec<PatchKey>,
        Vec<i64>,
        Vec<ChannelFx>,
        Vec<ChipFx>,
        IndexMap<i32, i32>,
    ) {
        (
            Vec::new(),
            IndexMap::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            IndexMap::from([(0, 0)]),
        )
    }

    #[test]
    fn opl2_key_on_opens_a_note() {
        let mut vgm = VgmFile::default();
        let clock = 3_579_545.0;
        // Block 4, f-num 0x1C8 on channel 0, then key on.
        write(&mut vgm, 0, "opl", 0xA0, 0xC8);
        write(&mut vgm, 0, "opl", 0xB0, 0x10);
        write(&mut vgm, 100, "opl", 0xB0, 0x30);
        vgm.total_samples = 1000;
        let (mut events, mut patches, mut order, mut keyons, mut chan_fx, mut chip_fx, base) =
            empty_shards();
        let (stats, warnings) = _collect_opl(
            &vgm,
            &mut events,
            &mut patches,
            &mut order,
            &mut keyons,
            &mut chan_fx,
            &mut chip_fx,
            "opl",
            &[0],
            &base,
            crate::opl::opl_module_clock(clock, false),
            false,
            None,
            None,
        );
        assert_eq!(stats["notes"], 1);
        assert_eq!(stats["drums"], 0);
        assert!(warnings.is_empty());
        let on = events.iter().find(|e| e.on).expect("one note");
        assert_eq!(on.ch, 0);
        assert_eq!(on.sweep_div, 2.0);
        assert_eq!(on.e5_lo, 0x00);
        assert_eq!(on.e5_hi, 0xFF);
        assert_eq!(keyons, vec![100]);
        assert!(order.len() == 1);
        assert!(patches[&order[0]].name.starts_with("OPL "));
    }

    #[test]
    fn rhythm_latch_opens_the_hihat_column() {
        let mut vgm = VgmFile::default();
        // Rhythm on + the hi-hat bit: channel 10 is a one-operator kit column.
        write(&mut vgm, 0, "opl", 0xBD, 0x21);
        vgm.total_samples = 500;
        let (mut events, mut patches, mut order, mut keyons, mut chan_fx, mut chip_fx, base) =
            empty_shards();
        let (stats, warnings) = _collect_opl(
            &vgm,
            &mut events,
            &mut patches,
            &mut order,
            &mut keyons,
            &mut chan_fx,
            &mut chip_fx,
            "opl",
            &[0],
            &base,
            crate::opl::opl_module_clock(3_579_545.0, false),
            true,
            None,
            None,
        );
        assert_eq!(stats["drums"], 1);
        assert_eq!(stats["notes"], 0);
        let on = events.iter().find(|e| e.on).unwrap();
        assert_eq!(on.ch, 10);
        assert!(on.drum);
        assert!(warnings[0].starts_with("rhythm mode is on"));
        assert!(patches[&order[0]].name == "OPL drum 00");
    }

    #[test]
    fn opl3_four_op_pair_counts_once() {
        let mut vgm = VgmFile::default();
        write(&mut vgm, 0, "opl3", 0x104, 0x01); // pair 0 heads 4 ops
        write(&mut vgm, 0, "opl3", 0xA0, 0x80);
        write(&mut vgm, 0, "opl3", 0xB0, 0x10);
        write(&mut vgm, 100, "opl3", 0xB0, 0x30);
        vgm.total_samples = 1000;
        let (mut events, mut patches, mut order, mut keyons, mut chan_fx, mut chip_fx, base) =
            empty_shards();
        let (stats, _w) = _collect_opl(
            &vgm,
            &mut events,
            &mut patches,
            &mut order,
            &mut keyons,
            &mut chan_fx,
            &mut chip_fx,
            "opl3",
            &[0],
            &base,
            crate::opl::opl_module_clock(14_318_180.0, true),
            false,
            None,
            None,
        );
        assert_eq!(stats["four_op"], 1);
        assert_eq!(stats["notes"], 1);
        assert!(patches[&order[0]].four);
        assert_eq!(patches[&order[0]].name, "OPL3 4op 00");
    }

    #[test]
    fn family_keyons_counts_rising_edges_only() {
        let mut vgm = VgmFile::default();
        write(&mut vgm, 0, "ym2612", 0x28, 0xF0);
        write(&mut vgm, 50, "ym2612", 0x28, 0xF0); // already keyed, same mask
        write(&mut vgm, 100, "ym2612", 0x28, 0x00);
        write(&mut vgm, 150, "ym2612", 0x28, 0x70);
        assert_eq!(_family_keyons(&vgm, &std::collections::HashSet::from(["ym2612"])), 2);
        assert_eq!(_family_keyons(&vgm, &std::collections::HashSet::from(["ym2203"])), 0);
    }

    #[test]
    fn family_sounds_ignores_bare_setup() {
        let mut vgm = VgmFile::default();
        write(&mut vgm, 0, "ym2608", 0x10, 0x00); // empty kit latch
        assert!(!_family_sounds(&vgm, &std::collections::HashSet::from(["ym2608"])));
        write(&mut vgm, 10, "ym2608", 0x10, 0x01); // a real drum trigger
        assert!(_family_sounds(&vgm, &std::collections::HashSet::from(["ym2608"])));
        let mut vgm = VgmFile::default();
        write(&mut vgm, 0, "ym2203_ssg", 0x00, 0x40);
        assert!(_family_sounds(&vgm, &std::collections::HashSet::from(["ym2203_ssg"])));
    }

    #[test]
    fn volume_ceiling_follows_the_first_system() {
        let opl = FmSystem {
            kind: "opl3".to_string(),
            ..FmSystem::default()
        };
        let opll = FmSystem {
            kind: "opll".to_string(),
            ..FmSystem::default()
        };
        assert_eq!(_fm_vol_max(&[opl], false, true), 63);
        assert_eq!(_fm_vol_max(&[opll], true, false), 15);
        assert_eq!(_fm_vol_max(&[], false, false), 127);
        assert_eq!(_fm_vol_max(&[], true, false), 15);
        assert_eq!(_fm_vol_max(&[], false, true), 63);
    }

    #[test]
    fn opl_rhythm_plan_requires_a_firing_latch() {
        let mut vgm = VgmFile::default();
        // Bit 5 poked during init and cleared in the same sample: no drum.
        write(&mut vgm, 0, "opl2", 0xBD, 0x20);
        write(&mut vgm, 0, "opl2", 0xBD, 0x00);
        vgm.total_samples = 100;
        let (drums, stats) = _opl_rhythm_plan(&vgm, "opl2", None);
        assert!(!drums);
        assert_eq!(stats["latches"], 0);
        assert_eq!(stats["segments"], 2);

        let mut vgm = VgmFile::default();
        write(&mut vgm, 0, "opl2", 0xBD, 0x30); // rhythm + kick
        write(&mut vgm, 200, "opl2", 0xBD, 0x00);
        vgm.total_samples = 400;
        let (drums, stats) = _opl_rhythm_plan(&vgm, "opl2", None);
        assert!(drums);
        assert_eq!(stats["latches"], 1);
        assert_eq!(stats["mode_samples"], 200);
    }
}
