//! uPD7759 stand-alone ROM -> ADPCM phrases -> WAV + JSON.
//!
//! Port of `vgm2151fur/upd7759.py` (v0.11.0). The uPD7759 in stand-alone mode
//! (the mode Konami Twin16 wires it in) plays phrases out of its own data ROM:
//!
//! * a 5-byte header: last phrase number, then `5a a5 69 55`
//! * one big-endian 16-bit vector per phrase; the ROM address is `value * 2`
//! * the command stream starts one byte after that address
//!
//! The stream is decoded the way MAME does (src/devices/sound/upd7759.cpp): a
//! 4-bit ADPCM state indexes a step table, block headers set the nibble rate
//! (`clock / (4 * (rate + 1))`, i.e. 5-17.8 kHz at the standard 640 kHz
//! clock), `00dddddd` blocks are silence, `0xC0|r` repeats the block that
//! follows `r + 1` times, and a `0x00` after a valid block ends the phrase.
//!
//! The level changes once per nibble, so a phrase is rendered by interpolating
//! between the event points (one point per nibble at its own rate). The nibble
//! rate is always below the output rate, so this is an upsample, not a
//! decimation - the same model a WAV render of MAME's stream would give.
//!
//! Timing matches MAME's state machine, including the 36-clock cost it charges
//! for a block header (and the extra 36 for the `10` block's count byte).
//! Those add up to ~0.2%, which is audible when a decoded phrase is compared
//! against a recording of the chip - the phases drift apart over a long phrase.

use std::collections::{BTreeSet, HashSet};
use std::io::Write;
use std::path::Path;

use indexmap::IndexMap;
use serde_json::{Value, json};

use crate::error::{Error, Result};
use crate::parity::{py_int, py_round, py_round_to};
use crate::samplecluster::cluster_fragments16;
use crate::vgm::{VGM_RATE, VgmFile, assemble_rom};

pub const STANDARD_CLOCK: u32 = 640_000;
pub const MAGIC: [u8; 4] = [0x5a, 0xa5, 0x69, 0x55];

/// MAME charges 36 clocks to the block header state (and 36 more to the count
/// byte state of a `10` block). The output holds the current level meanwhile.
pub const HEADER_CLOCKS: i64 = 36;

/// MAME's upd775x_step table (`upd7759.cpp`).
pub const STEP: [[i32; 16]; 16] = [
    [0, 0, 1, 2, 3, 5, 7, 10, 0, 0, -1, -2, -3, -5, -7, -10],
    [0, 1, 2, 3, 4, 6, 8, 13, 0, -1, -2, -3, -4, -6, -8, -13],
    [0, 1, 2, 4, 5, 7, 10, 15, 0, -1, -2, -4, -5, -7, -10, -15],
    [0, 1, 3, 4, 6, 9, 13, 19, 0, -1, -3, -4, -6, -9, -13, -19],
    [0, 2, 3, 5, 8, 11, 15, 23, 0, -2, -3, -5, -8, -11, -15, -23],
    [0, 2, 4, 7, 10, 14, 19, 29, 0, -2, -4, -7, -10, -14, -19, -29],
    [0, 3, 5, 8, 12, 16, 22, 33, 0, -3, -5, -8, -12, -16, -22, -33],
    [1, 4, 7, 10, 15, 20, 29, 43, -1, -4, -7, -10, -15, -20, -29, -43],
    [1, 4, 8, 13, 18, 25, 35, 53, -1, -4, -8, -13, -18, -25, -35, -53],
    [1, 6, 10, 16, 22, 31, 43, 64, -1, -6, -10, -16, -22, -31, -43, -64],
    [2, 7, 12, 19, 27, 37, 51, 76, -2, -7, -12, -19, -27, -37, -51, -76],
    [2, 9, 16, 24, 34, 46, 64, 96, -2, -9, -16, -24, -34, -46, -64, -96],
    [3, 11, 19, 29, 41, 57, 79, 117, -3, -11, -19, -29, -41, -57, -79, -117],
    [4, 13, 24, 36, 50, 69, 96, 143, -4, -13, -24, -36, -50, -69, -96, -143],
    [4, 16, 29, 44, 62, 85, 118, 175, -4, -16, -29, -44, -62, -85, -118, -175],
    [6, 20, 36, 54, 76, 104, 144, 214, -6, -20, -36, -54, -76, -104, -144, -214],
];

pub const STATE_TABLE: [i32; 16] = [-1, -1, 0, 0, 1, 2, 2, 3, -1, -1, 0, 0, 1, 2, 2, 3];

/// Phrases in the ROM (header byte 0 is the last phrase number).
pub fn phrase_count(rom: &[u8]) -> Result<usize> {
    if rom.len() < 5 || rom[1..5] != MAGIC {
        return Err(Error::msg(
            "not a uPD7759 stand-alone ROM (no 5a a5 69 55 header)",
        ));
    }
    Ok(rom[0] as usize + 1)
}

