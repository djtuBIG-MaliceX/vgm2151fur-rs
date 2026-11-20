//! Pattern row model and the low-level row/PATN encoders.
//!
//! Port of `furwrite.py` lines 2202-2448 and 2665-2677: `Row`,
//! `_blank_row`, `_bump`, `loss_total`, `keyon_loss`, `dev_max`,
//! `_slide_level`, `_add_fx`, `_add_quad_fx`, `_encode_patn_row`,
//! `_write_patn`, `_compat_phase1`, `_compat_phase2`, `_fx_keep`.

use indexmap::IndexMap;

use super::buf::{Buf, EMPTY};

/// Every counter the writer raises when content cannot be placed as written.
/// `loss_total` is the gate the condense modes compare against the 1x baseline.
pub const LOSS_KEYS: [&str; 11] = [
    "note_collision",   // a second note landed on a cell that already had one
    "off_swallowed",    // a note-off collided with a note
    "pcm_evicted",      // _assign_pcm_rows dropped an earlier trigger
    "pcm_unplaced",     // a PCM trigger got no row at all
    "fx_dropped",       // more effects on a row than it has columns
    "sweep_clipped",    // a pitch ramp was cut short by a later note
    "vol_step_dropped", // a fade/TL step lost to a note owning the row
    "e5_skipped",       // a fine-tune step lost to a foreign note row
    "legato_skipped",   // a quick-legato transpose lost to a foreign row
    "ed_stripped_loop", // an ED delay removed so Bxx could survive
    "orders_truncated", // >256 orders needed
];

/// One pattern cell.
#[derive(Clone, Debug)]
pub struct Row {
    /// furnace 0-179, 180=off, -1 empty
    pub note: i32,
    pub ins: i32,
    pub vol: i32,
    /// None until the row holds an effect. Blank rows are the overwhelming
    /// majority of a pattern, so the slot list is made on the first effect.
    pub fx: Option<Vec<(i32, i32)>>,
}

impl Default for Row {
    fn default() -> Self {
        Row {
            note: -1,
            ins: EMPTY,
            vol: EMPTY,
            fx: None,
        }
    }
}

impl Row {
    pub fn is_empty(&self) -> bool {
        if self.note >= 0 || self.ins != EMPTY || self.vol != EMPTY {
            return false;
        }
        match &self.fx {
            None => true,
            Some(fx) => fx.iter().all(|&(c, _)| c == EMPTY),
        }
    }
}

pub fn _blank_row() -> Row {
    Row::default()
}

/// Count a structural loss event (dropped/overwritten/retriggered content).
pub fn _bump(stats: Option<&mut IndexMap<String, i64>>, key: &str, n: i64) {
    if let Some(stats) = stats {
        *stats.entry(key.to_string()).or_insert(0) += n;
    }
}

pub fn loss_total(stats: &IndexMap<String, i64>) -> i64 {
    LOSS_KEYS
        .iter()
        .map(|k| *stats.get(*k).unwrap_or(&0))
        .sum()
}

/// Source key-ons the writer could not place on a pattern cell.
pub fn keyon_loss(stats: Option<&IndexMap<String, i64>>) -> i64 {
    match stats {
        None => 0,
        Some(m) if m.is_empty() => 0,
        Some(m) => ["note_collision", "pcm_unplaced", "pcm_evicted", "orders_truncated"]
            .iter()
            .map(|k| *m.get(*k).unwrap_or(&0))
            .sum(),
    }
}

/// Worst per-row pitch deviation from the driver's steps, in 1/64 st.
///
/// The fidelity number the condense pick compares; it grows as the grid
/// coarsens because a row can only render a straight-line slide.
pub fn dev_max(stats: &IndexMap<String, i64>) -> i64 {
    *stats.get("sweep_dev_max").unwrap_or(&0)
}

/// Column value that lands on `level` after the same-tick FA01 step.
///
/// The row sets the column, then the slide runs. 15 is the top of the
/// column, so a tooth that peaks there plays one level low.
pub fn _slide_level(level: i32) -> i32 {
    if level >= 15 {
        return 15;
    }
    level + 1
}

pub fn _add_fx(
    row: &mut Row,
    cmd: i32,
    val: i32,
    fx_cols: usize,
    stats: Option<&mut IndexMap<String, i64>>,
) {
    if row.fx.is_none() {
        row.fx = Some(vec![(EMPTY, EMPTY); fx_cols]);
    }
    let fx = row.fx.as_mut().unwrap();
    for pair in fx.iter_mut() {
        if pair.0 == cmd {
            *pair = (cmd, val);
            return;
        }
    }
    for pair in fx.iter_mut() {
        if pair.0 == EMPTY {
            *pair = (cmd, val);
            return;
        }
    }
    if !fx.is_empty() {
        *fx.last_mut().unwrap() = (cmd, val);
        _bump(stats, "fx_dropped", 1);
    } else if fx_cols > 0 {
        fx.push((cmd, val));
    }
}