/// First command byte of a phrase: vector value * 2, plus the +1 MAME adds.
pub fn phrase_address(rom: &[u8], index: usize) -> Result<i64> {
    let count = phrase_count(rom)?;
    if index >= count {
        return Err(Error::msg(format!(
            "phrase {index} out of range 0..{}",
            rom[0]
        )));
    }
    let value = ((rom[5 + 2 * index] as i64) << 8) | rom[6 + 2 * index] as i64;
    Ok(value * 2 + 1)
}

/// One decoded phrase: level/duration events in the chip's clock domain.
///
/// `events` holds `(level, clocks)` pairs in playback order; `level` is the
/// ADPCM accumulator MAME mixes as `level / 256` (256.0 = full scale).
#[derive(Clone, Debug, PartialEq, Default)]
pub struct Phrase {
    pub index: i32,
    pub start: i64,
    pub end: Option<i64>,
    pub events: Vec<(i32, i64)>,
    pub blocks: i32,
    pub rates: BTreeSet<i32>,
}

impl Phrase {
    /// `Phrase(index, start, end)` with the Python dataclass defaults.
    pub fn new(index: i32, start: i64, end: Option<i64>) -> Self {
        Phrase { index, start, end, events: Vec::new(), blocks: 0, rates: BTreeSet::new() }
    }

    pub fn clocks(&self) -> i64 {
        self.events.iter().map(|&(_level, clocks)| clocks).sum()
    }

    pub fn seconds(&self, clock: u32) -> f64 {
        self.clocks() as f64 / clock as f64
    }

    pub fn peak(&self) -> i32 {
        self.events.iter().map(|&(level, _clocks)| level.abs()).max().unwrap_or(0)
    }
}

/// Feed `count` nibbles from the stream into the ADPCM state machine.
///
/// Odd counts drop the low nibble of the last byte (hardware behavior).
/// Returns (new offset, level, state).
fn decode_nibbles(
    rom: &[u8],
    mut offset: usize,
    count: usize,
    rate: i32,
    phrase: &mut Phrase,
    mut level: i32,
    mut state: i32,
) -> (usize, i32, i32) {
    let mut left = count;
    while left > 0 {
        if offset >= rom.len() {
            break;
        }
        let byte = rom[offset];
        offset += 1;
        for nibble in [(byte >> 4) as usize, (byte & 0x0F) as usize] {
            if left == 0 {
                break;
            }
            level += STEP[state as usize][nibble];
            state += STATE_TABLE[nibble];
            state = if state < 0 {
                0
            } else if state > 15 {
                15
            } else {
                state
            };
            phrase.events.push((level, rate as i64 * 4));
            left -= 1;
        }
    }
    (offset, level, state)
}

/// Walk one phrase's command stream the way MAME's state machine does.
pub fn decode_phrase(rom: &[u8], index: usize, _clock: u32) -> Result<Phrase> {
    let addr = phrase_address(rom, index)?;
    let mut phrase = Phrase::new(index as i32, addr, None);
    let mut level = 0i32;
    let mut state = 0i32;
    let mut repeat_count = 0i32;
    let mut repeat_offset = addr;
    let mut offset = addr as usize;
    let mut first_valid = false;

    loop {
        if repeat_count != 0 {
            repeat_count -= 1;
            offset = repeat_offset as usize;
        }
        if offset >= rom.len() {
            break;
        }
        let header = rom[offset];
        offset += 1;
        let kind = header & 0xC0;

        if kind == 0x00 {
            // silence; 0x00 after a valid block is the end
            phrase.events.push((0, 1024 * ((header & 0x3F) as i64 + 1)));
            phrase.blocks += 1;
            level = 0;
            state = 0;
            if header == 0 && first_valid {
                phrase.end = Some(offset as i64);
                break;
            }
        } else if kind == 0x40 {
            // 256 nibbles
            let rate = (header & 0x3F) as i32 + 1;
            phrase.rates.insert(rate);
            phrase.blocks += 1;
            phrase.events.push((level, HEADER_CLOCKS));
            let (o, l, s) = decode_nibbles(rom, offset, 256, rate, &mut phrase, level, state);
            offset = o;
            level = l;
            state = s;
        } else if kind == 0x80 {
            // n nibbles
            let rate = (header & 0x3F) as i32 + 1;
            phrase.rates.insert(rate);
            phrase.blocks += 1;
            phrase.events.push((level, HEADER_CLOCKS));
            if offset >= rom.len() {
                return Err(Error::msg("index out of range"));
            }
            let count = rom[offset] as usize + 1;
            offset += 1;
            phrase.events.push((level, HEADER_CLOCKS));
            let (o, l, s) = decode_nibbles(rom, offset, count, rate, &mut phrase, level, state);
            offset = o;
            level = l;
            state = s;
        } else {
            // 0xC0 repeat: the block after this one plays r + 1 times
            repeat_count = (header & 0x07) as i32 + 1;
            repeat_offset = offset as i64;
            phrase.blocks += 1;
            phrase.events.push((level, HEADER_CLOCKS));
        }

        if header != 0 {
            first_valid = true;
        }
    }

    Ok(phrase)
}