/// C352 quad pan: front pair 0x81/0x82, rear pair 0x89/0x8A. The platform
/// scales the volume into all four, and a quad module carries the level here.
pub const QUAD_FX: (i32, i32, i32, i32) = (0x81, 0x82, 0x89, 0x8A);

/// Write the four C352 pan slots, skipping any already in force.
pub fn _add_quad_fx(
    row: &mut Row,
    quad: (i32, i32, i32, i32),
    fx_cols: usize,
    mut stats: Option<&mut IndexMap<String, i64>>,
    prev: Option<(i32, i32, i32, i32)>,
) {
    let quad_arr = [quad.0, quad.1, quad.2, quad.3];
    let prev_arr = prev.map(|p| [p.0, p.1, p.2, p.3]);
    let cmd_arr = [QUAD_FX.0, QUAD_FX.1, QUAD_FX.2, QUAD_FX.3];
    for i in 0..4 {
        let val = quad_arr[i];
        if let Some(prev) = prev_arr {
            if prev[i] == val {
                continue;
            }
        }
        _add_fx(row, cmd_arr[i], val & 0xFF, fx_cols, stats.as_deref_mut());
    }
}

/// One PATN row. Empty -> 0x00 (skip 1).
pub fn _encode_patn_row(row: &Row, fx_cols: usize) -> Vec<u8> {
    if row.is_empty() {
        return vec![0x00];
    }
    let fx: Vec<(i32, i32)> = match &row.fx {
        None => Vec::new(),
        Some(fx) => fx
            .iter()
            .take(fx_cols)
            .copied()
            .filter(|&(c, _)| c != EMPTY)
            .collect(),
    };
    let mut mask: i32 = 0;
    let mut extra: i32 = 0;
    let mut payload: Vec<u8> = Vec::new();
    // extra is the 16-bit effect presence mask (type/value pairs).
    if row.note >= 0 {
        mask |= 0x01;
        payload.push((row.note & 0xFF) as u8);
    }
    if row.ins != EMPTY {
        mask |= 0x02;
        payload.push((row.ins & 0xFF) as u8);
    }
    if row.vol != EMPTY {
        mask |= 0x04;
        payload.push((row.vol & 0xFF) as u8);
    }
    if !fx.is_empty() {
        // bit3/4: effect 0 + value 0 in the first mask
        mask |= 0x08 | 0x10;
        if fx.len() > 1 {
            mask |= 0x20; // extra byte for effects 0-3
            if fx.len() > 4 {
                mask |= 0x40;
            }
            extra = 0;
            let mut bit: i32 = 1;
            for _ in fx.iter().take(8) {
                extra |= bit | (bit << 1);
                bit <<= 2;
            }
        }
        for (c, v) in fx.iter().take(8) {
            payload.push((c & 0xFF) as u8);
            payload.push((v & 0xFF) as u8);
        }
    }
    let mut out: Vec<u8> = Vec::new();
    out.push((mask & 0xFF) as u8);
    if mask & 0x20 != 0 {
        out.push((extra & 0xFF) as u8);
    }
    if mask & 0x40 != 0 {
        out.push(((extra >> 8) & 0xFF) as u8);
    }
    out.extend_from_slice(&payload);
    out
}

pub fn _write_patn(
    b: &mut Buf,
    channel: i64,
    index: i64,
    rows: &[Option<Row>],
    fx_cols: usize,
    // Python default ""
    name: &str,
) {
    let (size_off, start) = b.begin_block(b"PATN");
    b.u8(0); // subsong
    b.u8(channel);
    b.u16(index);
    b.strz(name);
    // Empty rows encode as 0x00; a pattern is mostly empty ones, so they are
    // counted and written as runs instead of one call per row.
    let mut empty = 0usize;
    for row in rows {
        let blank = match row {
            None => true,
            Some(r) => {
                let fx_falsey = match &r.fx {
                    None => true,
                    Some(fx) => fx.is_empty(),
                };
                r.note < 0 && r.ins == EMPTY && r.vol == EMPTY && fx_falsey
            }
        };
        if blank {
            empty += 1;
            continue;
        }
        if empty > 0 {
            b.raw(&vec![0u8; empty]);
            empty = 0;
        }
        b.raw(&_encode_patn_row(row.as_ref().unwrap(), fx_cols));
    }
    if empty > 0 {
        b.raw(&vec![0u8; empty]);
    }
    b.u8(0xFF);
    b.end_block(size_off, start);
}