/// Decode a raw uPD7759 command stream (slave mode).
///
/// Slave logs send command bytes through the port instead of pointing the chip
/// at its own phrase ROM. The command grammar after the port handshake is the
/// same as a stand-alone phrase, so the same state machine applies.
pub fn decode_stream(data: &[u8], start: usize, _clock: u32) -> Phrase {
    let mut phrase = Phrase::new(-1, start as i64, None);
    let mut level = 0i32;
    let mut state = 0i32;
    let mut repeat_count = 0i32;
    let mut repeat_offset = start as i64;
    let mut offset = start;
    let mut first_valid = false;

    while offset < data.len() {
        if repeat_count != 0 {
            repeat_count -= 1;
            offset = repeat_offset as usize;
        }
        if offset >= data.len() {
            break;
        }
        let header = data[offset];
        offset += 1;
        let kind = header & 0xC0;

        if kind == 0x00 {
            phrase.events.push((0, 1024 * ((header & 0x3F) as i64 + 1)));
            phrase.blocks += 1;
            level = 0;
            state = 0;
            if header == 0 && first_valid {
                phrase.end = Some(offset as i64);
                break;
            }
        } else if kind == 0x40 {
            let rate = (header & 0x3F) as i32 + 1;
            phrase.rates.insert(rate);
            phrase.blocks += 1;
            phrase.events.push((level, HEADER_CLOCKS));
            let (o, l, s) = decode_nibbles(data, offset, 256, rate, &mut phrase, level, state);
            offset = o;
            level = l;
            state = s;
        } else if kind == 0x80 {
            let rate = (header & 0x3F) as i32 + 1;
            phrase.rates.insert(rate);
            phrase.blocks += 1;
            phrase.events.push((level, HEADER_CLOCKS));
            if offset >= data.len() {
                break;
            }
            let count = data[offset] as usize + 1;
            offset += 1;
            phrase.events.push((level, HEADER_CLOCKS));
            let (o, l, s) = decode_nibbles(data, offset, count, rate, &mut phrase, level, state);
            offset = o;
            level = l;
            state = s;
        } else {
            repeat_count = (header & 0x07) as i32 + 1;
            repeat_offset = offset as i64;
            phrase.blocks += 1;
            phrase.events.push((level, HEADER_CLOCKS));
        }

        if header != 0 {
            first_valid = true;
        }
    }

    phrase
}

#[derive(Clone, Debug, PartialEq, Default)]
pub struct Upd7759Hit {
    pub sample_time: i64,
    pub chip_id: i32,
    pub phrase: i32,
    pub clock: u32,
    /// Key-off sample time (0 = held to the end).
    pub end: i64,
}

impl Upd7759Hit {
    pub fn new(sample_time: i64, chip_id: i32, phrase: i32, clock: u32) -> Self {
        Upd7759Hit { sample_time, chip_id, phrase, clock, end: 0 }
    }
}

#[derive(Clone, Debug, PartialEq, Default)]
pub struct Upd7759Sample {
    pub chip_id: i32,
    pub phrase: i32,
    /// 16-bit signed little-endian PCM at `UPD_OUTPUT_RATE`.
    pub data: Vec<u8>,
    pub clock: u32,
    pub hits: Vec<Upd7759Hit>,
    pub name: String,
}

impl Upd7759Sample {
    pub fn new(chip_id: i32, phrase: i32, data: Vec<u8>, clock: u32) -> Self {
        Upd7759Sample { chip_id, phrase, data, clock, hits: Vec::new(), name: String::new() }
    }

    pub fn frames(&self) -> usize {
        self.data.len() / 2
    }
}

/// The module renders phrases at a fixed rate; the PCM DAC system uses the
/// same value so note 108 plays them back at 1x.
pub const UPD_OUTPUT_RATE: i32 = 44100;
/// The slave state machine consumes five bytes of handshake before the first
/// block header (start request, last-sample, dummy, address MSB/LSB).
pub const UPD_SLAVE_HANDSHAKE: usize = 5;
pub const UPD_MAX_PHRASE_SECONDS: f64 = 30.0;

#[derive(Clone, Debug)]
struct StreamState {
    reset: bool,
    start: bool,
    port: i32,
    bank: i64,
    active_until: i64,
    collecting: bool,
    burst_start: i64,
    burst: Vec<(i64, i32)>,
}