/// 20 bytes, version >= 69. Modern (non-broken) defaults; linear pitch = full.
pub fn _compat_phase1(b: &mut Buf) {
    b.u8(0); // limit slides
    b.u8(2); // linear pitch: full linear (<237)
    // Keep channel state across the loop. Reset-on-loop cuts every held voice
    // and sample at the seam, which is not what the source VGM does.
    b.u8(2); // loop modality: do nothing
    b.u8(1); // proper noise layout
    b.u8(0); // wave duty is volume
    b.u8(0); // reset macro on porta
    b.u8(0); // legacy volume slides
    b.u8(0); // compatible arpeggio
    b.u8(1); // note off resets slides
    b.u8(1); // target resets slides
    b.u8(0); // arp inhibits porta
    b.u8(0); // wack algorithm macro
    b.u8(0); // broken shortcut slides
    b.u8(1); // ignore duplicate slides
    b.u8(0); // stop porta on note off
    b.u8(0); // continuous vibrato
    b.u8(0); // broken DAC mode
    b.u8(0); // one tick cut
    b.u8(1); // ins change allowed during porta
    b.u8(1); // reset note base on arp stop
}

/// 28 bytes, version >= 130.
pub fn _compat_phase2(b: &mut Buf) {
    b.u8(0); // broken speed selection
    b.u8(0); // no slides on first tick
    b.u8(0); // next row reset arp pos
    b.u8(0); // ignore jump at end
    b.u8(0); // buggy porta after slide
    b.u8(1); // new ins affects GB env
    b.u8(1); // shared extch state
    b.u8(0); // ignore DAC mode change outside
    b.u8(1); // E1/E2 take priority
    b.u8(1); // new Sega PCM
    b.u8(0); // weird fnum pitch slides
    b.u8(0); // SN duty macro resets phase
    b.u8(1); // pitch macro is linear
    b.u8(1); // pitch slide speed in full linear (see _emit_sweep: the written rate
             // is calibrated by measurement, not by the 1/128 figure this flag names)
    b.u8(0); // old octave boundary
    b.u8(0); // disable OPN2 DAC volume
    b.u8(1); // new volume scaling
    b.u8(1); // volume macro lingers
    b.u8(0); // broken outVol
    b.u8(1); // E1/E2 stop on same note
    b.u8(0); // broken porta after arp
    b.u8(0); // SN periods under 8 are 1
    b.u8(0); // cut/delay policy (normal)
    b.u8(0); // jump treatment (normal)
    b.u8(1); // automatic system name
    b.u8(0); // disable sample macro
    b.u8(0); // broken outVol 2
    b.u8(0); // old arp strategy
}