fn phrase_samples(phrase: &Phrase, clock: u32) -> i64 {
    py_int(py_round(phrase.seconds(clock) * VGM_RATE as f64))
}

fn add_sample<'a>(
    samples: &'a mut IndexMap<(i32, Vec<u8>), Upd7759Sample>,
    clock: u32,
    cid: i32,
    source: i32,
    pcm: Vec<u8>,
) -> &'a mut Upd7759Sample {
    use indexmap::map::Entry;

    // The name is only observable when a new body is inserted; computing it
    // before the entry keeps `samples.len()` out of the entry borrow.
    let name = if source >= 0 {
        format!("uPD{}", source + 1)
    } else {
        format!("uPD stream {}", samples.len() + 1)
    };
    match samples.entry((cid, pcm)) {
        Entry::Occupied(entry) => entry.into_mut(),
        Entry::Vacant(entry) => {
            // The body also lives in the dedup key; the map needs its own copy.
            let data = entry.key().1.clone();
            entry.insert(Upd7759Sample {
                chip_id: cid,
                phrase: source,
                data,
                clock,
                hits: Vec::new(),
                name,
            })
        }
    }
}

fn finish_burst(
    cid: i32,
    state: &mut IndexMap<i32, StreamState>,
    warnings: &mut Vec<String>,
    pending: &mut Vec<(i64, i32, i32, Phrase)>,
    clock: u32,
) {
    let st = state.get_mut(&cid).unwrap();
    if !st.collecting {
        return;
    }
    st.collecting = false;
    let burst = std::mem::take(&mut st.burst);
    if burst.is_empty() {
        return;
    }
    // Skip the port handshake. The state machine consumes five bytes before
    // the first command header; keep at least one byte for a malformed log.
    let data: Vec<u8> = burst.iter().map(|&(_when, v)| v as u8).collect();
    let start = UPD_SLAVE_HANDSHAKE.min(data.len().saturating_sub(1));
    let phrase = decode_stream(&data, start, clock);
    if phrase.blocks == 0 {
        warnings.push(format!(
            "uPD7759 chip {} slave stream at {} had no decodable commands",
            cid + 1,
            st.burst_start
        ));
        return;
    }
    let burst_start = st.burst_start;
    st.active_until = burst_start + phrase_samples(&phrase, clock);
    pending.push((burst_start, cid, -1, phrase));
}

/// Turn uPD7759 register traffic into PCM phrase bodies and note hits.
pub fn collect_upd7759(
    vgm: &VgmFile,
    lossy: bool,
) -> Result<(Vec<Upd7759Sample>, Vec<String>)> {
    let mut warnings: Vec<String> = Vec::new();
    let id_set: BTreeSet<i32> = vgm
        .writes
        .iter()
        .filter(|w| w.chip == "upd7759")
        .map(|w| w.chip_id as i32)
        .collect();
    let ids: Vec<i32> = id_set.into_iter().collect();
    if ids.is_empty() {
        return Ok((Vec::new(), warnings));
    }

    let mut roms: IndexMap<i32, Vec<u8>> = IndexMap::new();
    for &cid in &ids {
        let rom = assemble_rom(vgm, "upd7759", cid as u8, 0xFF, false, true);
        if rom.is_empty() && !vgm.upd7759_slave {
            warnings.push(format!("uPD7759 chip {} has no type-0x8A ROM block", cid + 1));
        }
        roms.insert(cid, rom);
    }

    let clock = if vgm.upd7759_clock != 0 { vgm.upd7759_clock } else { STANDARD_CLOCK };
    let mut samples: IndexMap<(i32, Vec<u8>), Upd7759Sample> = IndexMap::new();
    // Decode first, then bank the command streams. Rendering stays at the
    // decoder's nominal level; no per-file gain adjustment.
    let mut pending: Vec<(i64, i32, i32, Phrase)> = Vec::new();

    let mut state: IndexMap<i32, StreamState> = IndexMap::new();
    for &cid in &ids {
        state.insert(
            cid,
            StreamState {
                // libvgm starts the chip with reset and start high; VGM logs
                // then toggle them. This is why some logs never write
                // register 0.
                reset: true,
                start: true,
                port: 0,
                bank: 0,
                active_until: 0,
                collecting: false,
                burst_start: 0,
                burst: Vec::new(),
            },
        );
    }

    for w in &vgm.writes {
        let cid = w.chip_id as i32;
        if w.chip != "upd7759" || !state.contains_key(&cid) {
            continue;
        }
        let t = w.sample;
        {
            let st = state.get_mut(&cid).unwrap();
            if st.active_until != 0 && t >= st.active_until {
                st.active_until = 0;
            }
        }
        let reg = w.reg;
        if reg == 3 {
            state.get_mut(&cid).unwrap().bank = (w.val as i64) << 17;
            continue;
        }
        if reg == 2 {
            let st = state.get_mut(&cid).unwrap();
            st.port = w.val as i32;
            if st.collecting {
                st.burst.push((t, w.val as i32));
            }
            continue;
        }
        if reg == 0 {
            let st = state.get_mut(&cid).unwrap();
            let old = st.reset;
            st.reset = w.val != 0;
            let fell = old && !st.reset;
            if fell {
                finish_burst(cid, &mut state, &mut warnings, &mut pending, clock);
                state.get_mut(&cid).unwrap().active_until = 0;
            }
            continue;
        }
        if reg != 1 {
            continue;
        }
        let (rising, falling) = {
            let st = state.get_mut(&cid).unwrap();
            let old = st.start;
            st.start = w.val != 0;
            let rising = st.start && !old;
            let falling = old && !st.start;
            (rising, falling)
        };
        if falling {
            finish_burst(cid, &mut state, &mut warnings, &mut pending, clock);
            continue;
        }
        {
            let st = &state[&cid];
            if !rising || !st.reset || st.active_until > t {
                continue;
            }
        }
        // A start rising edge while reset is high and the chip is idle.
        if vgm.upd7759_slave {
            let st = state.get_mut(&cid).unwrap();
            st.collecting = true;
            st.burst_start = t;
            st.burst = Vec::new();
            continue;
        }
        let view: &[u8] = {
            let rom = roms.get(&cid).map(|r| r.as_slice()).unwrap_or(&[]);
            let bank = state[&cid].bank;
            if bank >= 0 && (bank as usize) < rom.len() { &rom[bank as usize..] } else { &[] }
        };
        if view.is_empty() {
            let bank = state[&cid].bank;
            warnings.push(format!(
                "uPD7759 chip {} start at {}: no ROM at bank {:#x}",
                cid + 1,
                t,
                bank
            ));
            continue;
        }
        let count = match phrase_count(view) {
            Ok(count) => count,
            Err(_) => {
                let bank = state[&cid].bank;
                warnings.push(format!(
                    "uPD7759 chip {} start at {}: ROM bank {:#x} has no phrase header",
                    cid + 1,
                    t,
                    bank
                ));
                continue;
            }
        };
        let source = state[&cid].port;
        if source < 0 || source as usize >= count {
            warnings.push(format!(
                "uPD7759 chip {} sample {} is past the ROM's {} phrases",
                cid + 1,
                source,
                count
            ));
            continue;
        }
        let phrase = decode_phrase(view, source as usize, clock)?;
        if phrase.blocks == 0 {
            continue;
        }
        state.get_mut(&cid).unwrap().active_until = t + phrase_samples(&phrase, clock);
        pending.push((t, cid, source, phrase));
    }

    // Close a stream whose log ended while start was still high.
    for &cid in &ids {
        finish_burst(cid, &mut state, &mut warnings, &mut pending, clock);
    }

    let gain = 1.0;
    for (start, cid, source, phrase) in &pending {
        let mut pcm = render_wav_bytes(phrase, UPD_OUTPUT_RATE, clock, gain);
        let seconds = pcm.len() as f64 / 2.0 / UPD_OUTPUT_RATE as f64;
        if seconds > UPD_MAX_PHRASE_SECONDS {
            warnings.push(format!(
                "uPD7759 chip {} stream at {} decoded to {:.1}s; truncated",
                cid + 1,
                start,
                seconds
            ));
            let limit = (py_int(UPD_MAX_PHRASE_SECONDS * UPD_OUTPUT_RATE as f64) * 2) as usize;
            pcm.truncate(limit);
        }
        let frames = pcm.len() / 2;
        let sample = add_sample(&mut samples, clock, *cid, *source, pcm);
        sample.hits.push(Upd7759Hit {
            sample_time: *start,
            chip_id: *cid,
            phrase: *source,
            clock,
            end: *start + frames as i64,
        });
    }

    let mut audible: Vec<Upd7759Sample> =
        samples.into_values().filter(|s| !s.hits.is_empty() && s.frames() > 0).collect();
    audible.sort_by(|a, b| (a.chip_id, a.phrase, &a.name).cmp(&(b.chip_id, b.phrase, &b.name)));
    if lossy && audible.len() > 32 {
        let bodies: Vec<Vec<u8>> = audible.iter().map(|s| s.data.clone()).collect();
        let rates: Vec<i32> = vec![UPD_OUTPUT_RATE; audible.len()];
        let mut uses: IndexMap<i32, i64> = IndexMap::new();
        for (i, s) in audible.iter().enumerate() {
            uses.insert(i as i32, s.hits.len() as i64);
        }
        let (keep, remap) = cluster_fragments16(
            &bodies,
            &rates,
            &uses,
            crate::samplecluster::DEFAULT_TARGET,
            crate::samplecluster::DEFAULT_HOT,
        );
        // Only the moved hits are relocated; a kept sample's own hits stay put.
        let mut extra_hits: Vec<Vec<Upd7759Hit>> = vec![Vec::new(); keep.len()];
        let mut slots: Vec<Option<Upd7759Sample>> =
            audible.into_iter().map(Some).collect();
        for old in 0..slots.len() {
            let new = remap[old];
            if new < 0 || keep[new as usize] == old as i32 {
                continue;
            }
            let hits = std::mem::take(&mut slots[old].as_mut().unwrap().hits);
            extra_hits[new as usize].extend(hits);
        }
        let mut compact: Vec<Upd7759Sample> = Vec::with_capacity(keep.len());
        for (new, &old) in keep.iter().enumerate() {
            let mut sample = slots[old as usize].take().unwrap();
            sample.hits.extend(extra_hits[new].drain(..));
            compact.push(sample);
        }
        let before = remap.len();
        if compact.len() < before {
            warnings.push(format!(
                "uPD7759: lossy sample clustering {} -> {} bodies",
                before,
                compact.len()
            ));
        }
        audible = compact;
    }
    let mut seen: HashSet<String> = HashSet::new();
    let mut deduped: Vec<String> = Vec::with_capacity(warnings.len());
    for w in warnings {
        if seen.insert(w.clone()) {
            deduped.push(w);
        }
    }
    warnings = deduped;
    Ok((audible, warnings))
}