/// Rebuild a row's effect list without the `drop` codes, keeping order.
pub fn _fx_keep(cell: &mut Row, drop: &[i32]) {
    if let Some(fx) = cell.fx.as_mut() {
        if !fx.is_empty() {
            fx.retain(|(c, _)| !drop.contains(c));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stats_map(pairs: &[(&str, i64)]) -> IndexMap<String, i64> {
        let mut m = IndexMap::new();
        for (k, v) in pairs {
            m.insert((*k).to_string(), *v);
        }
        m
    }

    #[test]
    fn blank_row_is_empty() {
        let row = _blank_row();
        assert!(row.is_empty());
        assert_eq!(row.note, -1);
        assert_eq!(row.ins, EMPTY);
        assert_eq!(row.vol, EMPTY);
        assert!(row.fx.is_none());
        // An all-EMPTY effect list still counts as empty.
        let mut r = Row::default();
        r.fx = Some(vec![(EMPTY, EMPTY), (EMPTY, EMPTY)]);
        assert!(r.is_empty());
        // Any real value makes it non-empty.
        r.vol = 3;
        assert!(!r.is_empty());
    }

    #[test]
    fn add_fx_fills_in_order_and_drops() {
        let mut row = Row::default();
        let mut stats = stats_map(&[]);
        _add_fx(&mut row, 0x01, 0x10, 2, Some(&mut stats));
        _add_fx(&mut row, 0x02, 0x20, 2, Some(&mut stats));
        assert_eq!(row.fx, Some(vec![(0x01, 0x10), (0x02, 0x20)]));
        assert!(!stats.contains_key("fx_dropped"));
        // Same command replaces in place.
        _add_fx(&mut row, 0x01, 0x30, 2, Some(&mut stats));
        assert_eq!(row.fx, Some(vec![(0x01, 0x30), (0x02, 0x20)]));
        assert!(!stats.contains_key("fx_dropped"));
        // No free slot: overwrite the last column and count the loss.
        _add_fx(&mut row, 0x03, 0x40, 2, Some(&mut stats));
        assert_eq!(row.fx, Some(vec![(0x01, 0x30), (0x03, 0x40)]));
        assert_eq!(stats.get("fx_dropped"), Some(&1));
        // fx_cols = 0 leaves an empty Some(vec![]) behind, like Python.
        let mut zero = Row::default();
        _add_fx(&mut zero, 0x04, 1, 0, None);
        assert_eq!(zero.fx, Some(vec![]));
        // Quad pan skips slots already in force, and reports no loss.
        let mut quad_row = Row::default();
        _add_quad_fx(&mut quad_row, (1, 0x102, 3, 5), 8, Some(&mut stats), Some((1, 2, 3, 4)));
        assert_eq!(
            quad_row.fx,
            Some(vec![
                (0x82, 0x02),
                (0x8A, 0x05),
                (EMPTY, EMPTY),
                (EMPTY, EMPTY),
                (EMPTY, EMPTY),
                (EMPTY, EMPTY),
                (EMPTY, EMPTY),
                (EMPTY, EMPTY),
            ])
        );
        assert_eq!(stats.get("fx_dropped"), Some(&1));
        // Drop-and-replace keeps the remaining effects in order.
        _fx_keep(&mut quad_row, &[0x02]);
        assert_eq!(quad_row.fx.as_ref().unwrap()[0], (0x82, 0x02));
        // Slide tooth: 15 is the ceiling.
        assert_eq!(_slide_level(0), 1);
        assert_eq!(_slide_level(14), 15);
        assert_eq!(_slide_level(15), 15);
        assert_eq!(_slide_level(20), 15);
    }

    #[test]
    fn encode_patn_row_layout() {
        let mut row = Row::default();
        row.note = 60;
        row.ins = 5;
        row.vol = 10;
        row.fx = Some(vec![(0x01, 0x10)]);
        // mask = note|ins|vol|fx0|fxval0 = 0x1F
        assert_eq!(
            _encode_patn_row(&row, 4),
            vec![0x1F, 60, 5, 10, 0x01, 0x10]
        );
        // Two effects: 0x20 set, presence byte 0x0F for two pairs.
        row.fx = Some(vec![(0x01, 0x10), (0x02, 0x20)]);
        assert_eq!(
            _encode_patn_row(&row, 4),
            vec![0x3F, 0x0F, 60, 5, 10, 0x01, 0x10, 0x02, 0x20]
        );
        // Empty rows encode as a single skip byte.
        assert_eq!(_encode_patn_row(&_blank_row(), 4), vec![0x00]);
    }

    #[test]
    fn write_patn_header_and_rows() {
        let mut content = Row::default();
        content.note = 60;
        content.vol = 1;
        let rows = vec![Some(_blank_row()), Some(content), None, None];
        let mut b = Buf { data: Vec::new() };
        _write_patn(&mut b, 2, 7, &rows, 4, "x");
        // PATN, size u32, subsong 0, channel 2, index 7, name "x\0",
        // 0x00 (leading blank), row [0x05, 60, 1], 0x00 0x00 (trailing),
        // then 0xFF.
        let body = &b.data[8..];
        let size = u32::from_le_bytes(b.data[4..8].try_into().unwrap()) as usize;
        assert_eq!(size, b.data.len() - 8);
        assert_eq!(
            body,
            &[
                0, 2, 7, 0, b'x', 0, // header
                0x00, // one skipped row
                0x05, 60, 1, // mask note|vol, payload
                0x00, 0x00, // two skipped rows
                0xFF,
            ]
        );
        // Compat blocks are fixed-width versioned payloads.
        let mut c1 = Buf { data: Vec::new() };
        _compat_phase1(&mut c1);
        assert_eq!(c1.len(), 20);
        assert_eq!(c1.data[1], 2);
        let mut c2 = Buf { data: Vec::new() };
        _compat_phase2(&mut c2);
        assert_eq!(c2.len(), 28);
        assert_eq!(c2.data[12], 1);
    }

    #[test]
    fn loss_counters_over_hand_built_stats() {
        let stats = stats_map(&[
            ("note_collision", 2),
            ("off_swallowed", 1),
            ("pcm_evicted", 3),
            ("pcm_unplaced", 4),
            ("fx_dropped", 5),
            ("orders_truncated", 6),
            ("sweep_dev_max", 9),
            ("unrelated", 100),
        ]);
        // LOSS_KEYS sums every listed loss counter, not sweep_dev_max.
        assert_eq!(loss_total(&stats), 2 + 1 + 3 + 4 + 5 + 6);
        assert_eq!(keyon_loss(Some(&stats)), 2 + 4 + 3 + 6);
        assert_eq!(dev_max(&stats), 9);
        assert_eq!(keyon_loss(None), 0);
        assert_eq!(keyon_loss(Some(&IndexMap::new())), 0);
        assert_eq!(dev_max(&IndexMap::new()), 0);
    }
}