/// Level sample points: (time in clocks at clock/4, level) per event.
fn _event_points(phrase: &Phrase, _clock: u32) -> (Vec<f64>, Vec<i32>) {
    let mut times: Vec<f64> = Vec::new();
    let mut levels: Vec<i32> = Vec::new();
    let mut acc = 0.0;
    for &(level, clocks) in &phrase.events {
        let frames = clocks as f64 / 4.0;
        times.push(acc + frames / 2.0);
        levels.push(level);
        acc += frames;
    }
    (times, levels)
}

/// Phrase -> output levels at `rate` Hz (256.0 = full scale, MAME's scale).
pub fn render_levels(phrase: &Phrase, rate: i32, clock: u32) -> Vec<f64> {
    let (times, levels) = _event_points(phrase, clock);
    if levels.is_empty() {
        return Vec::new();
    }
    let duration_frames: f64 =
        phrase.events.iter().map(|&(_l, clocks)| clocks as f64).sum::<f64>() / 4.0;
    let out_n = std::cmp::max(
        1,
        py_int(py_round(duration_frames * rate as f64 / (clock as f64 / 4.0))) as usize,
    );
    let step = (clock as f64 / 4.0) / rate as f64;
    let mut out: Vec<f64> = Vec::new();
    let mut i = 0usize;
    let mut t = 0.0f64;
    for _ in 0..out_n {
        while i + 1 < times.len() && t > times[i + 1] {
            i += 1;
        }
        let value = if i + 1 < times.len() {
            let t0 = times[i];
            let t1 = times[i + 1];
            let mut frac = if t1 <= t0 { 0.0 } else { (t - t0) / (t1 - t0) };
            frac = if frac < 0.0 {
                0.0
            } else if frac > 1.0 {
                1.0
            } else {
                frac
            };
            levels[i] as f64 + (levels[i + 1] - levels[i]) as f64 * frac
        } else {
            levels[i] as f64
        };
        out.push(value);
        t += step;
    }
    out
}

/// Render to 16-bit mono PCM at `rate`. gain scales the level/256 scale.
pub fn render_wav_bytes(phrase: &Phrase, rate: i32, clock: u32, gain: f64) -> Vec<u8> {
    let values = render_levels(phrase, rate, clock);
    let scale = (32767.0 / 256.0) * gain;
    let mut out: Vec<u8> = Vec::with_capacity(values.len() * 2);
    for value in values {
        let raw = py_int(py_round(value * scale)).clamp(-32768, 32767) as i16;
        out.extend_from_slice(&raw.to_le_bytes());
    }
    out
}

/// Write 16-bit mono PCM as a RIFF/WAVE file (the format `wave` emits).
pub fn write_wav(path: &Path, pcm: &[u8], rate: i32) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let rate = std::cmp::max(1000, rate) as u32;
    let data_len = pcm.len() as u32;
    let mut out: Vec<u8> = Vec::with_capacity(44 + pcm.len());
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + data_len).to_le_bytes());
    out.extend_from_slice(b"WAVE");
    out.extend_from_slice(b"fmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&rate.to_le_bytes());
    out.extend_from_slice(&(rate * 2).to_le_bytes());
    out.extend_from_slice(&2u16.to_le_bytes());
    out.extend_from_slice(&16u16.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data_len.to_le_bytes());
    out.extend_from_slice(pcm);
    let mut file = std::fs::File::create(path)?;
    file.write_all(&out)?;
    Ok(())
}

/// Write one WAV per phrase plus `upd7759_index.json`.
///
/// A single gain is picked for the whole set so the loudest phrase stays
/// inside 16-bit while the relative levels between phrases are kept.
/// `code_base` names the files (and the index) the way a driver's sound latch
/// reaches the phrases: phrase i is code `code_base + i`. `target_peak` scales
/// the set so its loudest sample sits at that 16-bit peak.
///
/// The returned JSON object keeps the Python key set; `serde_json` object maps
/// sort keys, which only affects this developer dump's key order.
pub fn dump_rom(
    rom: &[u8],
    out_dir: &Path,
    source: &str,
    rate: i32,
    clock: u32,
    code_base: Option<i32>,
    target_peak: Option<i32>,
) -> Result<Value> {
    std::fs::create_dir_all(out_dir)?;
    let count = phrase_count(rom)?;
    let mut phrases: Vec<Phrase> = Vec::with_capacity(count);
    for i in 0..count {
        phrases.push(decode_phrase(rom, i, clock)?);
    }
    let peak = phrases.iter().map(|p| p.peak()).max().unwrap_or(0);
    let gain = if target_peak.unwrap_or(0) != 0 && peak != 0 {
        target_peak.unwrap() as f64 / (peak as f64 * (32767.0 / 256.0))
    } else if peak <= 250 {
        1.0
    } else {
        250.0 / peak as f64
    };
    let mut items: Vec<Value> = Vec::new();
    for phrase in &phrases {
        let code: Option<i32> = code_base.map(|base| base + phrase.index);
        let mut name = format!("voice_{:02}", phrase.index);
        if let Some(code) = code {
            name.push_str(&format!("_code{code:02X}"));
        }
        let file = out_dir.join(format!("{name}.wav"));
        let pcm = render_wav_bytes(phrase, rate, clock, gain);
        write_wav(&file, &pcm, rate)?;
        items.push(json!({
            "index": phrase.index,
            "code": code.map(|code| format!("0x{code:02X}")),
            "file": file.file_name().unwrap().to_string_lossy(),
            "start": phrase.start,
            "end": phrase.end,
            "blocks": phrase.blocks,
            "rates": phrase.rates.iter().copied().collect::<Vec<i32>>(),
            "events": phrase.events.len(),
            "seconds": py_round_to(phrase.seconds(clock), 4),
            "peak_raw": phrase.peak(),
        }));
    }
    let index = json!({
        "source": source,
        "clock": clock,
        "rom_size": rom.len(),
        "phrases": count,
        "rate": rate,
        "gain": py_round_to(gain, 6),
        "target_peak": target_peak,
        "items": items,
    });
    let mut text = serde_json::to_string_pretty(&index)
        .map_err(|e| Error::msg(format!("json: {e}")))?;
    text.push('\n');
    std::fs::write(out_dir.join("upd7759_index.json"), text)?;
    Ok(index)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vgm::{ChipWrite, VgmFile};

    /// Header + one vector pointing at offset 7 + the phrase bytes.
    fn rom_with_phrase(phrase: &[u8]) -> Vec<u8> {
        let mut rom = vec![0u8, 0x5a, 0xa5, 0x69, 0x55];
        rom.extend_from_slice(&3u16.to_be_bytes()); // 3 * 2 + 1 = 7
        rom.extend_from_slice(phrase);
        rom
    }

    #[test]
    fn phrase_count_and_address_follow_the_header() {
        let rom = rom_with_phrase(&[0x00]);
        assert_eq!(phrase_count(&rom).unwrap(), 1);
        assert_eq!(phrase_address(&rom, 0).unwrap(), 7);

        assert_eq!(
            phrase_count(&[0u8; 4]).unwrap_err().to_string(),
            "not a uPD7759 stand-alone ROM (no 5a a5 69 55 header)"
        );
        let mut bad = rom.clone();
        bad[2] = 0;
        assert!(phrase_count(&bad).is_err());
        assert_eq!(
            phrase_address(&rom, 1).unwrap_err().to_string(),
            "phrase 1 out of range 0..0"
        );
    }

    #[test]
    fn decode_phrase_and_stream_share_the_silence_grammar() {
        // 0x01: 1024 * 2 clocks of silence; 0x00 after a valid block ends it.
        let rom = rom_with_phrase(&[0x01, 0x00]);
        let phrase = decode_phrase(&rom, 0, STANDARD_CLOCK).unwrap();
        assert_eq!(phrase.start, 7);
        assert_eq!(phrase.end, Some(9));
        assert_eq!(phrase.blocks, 2);
        assert_eq!(phrase.events, vec![(0, 2048), (0, 1024)]);
        assert_eq!(phrase.clocks(), 3072);
        assert!(phrase.rates.is_empty());
        assert_eq!(phrase.peak(), 0);
        assert!((phrase.seconds(STANDARD_CLOCK) - 0.0048).abs() < 1e-12);

        // The slave stream skips its five handshake bytes, then decodes alike.
        let data = [0u8, 0, 0, 0, 0, 0x01, 0x00];
        let stream = decode_stream(&data, 5, STANDARD_CLOCK);
        assert_eq!(stream.index, -1);
        assert_eq!(stream.start, 5);
        assert_eq!(stream.end, Some(7));
        assert_eq!(stream.blocks, 2);
        assert_eq!(stream.events, vec![(0, 2048), (0, 1024)]);
    }

    #[test]
    fn decode_phrase_expands_a_full_nibble_block() {
        // 0x40: 256 nibbles at rate 1 (4 clocks each), then the terminator.
        let mut phrase_bytes = vec![0x40];
        phrase_bytes.extend(std::iter::repeat_n(0u8, 128));
        phrase_bytes.push(0x00);
        let rom = rom_with_phrase(&phrase_bytes);
        let phrase = decode_phrase(&rom, 0, STANDARD_CLOCK).unwrap();
        assert_eq!(phrase.blocks, 2);
        assert_eq!(phrase.rates, BTreeSet::from([1]));
        // one header event + 256 nibble events + the terminator
        assert_eq!(phrase.events.len(), 258);
        assert_eq!(phrase.events[0], (0, HEADER_CLOCKS));
        assert_eq!(phrase.events[1], (0, 4));
        assert_eq!(phrase.clocks(), HEADER_CLOCKS + 256 * 4 + 1024);
        assert_eq!(phrase.end, Some(rom.len() as i64));
    }

    #[test]
    fn render_interpolates_one_point_per_event() {
        let mut phrase = Phrase::new(0, 0, None);
        phrase.events = vec![(0, 4), (256, 4)];
        // rate == clock / 4 gives exactly one output frame per clock/4 step.
        let levels = render_levels(&phrase, 160_000, STANDARD_CLOCK);
        assert_eq!(levels, vec![0.0, 128.0]);
        // 128 * 32767/256 = 16383.5 -> half-even -> 16384.
        assert_eq!(render_wav_bytes(&phrase, 160_000, STANDARD_CLOCK, 1.0), vec![0, 0, 0, 0x40]);
    }

    #[test]
    fn collect_decodes_slave_bursts_and_warns_about_missing_roms() {
        let mut vgm = VgmFile::default();
        vgm.upd7759_slave = true;
        vgm.upd7759_clock = STANDARD_CLOCK;
        let mut writes = vec![
            ChipWrite::new(100, "upd7759".into(), 0, 2, 0),
            ChipWrite::new(200, "upd7759".into(), 0, 1, 0),
            ChipWrite::new(300, "upd7759".into(), 0, 1, 1),
        ];
        let burst = [0u32, 0, 0, 0, 0, 0x01, 0x00];
        for (i, value) in burst.iter().enumerate() {
            writes.push(ChipWrite::new(400 + i as i64, "upd7759".into(), 0, 2, *value));
        }
        writes.push(ChipWrite::new(500, "upd7759".into(), 0, 1, 0));
        vgm.writes = writes;

        let (samples, warnings) = collect_upd7759(&vgm, false).unwrap();
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(samples.len(), 1);
        let sample = &samples[0];
        assert_eq!(sample.name, "uPD stream 1");
        assert_eq!(sample.phrase, -1);
        assert_eq!(sample.chip_id, 0);
        assert_eq!(sample.clock, STANDARD_CLOCK);
        assert_eq!(sample.frames(), 212);
        assert_eq!(sample.data.len(), 424);
        assert_eq!(
            sample.hits[0],
            Upd7759Hit {
                sample_time: 300,
                chip_id: 0,
                phrase: -1,
                clock: STANDARD_CLOCK,
                end: 512,
            }
        );

        // A stand-alone log with no ROM block warns twice and yields no sample.
        let mut missing = VgmFile::default();
        missing.writes = vec![
            ChipWrite::new(0, "upd7759".into(), 0, 1, 0),
            ChipWrite::new(10, "upd7759".into(), 0, 1, 1),
        ];
        let (samples, warnings) = collect_upd7759(&missing, false).unwrap();
        assert!(samples.is_empty());
        assert_eq!(
            warnings,
            vec![
                "uPD7759 chip 1 has no type-0x8A ROM block",
                "uPD7759 chip 1 start at 10: no ROM at bank 0x0",
            ]
        );
    }
}
