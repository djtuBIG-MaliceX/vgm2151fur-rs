//! VGM / VGZ loader and command walker.
//!
//! Literal port of `vgm2151fur/vgm.py` from vgm2151fur 0.11.0. The header
//! gates, command sizes, data-block decoding and the command loop reproduce
//! the Python order of operations; warning strings are byte-for-byte copies.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::io::Read;
use std::path::{Path, PathBuf};

use flate2::read::MultiGzDecoder;
use indexmap::IndexMap;

use crate::error::{Error, Result};

pub const VGM_RATE: i64 = 44100;
pub const CMD_YM2151: u8 = 0x54;
// The OPL family. 0x5B is the YM3526 and 0x5A the YM3812; both address one
// flat register space. The YMF262 has two banks of nine channels, and the VGM
// spec gives each bank its own command: 0x5E is port 0, 0x5F port 1. Every
// per-channel and per-operator register exists in both banks and only the port
// tells them apart. The bank rides in bit 8 of ChipWrite.reg exactly like the
// YM2612's second register bank does.
pub const CMD_YM3526: u8 = 0x5B;
pub const CMD_YM3812: u8 = 0x5A;
// The Y8950 is an OPL1 with the delta-T (ADPCM-B) engine bolted on. Register
// writes arrive on 0x5C; the ADPCM registers are 0x07 (control), 0x08/0x09
// (start), 0x0A/0x0B (end), 0x10/0x11 (pitch) and 0x12 (level).
pub const CMD_Y8950: u8 = 0x5C;
// 0x5D is the YMZ280B (PCMD8): register, value.  A dual-chip log uses the
// second instance at 0xAD, the usual "+0x50" YM-family rule.
pub const CMD_YMZ280B: u8 = 0x5D;
pub const CMD_YMZ280B_2: u8 = 0xAD;
// A dual-OPL2 log (the Sound Blaster Pro layout) keeps the second chip's
// register stream in its own command. Confirmed on the Knights of Xentar rips:
// header bit 30 says "Dual YM3812" and 0xAA carries chip 1's writes while
// 0x5A stays on chip 0. There is no spec'd companion for the YM3526. A second
// OPL instance is reported instead of guessed at.
pub const CMD_YM3812_2: u8 = 0xAA;
pub const CMD_OPL3_P0: u8 = 0x5E;
pub const CMD_OPL3_P1: u8 = 0x5F;
// 0x52/0x53 are the first YM2612's two register banks; 0xA2/0xA3 are the
// second instance in a dual-chip log.  The chip's 9-bit register address is
// stored flat as (bank << 8) | register, matching ymfm's index.
pub const CMD_YM2612_P0: u8 = 0x52;
pub const CMD_YM2612_P1: u8 = 0x53;
pub const CMD_YM2612_B_P0: u8 = 0xA2;
pub const CMD_YM2612_B_P1: u8 = 0xA3;
// 0x58/0x59 are the YM2610's two register ports; 0xA8/0xA9 are the second
// instance.  Port 0 holds SSG, ADPCM-B and the first two FM channels; port 1
// holds ADPCM-A and the remaining FM channels.
pub const CMD_YM2610_P0: u8 = 0x58;
pub const CMD_YM2610_P1: u8 = 0x59;
pub const CMD_YM2610_B_P0: u8 = 0xA8;
pub const CMD_YM2610_B_P1: u8 = 0xA9;
// 0x56/0x57 are the YM2608's two register buses; 0xA6/0xA7 the second chip
// instance.  Bus A holds SSG (<0x10), rhythm (0x10-0x1F) and FM block 1;
// bus B holds ADPCM-B (<=0x0F) and FM block 2.  Bus B rides in bit 8 of reg
// like the YM2612's bank, so every OPN decode can treat one flat address space.
pub const CMD_YM2608_P0: u8 = 0x56;
pub const CMD_YM2608_P1: u8 = 0x57;
pub const CMD_YM2608_B_P0: u8 = 0xA6;
pub const CMD_YM2608_B_P1: u8 = 0xA7;
// 0x51 is the YM2413; its dual instance uses 0xA1 (YM family: +0x50). The chip
// addresses one flat space; the banked user-instrument registers are < 0x10 and
// rhythm control is $0E.  Bit 31 of the clock field flags a Konami VRC7, which
// is the same core with six channels and a different preset ROM.
pub const CMD_YM2413: u8 = 0x51;
pub const CMD_YM2413_2: u8 = 0xA1;
pub const CMD_SN76489: u8 = 0x50;
// 0x30 is the second SN76489 in a dual/T6W28 log; 0x3F/0x4F are the second/
// first Game Gear stereo masks (Furnace's vgmOps uses 0x3f for the second).
pub const CMD_SN76489_2: u8 = 0x30;
pub const CMD_GGSTEREO: u8 = 0x4F;
pub const CMD_GGSTEREO_2: u8 = 0x3F;
// 0xA4 is the second YM2151 (YM family: +0x50).
pub const CMD_YM2151_2: u8 = 0xA4;
// The OPL family's second-instance commands. 0xAB/0xAC are the YM3526 and
// Y8950 pairs, 0xAE/0xAF the YMF262's two ports.
pub const CMD_YM3526_2: u8 = 0xAB;
pub const CMD_Y8950_2: u8 = 0xAC;
pub const CMD_OPL3_B_P0: u8 = 0xAE;
pub const CMD_OPL3_B_P1: u8 = 0xAF;
// 0xB3 is the Game Boy DMG (LR35902) APU: register, data.  Register 0x00 maps
// FF10 (NR10), so 0x00-0x16 are the sound registers and 0x20-0x2F the wave RAM.
// The DMG has a single APU, so there is no instance bit.
pub const CMD_GB: u8 = 0xB3;
// 0xB4 is the NES APU (2A03): register, data.  Reg 0x00-0x17 map $4000-$4017;
// bit 7 of the register selects the second APU instance.
pub const CMD_NES: u8 = 0xB4;
// 0xB9 is the HuC6280 PSG (PC Engine/TurboGrafx-16): register, data.  The
// register is a channel-select latch at 0x00 followed by per-channel writes;
// bit 7 of the register selects the second chip instance.
pub const CMD_HUC6280: u8 = 0xB9;
pub const CMD_DAC_WRITE: u8 = 0x2A;
pub const CMD_DAC_ENABLE: u8 = 0x2B;
// 0x55 is the first YM2203's register port; 0xA5 is the second instance in a
// dual-chip log.  Registers 0x00-0x0F are the built-in SSG, everything above
// is FM.
pub const CMD_YM2203: u8 = 0x55;
pub const CMD_YM2203_2: u8 = 0xA5;
pub const CMD_K007232: u8 = 0x41;
// 0x42 is the Konami K005289 (Bubble System WSG): a 4-bit register offset in
// the high nibble of the first operand and a 12-bit value.  The waveform PROM
// is a 0xC3 data block (16-bit address, then 512 bytes).
pub const CMD_K005289: u8 = 0x42;
pub const CMD_AY8910: u8 = 0xA0;
pub const CMD_MSM6258: u8 = 0xB7;
pub const CMD_MSM6295: u8 = 0xB8;
pub const CMD_SEGAPCM: u8 = 0xC0;
pub const CMD_RF5C68: u8 = 0xB0;
pub const CMD_RF5C164: u8 = 0xB1;
pub const CMD_RF5C68_MEM: u8 = 0xC1;
pub const CMD_RF5C164_MEM: u8 = 0xC2;
// YMF278B/OPL4: port 0/1 are the OPL3-compatible FM banks, port 2 the 24
// wave slots (10 registers each, 0x08-0xF7).
pub const CMD_YMF278B: u8 = 0xD0;
pub const CMD_SCC: u8 = 0xD2;
// 0xD3 is the Konami K054539: 16-bit big-endian offset (bit 15 selects the
// second chip), then one byte of data.
pub const CMD_K054539: u8 = 0xD3;
pub const CMD_C140: u8 = 0xD4;
pub const CMD_C352: u8 = 0xE1;
pub const CMD_K053260: u8 = 0xBA;
// 0xC7 mmll dd is the Virtual Boy VSU: the register is a 16-bit big-endian
// *word index* (byte address / 4).  Furnace writes it as writeS_BE(addr>>2),
// and the reference rips use the same form.  Bit 15 selects a second VSU in a
// dual-chip log (Furnace's baseAddr2S rule).
pub const CMD_VSU: u8 = 0xC7;
pub const CMD_SAA1099: u8 = 0xBD;
// Atari POKEY: 0xBB register write. Bit 7 of the register byte selects the
// second chip (Dual Chip Support #2).
pub const CMD_POKEY: u8 = 0xBB;
pub const VSU_DUAL_BIT: u32 = 0x8000;
pub const CMD_DATA_BLOCK: u8 = 0x67;
pub const CMD_END: u8 = 0x66;
pub const CMD_STREAM_SETUP: u8 = 0x90;
// Stream-control chip type (0x90 tt), not the extra-header id. OKIM6258 is 0x17
// and the HuC6280 is 0x1B (libvgm's device index).
pub const STREAM_OKIM6258: u8 = 0x17;
pub const STREAM_C6280: u8 = 0x1B;
pub const STREAM_PWM: u8 = 0x11;
// The YM2612 DAC stream driver, the other type this tool reconstructs. Its data
// bank is the type-0x00 PCM database and its register is $2A.
pub const DAC_STREAM_YM2612: u8 = 0x02;
pub const K007232_ROM_TYPE: u8 = 0x94; // 0x80 + 0x14
pub const QSOUND_ROM_TYPE: u8 = 0x8F; // Capcom QSound sample ROM
pub const OKI_ROM_TYPE: u8 = 0x8B; // OKIM6295 ROM
pub const SEGAPCM_ROM_TYPE: u8 = 0x80; // SegaPCM ROM
// NES APU DPCM sample RAM (data-block type 0xC2): 16-bit start then bytes.
// It shares the 0xC2 number with the RF5C164 memory command, but this is the
// data-block type byte, not a stream command.
pub const NES_DPCM_RAM_TYPE: u8 = 0xC2;
// Type 0x07 is the NES APU DPCM *database*: a flat bank the 0x68 RAM-write
// command copies into $C000/$D000 while the song plays.
pub const NES_DPCM_BANK_TYPE: u8 = 0x07;
// Type 0x05 is the HuC6280 PCM database.  A DAC stream (0x91 bank id 5) plays
// 8-bit DDA levels out of the concatenation of these blocks.
pub const HUC6280_PCM_TYPE: u8 = 0x05;
pub const RF5C68_RAM_TYPE: u8 = 0xC0; // RF5C68 sample-RAM write (16-bit start + data)
pub const RF5C164_RAM_TYPE: u8 = 0xC1; // RF5C164 sample-RAM write (16-bit start + data)
pub const Y8950_DELTAT_ROM_TYPE: u8 = 0x88; // Y8950 delta-T (ADPCM-B) ROM
pub const YMF278B_ROM_TYPE: u8 = 0x84; // YMF278B (OPL4) ROM image
pub const YMF278B_RAM_TYPE: u8 = 0x87; // YMF278B (OPL4) RAM image
pub const YMZ280B_ROM_TYPE: u8 = 0x86; // YMZ280B external sample ROM
pub const MULTIPCM_ROM_TYPE: u8 = 0x89; // YMW258-F (MultiPCM) ROM image
pub const UPD7759_ROM_TYPE: u8 = 0x8A; // uPD7759 phrase ROM image
pub const K054539_ROM_TYPE: u8 = 0x8C; // Konami K054539 sample ROM
pub const C140_ROM_TYPE: u8 = 0x8D; // Namco C140 / C219 ROM
pub const C352_ROM_TYPE: u8 = 0x92; // Namco C352 ROM
pub const YM2610_ADPCM_ROM_TYPE: u8 = 0x82; // YM2610 ADPCM-A/B ROM
pub const YM2610_DELTAT_ROM_TYPE: u8 = 0x83; // YM2610 delta-T ROM
pub const YM2608_DELTAT_ROM_TYPE: u8 = 0x81; // YM2608 ADPCM-B (delta-T) ROM
pub const K053260_ROM_TYPE: u8 = 0x8E; // Konami K053260 ROM
// Real K007232 key-on registers are 0x05 (voice 0) and 0x0B (voice 1).
// 0x1F is retained only for older synthetic fixtures that used that offset.
pub const K007232_TRIGGER_REG: u8 = 0x1F;
// Extra-header chip ids (libvgm device index). 0x02 is YM2612, 0x2A K007232.
pub const EXTRA_MSM6258: u8 = 0x16;
pub const EXTRA_MSM6295: u8 = 0x17;
pub const EXTRA_YM2612: u8 = 0x02;
pub const EXTRA_YM2203: u8 = 0x06;
pub const EXTRA_YM2610: u8 = 0x08;
pub const EXTRA_SEGAPCM: u8 = 0x04;
pub const EXTRA_AY8910: u8 = 0x12;
pub const EXTRA_K051649: u8 = 0x19;
pub const EXTRA_HUC6280: u8 = 0x1B;
pub const EXTRA_C140: u8 = 0x1C;
pub const EXTRA_K054539: u8 = 0x1A;
pub const EXTRA_K053260: u8 = 0x1D;
pub const EXTRA_C352: u8 = 0x27;
pub const EXTRA_K007232: u8 = 0x2A;
pub const EXTRA_VSU: u8 = 0x22; // Virtual Boy VSU (VGM chip-volume id 34)
pub const EXTRA_YMZ280B: u8 = 0x0F;
pub const EXTRA_MULTIPCM: u8 = 0x15; // Yamaha YMW258-F (MultiPCM)
pub const EXTRA_UPD7759: u8 = 0x16; // NEC uPD7759
pub const EXTRA_POKEY: u8 = 0x1E; // Atari POKEY (same id as libvgm's DEVID_POKEY)
pub const EXTRA_MIKEY: u8 = 0x29; // Atari Lynx Mikey (libvgm DEVID_MIKEY)
pub const EXTRA_X1_010: u8 = 0x26; // Seta X1-010 (libvgm DEVID_X1_010)
pub const EXTRA_QSOUND: u8 = 0x1F; // Capcom QSound (libvgm DEVID_QSOUND)
pub const EXTRA_WSWAN: u8 = 0x21; // Bandai WonderSwan (libvgm DEVID_WSWAN)
// Main-header clocks (VGMFile.h layout, spec v1.71). 0x98 is MSM6295, 0x90
// MSM6258, 0x94 its flags. K007232 has no spec field; VGMPlay tools put it at
// 0xE8 (1.71+). C140 is 0xA8 and its type byte 0x96. AY-8910 is 0x74 with its
// type byte at 0x78 and flags at 0x79 (1.51+); K051649 is 0x9C (1.61+), where
// bit 31 selects the K052539 (SCC-I / SCC+) variant.
pub const SEGAPCM_CLOCK_OFF: usize = 0x38;
pub const SEGAPCM_INTF_OFF: usize = 0x3C; // bank shift; mask is the byte at 0x3E (0 -> 0x70)
pub const SEGAPCM_MASK_OFF: usize = 0x3E;
// RF5C68 header 0x40 and RF5C164 header 0x6C (both VGM 1.51+). The two chips
// share one register interface: $B0/$B1 write (reg, value) and $C1/$C2 write
// (offset, value) into sample RAM. Furnace hosts both as one system with a
// chipType flag, so the converter reads the clock to tell them apart.
pub const RF5C68_CLOCK_OFF: usize = 0x40;
pub const RF5C164_CLOCK_OFF: usize = 0x6C;
pub const RF5C68_DEFAULT_CLOCK: u32 = 8_000_000;
pub const RF5C164_DEFAULT_CLOCK: u32 = 12_500_000;
// Y8950 header clock 0x58 (VGM 1.51+). The FM half is an OPL1 and the ADPCM
// half the same delta-T engine the YM2608 carries; its ROM block is 0x88.
pub const Y8950_CLOCK_OFF: usize = 0x58;
pub const Y8950_DEFAULT_CLOCK: u32 = 3_579_545;
// YMF278B/OPL4 header clock 0x60 (VGM 1.51+), usual value 33868800. The FM
// half is OPL3-shaped and the wave half has 24 sample slots fed by the 0x84
// ROM and 0x87 RAM blocks.
pub const YMF278B_CLOCK_OFF: usize = 0x60;
pub const YMF278B_DEFAULT_CLOCK: u32 = 33_868_800;
// YMZ280B header 0x68 (VGM 1.51+).  Bit 30 marks a second chip; its writes
// arrive on command 0xAD and a data block's size bit 31 selects its ROM.
pub const YMZ280B_CLOCK_OFF: usize = 0x68;
pub const YMZ280B_DEFAULT_CLOCK: u32 = 16_934_400;
pub const YMZ280B_DUAL_BIT: u32 = 0x40000000;
// Virtual Boy VSU clock at 0x68? No: VGM 1.71 puts it at 0xC4.  Bit 30 marks
// a second chip, whose writes use register bit 15.  The usual clock is 5 MHz.
pub const VSU_CLOCK_OFF: usize = 0xC4;
pub const VSU_DEFAULT_CLOCK: u32 = 5_000_000;
pub const VSU_HEADER_DUAL_BIT: u32 = 0x40000000;
// YM2612 clock at 0x2C.  Bit 30 is the dual-instance flag; 0xA2/0xA3 then
// answer for the second chip.  The field predates the 1.50 data-offset header.
pub const YM2612_CLOCK_OFF: usize = 0x2C;
pub const YM2612_DUAL_BIT: u32 = 0x40000000;
pub const YM2612_DEFAULT_CLOCK: u32 = 7_670_453; // NTSC Genesis master clock (COLOR_NTSC * 15/7)
// VGM 0x0C is the SN76489 clock.  The Genesis feeds it COLOR_NTSC/1 = 3579545;
// Furnace's SMS core runs the same clock and derives note periods with
// toneDivider 64.
pub const SN76489_CLOCK_OFF: usize = 0x0C;
pub const SN76489_DEFAULT_CLOCK: u32 = 3_579_545;
// The clock field's high bits are the usual VGM flags: bit 30 marks a second
// SN76489 instance, and bit 31 selects the Toshiba T6W28 variant, whose two
// halves are the same silicon wired as a stereo pair.  vgm2txt labels a
// field T6W28 only when both bits are set; the second half then answers on
// command 0x30.
pub const SN76489_DUAL_BIT: u32 = 0x40000000;
pub const SN76489_T6W28_BIT: u32 = 0x80000000;
pub const EXTRA_SN76489: u8 = 0x00; // extra-header clock id for the first/only SN76489
// YM2203 clock at 0x44.  Bit 30 is the "cheat mode" flag for a second
// instance, which then answers on command 0xA5; the clock itself is the same.
pub const YM2203_CLOCK_OFF: usize = 0x44;
pub const YM2203_DUAL_BIT: u32 = 0x40000000;
pub const YM2203_DEFAULT_CLOCK: u32 = 3_579_545;
// YM2610 clock at 0x4C.  Bit 31 selects the YM2610B (six FM channels), bit 30
// marks the dual-instance cheat.  The B bit is part of the same field.
pub const YM2610_CLOCK_OFF: usize = 0x4C;
pub const YM2610_B_BIT: u32 = 0x80000000;
pub const YM2610_DUAL_BIT: u32 = 0x40000000;
// YM2608 joined the header in 1.51 at 0x48; bit 30 is its second instance
// ($A6/$A7). The rhythm/DTA stream additions are 1.61, but the clock field
// itself is 1.51 and rips sometimes carry older versions with it set anyway,
// so version-gate the field, not the feature. YM2413 shares the grid at 0x10;
// for VGM <= 1.4 that dword held the YM2612 clock (spec note on 0x2C).
pub const YM2608_CLOCK_OFF: usize = 0x48;
pub const YM2608_DUAL_BIT: u32 = 0x40000000;
pub const YM2413_CLOCK_OFF: usize = 0x10;
// Bit 31 of the YM2413 clock field flags a Konami VRC7 (same OPLL core, six
// channels, its own preset ROM); the low 30 bits stay the clock.
pub const YM2413_VRC7_BIT: u32 = 0x80000000;
pub const YM2610_DEFAULT_CLOCK: u32 = 8_000_000;
// Game Boy DMG header 0x80 (VGM 1.51+).  The clock is the DMG CPU clock, always
// 4194304 Hz on real hardware.  It sits just before the NES APU field.
pub const GB_DMG_CLOCK_OFF: usize = 0x80;
pub const GB_DMG_DEFAULT_CLOCK: u32 = 4_194_304;
pub const EXTRA_GB: u8 = 0x13; // extra-header clock-list chip id (libvgm DMG)
// NES APU header 0x84 (VGM 1.51+).  0x80 is the Game Boy DMG clock, inserted
// ahead of the NES when the header grew; the NES field moved to 0x84.  Bit 30
// marks a second APU, bit 31 an FDS channel.  The clock is the CPU clock:
// COLOR_NTSC / 2 on an NTSC console.  (This was 0x80 and worked only because
// the NTSC default fallback happened to match the files.)
pub const NES_CLOCK_OFF: usize = 0x84;
pub const NES_DUAL_BIT: u32 = 0x40000000;
pub const NES_FDS_BIT: u32 = 0x80000000;
pub const NES_DEFAULT_CLOCK: u32 = 1_789_773; // COLOR_NTSC / 2
pub const EXTRA_NES: u8 = 0x14; // extra-header clock-list chip id
// HuC6280 (PC Engine) header 0xA4 (VGM 1.61+).  The clock is the PSG clock
// (COLOR_NTSC on NTSC hardware); the CPU/PSG core divides it internally.
pub const HUC6280_CLOCK_OFF: usize = 0xA4;
pub const HUC6280_DEFAULT_CLOCK: u32 = 3_579_545; // COLOR_NTSC, the PSG clock
// The OPL family joined the header in 1.51: YM3812 at 0x50, YM3526 at 0x54 and
// YMF262 at 0x5C. Bit 30 of the YM3812 field is the dual-chip flag that lets
// command 0xAA address the second one (the Sound Blaster Pro pair); rips also
// set bit 31, which the players use for their own chip-count bookkeeping. The
// spec defines no second-instance command for the YM3526. A dual OPL flag is
// reported rather than guessed at.
pub const OPL2_CLOCK_OFF: usize = 0x50;
pub const OPL_CLOCK_OFF: usize = 0x54;
pub const OPL3_CLOCK_OFF: usize = 0x5C;
pub const OPL_DUAL_BIT: u32 = 0x40000000;
pub const OPL_DEFAULT_CLOCK: u32 = 3_579_545; // COLOR_NTSC, the AdLib and arcade standard
pub const OPL2_DEFAULT_CLOCK: u32 = 3_579_545; // YM3812; the same crystal as the YM3526
pub const OPL3_DEFAULT_CLOCK: u32 = 14_318_180; // COLOR_NTSC * 4, the OPL3 board standard
pub const AY8910_CLOCK_OFF: usize = 0x74;
pub const AY8910_TYPE_OFF: usize = 0x78;
pub const AY8910_FLAGS_OFF: usize = 0x79;
pub const MSM6258_CLOCK_OFF: usize = 0x90;
pub const MSM6295_CLOCK_OFF: usize = 0x98;
pub const K051649_CLOCK_OFF: usize = 0x9C;
pub const K007232_EXTRA_CLOCK_OFF: usize = 0xE8;
// Konami K005289 (Bubble System WSG) clock 0xEC (VGM 1.71+).  Furnace's WSG
// platform defaults to COLOR_NTSC and divides by 32; the 512-byte 4-bit PROM
// (8 waveforms per channel) arrives as a 0xC3 RAM block.
pub const K005289_CLOCK_OFF: usize = 0xEC;
pub const K005289_DEFAULT_CLOCK: u32 = 3_579_545;
pub const K005289_RAM_TYPE: u8 = 0xC3;
pub const C140_CLOCK_OFF: usize = 0xA8;
pub const C140_TYPE_OFF: usize = 0x96;
// Philips SAA1099 header 0xC8 (VGM 1.71+).  6 square/noise voices with two
// 3-channel noise generators; bit 30 marks a second chip, which shares the
// clock and selects itself with bit 7 of the 0xBD register byte.
pub const SAA1099_CLOCK_OFF: usize = 0xC8;
pub const SAA1099_DEFAULT_CLOCK: u32 = 8_000_000;
pub const SAA1099_DUAL_BIT: u32 = 0x40000000;
// Atari POKEY header 0xB0 (VGM 1.50+).  Bit 30 marks a second chip; the second
// instance selects itself with bit 7 of the 0xBB register byte and normally
// shares this clock (the extra header can carry a separate one).
pub const POKEY_CLOCK_OFF: usize = 0xB0;
pub const POKEY_DEFAULT_CLOCK: u32 = 1_789_772;
pub const POKEY_DUAL_BIT: u32 = 0x40000000;
pub const K053260_CLOCK_OFF: usize = 0xAC; // VGM 1.61+, one clock for both instances
pub const K053260_DEFAULT_CLOCK: u32 = 3_579_545;
// MultiPCM header 0x88 (VGM 1.61+): 28 sample voices.  Bit 30 is the second
// instance.  The VGM clock is based on the old /180 divider; VGMPlay converts
// it with *224/180 before the core (which divides by 224).
pub const MULTIPCM_CLOCK_OFF: usize = 0x88;
pub const MULTIPCM_DEFAULT_CLOCK: u32 = 8_053_975;
pub const MULTIPCM_DUAL_BIT: u32 = 0x40000000;
// uPD7759 header 0x8C (VGM 1.61+).  Bit 30 is a second chip, bit 31 selects
// slave mode (the CPU streams command bytes through the port instead of using
// the chip's own phrase ROM).
pub const UPD7759_CLOCK_OFF: usize = 0x8C;
pub const UPD7759_DEFAULT_CLOCK: u32 = 640_000;
pub const UPD7759_DUAL_BIT: u32 = 0x40000000;
pub const UPD7759_SLAVE_BIT: u32 = 0x80000000;
// Sega 32X PWM header 0x70 (VGM 1.51+).  The chip is a stereo streamed DAC;
// command 0xB2 writes its control/cycle/level registers and the DAC-stream
// control commands feed 12-bit samples to the left/right ports.
pub const PWM_CLOCK_OFF: usize = 0x70;
pub const PWM_DEFAULT_CLOCK: u32 = 23_011_361;
pub const PWM_DUAL_BIT: u32 = 0x40000000;
pub const PWM_DATA_TYPE: u8 = 0x03; // PWM PCM data bank
// Atari Lynx Mikey header 0xE4 (VGM 1.72+).  Command 0x40 writes one 8-bit
// register; four voices can run as LFSR tones or CPU-fed 8-bit DAC samples.
pub const MIKEY_CLOCK_OFF: usize = 0xE4;
pub const MIKEY_DEFAULT_CLOCK: u32 = 16_000_000;
pub const MIKEY_DUAL_BIT: u32 = 0x40000000;
pub const CMD_MIKEY: u8 = 0x40;
// Seta/Allumer X1-010 header 0xD8 (VGM 1.71+).  Command 0xC8 writes a 16-bit
// big-endian register/memory offset plus one byte; 0x91 is its 1 MB sample ROM.
pub const X1_010_CLOCK_OFF: usize = 0xD8;
pub const X1_010_DEFAULT_CLOCK: u32 = 16_000_000;
pub const X1_010_DUAL_BIT: u32 = 0x40000000;
pub const X1_010_ROM_TYPE: u8 = 0x91;
pub const CMD_X1_010: u8 = 0xC8;
// Capcom QSound header 0xB4 (VGM 1.51+).  The 4 MHz field used by old rips
// represents the same chip that libvgm runs at 60 MHz (multiply by 15).
pub const QSOUND_CLOCK_OFF: usize = 0xB4;
pub const QSOUND_DEFAULT_CLOCK: u32 = 60_000_000;
pub const QSOUND_LEGACY_CLOCK: u32 = 4_000_000;
pub const CMD_QSOUND: u8 = 0xC4;
// Bandai WonderSwan header 0xC0 (VGM 1.71+).  Register writes are 0xBC and
// internal-RAM writes 0xC6.  Bit 30 of the clock field marks a second chip.
pub const WSWAN_CLOCK_OFF: usize = 0xC0;
pub const WSWAN_DEFAULT_CLOCK: u32 = 3_072_000;
pub const WSWAN_DUAL_BIT: u32 = 0x40000000;
pub const CMD_WSWAN: u8 = 0xBC;
pub const CMD_WSWAN_MEM: u8 = 0xC6;
pub const C352_CLOCK_OFF: usize = 0xDC; // VGM 1.71 crystal; bit 31 mutes the rear pair
pub const C352_DIV_OFF: usize = 0xD6; // hardware divider / 4; 72 means a divider of 288
pub const SEGAPCM_CLOCK_X: u32 = 4_000_000; // X board / Hang-On
pub const SEGAPCM_CLOCK_Y: u32 = 4_026_987; // Y board: 32215900/8
pub const C140_DEFAULT_CLOCK: u32 = 12_288_000; // System 2/21 rips with a bogus header field
pub const C219_DEFAULT_CLOCK: u32 = 25_056_500; // libvgm's fallback for a missing C219 clock
pub const C219_CLOCK_MAX: u32 = 100_000_000;
// Konami K054539 header 0xA0.  The flag byte at 0x95 is copied by libvgm into
// the core configuration; the clock field has bit 30 for a second chip.
pub const K054539_CLOCK_OFF: usize = 0xA0;
pub const K054539_FLAGS_OFF: usize = 0x95;
pub const K054539_DEFAULT_CLOCK: u32 = 18_432_000;
pub const K054539_DUAL_BIT: u32 = 0x40000000;
// Irem GA20 header 0xE0 (VGM 1.71+).  Four 8-bit sample voices; command 0xBF
// writes one register, and ROM block 0x93 carries the sample ROM.
pub const GA20_CLOCK_OFF: usize = 0xE0;
pub const GA20_DEFAULT_CLOCK: u32 = 3_579_545;
pub const GA20_DUAL_BIT: u32 = 0x40000000;
pub const GA20_ROM_TYPE: u8 = 0x93;
pub const CMD_GA20: u8 = 0xBF;
pub const EXTRA_GA20: u8 = 0x28;
// OKI MSM5205 header 0xF0 (VGM 1.72+).  The CPU streams ADPCM nibbles through
// command 0x32; there is no sample ROM block.
pub const MSM5205_CLOCK_OFF: usize = 0xF0;
pub const MSM5205_DEFAULT_CLOCK: u32 = 384_000;
pub const MSM5205_DUAL_BIT: u32 = 0x40000000;
pub const MSM5205_FLAGS_OFF: usize = 0xD7;
pub const CMD_MSM5205: u8 = 0x32;
pub const EXTRA_MSM5205: u8 = 0x2C;
// OKI MSM5232 header 0xF4 (VGM 1.72+).  Eight tone/noise voices with two
// four-channel envelope groups; command 0x43 writes one register.  Some older
// libvgm builds used 0x2D, so accept that spelling too.
pub const MSM5232_CLOCK_OFF: usize = 0xF4;
pub const MSM5232_DEFAULT_CLOCK: u32 = 2_119_040;
pub const MSM5232_DUAL_BIT: u32 = 0x40000000;
pub const CMD_MSM5232: u8 = 0x43;
// Ensoniq ES5503 header 0xCC (VGM 1.71+), output-channel count at 0xD4.
// Command 0xD5 is an 8-bit port (chip), 8-bit register and byte of data.
// Wave RAM arrives as data-block type 0xE1: a 32-bit start address and bytes.
pub const ES5503_CLOCK_OFF: usize = 0xCC;
pub const ES5503_CHANNELS_OFF: usize = 0xD4;
pub const ES5503_DEFAULT_CLOCK: u32 = 7_159_090;
pub const ES5503_RAM_TYPE: u8 = 0xE1;
pub const CMD_ES5503: u8 = 0xD5;
pub const CMD_MSM5232_LEGACY: u8 = 0x2D;
pub const EXTRA_MSM5232: u8 = 0x2D;
pub const AY8910_DEFAULT_CLOCK: u32 = 1_789_772; // COLOR_NTSC/2, the MSX/ZX PSG clock
pub const K051649_DEFAULT_CLOCK: u32 = 1_789_772; // MSX SCC (half the 3.579545 MHz slot clock)
pub const CLOCK_MIN: u32 = 100_000;
pub const CLOCK_MAX: u32 = 40_000_000;
// Sample-chip write commands this tool does not convert. Anything here that
// shows up in a stream means samples are being dropped. Say so.
pub const UNCONVERTED_SAMPLE_CMDS: &[(u8, &str)] = &[(0x40, "NES APU DPCM")];

/// Commands defined by the VGM spec. 0x60/0x64/0x65/0x69-0x6F are undefined;
/// 0x00-0x2F are reserved. Anything outside this set stops parsing.
pub fn is_known_cmd(cmd: u8) -> bool {
    cmd == 0x2D
        || (0x61..=0x63).contains(&cmd)
        || cmd == 0x66
        || cmd == 0x67
        || cmd == 0x68
        || (0x30..=0x5F).contains(&cmd)
        || (0x70..=0x95).contains(&cmd)
        || (0xA0..=0xFF).contains(&cmd)
}

fn le_u32_at(buf: &[u8], off: usize) -> u32 {
    let mut value = 0u32;
    for i in 0..4 {
        if let Some(&byte) = buf.get(off + i) {
            value |= (byte as u32) << (8 * i);
        }
    }
    value
}

fn le_u16_at(buf: &[u8], off: usize) -> u16 {
    let mut value = 0u16;
    for i in 0..2 {
        if let Some(&byte) = buf.get(off + i) {
            value |= (byte as u16) << (8 * i);
        }
    }
    value
}

fn be_u16_at(buf: &[u8], off: usize) -> u16 {
    let hi = buf.get(off).copied().unwrap_or(0) as u16;
    let lo = buf.get(off + 1).copied().unwrap_or(0) as u16;
    (hi << 8) | lo
}

/// Python `repr(bytes)` for the four-byte prefix in the `data_offset` error.
fn py_bytes_repr(bytes: &[u8]) -> String {
    let has_single = bytes.contains(&b'\'');
    let has_double = bytes.contains(&b'"');
    let quote = if has_single && !has_double { b'"' } else { b'\'' };
    let mut out = String::from("b");
    out.push(quote as char);
    for &byte in bytes {
        match byte {
            b'\\' => out.push_str("\\\\"),
            b'\t' => out.push_str("\\t"),
            b'\n' => out.push_str("\\n"),
            b'\r' => out.push_str("\\r"),
            _ if byte == quote => {
                out.push('\\');
                out.push(quote as char);
            }
            0x20..=0x7E => out.push(byte as char),
            _ => out.push_str(&format!("\\x{byte:02x}")),
        }
    }
    out.push(quote as char);
    out
}

pub fn read_vgm_bytes(path: &Path) -> Result<Vec<u8>> {
    let raw = std::fs::read(path).map_err(|e| Error::io(path, e))?;
    if raw.len() >= 2 && raw[0] == 0x1f && raw[1] == 0x8b {
        let mut out = Vec::new();
        MultiGzDecoder::new(raw.as_slice())
            .read_to_end(&mut out)
            .map_err(|e| Error::io(path, e))?;
        return Ok(out);
    }
    Ok(raw)
}

pub fn data_offset(buf: &[u8]) -> Result<usize> {
    if buf.len() < 0x40 || buf.len() < 4 || &buf[..4] != b"Vgm " {
        let prefix = &buf[..buf.len().min(4)];
        return Err(Error::msg(format!("not a VGM: {}", py_bytes_repr(prefix))));
    }
    let ver = le_u32_at(buf, 8);
    if ver < 0x150 {
        return Ok(0x40);
    }
    let rel = le_u32_at(buf, 0x34);
    Ok(if rel != 0 { 0x34 + rel as usize } else { 0x40 })
}

pub fn cmd_size(cmd: u8) -> usize {
    if matches!(cmd, 0x62 | 0x63 | 0x66) || (0x70..=0x8F).contains(&cmd) {
        return 1;
    }
    if matches!(cmd, 0x4F | 0x50 | 0x94) || (0x30..=0x3F).contains(&cmd) {
        return 2;
    }
    if matches!(cmd, 0x2D | 0x61)
        || (0x40..=0x4E).contains(&cmd)
        || (0x51..=0x5F).contains(&cmd)
        || (0xA0..=0xBF).contains(&cmd)
    {
        return 3;
    }
    if (0xC0..=0xDF).contains(&cmd) {
        return 4;
    }
    if matches!(cmd, 0x90 | 0x91 | 0x95) || (0xE0..=0xFF).contains(&cmd) {
        return 5;
    }
    if cmd == 0x92 {
        return 6;
    }
    if cmd == 0x93 {
        return 11;
    }
    if cmd == 0x68 {
        return 12;
    }
    2
}

fn decode_utf16le_lossy(bytes: &[u8]) -> String {
    let mut units = Vec::with_capacity(bytes.len() / 2);
    let mut i = 0usize;
    while i + 1 < bytes.len() {
        units.push(u16::from_le_bytes([bytes[i], bytes[i + 1]]));
        i += 2;
    }
    let mut out: String = char::decode_utf16(units.into_iter())
        .map(|r| r.unwrap_or(char::REPLACEMENT_CHARACTER))
        .collect();
    if i < bytes.len() {
        out.push(char::REPLACEMENT_CHARACTER);
    }
    out
}

pub fn parse_gd3(buf: &[u8]) -> IndexMap<String, String> {
    const KEYS: [&str; 11] = [
        "track", "track_jp", "game", "game_jp", "system", "system_jp", "author", "author_jp",
        "date", "converter", "notes",
    ];
    let mut out: IndexMap<String, String> = IndexMap::new();
    for key in KEYS {
        out.insert(key.to_string(), String::new());
    }
    if buf.len() < 0x18 {
        return out;
    }
    let rel = le_u32_at(buf, 0x14);
    if rel == 0 {
        return out;
    }
    let mut pos = 0x14 + rel as usize;
    if pos + 4 > buf.len() || &buf[pos..pos + 4] != b"Gd3 " {
        return out;
    }
    pos += 12;
    for key in KEYS {
        let mut end = pos;
        while end + 1 < buf.len() && &buf[end..end + 2] != b"\x00\x00" {
            end += 2;
        }
        if pos <= end && end <= buf.len() {
            out.insert(key.to_string(), decode_utf16le_lossy(&buf[pos..end]));
        }
        pos = end + 2;
        if pos >= buf.len() {
            break;
        }
    }
    out
}

#[derive(Debug, Clone, PartialEq)]
pub struct RomSlice {
    pub chip: u8,
    pub rom_size: i64,
    pub start: i64,
    pub data: Vec<u8>,
    pub kind: &'static str,
    // Stream position of a RAM-image dump (0xC0-0xDF). A driver can reload the
    // same NES DPCM window several times while the song plays, so the samples
    // are only valid from this point on. ROM dumps leave it at 0.
    pub time: i64,
}

impl Default for RomSlice {
    fn default() -> Self {
        RomSlice {
            chip: 0,
            rom_size: 0,
            start: 0,
            data: Vec::new(),
            kind: "k007232",
            time: 0,
        }
    }
}

/// One OKIM6258 DAC-stream start (0x95, or 0x93).
#[derive(Debug, Clone, PartialEq)]
pub struct Msm6258Play {
    pub sample: i64,
    pub chip_id: u8,
    pub block: i32,
    pub rate: u32,
    pub loop_: bool,
    pub reverse: bool,
    pub offset: u32,
    pub nbytes: u32,
    pub end: i64,
    pub step: u32,
    pub step_base: u32,
    pub length_mode: u8,
}

impl Default for Msm6258Play {
    fn default() -> Self {
        // The reference dataclass defaults `length_mode` to 3 ("play the whole
        // block"); a derived 0 would take the concatenated-bank path.
        Msm6258Play {
            sample: 0,
            chip_id: 0,
            block: 0,
            rate: 0,
            loop_: false,
            reverse: false,
            offset: 0,
            nbytes: 0,
            end: 0,
            step: 1,
            step_base: 0,
            length_mode: 3,
        }
    }
}

/// One 0x68 PCM RAM write (a chunk copied from a data-block bank).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PcmRamWrite {
    pub sample: i64,
    pub type_: u8,
    pub chip_id: u8,
    pub src: i64,
    pub dest: i64,
    pub length: i64,
}

/// One generic DAC-stream start (0x93) for a chip other than the OKIM6258.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct DacStreamPlay {
    pub sample: i64,
    pub chip_type: u8,
    pub chip_id: u8,
    pub stream: u8,
    pub port: u8,
    pub reg: u8,
    pub offset: u32,
    pub length: u32,
    pub mode: u8,
    pub rate: u32,
    pub loop_: bool,
    pub reverse: bool,
    pub step: u32,
    pub step_base: u32,
    pub bank: u8,
    // 0x95 plays a whole data block by id; -1 for a 0x93 offset play.
    pub block: i32,
    // The 0x94 stop (or the next 0x93 start) that ends this play.  0 = the
    // stream ran to its declared length.
    pub stop_sample: i64,
}

impl DacStreamPlay {
    pub fn length_mode(&self) -> u8 {
        self.mode & 0x07
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ChipWrite {
    pub sample: i64,
    pub chip: &'static str,
    pub chip_id: u8,
    pub reg: u32,
    pub val: u32,
}

impl ChipWrite {
    pub fn new(sample: i64, chip: &'static str, chip_id: u8, reg: u32, val: u32) -> Self {
        ChipWrite {
            sample,
            chip,
            chip_id,
            reg,
            val,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct VgmFile {
    pub path: PathBuf,
    pub buf: Vec<u8>,
    pub version: u32,
    // Corrected to the 0x66 sample when the header disagrees: several logs
    // declare a length far past their last command, and patterns, grid
    // budgets and source-vs-export checks need the timeline the log covers.
    pub total_samples: i64,
    pub loop_samples: i64,
    pub loop_file_pos: Option<usize>,
    pub loop_sample: Option<i64>,
    pub ym2151_clock: u32,
    pub k007232_clock: u32,
    pub gd3: IndexMap<String, String>,
    pub ym2151_dual: bool,
    // VGM header 0x7C: output volume of the first chip (0x100 = unity).
    // 0.0 means the field is absent; the player default then applies.
    pub main_volume: f64,
    pub roms: Vec<RomSlice>,
    pub writes: Vec<ChipWrite>,
    pub warnings: Vec<String>,
    // YM2612 header 0x2C.  A dual-chip log carries bit 30; 0x52/0x53 target
    // the first instance and 0xA2/0xA3 the second.  Register addresses are
    // kept flat as bank << 8 | register.
    pub ym2612_clock: u32,
    pub ym2612_dual: bool,
    // YM2610 header 0x4C.  Bit 31 selects the B variant, bit 30 the second
    // instance.  Port 1 registers are stored as 0x100 | register.
    pub ym2610_clock: u32,
    pub ym2610_dual: bool,
    pub ym2610_b: bool,
    // YM2608 header 0x48 (VGM 1.51+). Bit 30 marks a second chip; its writes
    // arrive on $A6/$A7. Both the dual bit and the rhythm/ADPCM-B blocks are
    // new in 1.61, so version-gate them the way the header is gated elsewhere.
    pub ym2608_clock: u32,
    pub ym2608_dual: bool,
    // YM2413 header 0x10 (the first chip-clock field the format ever had).
    pub ym2413_clock: u32,
    pub ym2413_dual: bool,
    pub ym2413_vrc7: bool,
    // Game Boy DMG header 0x80 (VGM 1.61+).
    pub gb_dmg_clock: u32,
    pub gb_dmg_dual: bool,
    // NES APU (2A03) header 0x84 (VGM 1.51+).
    pub nes_clock: u32,
    pub nes_dual: bool,
    pub nes_fds: bool,
    pub nes_dpcm_bank: Vec<u8>,
    pub nes_dpcm_bank_base: IndexMap<u8, usize>,
    pub pcm_ram_writes: Vec<PcmRamWrite>,
    // HuC6280 (PC Engine) header 0xA4 (VGM 1.61+).
    pub huc6280_clock: u32,
    pub huc6280_dual: bool,
    pub huc6280_pcm: Vec<u8>,
    pub huc6280_pcm_blocks: Vec<Vec<u8>>,
    // Genesis SN76489 header 0x0C.
    pub sn76489_clock: u32,
    pub sn76489_clock_2: u32,
    pub sn76489_dual: bool,
    pub sn76489_t6w28: bool,
    pub sn76489_stereo: bool,
    // VGM 1.50 YM2612 PCM.
    pub dac_data: Vec<u8>,
    pub dac_events: Vec<(i64, u8, i64)>,
    pub dac_blocks: Vec<Vec<u8>>,
    pub dac_block_plays: Vec<(i64, i64, i64)>,
    pub dac_stream_plays: Vec<DacStreamPlay>,
    pub dac_stream_rates: IndexMap<u8, u32>,
    // YM2203 header 0x44.
    pub ym2203_clock: u32,
    pub ym2203_dual: bool,
    // The OPL family (header 1.51+).
    pub opl_clock: u32,
    pub opl_dual: bool,
    pub opl2_clock: u32,
    pub opl2_dual: bool,
    pub opl2_pan: bool,
    pub opl3_clock: u32,
    pub opl3_dual: bool,
    // MSM6295 header 0x98 (VGM 1.61+).
    pub msm6295_clock: u32,
    pub msm6295_pin7: bool,
    pub msm6295_dual: bool,
    // MSM6258 header 0x90 and flag byte 0x94 (divider in bits 0-1).
    pub msm6258_clock: u32,
    pub msm6258_flags: u8,
    pub msm6258_dual: bool,
    // SegaPCM header 0x38 (VGM 1.51+); C140 header 0xA8 + type byte 0x96.
    pub segapcm_clock: u32,
    pub segapcm_bankshift: u8,
    pub segapcm_bankmask: u8,
    pub segapcm_dual: bool,
    pub c140_clock: u32,
    pub c140_type: u8,
    pub c140_dual: bool,
    // Konami K054539 header 0xA0, flags 0x95.
    pub k054539_clock: u32,
    pub k054539_clock_2: u32,
    pub k054539_dual: bool,
    pub k054539_flags: u8,
    // RF5C68 header 0x40 and RF5C164 header 0x6C (VGM 1.51+).
    pub rf5c68_clock: u32,
    pub rf5c164_clock: u32,
    pub rf5c68_dual: bool,
    pub rf5c164_dual: bool,
    // Y8950 header 0x58 (VGM 1.51+).
    pub y8950_clock: u32,
    pub y8950_dual: bool,
    // YMF278B (OPL4) header 0x60 (VGM 1.51+).
    pub ymf278b_clock: u32,
    pub ymf278b_dual: bool,
    // YMZ280B header 0x68 (VGM 1.51+).
    pub ymz280b_clock: u32,
    pub ymz280b_clock_2: u32,
    pub ymz280b_dual: bool,
    // Virtual Boy VSU header 0xC4 (VGM 1.71+).
    pub vsu_clock: u32,
    pub vsu_clock_2: u32,
    pub vsu_dual: bool,
    // Philips SAA1099 header 0xC8 (VGM 1.71+).
    pub saa1099_clock: u32,
    pub saa1099_dual: bool,
    // K053260 header 0xAC (VGM 1.61+).
    pub k053260_clock: u32,
    pub k053260_dual: bool,
    // C352 header 0xDC / divider byte 0xD6 (VGM 1.71).
    pub c352_clock: u32,
    pub c352_divider: u8,
    pub c352_mute_rear: bool,
    // AY-8910 header 0x74 with type byte 0x78 and flags 0x79 (VGM 1.51+).
    pub ay8910_clock: u32,
    pub ay8910_type: u8,
    pub ay8910_flags: u8,
    pub ay8910_dual: bool,
    // Atari POKEY header 0xB0.
    pub pokey_clock: u32,
    pub pokey_clock_2: u32,
    pub pokey_dual: bool,
    // K051649 / K052539 (SCC) header 0x9C (VGM 1.61+).
    pub k051649_clock: u32,
    pub k051649_plus: bool,
    pub k051649_dual: bool,
    // K007232 header 0xE8 (VGM 1.71+); register bit 7 selects a second chip.
    pub k007232_dual: bool,
    // Konami K005289 (Bubble System WSG) header 0xEC (VGM 1.71+).
    pub k005289_clock: u32,
    // YMW258-F (MultiPCM) header 0x88 (VGM 1.61+).
    pub multipcm_clock: u32,
    pub multipcm_clock_2: u32,
    pub multipcm_dual: bool,
    // NEC uPD7759 header 0x8C (VGM 1.61+).
    pub upd7759_clock: u32,
    pub upd7759_clock_2: u32,
    pub upd7759_dual: bool,
    pub upd7759_slave: bool,
    // Sega 32X PWM header 0x70 (VGM 1.51+).
    pub pwm_clock: u32,
    pub pwm_clock_2: u32,
    pub pwm_dual: bool,
    pub pwm_bank: Vec<u8>,
    pub pwm_blocks: Vec<Vec<u8>>,
    // Atari Lynx Mikey header 0xE4 (VGM 1.72+).
    pub mikey_clock: u32,
    pub mikey_dual: bool,
    // Seta X1-010 header 0xD8 (VGM 1.71+).
    pub x1_010_clock: u32,
    pub x1_010_dual: bool,
    // Capcom QSound header 0xB4.
    pub qsound_clock: u32,
    // Bandai WonderSwan header 0xC0 (VGM 1.71+).
    pub wonderswan_clock: u32,
    pub wonderswan_dual: bool,
    // Irem GA20 header 0xE0 (VGM 1.71+).
    pub ga20_clock: u32,
    pub ga20_dual: bool,
    // OKI MSM5205 header 0xF0 (VGM 1.72+).
    pub msm5205_clock: u32,
    pub msm5205_flags: u8,
    pub msm5205_dual: bool,
    // OKI MSM5232 header 0xF4 (VGM 1.72+).
    pub msm5232_clock: u32,
    pub msm5232_dual: bool,
    // Ensoniq ES5503 header 0xCC (VGM 1.71+).
    pub es5503_clock: u32,
    pub es5503_channels: u8,
    // Extra-header clocks, raw (bit 31 kept). Keyed by libvgm chip id.
    pub extra_clocks: IndexMap<u8, u32>,
    // Extra-header volume mix: chip id -> (flags, raw value, 0x100 = 1.0).
    pub extra_volumes: IndexMap<u8, (u8, u16)>,
    pub stream_setups: i64,
    // OKIM6258 ADPCM bank. Data-block type 0x04, in file order.
    pub adpcm_blocks: Vec<Vec<u8>>,
    pub msm6258_plays: Vec<Msm6258Play>,
}

impl Default for VgmFile {
    fn default() -> Self {
        VgmFile {
            path: PathBuf::new(),
            buf: Vec::new(),
            version: 0,
            total_samples: 0,
            loop_samples: 0,
            loop_file_pos: None,
            loop_sample: None,
            ym2151_clock: 0,
            k007232_clock: 0,
            gd3: IndexMap::new(),
            ym2151_dual: false,
            main_volume: 0.0,
            roms: Vec::new(),
            writes: Vec::new(),
            warnings: Vec::new(),
            ym2612_clock: 0,
            ym2612_dual: false,
            ym2610_clock: 0,
            ym2610_dual: false,
            ym2610_b: false,
            ym2608_clock: 0,
            ym2608_dual: false,
            ym2413_clock: 0,
            ym2413_dual: false,
            ym2413_vrc7: false,
            gb_dmg_clock: 0,
            gb_dmg_dual: false,
            nes_clock: 0,
            nes_dual: false,
            nes_fds: false,
            nes_dpcm_bank: Vec::new(),
            nes_dpcm_bank_base: IndexMap::new(),
            pcm_ram_writes: Vec::new(),
            huc6280_clock: 0,
            huc6280_dual: false,
            huc6280_pcm: Vec::new(),
            huc6280_pcm_blocks: Vec::new(),
            sn76489_clock: 0,
            sn76489_clock_2: 0,
            sn76489_dual: false,
            sn76489_t6w28: false,
            sn76489_stereo: false,
            dac_data: Vec::new(),
            dac_events: Vec::new(),
            dac_blocks: Vec::new(),
            dac_block_plays: Vec::new(),
            dac_stream_plays: Vec::new(),
            dac_stream_rates: IndexMap::new(),
            ym2203_clock: 0,
            ym2203_dual: false,
            opl_clock: 0,
            opl_dual: false,
            opl2_clock: 0,
            opl2_dual: false,
            opl2_pan: false,
            opl3_clock: 0,
            opl3_dual: false,
            msm6295_clock: 0,
            msm6295_pin7: false,
            msm6295_dual: false,
            msm6258_clock: 0,
            msm6258_flags: 0,
            msm6258_dual: false,
            segapcm_clock: 0,
            segapcm_bankshift: 0,
            segapcm_bankmask: 0,
            segapcm_dual: false,
            c140_clock: 0,
            c140_type: 0,
            c140_dual: false,
            k054539_clock: 0,
            k054539_clock_2: 0,
            k054539_dual: false,
            k054539_flags: 0,
            rf5c68_clock: 0,
            rf5c164_clock: 0,
            rf5c68_dual: false,
            rf5c164_dual: false,
            y8950_clock: 0,
            y8950_dual: false,
            ymf278b_clock: 0,
            ymf278b_dual: false,
            ymz280b_clock: 0,
            ymz280b_clock_2: 0,
            ymz280b_dual: false,
            vsu_clock: 0,
            vsu_clock_2: 0,
            vsu_dual: false,
            saa1099_clock: 0,
            saa1099_dual: false,
            k053260_clock: 0,
            k053260_dual: false,
            c352_clock: 0,
            c352_divider: 0,
            c352_mute_rear: false,
            ay8910_clock: 0,
            ay8910_type: 0,
            ay8910_flags: 0,
            ay8910_dual: false,
            pokey_clock: 0,
            pokey_clock_2: 0,
            pokey_dual: false,
            k051649_clock: 0,
            k051649_plus: false,
            k051649_dual: false,
            k007232_dual: false,
            k005289_clock: 0,
            multipcm_clock: 0,
            multipcm_clock_2: 0,
            multipcm_dual: false,
            upd7759_clock: 0,
            upd7759_clock_2: 0,
            upd7759_dual: false,
            upd7759_slave: false,
            pwm_clock: 0,
            pwm_clock_2: 0,
            pwm_dual: false,
            pwm_bank: Vec::new(),
            pwm_blocks: Vec::new(),
            mikey_clock: 0,
            mikey_dual: false,
            x1_010_clock: 0,
            x1_010_dual: false,
            qsound_clock: 0,
            wonderswan_clock: 0,
            wonderswan_dual: false,
            ga20_clock: 0,
            ga20_dual: false,
            msm5205_clock: 0,
            msm5205_flags: 0,
            msm5205_dual: false,
            msm5232_clock: 0,
            msm5232_dual: false,
            es5503_clock: 0,
            es5503_channels: 0,
            extra_clocks: IndexMap::new(),
            extra_volumes: IndexMap::new(),
            stream_setups: 0,
            adpcm_blocks: Vec::new(),
            msm6258_plays: Vec::new(),
        }
    }
}

impl VgmFile {
    /// Relative output volume of a chip from the extra header, or None.
    pub fn chip_volume(&self, chip: u8) -> Option<f64> {
        let entry = self.extra_volumes.get(&chip)?;
        if entry.0 != 0 {
            return None;
        }
        Some(entry.1 as f64 / 256.0)
    }

    pub fn name(&self) -> String {
        match self.gd3.get("track") {
            Some(track) if !track.is_empty() => track.clone(),
            _ => self
                .path
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default(),
        }
    }
}

/// True when off lies in the header, before the command stream.
pub fn _field_in_header(buf: &[u8], off: usize, n: usize) -> bool {
    let Some(end) = off.checked_add(n) else {
        return false;
    };
    if end > buf.len() {
        return false;
    }
    if buf.len() < 0x40 || buf.len() < 4 || &buf[..4] != b"Vgm " {
        return true;
    }
    end <= data_offset(buf).unwrap_or(0)
}

pub fn _header_raw(buf: &[u8], off: usize) -> u32 {
    if !_field_in_header(buf, off, 4) {
        return 0;
    }
    le_u32_at(buf, off)
}

pub fn _header_clock(buf: &[u8], off: usize) -> u32 {
    _header_raw(buf, off) & 0x3FFFFFFF
}

/// Keep only values that can be a chip clock.
pub fn _sane_clock(raw: u32) -> u32 {
    let raw = raw & 0x3FFFFFFF;
    if (CLOCK_MIN..=CLOCK_MAX).contains(&raw) {
        raw
    } else {
        0
    }
}

pub fn _clock_or_zero(buf: &[u8], off: usize) -> u32 {
    _sane_clock(_header_clock(buf, off))
}

/// Resolve a C140 header clock the way the players do.
pub fn _c140_effective_clock(raw: u32) -> u32 {
    let mut raw = raw & 0x3FFFFFFF;
    if raw == 21390 {
        return C140_DEFAULT_CLOCK;
    }
    if (CLOCK_MIN..1_000_000).contains(&raw) {
        raw = (raw as u64 * 576) as u32;
    }
    if (CLOCK_MIN..=CLOCK_MAX).contains(&raw) {
        return raw;
    }
    C140_DEFAULT_CLOCK
}

/// Resolve an ASIC219 clock, whose range is wider than the C140's.
pub fn _c219_effective_clock(raw: u32) -> u32 {
    let mut raw = raw & 0x3FFFFFFF;
    if (CLOCK_MIN..1_000_000).contains(&raw) {
        raw = (raw as u64 * 576) as u32;
    }
    if (CLOCK_MIN..=C219_CLOCK_MAX).contains(&raw) {
        return raw;
    }
    C219_DEFAULT_CLOCK
}

/// Resolve a K054539 clock, including libvgm's legacy rate form.
pub fn _k054539_effective_clock(raw: u32) -> u32 {
    let mut raw = raw & 0x3FFFFFFF;
    if (CLOCK_MIN..1_000_000).contains(&raw) {
        raw = (raw as u64 * 384) as u32;
    }
    if (CLOCK_MIN..=CLOCK_MAX).contains(&raw) {
        return raw;
    }
    K054539_DEFAULT_CLOCK
}

/// Crystal a /288 C352 core should run at.
pub fn c352_effective_clock(raw: u32, divider: u32) -> u32 {
    let raw = raw & 0x3FFFFFFF;
    if raw == 0 {
        return 0;
    }
    let scaled: u64 = if divider != 0 {
        raw as u64 * 72 / divider as u64
    } else if raw >= 1_000_000 {
        raw as u64
    } else {
        raw as u64 * 72
    };
    if scaled >= CLOCK_MIN as u64 && scaled <= CLOCK_MAX as u64 {
        scaled as u32
    } else {
        0
    }
}

/// Clock list: count byte, then (u8 chip, u32 clock) entries.
pub fn _parse_clock_list(buf: &[u8], pos: usize) -> IndexMap<u8, u32> {
    let mut out: IndexMap<u8, u32> = IndexMap::new();
    if pos >= buf.len() {
        return out;
    }
    let count = buf[pos];
    if !(0 < count && count <= 64) {
        return out;
    }
    let mut pos = pos + 1;
    for _ in 0..count {
        if pos + 5 > buf.len() {
            return IndexMap::new();
        }
        let chip = buf[pos];
        let clock = le_u32_at(buf, pos + 1) & 0x3FFFFFFF;
        if chip > 0x7F || !(1000..=80_000_000).contains(&clock) {
            return IndexMap::new();
        }
        out.insert(chip, clock);
        pos += 5;
    }
    out
}

/// Volume list: count byte, then (u8 chip, u8 flags, u16 value) entries.
pub fn _parse_volume_list(buf: &[u8], pos: usize) -> IndexMap<u8, (u8, u16)> {
    let mut out: IndexMap<u8, (u8, u16)> = IndexMap::new();
    if pos >= buf.len() {
        return out;
    }
    let count = buf[pos];
    if !(0 < count && count <= 64) {
        return out;
    }
    let mut pos = pos + 1;
    for _ in 0..count {
        if pos + 4 > buf.len() {
            return IndexMap::new();
        }
        let chip = buf[pos];
        let flags = buf[pos + 1];
        let value = le_u16_at(buf, pos + 2);
        out.insert(chip, (flags, value));
        pos += 4;
    }
    out
}

/// VGM extra header (1.70+): the clock and volume lists.
pub fn _extra_chip_lists(buf: &[u8]) -> (IndexMap<u8, u32>, IndexMap<u8, (u8, u16)>) {
    let mut clocks: IndexMap<u8, u32> = IndexMap::new();
    let mut volumes: IndexMap<u8, (u8, u16)> = IndexMap::new();
    if buf.len() < 0xC0 {
        return (clocks, volumes);
    }
    let rel = le_u32_at(buf, 0xBC);
    if rel == 0 {
        return (clocks, volumes);
    }
    let extra = 0xBC + rel as usize;
    if extra + 8 > buf.len() {
        return (clocks, volumes);
    }
    let size = le_u32_at(buf, extra);
    if size < 8 {
        return (clocks, volumes);
    }
    let off1 = le_u32_at(buf, extra + 4);
    if off1 != 0 {
        clocks = _parse_clock_list(buf, extra + 4 + off1 as usize);
    }
    if size >= 12 && extra + 12 <= buf.len() {
        let off2 = le_u32_at(buf, extra + 8);
        if off2 != 0 {
            volumes = _parse_volume_list(buf, extra + 8 + off2 as usize);
        }
    }
    (clocks, volumes)
}

/// The first `want` bytes of a VGM/VGZ. .vgz is inflated as far as needed.
pub fn head_bytes(path: &Path, want: usize) -> Result<Vec<u8>> {
    let raw = std::fs::read(path).map_err(|e| Error::io(path, e))?;
    if raw.len() >= 2 && raw[0] == 0x1f && raw[1] == 0x8b {
        let cap = want.saturating_mul(4).min(raw.len());
        let mut out = vec![0u8; want];
        let mut filled = 0usize;
        let mut decoder = MultiGzDecoder::new(&raw[..cap]);
        while filled < want {
            match decoder.read(&mut out[filled..]) {
                Ok(0) => break,
                Ok(k) => filled += k,
                // A stream cut by `raw[..want * 4]` ends here; Python's
                // decompressobj returns the bytes decoded so far. A corrupt
                // stream makes Python raise, and the caller then returns b"".
                Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
                Err(_) => return Ok(Vec::new()),
            }
        }
        out.truncate(filled);
        return Ok(out);
    }
    Ok(raw[..want.min(raw.len())].to_vec())
}

/// Chip kinds visible in a VGM header, no command stream read.
pub fn chips_in_head(buf: &[u8]) -> HashSet<&'static str> {
    let mut kinds: HashSet<&'static str> = HashSet::new();
    if buf.len() < 4 || &buf[..4] != b"Vgm " {
        return kinds;
    }
    let version = _header_raw(buf, 8);
    let ym2151_head = _header_raw(buf, 0x30);
    if ym2151_head & 0x3FFFFFFF != 0 {
        kinds.insert("ym2151");
        if ym2151_head & 0x40000000 != 0 {
            kinds.insert("ym2151_dual");
        }
    }
    if _field_in_header(buf, YM2612_CLOCK_OFF, 4) {
        let ym2612_raw = _header_raw(buf, YM2612_CLOCK_OFF);
        if (CLOCK_MIN..=CLOCK_MAX).contains(&(ym2612_raw & 0x3FFFFFFF)) {
            kinds.insert("ym2612");
            if ym2612_raw & YM2612_DUAL_BIT != 0 {
                kinds.insert("ym2612_dual");
            }
        }
    }
    let sn_head = _header_raw(buf, SN76489_CLOCK_OFF);
    if (100_000..=40_000_000).contains(&(sn_head & 0x3FFFFFFF)) {
        if (sn_head & SN76489_DUAL_BIT) != 0 && (sn_head & SN76489_T6W28_BIT) != 0 {
            kinds.insert("t6w28");
        } else {
            kinds.insert("sn76489");
            if sn_head & SN76489_DUAL_BIT != 0 {
                kinds.insert("sn76489_dual");
            }
        }
    }
    if version >= 0x131 && _field_in_header(buf, YM2203_CLOCK_OFF, 4) {
        let ym2203_raw = _header_raw(buf, YM2203_CLOCK_OFF);
        if (100_000..=40_000_000).contains(&(ym2203_raw & 0x3FFFFFFF)) {
            kinds.insert("ym2203");
            if ym2203_raw & YM2203_DUAL_BIT != 0 {
                kinds.insert("ym2203_dual");
            }
        }
    }
    if version >= 0x151 && _field_in_header(buf, YM2610_CLOCK_OFF, 4) {
        let ym2610_raw = _header_raw(buf, YM2610_CLOCK_OFF);
        if (100_000..=40_000_000).contains(&(ym2610_raw & 0x3FFFFFFF)) {
            kinds.insert("ym2610");
            if ym2610_raw & YM2610_B_BIT != 0 {
                kinds.insert("ym2610_b");
            }
            if ym2610_raw & YM2610_DUAL_BIT != 0 {
                kinds.insert("ym2610_dual");
            }
        }
    }
    // YM2608 (header 0x48, VGM 1.51+). Detected apart from its SSG part; the
    // AY of an OPNA is intrinsic, not a second entry in the chip set.
    if version >= 0x151 && _field_in_header(buf, YM2608_CLOCK_OFF, 4) {
        let ym2608_raw = _header_raw(buf, YM2608_CLOCK_OFF);
        if (100_000..=40_000_000).contains(&(ym2608_raw & 0x3FFFFFFF)) {
            kinds.insert("ym2608");
            if ym2608_raw & YM2608_DUAL_BIT != 0 {
                kinds.insert("ym2608_dual");
            }
        }
    }
    // YM2413 (header 0x10). Under VGM 1.50 this dword meant YM2612; only
    // report it as OPLL when a 0x51/$A1 stream actually exists, which
    // chips_in_head callers cross-check against the parsed writes.
    if version >= 0x150 && _field_in_header(buf, YM2413_CLOCK_OFF, 4) {
        if (100_000..=40_000_000).contains(&_header_clock(buf, YM2413_CLOCK_OFF)) {
            kinds.insert(if _header_raw(buf, YM2413_CLOCK_OFF) & YM2413_VRC7_BIT != 0 {
                "vrc7"
            } else {
                "opll"
            });
        }
    }
    // Game Boy DMG (header 0x80, 1.61+).
    if version >= 0x161 && _field_in_header(buf, GB_DMG_CLOCK_OFF, 4) {
        if (100_000..=40_000_000).contains(&_header_clock(buf, GB_DMG_CLOCK_OFF)) {
            kinds.insert("gb");
        }
    }
    // NES APU (header 0x84, 1.51+).
    if version >= 0x151 && _field_in_header(buf, NES_CLOCK_OFF, 4) {
        let nes_raw = _header_raw(buf, NES_CLOCK_OFF);
        if (100_000..=40_000_000).contains(&(nes_raw & 0x3FFFFFFF)) {
            kinds.insert("nes");
            if nes_raw & NES_FDS_BIT != 0 {
                kinds.insert("fds");
            }
        }
    }
    if _field_in_header(buf, QSOUND_CLOCK_OFF, 4) {
        if (100_000..=100_000_000).contains(&_header_clock(buf, QSOUND_CLOCK_OFF)) {
            kinds.insert("qsound");
        }
    }
    if version >= 0x171 && _field_in_header(buf, WSWAN_CLOCK_OFF, 4) {
        let ws_raw = _header_raw(buf, WSWAN_CLOCK_OFF);
        if (100_000..=40_000_000).contains(&(ws_raw & 0x3FFFFFFF)) {
            kinds.insert("wonderswan");
            if ws_raw & WSWAN_DUAL_BIT != 0 {
                kinds.insert("wonderswan_dual");
            }
        }
    }
    if version >= 0x171 && _field_in_header(buf, GA20_CLOCK_OFF, 4) {
        let raw = _header_raw(buf, GA20_CLOCK_OFF);
        if (100_000..=40_000_000).contains(&(raw & 0x3FFFFFFF)) {
            kinds.insert("ga20");
            if raw & GA20_DUAL_BIT != 0 {
                kinds.insert("ga20_dual");
            }
        }
    }
    if version >= 0x171 && _field_in_header(buf, ES5503_CLOCK_OFF, 4) {
        let raw = _header_raw(buf, ES5503_CLOCK_OFF);
        if (100_000..=40_000_000).contains(&(raw & 0x3FFFFFFF)) {
            kinds.insert("es5503");
        }
    }
    if version >= 0x172 && _field_in_header(buf, MSM5205_CLOCK_OFF, 4) {
        let raw = _header_raw(buf, MSM5205_CLOCK_OFF);
        if (100_000..=40_000_000).contains(&(raw & 0x3FFFFFFF)) {
            kinds.insert("msm5205");
            if raw & MSM5205_DUAL_BIT != 0 {
                kinds.insert("msm5205_dual");
            }
        }
    }
    if version >= 0x172 && _field_in_header(buf, MSM5232_CLOCK_OFF, 4) {
        let raw = _header_raw(buf, MSM5232_CLOCK_OFF);
        if (100_000..=100_000_000).contains(&(raw & 0x3FFFFFFF)) {
            kinds.insert("msm5232");
            if raw & MSM5232_DUAL_BIT != 0 {
                kinds.insert("msm5232_dual");
            }
        }
    }
    if version >= 0x151 && _field_in_header(buf, SEGAPCM_CLOCK_OFF, 4) {
        let raw = _header_raw(buf, SEGAPCM_CLOCK_OFF);
        if (100_000..=40_000_000).contains(&(raw & 0x3FFFFFFF)) {
            kinds.insert("segapcm");
            if raw & 0x40000000 != 0 {
                kinds.insert("segapcm_dual");
            }
        }
    }
    if version >= 0x151 && _field_in_header(buf, RF5C68_CLOCK_OFF, 4) {
        let raw = _header_raw(buf, RF5C68_CLOCK_OFF);
        if (100_000..=40_000_000).contains(&(raw & 0x3FFFFFFF)) {
            kinds.insert("rf5c68");
            if raw & 0x40000000 != 0 {
                kinds.insert("rf5c68_dual");
            }
        }
    }
    if version >= 0x151 && _field_in_header(buf, RF5C164_CLOCK_OFF, 4) {
        let raw = _header_raw(buf, RF5C164_CLOCK_OFF);
        if (100_000..=40_000_000).contains(&(raw & 0x3FFFFFFF)) {
            kinds.insert("rf5c164");
            if raw & 0x40000000 != 0 {
                kinds.insert("rf5c164_dual");
            }
        }
    }
    if version >= 0x151 && (100_000..=40_000_000).contains(&_header_clock(buf, Y8950_CLOCK_OFF)) {
        kinds.insert("y8950");
    }
    if version >= 0x151 && (100_000..=40_000_000).contains(&_header_clock(buf, YMF278B_CLOCK_OFF)) {
        kinds.insert("ymf278b");
    }
    if version >= 0x151 && _field_in_header(buf, YMZ280B_CLOCK_OFF, 4) {
        let ymz_raw = _header_raw(buf, YMZ280B_CLOCK_OFF);
        if (100_000..=40_000_000).contains(&(ymz_raw & 0x3FFFFFFF)) {
            kinds.insert("ymz280b");
            if ymz_raw & YMZ280B_DUAL_BIT != 0 {
                kinds.insert("ymz280b_dual");
            }
        }
    }
    if version >= 0x151 && _field_in_header(buf, PWM_CLOCK_OFF, 4) {
        let raw = _header_raw(buf, PWM_CLOCK_OFF);
        if (100_000..=40_000_000).contains(&(raw & 0x3FFFFFFF)) {
            kinds.insert("pwm");
            if raw & PWM_DUAL_BIT != 0 {
                kinds.insert("pwm_dual");
            }
        }
    }
    if version >= 0x161 && _field_in_header(buf, MULTIPCM_CLOCK_OFF, 4) {
        let raw = _header_raw(buf, MULTIPCM_CLOCK_OFF);
        if (100_000..=40_000_000).contains(&(raw & 0x3FFFFFFF)) {
            kinds.insert("multipcm");
            if raw & MULTIPCM_DUAL_BIT != 0 {
                kinds.insert("multipcm_dual");
            }
        }
    }
    if version >= 0x161 && _field_in_header(buf, UPD7759_CLOCK_OFF, 4) {
        let raw = _header_raw(buf, UPD7759_CLOCK_OFF);
        if (100_000..=40_000_000).contains(&(raw & 0x3FFFFFFF)) {
            kinds.insert("upd7759");
            if raw & UPD7759_DUAL_BIT != 0 {
                kinds.insert("upd7759_dual");
            }
            if raw & UPD7759_SLAVE_BIT != 0 {
                kinds.insert("upd7759_slave");
            }
        }
    }
    if version >= 0x171 && _field_in_header(buf, VSU_CLOCK_OFF, 4) {
        let vsu_raw = _header_raw(buf, VSU_CLOCK_OFF);
        if (100_000..=40_000_000).contains(&(vsu_raw & 0x3FFFFFFF)) {
            kinds.insert("vsu");
            if vsu_raw & VSU_HEADER_DUAL_BIT != 0 {
                kinds.insert("vsu_dual");
            }
        }
    }
    if version >= 0x171 && _field_in_header(buf, SAA1099_CLOCK_OFF, 4) {
        let saa_raw = _header_raw(buf, SAA1099_CLOCK_OFF);
        if (100_000..=40_000_000).contains(&(saa_raw & 0x3FFFFFFF)) {
            kinds.insert("saa1099");
            if saa_raw & SAA1099_DUAL_BIT != 0 {
                kinds.insert("saa1099_dual");
            }
        }
    }
    if version >= 0x151 && _field_in_header(buf, AY8910_CLOCK_OFF, 4) {
        let raw = _header_raw(buf, AY8910_CLOCK_OFF);
        if (100_000..=40_000_000).contains(&(raw & 0x3FFFFFFF)) {
            kinds.insert("ay8910");
            if raw & 0x40000000 != 0 {
                kinds.insert("ay8910_dual");
            }
        }
    }
    if version >= 0x150 && _field_in_header(buf, POKEY_CLOCK_OFF, 4) {
        let pokey_raw = _header_raw(buf, POKEY_CLOCK_OFF);
        if (100_000..=40_000_000).contains(&(pokey_raw & 0x3FFFFFFF)) {
            kinds.insert("pokey");
            if pokey_raw & POKEY_DUAL_BIT != 0 {
                kinds.insert("pokey_dual");
            }
        }
    }
    // The OPL family joined the header in 1.51.
    if version >= 0x151 && _field_in_header(buf, OPL2_CLOCK_OFF, 4) {
        let opl2_raw = _header_raw(buf, OPL2_CLOCK_OFF);
        if (100_000..=40_000_000).contains(&(opl2_raw & 0x3FFFFFFF)) {
            kinds.insert("opl2");
            if opl2_raw & OPL_DUAL_BIT != 0 {
                kinds.insert("opl2_dual");
            }
        }
    }
    if version >= 0x151 && _field_in_header(buf, OPL_CLOCK_OFF, 4) {
        let raw = _header_raw(buf, OPL_CLOCK_OFF);
        if (100_000..=40_000_000).contains(&(raw & 0x3FFFFFFF)) {
            kinds.insert("opl");
            if raw & OPL_DUAL_BIT != 0 {
                kinds.insert("opl_dual");
            }
        }
    }
    if version >= 0x151 && _field_in_header(buf, OPL3_CLOCK_OFF, 4) {
        let raw = _header_raw(buf, OPL3_CLOCK_OFF);
        if (100_000..=40_000_000).contains(&(raw & 0x3FFFFFFF)) {
            kinds.insert("opl3");
            if raw & OPL_DUAL_BIT != 0 {
                kinds.insert("opl3_dual");
            }
        }
    }
    if version >= 0x151 && _field_in_header(buf, Y8950_CLOCK_OFF, 4) {
        let raw = _header_raw(buf, Y8950_CLOCK_OFF);
        if (100_000..=40_000_000).contains(&(raw & 0x3FFFFFFF)) {
            kinds.insert("y8950");
            if raw & 0x40000000 != 0 {
                kinds.insert("y8950_dual");
            }
        }
    }
    if version >= 0x151 && _field_in_header(buf, YMF278B_CLOCK_OFF, 4) {
        let raw = _header_raw(buf, YMF278B_CLOCK_OFF);
        if (100_000..=40_000_000).contains(&(raw & 0x3FFFFFFF)) {
            kinds.insert("ymf278b");
            if raw & 0x40000000 != 0 {
                kinds.insert("ymf278b_dual");
            }
        }
    }
    let k051 = _header_raw(buf, K051649_CLOCK_OFF);
    if version >= 0x161 && _field_in_header(buf, K051649_CLOCK_OFF, 4) {
        if (100_000..=40_000_000).contains(&(k051 & 0x3FFFFFFF)) {
            kinds.insert("k051649");
            if k051 & 0x80000000 != 0 {
                kinds.insert("scc_plus");
            }
            if k051 & 0x40000000 != 0 {
                kinds.insert("k051649_dual");
            }
        }
    }
    let ctype = if version >= 0x161 && buf.len() > C140_TYPE_OFF {
        buf[C140_TYPE_OFF]
    } else {
        0
    };
    let c140_raw = _header_raw(buf, C140_CLOCK_OFF);
    let c140 = c140_raw & 0x3FFFFFFF;
    if c140 == 21390 || (100_000..=40_000_000).contains(&c140) {
        kinds.insert("c140");
        if c140_raw & 0x40000000 != 0 {
            kinds.insert("c140_dual");
        }
    }
    if ctype == 2 {
        kinds.remove("c140");
        kinds.insert("c219");
    }
    if version >= 0x151 && _field_in_header(buf, K054539_CLOCK_OFF, 4) {
        let raw = _header_raw(buf, K054539_CLOCK_OFF);
        if (100_000..=40_000_000).contains(&(raw & 0x3FFFFFFF)) {
            kinds.insert("k054539");
            if raw & K054539_DUAL_BIT != 0 {
                kinds.insert("k054539_dual");
            }
        }
    }
    if version >= 0x161 && _field_in_header(buf, K053260_CLOCK_OFF, 4) {
        let raw = _header_raw(buf, K053260_CLOCK_OFF);
        if (100_000..=40_000_000).contains(&(raw & 0x3FFFFFFF)) {
            kinds.insert("k053260");
            if raw & 0x40000000 != 0 {
                kinds.insert("k053260_dual");
            }
        }
    }
    if version >= 0x161 && _field_in_header(buf, MSM6295_CLOCK_OFF, 4) {
        let raw = _header_raw(buf, MSM6295_CLOCK_OFF);
        if (100_000..=40_000_000).contains(&(raw & 0x3FFFFFFF)) {
            kinds.insert("msm6295");
            if raw & 0x40000000 != 0 {
                kinds.insert("msm6295_dual");
            }
        }
    }
    if version >= 0x161 && _field_in_header(buf, MSM6258_CLOCK_OFF, 4) {
        let raw = _header_raw(buf, MSM6258_CLOCK_OFF);
        if (100_000..=40_000_000).contains(&(raw & 0x3FFFFFFF)) {
            kinds.insert("msm6258");
            if raw & 0x40000000 != 0 {
                kinds.insert("msm6258_dual");
            }
        }
    }
    if version >= 0x171 && _field_in_header(buf, K007232_EXTRA_CLOCK_OFF, 4) {
        let raw = _header_raw(buf, K007232_EXTRA_CLOCK_OFF);
        if (100_000..=40_000_000).contains(&(raw & 0x3FFFFFFF)) {
            kinds.insert("k007232");
            if raw & 0x40000000 != 0 {
                kinds.insert("k007232_dual");
            }
        }
    }
    if version >= 0x171 && _field_in_header(buf, K005289_CLOCK_OFF, 4) {
        if (100_000..=40_000_000).contains(&_header_clock(buf, K005289_CLOCK_OFF)) {
            kinds.insert("k005289");
        }
    }
    if buf.len() >= 0xC0 {
        let (clocks, _volumes) = _extra_chip_lists(buf);
        let has = |id: u8| clocks.get(&id).copied().unwrap_or(0) != 0;
        if has(EXTRA_YM2612) {
            kinds.insert("ym2612");
        }
        if has(EXTRA_YM2203) {
            kinds.insert("ym2203");
        }
        if has(EXTRA_YM2610) {
            kinds.insert("ym2610");
        }
        if has(EXTRA_SEGAPCM) {
            kinds.insert("segapcm");
        }
        if has(EXTRA_AY8910) {
            kinds.insert("ay8910");
        }
        if has(EXTRA_K051649) {
            kinds.insert("k051649");
        }
        if has(EXTRA_C140) {
            kinds.insert(if ctype == 2 { "c219" } else { "c140" });
        }
        if has(EXTRA_K054539) {
            kinds.insert("k054539");
        }
        if has(EXTRA_K053260) {
            kinds.insert("k053260");
        }
        if has(EXTRA_MSM6295) {
            kinds.insert("msm6295");
        }
        if has(EXTRA_MSM6258) {
            kinds.insert("msm6258");
        }
        if has(EXTRA_K007232) {
            kinds.insert("k007232");
        }
        if has(EXTRA_C352) {
            kinds.insert("c352");
        }
        if has(EXTRA_YMZ280B) {
            kinds.insert("ymz280b");
        }
        if has(EXTRA_GA20) {
            kinds.insert("ga20");
        }
        if has(EXTRA_MSM5205) {
            kinds.insert("msm5205");
        }
        if has(EXTRA_MSM5232) {
            kinds.insert("msm5232");
        }
    }
    if version >= 0x171 && _field_in_header(buf, C352_CLOCK_OFF, 4) {
        let raw = _header_raw(buf, C352_CLOCK_OFF) & 0x3FFFFFFF;
        if (1000..=50_000_000).contains(&raw) {
            kinds.insert("c352");
        }
    }
    kinds
}

/// Primary K007232 clock: header 0xE8 (VGM 1.71+), never extra-header volume.
pub fn _k007232_clock(buf: &[u8], extra: &IndexMap<u8, u32>, ym_clock: u32, version: u32) -> u32 {
    let clk = if version >= 0x171 {
        _header_clock(buf, K007232_EXTRA_CLOCK_OFF)
    } else {
        0
    };
    if clk >= 100_000 {
        return clk;
    }
    let extra_clk = extra.get(&EXTRA_K007232).copied().unwrap_or(0) & 0x3FFFFFFF;
    if extra_clk >= 100_000 {
        return extra_clk;
    }
    if ym_clock != 0 {
        ym_clock
    } else {
        3_579_545
    }
}

pub type CompressionTable = (u8, u8, u8, u8, Vec<i64>);

fn read_bits(src: &[u8], bitpos: &mut usize, bc: u32, total_bits: usize) -> Option<u32> {
    if *bitpos + bc as usize > total_bits {
        return None;
    }
    let mut raw = 0u32;
    for _ in 0..bc {
        let byte = src[*bitpos >> 3];
        let bit = 7 - (*bitpos & 7);
        raw = (raw << 1) | ((byte >> bit) & 1) as u32;
        *bitpos += 1;
    }
    if bc <= 8 {
        return Some(raw);
    }
    let rem = bc - 8;
    let chunk0 = raw >> rem;
    let chunk1 = raw & ((1 << rem) - 1);
    Some(chunk0 | (chunk1 << 8))
}

/// Decode a compressed VGM data block (types 0x40-0x7E).
pub fn _decompress_data_block(
    typ: u8,
    payload: &[u8],
    table: Option<&CompressionTable>,
) -> Option<Vec<u8>> {
    if !(0x40..=0x7E).contains(&typ) || payload.len() < 10 {
        return None;
    }
    let comp = payload[0];
    let size = le_u32_at(payload, 1);
    let bd = payload[5] as u32;
    let bc = payload[6] as u32;
    if bd == 0 || bc == 0 || bd > 16 || bc > 16 {
        return None;
    }
    let val_size = ((bd + 7) / 8) as usize;
    let count = (size as usize) / val_size;
    let src = &payload[10..];
    let mut out = vec![0u8; count * val_size];
    let mut bitpos = 0usize;
    let total_bits = src.len() * 8;
    let mask = (1u32 << bc) - 1;
    let out_mask = (1u32 << bd) - 1;

    let tbl_values: Vec<i64> = match table {
        Some((_tcomp, _tsub, tbd, tbc, entries)) if *tbd == bd as u8 && *tbc == bc as u8 => {
            entries.clone()
        }
        _ => Vec::new(),
    };

    if comp == 0x00 {
        let sub = payload[7];
        let add = le_u16_at(payload, 8) as u32;
        if sub == 2 && tbl_values.is_empty() {
            return None;
        }
        for i in 0..count {
            let value = match read_bits(src, &mut bitpos, bc, total_bits) {
                Some(v) => v,
                None => break,
            };
            let value = match sub {
                0 => value & mask,
                1 => {
                    if bd < bc {
                        return None;
                    }
                    (value & mask) << (bd - bc)
                }
                2 => {
                    if value as usize >= tbl_values.len() {
                        return None;
                    }
                    tbl_values[value as usize] as u32
                }
                _ => return None,
            };
            let value = (value + add) & out_mask;
            for k in 0..val_size {
                out[i * val_size + k] = ((value >> (8 * k)) & 0xFF) as u8;
            }
        }
        return Some(out);
    }

    if comp == 0x01 {
        if tbl_values.is_empty() {
            return None;
        }
        let mut value = le_u16_at(payload, 8) as u32 & out_mask;
        for i in 0..count {
            let index = match read_bits(src, &mut bitpos, bc, total_bits) {
                Some(v) => v,
                None => break,
            };
            if index as usize >= tbl_values.len() {
                return None;
            }
            value = ((value as i64 + tbl_values[index as usize]) & out_mask as i64) as u32;
            for k in 0..val_size {
                out[i * val_size + k] = ((value >> (8 * k)) & 0xFF) as u8;
            }
        }
        return Some(out);
    }

    None
}

/// Decode a VGM type-0x7F compression table block.
pub fn _parse_compression_table(payload: &[u8]) -> Option<CompressionTable> {
    if payload.len() < 6 {
        return None;
    }
    let comp = payload[0];
    let sub = payload[1];
    let bd = payload[2];
    let bc = payload[3];
    let count = le_u16_at(payload, 4) as usize;
    let val_size = ((bd as usize) + 7) / 8;
    if bd == 0 || bc == 0 || bd > 16 || bc > 16 || val_size > 2 {
        return None;
    }
    if payload.len() < 6 + count * val_size {
        return None;
    }
    let entries: Vec<i64> = (0..count)
        .map(|i| {
            let off = 6 + i * val_size;
            let mut value = 0i64;
            for k in 0..val_size {
                value |= (payload[off + k] as i64) << (8 * k);
            }
            value
        })
        .collect();
    Some((comp, sub, bd, bc, entries))
}

#[derive(Debug, Clone, Copy, Default)]
struct StreamSetup {
    type_: u8,
    chip_id: u8,
    port: u8,
    reg: u8,
    bank: u8,
    step: u32,
    base: u32,
    pos: u32,
    rate: u32,
}

fn close_adpcm(
    active: &mut HashMap<u8, usize>,
    plays: &mut [Msm6258Play],
    sid: u8,
    end_t: i64,
) {
    if let Some(index) = active.remove(&sid) {
        let play = &mut plays[index];
        if play.end == 0 && end_t > play.sample {
            play.end = end_t;
        }
    }
}

fn stop_active_stream(
    active: &mut HashMap<u8, usize>,
    plays: &mut [DacStreamPlay],
    sid: u8,
    stop: i64,
) {
    if let Some(index) = active.remove(&sid) {
        plays[index].stop_sample = stop;
    }
}

fn has_chip(writes: &[ChipWrite], name: &str) -> bool {
    writes.iter().any(|w| w.chip == name)
}

fn has_chip_any(writes: &[ChipWrite], names: &[&str]) -> bool {
    writes.iter().any(|w| names.contains(&w.chip))
}

fn unconverted_name(cmd: u8) -> Option<&'static str> {
    UNCONVERTED_SAMPLE_CMDS
        .iter()
        .find(|(c, _)| *c == cmd)
        .map(|(_, name)| *name)
}

pub fn load_vgm(path: &Path) -> Result<VgmFile> {
    let buf = read_vgm_bytes(path)?;
    if buf.len() < 4 || &buf[..4] != b"Vgm " {
        return Err(Error::msg(format!("{}: not a VGM", path.display())));
    }
    let ver = le_u32_at(&buf, 8);
    let total = le_u32_at(&buf, 0x18) as i64;
    let loop_rel = le_u32_at(&buf, 0x1C);
    let loop_samples = le_u32_at(&buf, 0x20) as i64;
    let loop_pos = if loop_rel != 0 {
        Some(0x1C + loop_rel as usize)
    } else {
        None
    };
    let (clocks, volumes) = _extra_chip_lists(&buf);
    let ym2151_raw = _header_raw(&buf, 0x30);
    let ym2151_dual = ym2151_raw & 0x40000000 != 0;
    let ym_clock = if ym2151_raw & 0x3FFFFFFF != 0 {
        ym2151_raw & 0x3FFFFFFF
    } else {
        3_579_545
    };
    // YM2612: the clock field also carries the dual-instance flag in bit 30.
    let ym2612_raw = _header_raw(&buf, YM2612_CLOCK_OFF);
    let ym2612_dual = ym2612_raw & YM2612_DUAL_BIT != 0;
    let mut ym2612_clock = _sane_clock(ym2612_raw);
    if ym2612_clock == 0 {
        ym2612_clock = _sane_clock(clocks.get(&EXTRA_YM2612).copied().unwrap_or(0));
    }
    // YM2203: the clock field also carries the dual-instance flag in bit 30.
    let ym2203_raw = _header_raw(&buf, YM2203_CLOCK_OFF);
    let ym2203_dual = ym2203_raw & YM2203_DUAL_BIT != 0;
    let mut ym2203_clock = _sane_clock(ym2203_raw);
    if ym2203_clock == 0 {
        ym2203_clock = _sane_clock(clocks.get(&EXTRA_YM2203).copied().unwrap_or(0));
    }
    // YM2610: bit 31 selects the B variant, bit 30 the second instance.
    let ym2610_raw = _header_raw(&buf, YM2610_CLOCK_OFF);
    let ym2610_b = ym2610_raw & YM2610_B_BIT != 0;
    let ym2610_dual = ym2610_raw & YM2610_DUAL_BIT != 0;
    let mut ym2610_clock = _sane_clock(ym2610_raw);
    if ym2610_clock == 0 {
        ym2610_clock = _sane_clock(clocks.get(&EXTRA_YM2610).copied().unwrap_or(0));
    }
    // YM2608: bit 30 of clock 0x48 marks the second chip ($A6/$A7).
    let ym2608_raw = if ver >= 0x151 {
        _header_raw(&buf, YM2608_CLOCK_OFF)
    } else {
        0
    };
    let ym2608_dual = ym2608_raw & YM2608_DUAL_BIT != 0;
    let ym2608_clock = _sane_clock(ym2608_raw);
    // YM2413: the header's first chip clock, 0x10.
    let ym2413_raw = if ver >= 0x150 {
        _header_raw(&buf, YM2413_CLOCK_OFF)
    } else {
        0
    };
    let ym2413_dual = ym2413_raw & YM2608_DUAL_BIT != 0;
    let ym2413_vrc7 = ym2413_raw & YM2413_VRC7_BIT != 0;
    let ym2413_clock = _sane_clock(ym2413_raw);
    // Game Boy DMG clock 0x80 (1.61+), falling back to extra-header id 0x13.
    let gb_raw = if ver >= 0x161 {
        _header_raw(&buf, GB_DMG_CLOCK_OFF)
    } else {
        0
    };
    let mut gb_dmg_clock = _sane_clock(gb_raw);
    if gb_dmg_clock == 0 {
        gb_dmg_clock = _sane_clock(clocks.get(&EXTRA_GB).copied().unwrap_or(0));
    }
    let gb_dmg_dual = gb_raw & 0x40000000 != 0;
    // NES APU clock 0x84 (1.51+), falling back to extra-header id 0x14.
    let nes_raw = if ver >= 0x151 {
        _header_raw(&buf, NES_CLOCK_OFF)
    } else {
        0
    };
    let mut nes_clock = _sane_clock(nes_raw);
    if nes_clock == 0 {
        nes_clock = _sane_clock(clocks.get(&EXTRA_NES).copied().unwrap_or(0));
    }
    let nes_dual = nes_raw & NES_DUAL_BIT != 0;
    let nes_fds = nes_raw & NES_FDS_BIT != 0;
    // HuC6280 (PC Engine) clock 0xA4 (1.61+).
    let huc_raw = if ver >= 0x161 {
        _header_raw(&buf, HUC6280_CLOCK_OFF)
    } else {
        0
    };
    let mut huc6280_clock = _sane_clock(huc_raw);
    if huc6280_clock == 0 {
        huc6280_clock = _sane_clock(clocks.get(&EXTRA_HUC6280).copied().unwrap_or(0));
    }
    let huc6280_dual = huc_raw & 0x40000000 != 0;
    // OPL family.
    let opl2_raw = if ver >= 0x151 {
        _header_raw(&buf, OPL2_CLOCK_OFF)
    } else {
        0
    };
    let opl2_clock = _sane_clock(opl2_raw);
    let opl2_dual = opl2_raw & OPL_DUAL_BIT != 0;
    let opl2_pan = opl2_raw & 0x80000000 != 0;
    let opl_raw = if ver >= 0x151 {
        _header_raw(&buf, OPL_CLOCK_OFF)
    } else {
        0
    };
    let opl_clock = _sane_clock(opl_raw);
    let opl_dual = opl_raw & OPL_DUAL_BIT != 0;
    let opl3_raw = if ver >= 0x151 {
        _header_raw(&buf, OPL3_CLOCK_OFF)
    } else {
        0
    };
    let opl3_clock = _sane_clock(opl3_raw);
    let opl3_dual = opl3_raw & OPL_DUAL_BIT != 0;
    // SN76489/T6W28: bit 30 is the second instance, bit 31 T6W28 mode.
    let sn76489_raw = _header_raw(&buf, SN76489_CLOCK_OFF);
    let mut sn76489_clock = _sane_clock(sn76489_raw);
    let sn76489_dual = sn76489_raw & SN76489_DUAL_BIT != 0;
    let sn76489_t6w28 =
        sn76489_raw & SN76489_T6W28_BIT != 0 && sn76489_raw & SN76489_DUAL_BIT != 0;
    let mut sn76489_clock_2 = 0;
    if sn76489_dual {
        sn76489_clock_2 = _sane_clock(clocks.get(&EXTRA_SN76489).copied().unwrap_or(0));
    }
    if sn76489_clock == 0 {
        sn76489_clock = _sane_clock(clocks.get(&EXTRA_SN76489).copied().unwrap_or(0));
    }
    let k_clock = _k007232_clock(&buf, &clocks, ym_clock, ver);
    let oki_raw = if ver >= 0x161 {
        _header_raw(&buf, 0x98)
    } else {
        0
    };
    let adpcm_raw = if ver >= 0x161 {
        _header_raw(&buf, 0x90)
    } else {
        0
    };
    let adpcm_flags = if ver >= 0x161 && _field_in_header(&buf, 0x94, 1) {
        buf[0x94]
    } else {
        0
    };
    let spcm_raw = if ver >= 0x151 {
        _header_raw(&buf, SEGAPCM_CLOCK_OFF)
    } else {
        0
    };
    let mut spcm_clock = _sane_clock(spcm_raw);
    let segapcm_dual = spcm_raw & 0x40000000 != 0;
    if spcm_clock == 0 {
        spcm_clock = _sane_clock(clocks.get(&EXTRA_SEGAPCM).copied().unwrap_or(0));
    }
    let spcm_shift = if ver >= 0x151 && _field_in_header(&buf, SEGAPCM_INTF_OFF, 1) {
        buf[SEGAPCM_INTF_OFF]
    } else {
        0
    };
    let spcm_mask = if ver >= 0x151 && _field_in_header(&buf, SEGAPCM_MASK_OFF, 1) {
        buf[SEGAPCM_MASK_OFF]
    } else {
        0
    };
    // RF5C68 and RF5C164 share the register set but ride different header
    // clocks. The extra-header list (chip id 0x05 = RF5C68) is the fallback.
    let rf5c68_raw = if ver >= 0x151 {
        _header_raw(&buf, RF5C68_CLOCK_OFF)
    } else {
        0
    };
    let mut rf5c68_clock = _sane_clock(rf5c68_raw);
    let rf5c68_dual = rf5c68_raw & 0x40000000 != 0;
    if rf5c68_clock == 0 {
        rf5c68_clock = _sane_clock(clocks.get(&0x05).copied().unwrap_or(0));
    }
    let rf5c164_raw = if ver >= 0x151 {
        _header_raw(&buf, RF5C164_CLOCK_OFF)
    } else {
        0
    };
    let rf5c164_clock = _sane_clock(rf5c164_raw);
    let rf5c164_dual = rf5c164_raw & 0x40000000 != 0;
    let y8950_raw = if ver >= 0x151 {
        _header_raw(&buf, Y8950_CLOCK_OFF)
    } else {
        0
    };
    let y8950_clock = _sane_clock(y8950_raw);
    let y8950_dual = y8950_raw & 0x40000000 != 0;
    let ymf278b_raw = if ver >= 0x151 {
        _header_raw(&buf, YMF278B_CLOCK_OFF)
    } else {
        0
    };
    let ymf278b_clock = _sane_clock(ymf278b_raw);
    let ymf278b_dual = ymf278b_raw & 0x40000000 != 0;
    // YMZ280B clock 0x68 (1.51+), bit 30 selecting a second chip.
    let ymz_raw = if ver >= 0x151 {
        _header_raw(&buf, YMZ280B_CLOCK_OFF)
    } else {
        0
    };
    let ymz280b_dual = ymz_raw & YMZ280B_DUAL_BIT != 0;
    let mut ymz280b_clock = _sane_clock(ymz_raw);
    let mut ymz280b_clock_2 = 0;
    if ymz280b_dual {
        ymz280b_clock_2 = _sane_clock(clocks.get(&EXTRA_YMZ280B).copied().unwrap_or(0));
    }
    if ymz280b_clock == 0 {
        ymz280b_clock = _sane_clock(clocks.get(&EXTRA_YMZ280B).copied().unwrap_or(0));
    }
    // Virtual Boy VSU clock 0xC4 (VGM 1.71+).
    let vsu_raw = if ver >= 0x171 {
        _header_raw(&buf, VSU_CLOCK_OFF)
    } else {
        0
    };
    let vsu_dual = vsu_raw & VSU_HEADER_DUAL_BIT != 0;
    let mut vsu_clock = _sane_clock(vsu_raw);
    let mut vsu_clock_2 = 0;
    if vsu_dual {
        vsu_clock_2 = _sane_clock(clocks.get(&EXTRA_VSU).copied().unwrap_or(0));
    }
    if vsu_clock == 0 {
        vsu_clock = _sane_clock(clocks.get(&EXTRA_VSU).copied().unwrap_or(0));
    }
    // Philips SAA1099 clock 0xC8 (VGM 1.71+).
    let saa_raw = if ver >= 0x171 {
        _header_raw(&buf, SAA1099_CLOCK_OFF)
    } else {
        0
    };
    let saa1099_dual = saa_raw & SAA1099_DUAL_BIT != 0;
    let saa1099_clock = _sane_clock(saa_raw);
    // Read the field for every version: the legacy System 21 rips carry
    // 21390 there even in 1.51 files, and players resolve it the same way.
    let c140_header = _header_raw(&buf, C140_CLOCK_OFF);
    let c140_dual = c140_header & 0x40000000 != 0;
    let mut c140_raw = c140_header & 0x3FFFFFFF;
    if c140_raw == 0 {
        c140_raw = clocks.get(&EXTRA_C140).copied().unwrap_or(0) & 0x3FFFFFFF;
    }
    let c140_type = if ver >= 0x161 && _field_in_header(&buf, C140_TYPE_OFF, 1) {
        buf[C140_TYPE_OFF] & 0x03
    } else if c140_raw == 21390 {
        // Pre-1.61 System 21 rips log the legacy 8MHz/374 rate instead of a
        // clock; their register stream still needs the System 21 mapping.
        1
    } else {
        0
    };
    let c140_clock = if c140_type == 2 {
        _c219_effective_clock(c140_raw)
    } else {
        _c140_effective_clock(c140_raw)
    };
    // K054539 clock 0xA0 and flag byte 0x95 (VGM 1.51+).
    let mut k054539_raw = if ver >= 0x151 {
        _header_raw(&buf, K054539_CLOCK_OFF)
    } else {
        0
    };
    let k054539_dual = k054539_raw & K054539_DUAL_BIT != 0;
    if k054539_raw & 0x3FFFFFFF == 0 {
        k054539_raw = clocks.get(&EXTRA_K054539).copied().unwrap_or(0);
    }
    let k054539_clock = _k054539_effective_clock(k054539_raw);
    let mut k054539_clock_2 = 0;
    if k054539_dual {
        let extra_clock = clocks.get(&EXTRA_K054539).copied().unwrap_or(0);
        k054539_clock_2 = if extra_clock != 0 {
            _k054539_effective_clock(extra_clock)
        } else {
            k054539_clock
        };
    }
    let k054539_flags = if ver >= 0x151 && _field_in_header(&buf, K054539_FLAGS_OFF, 1) {
        buf[K054539_FLAGS_OFF]
    } else {
        0
    };
    let k053260_raw = if ver >= 0x161 {
        _header_raw(&buf, K053260_CLOCK_OFF)
    } else {
        0
    };
    let mut k053260_clock = _sane_clock(k053260_raw);
    let k053260_dual = k053260_raw & 0x40000000 != 0;
    if k053260_clock == 0 {
        k053260_clock = _sane_clock(clocks.get(&EXTRA_K053260).copied().unwrap_or(0));
    }
    let mut c352_clock = 0;
    let mut c352_divider = 0;
    let mut c352_mute_rear = false;
    if ver >= 0x171 && _field_in_header(&buf, C352_CLOCK_OFF, 4) {
        let c352_raw = _header_raw(&buf, C352_CLOCK_OFF);
        c352_mute_rear = c352_raw & 0x80000000 != 0;
        if _field_in_header(&buf, C352_DIV_OFF, 1) {
            c352_divider = buf[C352_DIV_OFF];
        }
        c352_clock = c352_effective_clock(c352_raw, c352_divider as u32);
    }
    if c352_clock == 0 {
        c352_clock = _sane_clock(clocks.get(&EXTRA_C352).copied().unwrap_or(0));
    }
    let ay_raw = if ver >= 0x151 {
        _header_raw(&buf, AY8910_CLOCK_OFF)
    } else {
        0
    };
    let mut ay_clock = _sane_clock(ay_raw);
    let ay8910_dual = ay_raw & 0x40000000 != 0;
    if ay_clock == 0 {
        ay_clock = _sane_clock(clocks.get(&EXTRA_AY8910).copied().unwrap_or(0));
    }
    let ay_type = if ver >= 0x151 && _field_in_header(&buf, AY8910_TYPE_OFF, 1) {
        buf[AY8910_TYPE_OFF]
    } else {
        0
    };
    let ay_flags = if ver >= 0x151 && _field_in_header(&buf, AY8910_FLAGS_OFF, 1) {
        buf[AY8910_FLAGS_OFF]
    } else {
        0
    };
    let pokey_raw = if ver >= 0x150 {
        _header_raw(&buf, POKEY_CLOCK_OFF)
    } else {
        0
    };
    let mut pokey_clock = _sane_clock(pokey_raw);
    let pokey_dual = pokey_raw & POKEY_DUAL_BIT != 0;
    if pokey_clock == 0 {
        pokey_clock = _sane_clock(clocks.get(&EXTRA_POKEY).copied().unwrap_or(0));
    }
    let pokey_clock_2 = if pokey_dual {
        _sane_clock(clocks.get(&EXTRA_POKEY).copied().unwrap_or(0))
    } else {
        0
    };
    let mut k051_clock = 0;
    let mut k051_plus = false;
    let mut k051649_dual = false;
    if ver >= 0x161 && _field_in_header(&buf, K051649_CLOCK_OFF, 4) {
        let raw = _header_raw(&buf, K051649_CLOCK_OFF);
        k051_plus = raw & 0x80000000 != 0;
        k051649_dual = raw & 0x40000000 != 0;
        k051_clock = _sane_clock(raw & 0x3FFFFFFF);
    }
    if k051_clock == 0 {
        k051_clock = _sane_clock(clocks.get(&EXTRA_K051649).copied().unwrap_or(0));
    }
    let k007232_header = if ver >= 0x171 {
        _header_raw(&buf, K007232_EXTRA_CLOCK_OFF)
    } else {
        0
    };
    let k007232_dual = k007232_header & 0x40000000 != 0;
    // K005289 clock 0xEC (VGM 1.71+).  The field has no flag bits.
    let mut k005289_clock = 0;
    if ver >= 0x171 && _field_in_header(&buf, K005289_CLOCK_OFF, 4) {
        k005289_clock = _sane_clock(_header_raw(&buf, K005289_CLOCK_OFF));
    }
    // MultiPCM clock 0x88 (VGM 1.61+).
    let multipcm_raw = if ver >= 0x161 {
        _header_raw(&buf, MULTIPCM_CLOCK_OFF)
    } else {
        0
    };
    let multipcm_dual = multipcm_raw & MULTIPCM_DUAL_BIT != 0;
    let mut multipcm_clock = _sane_clock(multipcm_raw);
    if multipcm_clock == 0 {
        multipcm_clock = _sane_clock(clocks.get(&EXTRA_MULTIPCM).copied().unwrap_or(0));
    }
    let mut multipcm_clock_2 = 0;
    if multipcm_dual {
        multipcm_clock_2 = _sane_clock(clocks.get(&EXTRA_MULTIPCM).copied().unwrap_or(0));
    }
    // uPD7759 clock 0x8C (VGM 1.61+).
    let upd_raw = if ver >= 0x161 {
        _header_raw(&buf, UPD7759_CLOCK_OFF)
    } else {
        0
    };
    let upd7759_dual = upd_raw & UPD7759_DUAL_BIT != 0;
    let mut upd7759_slave = upd_raw & UPD7759_SLAVE_BIT != 0;
    let mut upd7759_clock = _sane_clock(upd_raw);
    if upd7759_clock == 0 {
        upd7759_clock = _sane_clock(clocks.get(&EXTRA_UPD7759).copied().unwrap_or(0));
    }
    let mut upd7759_clock_2 = 0;
    if upd7759_dual {
        upd7759_clock_2 = _sane_clock(clocks.get(&EXTRA_UPD7759).copied().unwrap_or(0));
        if clocks.get(&EXTRA_UPD7759).copied().unwrap_or(0) & UPD7759_SLAVE_BIT != 0 {
            upd7759_slave = true;
        }
    }
    // Sega 32X PWM clock 0x70 (VGM 1.51+).
    let pwm_raw = if ver >= 0x151 {
        _header_raw(&buf, PWM_CLOCK_OFF)
    } else {
        0
    };
    let pwm_dual = pwm_raw & PWM_DUAL_BIT != 0;
    let mut pwm_clock = _sane_clock(pwm_raw);
    if pwm_clock == 0 {
        pwm_clock = _sane_clock(clocks.get(&STREAM_PWM).copied().unwrap_or(0));
    }
    let pwm_clock_2 = if pwm_dual {
        _sane_clock(clocks.get(&STREAM_PWM).copied().unwrap_or(0))
    } else {
        0
    };
    // Atari Lynx Mikey clock 0xE4 (VGM 1.72+).
    let mikey_raw = if ver >= 0x172 {
        _header_raw(&buf, MIKEY_CLOCK_OFF)
    } else {
        0
    };
    let mikey_dual = mikey_raw & MIKEY_DUAL_BIT != 0;
    let mut mikey_clock = _sane_clock(mikey_raw);
    if mikey_clock == 0 {
        mikey_clock = _sane_clock(clocks.get(&EXTRA_MIKEY).copied().unwrap_or(0));
    }
    // Seta X1-010 clock 0xD8 (VGM 1.71+).
    let x1_raw = if ver >= 0x171 {
        _header_raw(&buf, X1_010_CLOCK_OFF)
    } else {
        0
    };
    let x1_010_dual = x1_raw & X1_010_DUAL_BIT != 0;
    let mut x1_010_clock = _sane_clock(x1_raw);
    if x1_010_clock == 0 {
        x1_010_clock = _sane_clock(clocks.get(&EXTRA_X1_010).copied().unwrap_or(0));
    }
    // Capcom QSound clock 0xB4.
    let mut qsound_raw = _header_raw(&buf, QSOUND_CLOCK_OFF) & 0x3FFFFFFF;
    let mut qsound_clock = if (CLOCK_MIN..=100_000_000).contains(&qsound_raw) {
        qsound_raw
    } else {
        0
    };
    if qsound_clock == 0 {
        qsound_raw = clocks.get(&EXTRA_QSOUND).copied().unwrap_or(0) & 0x3FFFFFFF;
        if (CLOCK_MIN..=100_000_000).contains(&qsound_raw) {
            qsound_clock = qsound_raw;
        }
    }
    // Bandai WonderSwan clock 0xC0 (VGM 1.71+).
    let ws_raw = if ver >= 0x171 {
        _header_raw(&buf, WSWAN_CLOCK_OFF)
    } else {
        0
    };
    let wonderswan_dual = ws_raw & WSWAN_DUAL_BIT != 0;
    let mut wonderswan_clock = _sane_clock(ws_raw);
    if wonderswan_clock == 0 {
        wonderswan_clock = _sane_clock(clocks.get(&EXTRA_WSWAN).copied().unwrap_or(0));
    }
    // Irem GA20 clock 0xE0 (VGM 1.71+).
    let ga20_raw = if ver >= 0x171 {
        _header_raw(&buf, GA20_CLOCK_OFF)
    } else {
        0
    };
    let ga20_dual = ga20_raw & GA20_DUAL_BIT != 0;
    let mut ga20_clock = _sane_clock(ga20_raw);
    if ga20_clock == 0 {
        ga20_clock = _sane_clock(clocks.get(&EXTRA_GA20).copied().unwrap_or(0));
    }
    // OKI MSM5205 clock 0xF0 and OKI MSM5232 clock 0xF4 (VGM 1.72+).
    let msm5205_raw = if ver >= 0x172 {
        _header_raw(&buf, MSM5205_CLOCK_OFF)
    } else {
        0
    };
    let msm5205_dual = msm5205_raw & MSM5205_DUAL_BIT != 0;
    let mut msm5205_clock = _sane_clock(msm5205_raw);
    if msm5205_clock == 0 {
        msm5205_clock = _sane_clock(clocks.get(&EXTRA_MSM5205).copied().unwrap_or(0));
    }
    let msm5205_flags = if buf.len() > MSM5205_FLAGS_OFF {
        buf[MSM5205_FLAGS_OFF]
    } else {
        0
    };
    let msm5232_raw = if ver >= 0x172 {
        _header_raw(&buf, MSM5232_CLOCK_OFF)
    } else {
        0
    };
    let msm5232_dual = msm5232_raw & MSM5232_DUAL_BIT != 0;
    let mut msm5232_clock = _sane_clock(msm5232_raw);
    if msm5232_clock == 0 {
        msm5232_clock = _sane_clock(clocks.get(&EXTRA_MSM5232).copied().unwrap_or(0));
    }
    // Ensoniq ES5503 clock 0xCC and output-channel count 0xD4 (VGM 1.71+).
    let es5503_raw = if ver >= 0x171 {
        _header_raw(&buf, ES5503_CLOCK_OFF)
    } else {
        0
    };
    let es5503_clock = _sane_clock(es5503_raw);
    let es5503_channels = if ver >= 0x171 && _field_in_header(&buf, ES5503_CHANNELS_OFF, 1) {
        buf[ES5503_CHANNELS_OFF] & 0xFF
    } else {
        0
    };

    let mut main_volume = 0.0;
    // Bytes past the declared command offset are not header fields. Synthetic
    // test logs and old 0x40-byte headers leave this area as commands.
    if ver >= 0x150 && data_offset(&buf)? >= 0x7E && buf.len() >= 0x7E {
        let raw_volume = le_u16_at(&buf, 0x7C) as u32;
        if 0 < raw_volume && raw_volume <= 0x800 {
            main_volume = raw_volume as f64 / 0x100 as f64;
        }
    }

    let mut vgm = VgmFile {
        path: path.to_path_buf(),
        version: ver,
        total_samples: total,
        loop_samples,
        loop_file_pos: loop_pos,
        ym2151_clock: ym_clock,
        k007232_clock: k_clock,
        gd3: parse_gd3(&buf),
        main_volume,
        ym2151_dual,
        ym2612_clock,
        ym2612_dual,
        sn76489_clock,
        sn76489_clock_2,
        sn76489_dual,
        sn76489_t6w28,
        ym2203_clock,
        ym2203_dual,
        ym2610_clock,
        ym2610_dual,
        ym2610_b,
        ym2608_clock,
        ym2608_dual,
        ym2413_clock,
        ym2413_dual,
        ym2413_vrc7,
        nes_clock,
        nes_dual,
        nes_fds,
        qsound_clock,
        wonderswan_clock,
        wonderswan_dual,
        ga20_clock,
        ga20_dual,
        msm5205_clock,
        msm5205_flags,
        msm5205_dual,
        msm5232_clock,
        msm5232_dual,
        es5503_clock,
        es5503_channels,
        gb_dmg_clock,
        gb_dmg_dual,
        huc6280_clock,
        huc6280_dual,
        opl_clock,
        opl_dual,
        opl2_clock,
        opl2_dual,
        opl2_pan,
        opl3_clock,
        opl3_dual,
        msm6295_clock: oki_raw & 0x3FFFFFFF,
        msm6295_pin7: oki_raw & 0x80000000 != 0,
        msm6295_dual: oki_raw & 0x40000000 != 0,
        msm6258_clock: adpcm_raw & 0x3FFFFFFF,
        msm6258_flags: adpcm_flags,
        msm6258_dual: adpcm_raw & 0x40000000 != 0,
        segapcm_clock: spcm_clock,
        segapcm_bankshift: spcm_shift,
        segapcm_bankmask: spcm_mask,
        segapcm_dual,
        rf5c68_clock,
        rf5c164_clock,
        rf5c68_dual,
        rf5c164_dual,
        y8950_clock,
        y8950_dual,
        ymf278b_clock,
        ymf278b_dual,
        ymz280b_clock,
        ymz280b_clock_2,
        ymz280b_dual,
        vsu_clock,
        vsu_clock_2,
        vsu_dual,
        saa1099_clock,
        saa1099_dual,
        c140_clock,
        c140_type,
        c140_dual,
        k054539_clock,
        k054539_clock_2,
        k054539_dual,
        k054539_flags,
        k053260_clock,
        k053260_dual,
        c352_clock,
        c352_divider,
        c352_mute_rear,
        ay8910_clock: ay_clock,
        ay8910_type: ay_type,
        ay8910_flags: ay_flags,
        ay8910_dual,
        pokey_clock,
        pokey_clock_2,
        pokey_dual,
        k051649_clock: k051_clock,
        k051649_plus: k051_plus,
        k051649_dual,
        k007232_dual,
        k005289_clock,
        multipcm_clock,
        multipcm_clock_2,
        multipcm_dual,
        upd7759_clock,
        upd7759_clock_2,
        upd7759_dual,
        upd7759_slave,
        pwm_clock,
        pwm_clock_2,
        pwm_dual,
        mikey_clock,
        mikey_dual,
        x1_010_clock,
        x1_010_dual,
        ..Default::default()
    };
    let n = buf.len();
    let mut pos = data_offset(&buf)?;
    let mut sample: i64 = 0;
    let mut loop_sample: Option<i64> = None;
    let mut skipped_sample_cmds: HashSet<u8> = HashSet::new();
    let mut stray_ym2203_second = false;
    let mut stray_ym2612_second = false;
    let mut stray_ym2610_second = false;
    let mut stray_ym2608_second = false;
    let mut stray_opll_second = false;
    let mut stray_ym2151_second = false;
    let mut stray_opl_second = false;
    let mut stray_opl2_second = false;
    let mut stray_opl3_second = false;
    let mut stray_y8950_second = false;
    let mut stray_opl4_second = false;
    let mut stray_sn_second = false;
    let mut stray_rf5c_second = false;
    let mut stray_segapcm_second = false;
    let mut stray_k007232_second = false;
    let mut stray_c140_second = false;
    let mut stray_k054539_second = false;
    let mut stray_k053260_second = false;
    let mut stray_ay_second = false;
    let mut stray_pokey_second = false;
    let mut stray_scc_second = false;
    let mut stray_saa_second = false;
    let mut stray_ymz_second = false;
    let mut stray_vsu_second = false;
    let mut stray_msm6258_second = false;
    let mut stray_msm6295_second = false;
    let mut stray_ga20_second = false;
    let mut stray_msm5205_second = false;
    let mut stray_msm5232_second = false;
    let mut saw_end = false;
    let mut saw_adpcm_compressed = false;
    // stream id -> setup recorded by 0x90/0x91/0x92
    let mut streams: HashMap<u8, StreamSetup> = HashMap::new();
    // Most recent type-0x7F compression table, shared by later blocks.
    let mut compression_table: Option<CompressionTable> = None;
    let mut active_adpcm: HashMap<u8, usize> = HashMap::new();
    // Generic 0x93 plays that a 0x94 (or the next 0x93) ends.
    let mut active_stream: HashMap<u8, usize> = HashMap::new();
    // The RF5C pair bank their sample RAM.
    let mut rf5c_wbank: HashMap<(&'static str, u8), u32> = HashMap::new();

    while pos < n {
        if loop_pos.is_some() && loop_sample.is_none() && pos >= loop_pos.unwrap() {
            loop_sample = Some(sample);
        }
        let cmd = buf[pos];
        if cmd == CMD_END {
            saw_end = true;
            break;
        }
        if cmd == CMD_DATA_BLOCK {
            if pos + 7 > n {
                break;
            }
            let typ = buf[pos + 2];
            let mut size = le_u32_at(&buf, pos + 3);
            let chip_id = ((size >> 31) & 1) as u8;
            size &= 0x7FFFFFFF;
            let payload_end = (pos + 7 + size as usize).min(n);
            let payload = &buf[pos + 7..payload_end];
            if typ == 0x7F {
                compression_table = _parse_compression_table(payload);
            } else if (0x40..=0x7E).contains(&typ) && (typ & 0x3F) == 0x00 {
                // Compressed YM2612 PCM bank.
                let data = _decompress_data_block(typ, payload, compression_table.as_ref());
                match data {
                    None => vgm.warnings.push(format!(
                        "YM2612 PCM data block type {typ:#04x}: unsupported compression; DAC stream will be silent"
                    )),
                    Some(data) => {
                        vgm.dac_blocks.push(data.clone());
                        vgm.dac_data.extend_from_slice(&data);
                    }
                }
            } else if typ == 0x00 {
                // YM2612 PCM sample bank.
                vgm.dac_blocks.push(payload.to_vec());
                vgm.dac_data.extend_from_slice(payload);
            } else if typ == NES_DPCM_BANK_TYPE {
                // NES DPCM database.
                let bank_len = vgm.nes_dpcm_bank.len();
                vgm.nes_dpcm_bank_base.entry(chip_id).or_insert(bank_len);
                vgm.nes_dpcm_bank.extend_from_slice(payload);
            } else if typ == 0x04 {
                // OKIM6258 ADPCM bank.
                vgm.adpcm_blocks.push(payload.to_vec());
            } else if typ == HUC6280_PCM_TYPE {
                // HuC6280 PCM database.
                vgm.huc6280_pcm.extend_from_slice(payload);
                vgm.huc6280_pcm_blocks.push(payload.to_vec());
            } else if typ == 0x44 {
                saw_adpcm_compressed = true;
            } else if typ == PWM_DATA_TYPE
                || ((0x40..=0x7E).contains(&typ) && (typ & 0x3F) == PWM_DATA_TYPE)
            {
                // PWM PCM data bank.
                let data = if typ < 0x40 {
                    Some(payload.to_vec())
                } else {
                    _decompress_data_block(typ, payload, None)
                };
                match data {
                    None => vgm.warnings.push(format!(
                        "PWM data block type {typ:#04x}: unsupported compression; stream will be silent"
                    )),
                    Some(data) => {
                        vgm.pwm_bank.extend_from_slice(&data);
                        vgm.pwm_blocks.push(data);
                    }
                }
            } else if typ == RF5C68_RAM_TYPE
                || typ == RF5C164_RAM_TYPE
                || typ == NES_DPCM_RAM_TYPE
            {
                // C0-DF RAM writes: a 16-bit start offset then the bytes.
                if payload.len() >= 2 {
                    let start = le_u16_at(payload, 0) as i64;
                    let kind = if typ == RF5C68_RAM_TYPE {
                        "rf5c68"
                    } else if typ == RF5C164_RAM_TYPE {
                        "rf5c164"
                    } else {
                        "nes_dpcm"
                    };
                    vgm.roms.push(RomSlice {
                        chip: chip_id,
                        rom_size: 0,
                        start,
                        data: payload[2..].to_vec(),
                        kind,
                        time: sample,
                    });
                }
            } else if typ == ES5503_RAM_TYPE {
                // E0-FF RAM writes carry a 32-bit start address.
                if payload.len() >= 4 {
                    let start = le_u32_at(payload, 0) as i64;
                    vgm.roms.push(RomSlice {
                        chip: chip_id,
                        rom_size: 0x20000,
                        start,
                        data: payload[4..].to_vec(),
                        kind: "es5503",
                        time: sample,
                    });
                }
            } else if typ == K005289_RAM_TYPE {
                // C3: the K005289's 512-byte 4-bit waveform PROM.
                if payload.len() >= 2 {
                    let start = le_u16_at(payload, 0) as i64;
                    vgm.roms.push(RomSlice {
                        chip: 0,
                        rom_size: 0x200,
                        start,
                        data: payload[2..].to_vec(),
                        kind: "k005289",
                        time: sample,
                    });
                }
            } else if payload.len() >= 8 {
                let kind: Option<&'static str> = match typ {
                    K007232_ROM_TYPE => Some("k007232"),
                    OKI_ROM_TYPE => Some("msm6295"),
                    MULTIPCM_ROM_TYPE => Some("multipcm"),
                    UPD7759_ROM_TYPE => Some("upd7759"),
                    SEGAPCM_ROM_TYPE => Some("segapcm"),
                    K054539_ROM_TYPE => Some("k054539"),
                    C140_ROM_TYPE => Some(if c140_type == 2 { "c219" } else { "c140" }),
                    C352_ROM_TYPE => Some("c352"),
                    K053260_ROM_TYPE => Some("k053260"),
                    YM2610_ADPCM_ROM_TYPE => Some("ym2610_adpcm"),
                    YM2610_DELTAT_ROM_TYPE => Some("ym2610_deltat"),
                    YM2608_DELTAT_ROM_TYPE => Some("ym2608_deltat"),
                    Y8950_DELTAT_ROM_TYPE => Some("y8950_deltat"),
                    YMF278B_ROM_TYPE => Some("ymf278b_rom"),
                    YMF278B_RAM_TYPE => Some("ymf278b_ram"),
                    YMZ280B_ROM_TYPE => Some("ymz280b"),
                    X1_010_ROM_TYPE => Some("x1_010"),
                    QSOUND_ROM_TYPE => Some("qsound"),
                    GA20_ROM_TYPE => Some("ga20"),
                    _ => None,
                };
                if let Some(kind) = kind {
                    let rom_size = le_u32_at(payload, 0) as i64;
                    let start = le_u32_at(payload, 4) as i64;
                    vgm.roms.push(RomSlice {
                        chip: chip_id,
                        rom_size,
                        start,
                        data: payload[8..].to_vec(),
                        kind,
                        time: 0,
                    });
                }
            }
            pos += 7 + size as usize;
            continue;
        }
        if cmd == 0x61 && pos + 3 <= n {
            sample += le_u16_at(&buf, pos + 1) as i64;
            pos += 3;
            continue;
        }
        if cmd == 0x62 {
            sample += 735;
            pos += 1;
            continue;
        }
        if cmd == 0x63 {
            sample += 882;
            pos += 1;
            continue;
        }
        if (0x70..=0x7F).contains(&cmd) {
            sample += (cmd & 0x0F) as i64 + 1;
            pos += 1;
            continue;
        }
        if (0x80..=0x8F).contains(&cmd) {
            // VGM 1.50 YM2612 PCM stream: play one byte, then wait 0-15 samples.
            let wait = cmd & 0x0F;
            vgm.dac_events.push((sample, 1, wait as i64));
            sample += wait as i64;
            pos += 1;
            continue;
        }
        if cmd == 0xE0 && pos + 5 <= n {
            let ptr = le_u32_at(&buf, pos + 1) as i64;
            vgm.dac_events.push((sample, 0, ptr));
            pos += 5;
            continue;
        }
        if cmd == 0x92 && pos + 6 <= n {
            let stream = buf[pos + 1];
            let rate = le_u32_at(&buf, pos + 2);
            vgm.dac_stream_rates.insert(stream, rate);
            if let Some(st) = streams.get_mut(&stream) {
                st.rate = rate;
            }
            pos += 6;
            continue;
        }
        if cmd == 0x95 && pos + 5 <= n {
            let stream = buf[pos + 1];
            let block = le_u16_at(&buf, pos + 2);
            let flags = buf[pos + 4];
            let st = streams.get(&stream).copied();
            if let Some(st) = st {
                if st.type_ == STREAM_OKIM6258 {
                    close_adpcm(&mut active_adpcm, &mut vgm.msm6258_plays, stream, sample);
                    let rate = if st.rate != 0 {
                        st.rate
                    } else {
                        vgm.dac_stream_rates.get(&stream).copied().unwrap_or(0)
                    };
                    vgm.msm6258_plays.push(Msm6258Play {
                        sample,
                        chip_id: st.chip_id,
                        block: block as i32,
                        rate,
                        loop_: flags & 0x01 != 0,
                        reverse: flags & 0x10 != 0,
                        offset: 0,
                        nbytes: 0,
                        end: 0,
                        step: st.step,
                        step_base: st.base,
                        length_mode: 3,
                    });
                    let index = vgm.msm6258_plays.len() - 1;
                    active_adpcm.insert(stream, index);
                } else if st.type_ == STREAM_C6280 || st.type_ == STREAM_PWM {
                    // 0x95 plays one whole data block for the chip's data bank.
                    stop_active_stream(
                        &mut active_stream,
                        &mut vgm.dac_stream_plays,
                        stream,
                        sample,
                    );
                    let rate = if st.rate != 0 {
                        st.rate
                    } else {
                        vgm.dac_stream_rates.get(&stream).copied().unwrap_or(0)
                    };
                    vgm.dac_stream_plays.push(DacStreamPlay {
                        sample,
                        chip_type: st.type_,
                        chip_id: st.chip_id,
                        stream,
                        port: st.port,
                        reg: st.reg,
                        offset: 0,
                        length: 0,
                        mode: 3,
                        rate,
                        loop_: flags & 0x80 != 0,
                        reverse: flags & 0x10 != 0,
                        step: st.step,
                        step_base: st.base,
                        bank: st.bank,
                        block: block as i32,
                        stop_sample: 0,
                    });
                    let index = vgm.dac_stream_plays.len() - 1;
                    active_stream.insert(stream, index);
                } else {
                    let rate = vgm
                        .dac_stream_rates
                        .get(&stream)
                        .copied()
                        .unwrap_or(VGM_RATE as u32);
                    vgm.dac_block_plays.push((sample, block as i64, rate as i64));
                }
            } else {
                let rate = vgm
                    .dac_stream_rates
                    .get(&stream)
                    .copied()
                    .unwrap_or(VGM_RATE as u32);
                vgm.dac_block_plays.push((sample, block as i64, rate as i64));
            }
            pos += 5;
            continue;
        }
        if cmd == 0x90 && pos + 5 <= n {
            // ss tt pp cc. Bit 7 of tt selects the second chip.
            let sid = buf[pos + 1];
            let tt = buf[pos + 2];
            streams.insert(
                sid,
                StreamSetup {
                    type_: tt & 0x7F,
                    chip_id: (tt >> 7) & 1,
                    port: buf[pos + 3],
                    reg: buf[pos + 4],
                    bank: 0,
                    step: 1,
                    base: 0,
                    pos: 0,
                    rate: vgm.dac_stream_rates.get(&sid).copied().unwrap_or(0),
                },
            );
            vgm.stream_setups += 1;
            pos += 5;
            continue;
        }
        if cmd == 0x91 && pos + 5 <= n {
            // ss dd ll bb: bank id, step size, step base.
            let sid = buf[pos + 1];
            if let Some(st) = streams.get_mut(&sid) {
                st.bank = buf[pos + 2];
                st.step = if buf[pos + 3] != 0 {
                    buf[pos + 3] as u32
                } else {
                    1
                };
                st.base = buf[pos + 4] as u32;
            }
            pos += 5;
            continue;
        }
        if cmd == 0x93 && pos + 11 <= n {
            // ss offset mode length. Mode 0 only moves the cursor.
            let sid = buf[pos + 1];
            let offset = le_u32_at(&buf, pos + 2);
            let mode = buf[pos + 6];
            let length = le_u32_at(&buf, pos + 7);
            let mut st = streams.get(&sid).copied();
            let kind = mode & 0x07;
            if let Some(current) = st.as_mut() {
                if offset != 0xFFFFFFFF {
                    current.pos = offset;
                    if let Some(map_entry) = streams.get_mut(&sid) {
                        map_entry.pos = offset;
                    }
                }
            }
            if let Some(st) = st {
                if st.type_ == STREAM_OKIM6258 && kind != 0 {
                    close_adpcm(&mut active_adpcm, &mut vgm.msm6258_plays, sid, sample);
                    let rate = if st.rate != 0 {
                        st.rate
                    } else {
                        vgm.dac_stream_rates.get(&sid).copied().unwrap_or(0)
                    };
                    vgm.msm6258_plays.push(Msm6258Play {
                        sample,
                        chip_id: st.chip_id,
                        block: -1,
                        rate,
                        loop_: mode & 0x80 != 0,
                        reverse: mode & 0x10 != 0,
                        offset: if offset == 0xFFFFFFFF { 0 } else { offset },
                        nbytes: length,
                        end: 0,
                        step: st.step,
                        step_base: st.base,
                        length_mode: kind,
                    });
                    let index = vgm.msm6258_plays.len() - 1;
                    active_adpcm.insert(sid, index);
                } else if kind != 0 {
                    // Any other chip: the stream writes its data bank to the
                    // register the 0x90 named, at the 0x92 rate.
                    stop_active_stream(
                        &mut active_stream,
                        &mut vgm.dac_stream_plays,
                        sid,
                        sample,
                    );
                    let rate = if st.rate != 0 {
                        st.rate
                    } else {
                        vgm.dac_stream_rates.get(&sid).copied().unwrap_or(0)
                    };
                    vgm.dac_stream_plays.push(DacStreamPlay {
                        sample,
                        chip_type: st.type_,
                        chip_id: st.chip_id,
                        stream: sid,
                        port: st.port,
                        reg: st.reg,
                        offset: if offset == 0xFFFFFFFF { st.pos } else { offset },
                        length,
                        mode,
                        rate,
                        loop_: mode & 0x80 != 0,
                        reverse: mode & 0x10 != 0,
                        step: st.step,
                        step_base: st.base,
                        bank: st.bank,
                        block: -1,
                        stop_sample: 0,
                    });
                    let index = vgm.dac_stream_plays.len() - 1;
                    active_stream.insert(sid, index);
                }
            }
            pos += 11;
            continue;
        }
        if cmd == 0x94 && pos + 2 <= n {
            let sid = buf[pos + 1];
            if sid == 0xFF {
                let keys: Vec<u8> = active_adpcm.keys().copied().collect();
                for key in keys {
                    close_adpcm(&mut active_adpcm, &mut vgm.msm6258_plays, key, sample);
                }
                let keys: Vec<u8> = active_stream.keys().copied().collect();
                for key in keys {
                    stop_active_stream(
                        &mut active_stream,
                        &mut vgm.dac_stream_plays,
                        key,
                        sample,
                    );
                }
            } else {
                close_adpcm(&mut active_adpcm, &mut vgm.msm6258_plays, sid, sample);
                stop_active_stream(
                    &mut active_stream,
                    &mut vgm.dac_stream_plays,
                    sid,
                    sample,
                );
            }
            pos += 2;
            continue;
        }
        if cmd == CMD_DAC_WRITE && pos + 2 <= n {
            vgm.dac_events.push((sample, 2, buf[pos + 1] as i64));
            pos += 2;
            continue;
        }
        if cmd == CMD_DAC_ENABLE && pos + 2 <= n {
            // Some logs use the standalone DAC enable command instead of
            // writing YM2612 register 0x2B through 0x52.
            vgm.writes.push(ChipWrite {
                sample,
                chip: "ym2612",
                chip_id: 0,
                reg: 0x2B,
                val: buf[pos + 1] as u32,
            });
            pos += 2;
            continue;
        }
        if (cmd == CMD_GGSTEREO || cmd == CMD_GGSTEREO_2) && pos + 2 <= n {
            // Game Gear stereo: four left and four right enable bits.
            let value = buf[pos + 1];
            let cid = if cmd == CMD_GGSTEREO { 0 } else { 1 };
            if cid != 0 && !sn76489_dual {
                stray_sn_second = true;
            }
            vgm.writes.push(ChipWrite {
                sample,
                chip: "sn76489",
                chip_id: cid,
                reg: 1,
                val: value as u32,
            });
            if value != 0xFF {
                vgm.sn76489_stereo = true;
            }
            pos += 2;
            continue;
        }
        if (cmd == CMD_SN76489 || cmd == CMD_SN76489_2) && pos + 2 <= n {
            // One SN76489 serial data byte.
            let cid = if cmd == CMD_SN76489 { 0 } else { 1 };
            if cid != 0 && !sn76489_dual {
                stray_sn_second = true;
            }
            vgm.writes.push(ChipWrite {
                sample,
                chip: "sn76489",
                chip_id: cid,
                reg: 0,
                val: buf[pos + 1] as u32,
            });
            pos += 2;
            continue;
        }
        if (cmd == CMD_YM2151 || cmd == CMD_YM2151_2) && pos + 3 <= n {
            let mut cid = 0;
            if cmd == CMD_YM2151_2 {
                if ym2151_dual {
                    cid = 1;
                } else {
                    stray_ym2151_second = true;
                }
            }
            if cmd != CMD_YM2151_2 || ym2151_dual {
                vgm.writes.push(ChipWrite {
                    sample,
                    chip: "ym2151",
                    chip_id: cid,
                    reg: buf[pos + 1] as u32,
                    val: buf[pos + 2] as u32,
                });
            }
            pos += 3;
            continue;
        }
        if (cmd == CMD_YM3526
            || cmd == CMD_YM3526_2
            || cmd == CMD_YM3812
            || cmd == CMD_YM3812_2
            || cmd == CMD_OPL3_P0
            || cmd == CMD_OPL3_P1
            || cmd == CMD_OPL3_B_P0
            || cmd == CMD_OPL3_B_P1
            || cmd == CMD_Y8950
            || cmd == CMD_Y8950_2)
            && pos + 3 <= n
        {
            // The OPL chips take (register, value) in one flat space.
            if cmd == CMD_YM3526 || cmd == CMD_YM3526_2 {
                let mut cid = 0;
                if cmd == CMD_YM3526_2 {
                    if opl_dual {
                        cid = 1;
                    } else {
                        stray_opl_second = true;
                    }
                }
                if cmd != CMD_YM3526_2 || opl_dual {
                    vgm.writes.push(ChipWrite {
                        sample,
                        chip: "opl",
                        chip_id: cid,
                        reg: buf[pos + 1] as u32,
                        val: buf[pos + 2] as u32,
                    });
                }
            } else if cmd == CMD_YM3812 || cmd == CMD_YM3812_2 {
                let mut cid = 0;
                if cmd == CMD_YM3812_2 {
                    if opl2_dual {
                        cid = 1;
                    } else {
                        stray_opl2_second = true;
                    }
                }
                if cmd != CMD_YM3812_2 || opl2_dual {
                    vgm.writes.push(ChipWrite {
                        sample,
                        chip: "opl2",
                        chip_id: cid,
                        reg: buf[pos + 1] as u32,
                        val: buf[pos + 2] as u32,
                    });
                }
            } else if cmd == CMD_Y8950 || cmd == CMD_Y8950_2 {
                let mut cid = 0;
                if cmd == CMD_Y8950_2 {
                    if y8950_dual {
                        cid = 1;
                    } else {
                        stray_y8950_second = true;
                    }
                }
                if cmd != CMD_Y8950_2 || y8950_dual {
                    vgm.writes.push(ChipWrite {
                        sample,
                        chip: "y8950",
                        chip_id: cid,
                        reg: buf[pos + 1] as u32,
                        val: buf[pos + 2] as u32,
                    });
                }
            } else {
                let mut cid = 0;
                if cmd == CMD_OPL3_B_P0 || cmd == CMD_OPL3_B_P1 {
                    if opl3_dual {
                        cid = 1;
                    } else {
                        stray_opl3_second = true;
                    }
                }
                if !(cmd == CMD_OPL3_B_P0 || cmd == CMD_OPL3_B_P1) || opl3_dual {
                    let port = if cmd == CMD_OPL3_P1 || cmd == CMD_OPL3_B_P1 {
                        1
                    } else {
                        0
                    };
                    vgm.writes.push(ChipWrite {
                        sample,
                        chip: "opl3",
                        chip_id: cid,
                        reg: (port << 8) | buf[pos + 1] as u32,
                        val: buf[pos + 2] as u32,
                    });
                }
            }
            pos += 3;
            continue;
        }
        if cmd == CMD_YMF278B && pos + 4 <= n {
            // 0xD0 pp aa dd.
            let pp = buf[pos + 1];
            let cid = (pp >> 7) & 1;
            if cid != 0 && !ymf278b_dual {
                stray_opl4_second = true;
            }
            let port = (pp & 0x7F) as u32;
            vgm.writes.push(ChipWrite {
                sample,
                chip: "ymf278b",
                chip_id: cid,
                reg: (port << 8) | buf[pos + 2] as u32,
                val: buf[pos + 3] as u32,
            });
            pos += 4;
            continue;
        }
        if cmd == CMD_X1_010 && pos + 4 <= n {
            // 0xC8: 16-bit big-endian register/memory offset, then data.
            vgm.writes.push(ChipWrite {
                sample,
                chip: "x1_010",
                chip_id: 0,
                reg: be_u16_at(&buf, pos + 1) as u32,
                val: buf[pos + 3] as u32,
            });
            pos += 4;
            continue;
        }
        if cmd == CMD_QSOUND && pos + 4 <= n {
            // 0xC4 data_hi data_lo register.
            vgm.writes.push(ChipWrite {
                sample,
                chip: "qsound",
                chip_id: 0,
                reg: buf[pos + 3] as u32,
                val: be_u16_at(&buf, pos + 1) as u32,
            });
            pos += 4;
            continue;
        }
        if cmd == CMD_WSWAN && pos + 3 <= n {
            // 0xBC 0x80+register data.
            let rr = buf[pos + 1];
            let cid = (rr >> 7) & 1;
            let reg = 0x80 + (rr & 0x7F) as u32;
            if reg > 0x94 {
                vgm.warnings.push(format!(
                    "WonderSwan register {reg:#04x} is outside the sound port map"
                ));
            }
            vgm.writes.push(ChipWrite {
                sample,
                chip: "wonderswan",
                chip_id: cid,
                reg,
                val: buf[pos + 2] as u32,
            });
            pos += 3;
            continue;
        }
        if cmd == CMD_WSWAN_MEM && pos + 4 <= n {
            // 0xC6 address16 data.
            let addr = be_u16_at(&buf, pos + 1) as u32 & 0x7FFF;
            vgm.writes.push(ChipWrite {
                sample,
                chip: "wonderswan",
                chip_id: 0,
                reg: 0x10000 + addr,
                val: buf[pos + 3] as u32,
            });
            pos += 4;
            continue;
        }
        if cmd == CMD_MIKEY && pos + 3 <= n {
            // Atari Lynx Mikey register write: 40 aa vv.
            vgm.writes.push(ChipWrite {
                sample,
                chip: "mikey",
                chip_id: 0,
                reg: buf[pos + 1] as u32,
                val: buf[pos + 2] as u32,
            });
            pos += 3;
            continue;
        }
        if cmd == 0xB2 && pos + 3 <= n {
            // PWM: ad dd, where a is the 4-bit register.
            let first = buf[pos + 1];
            let value = (((first & 0x0F) as u32) << 8) | buf[pos + 2] as u32;
            vgm.writes.push(ChipWrite {
                sample,
                chip: "pwm",
                chip_id: (first >> 7) & 1,
                reg: ((first >> 4) & 0x07) as u32,
                val: value,
            });
            pos += 3;
            continue;
        }
        if (cmd == 0xB5 || cmd == 0xB6) && pos + 3 <= n {
            // MultiPCM (0xB5) and uPD7759 (0xB6) register writes.
            let name = if cmd == 0xB5 { "multipcm" } else { "upd7759" };
            let reg = buf[pos + 1];
            let cid = (reg >> 7) & 1;
            vgm.writes.push(ChipWrite {
                sample,
                chip: name,
                chip_id: cid,
                reg: (reg & 0x7F) as u32,
                val: buf[pos + 2] as u32,
            });
            pos += 3;
            continue;
        }
        if cmd == 0xC3 && pos + 4 <= n {
            // MultiPCM ROM bank.
            let cc = buf[pos + 1];
            let cid = (cc >> 7) & 1;
            vgm.writes.push(ChipWrite {
                sample,
                chip: "multipcm_bank",
                chip_id: cid,
                reg: (cc & 0x03) as u32,
                val: buf[pos + 2] as u32,
            });
            pos += 4;
            continue;
        }
        if cmd == CMD_VSU && pos + 4 <= n {
            // 0xC7 mmll dd: 16-bit big-endian register *word* index.
            let reg = be_u16_at(&buf, pos + 1) as u32;
            let cid = if reg & VSU_DUAL_BIT != 0 { 1 } else { 0 };
            if cid != 0 && !vsu_dual {
                stray_vsu_second = true;
            }
            vgm.writes.push(ChipWrite {
                sample,
                chip: "vsu",
                chip_id: cid,
                reg: reg & 0x7FFF,
                val: buf[pos + 3] as u32,
            });
            pos += 4;
            continue;
        }
        if (cmd == CMD_YMZ280B || cmd == CMD_YMZ280B_2) && pos + 3 <= n {
            // 0x5D is the first YMZ280B, 0xAD the second.
            let cid = if cmd == CMD_YMZ280B { 0 } else { 1 };
            if cid != 0 && !ymz280b_dual {
                stray_ymz_second = true;
            }
            vgm.writes.push(ChipWrite {
                sample,
                chip: "ymz280b",
                chip_id: cid,
                reg: buf[pos + 1] as u32,
                val: buf[pos + 2] as u32,
            });
            pos += 3;
            continue;
        }
        if (cmd == CMD_YM2612_P0
            || cmd == CMD_YM2612_P1
            || cmd == CMD_YM2612_B_P0
            || cmd == CMD_YM2612_B_P1)
            && pos + 3 <= n
        {
            // 0x52/0x53 are the first YM2612's two banks, 0xA2/0xA3 the second.
            let mut cid = 0;
            if cmd == CMD_YM2612_B_P0 || cmd == CMD_YM2612_B_P1 {
                if ym2612_dual {
                    cid = 1;
                } else {
                    stray_ym2612_second = true;
                }
            }
            let bank = if cmd == CMD_YM2612_P1 || cmd == CMD_YM2612_B_P1 {
                0x100
            } else {
                0
            };
            vgm.writes.push(ChipWrite {
                sample,
                chip: "ym2612",
                chip_id: cid,
                reg: bank | buf[pos + 1] as u32,
                val: buf[pos + 2] as u32,
            });
            pos += 3;
            continue;
        }
        if (cmd == CMD_YM2203 || cmd == CMD_YM2203_2) && pos + 3 <= n {
            // 0x55 is the first instance; 0xA5 is the second.
            let mut cid = 0;
            if cmd == CMD_YM2203_2 {
                if ym2203_dual {
                    cid = 1;
                } else {
                    stray_ym2203_second = true;
                }
            }
            let reg = buf[pos + 1];
            let val = buf[pos + 2];
            let name = if reg < 0x10 { "ym2203_ssg" } else { "ym2203" };
            vgm.writes.push(ChipWrite {
                sample,
                chip: name,
                chip_id: cid,
                reg: reg as u32,
                val: val as u32,
            });
            pos += 3;
            continue;
        }
        if (cmd == CMD_YM2610_P0
            || cmd == CMD_YM2610_P1
            || cmd == CMD_YM2610_B_P0
            || cmd == CMD_YM2610_B_P1)
            && pos + 3 <= n
        {
            // 0x58/0x59 are the first chip's two ports, 0xA8/0xA9 the second.
            let mut cid = 0;
            if cmd == CMD_YM2610_B_P0 || cmd == CMD_YM2610_B_P1 {
                if ym2610_dual {
                    cid = 1;
                } else {
                    stray_ym2610_second = true;
                }
            }
            let port1 = cmd == CMD_YM2610_P1 || cmd == CMD_YM2610_B_P1;
            let reg = buf[pos + 1];
            let name = if !port1 && reg < 0x10 {
                "ym2610_ssg"
            } else {
                "ym2610"
            };
            vgm.writes.push(ChipWrite {
                sample,
                chip: name,
                chip_id: cid,
                reg: (if port1 { 0x100 } else { 0 }) | reg as u32,
                val: buf[pos + 2] as u32,
            });
            pos += 3;
            continue;
        }
        if (cmd == CMD_YM2608_P0
            || cmd == CMD_YM2608_P1
            || cmd == CMD_YM2608_B_P0
            || cmd == CMD_YM2608_B_P1)
            && pos + 3 <= n
        {
            // 0x56/0x57 are the first chip's buses A/B; 0xA6/0xA7 the second.
            let mut cid = 0;
            if cmd == CMD_YM2608_B_P0 || cmd == CMD_YM2608_B_P1 {
                if ym2608_dual {
                    cid = 1;
                } else {
                    stray_ym2608_second = true;
                }
            }
            let port1 = cmd == CMD_YM2608_P1 || cmd == CMD_YM2608_B_P1;
            let reg = buf[pos + 1];
            let name = if !port1 && reg < 0x10 {
                "ym2608_ssg"
            } else {
                "ym2608"
            };
            vgm.writes.push(ChipWrite {
                sample,
                chip: name,
                chip_id: cid,
                reg: (if port1 { 0x100 } else { 0 }) | reg as u32,
                val: buf[pos + 2] as u32,
            });
            pos += 3;
            continue;
        }
        if (cmd == CMD_YM2413 || cmd == CMD_YM2413_2) && pos + 3 <= n {
            // One flat register space; 0xA1 is the second chip's stream.
            let mut cid = 0;
            if cmd == CMD_YM2413_2 {
                if ym2413_dual {
                    cid = 1;
                } else {
                    stray_opll_second = true;
                }
            }
            vgm.writes.push(ChipWrite {
                sample,
                chip: "opll",
                chip_id: cid,
                reg: buf[pos + 1] as u32,
                val: buf[pos + 2] as u32,
            });
            pos += 3;
            continue;
        }
        if cmd == CMD_K007232 && pos + 3 <= n {
            let aa = buf[pos + 1];
            let cid = (aa >> 7) & 1;
            if cid != 0 {
                stray_k007232_second = true;
            }
            vgm.writes.push(ChipWrite {
                sample,
                chip: "k007232",
                chip_id: cid,
                reg: (aa & 0x7F) as u32,
                val: buf[pos + 2] as u32,
            });
            pos += 3;
            continue;
        }
        if cmd == CMD_K005289 && pos + 3 <= n {
            // 0x42: high nibble of the first operand is the register offset.
            let off = ((buf[pos + 1] >> 4) & 0x0F) as u32;
            let val = (((buf[pos + 1] & 0x0F) as u32) << 8) | buf[pos + 2] as u32;
            vgm.writes.push(ChipWrite {
                sample,
                chip: "k005289",
                chip_id: 0,
                reg: off,
                val,
            });
            pos += 3;
            continue;
        }
        if cmd == CMD_POKEY && pos + 3 <= n {
            // 0xBB: register, then data.
            let rr = buf[pos + 1];
            let cid = (rr >> 7) & 1;
            if cid != 0 && !pokey_dual {
                stray_pokey_second = true;
            }
            vgm.writes.push(ChipWrite {
                sample,
                chip: "pokey",
                chip_id: cid,
                reg: (rr & 0x0F) as u32,
                val: buf[pos + 2] as u32,
            });
            pos += 3;
            continue;
        }
        if cmd == CMD_AY8910 && pos + 3 <= n {
            // 0xA0: register, then data.
            let rr = buf[pos + 1];
            let cid = (rr >> 7) & 1;
            if cid != 0 && !ay8910_dual {
                stray_ay_second = true;
            }
            vgm.writes.push(ChipWrite {
                sample,
                chip: "ay8910",
                chip_id: cid,
                reg: (rr & 0x7F) as u32,
                val: buf[pos + 2] as u32,
            });
            pos += 3;
            continue;
        }
        if cmd == CMD_SAA1099 && pos + 3 <= n {
            // 0xBD: register, then data.
            let rr = buf[pos + 1];
            let cid = (rr >> 7) & 1;
            if cid != 0 && !saa1099_dual {
                stray_saa_second = true;
            }
            vgm.writes.push(ChipWrite {
                sample,
                chip: "saa1099",
                chip_id: cid,
                reg: (rr & 0x1F) as u32,
                val: buf[pos + 2] as u32,
            });
            pos += 3;
            continue;
        }
        if (cmd == CMD_MSM6295 || cmd == CMD_MSM6258) && pos + 3 <= n {
            let aa = buf[pos + 1];
            let name = if cmd == CMD_MSM6295 {
                "msm6295"
            } else {
                "msm6258"
            };
            let cid = (aa >> 7) & 1;
            let dual = if name == "msm6295" {
                vgm.msm6295_dual
            } else {
                adpcm_raw & 0x40000000 != 0
            };
            if cid != 0 && !dual {
                if name == "msm6295" {
                    stray_msm6295_second = true;
                } else {
                    stray_msm6258_second = true;
                }
            }
            vgm.writes.push(ChipWrite {
                sample,
                chip: name,
                chip_id: cid,
                reg: (aa & 0x7F) as u32,
                val: buf[pos + 2] as u32,
            });
            pos += 3;
            continue;
        }
        if cmd == CMD_SEGAPCM && pos + 4 <= n {
            // 0xC0: u16-LE address, then data.
            let addr = le_u16_at(&buf, pos + 1);
            let cid = ((addr >> 15) & 1) as u8;
            if cid != 0 {
                stray_segapcm_second = true;
            }
            vgm.writes.push(ChipWrite {
                sample,
                chip: "segapcm",
                chip_id: cid,
                reg: (addr & 0x7FF) as u32,
                val: buf[pos + 3] as u32,
            });
            pos += 4;
            continue;
        }
        if cmd == CMD_GB && pos + 3 <= n {
            // 0xB3: register, data.  Register 0x00 is FF10 (NR10).
            let rr = buf[pos + 1];
            let gb_cid = (rr >> 7) & 1;
            if gb_cid != 0 && !gb_dmg_dual {
                vgm.warnings.push(
                    "0xB3 Game Boy writes with register bit 7 set but no dual-instance header bit; they were treated as the second APU".to_string(),
                );
            }
            vgm.writes.push(ChipWrite {
                sample,
                chip: "gb",
                chip_id: gb_cid,
                reg: (rr & 0x7F) as u32,
                val: buf[pos + 2] as u32,
            });
            pos += 3;
            continue;
        }
        if cmd == CMD_NES && pos + 3 <= n {
            // 0xB4: register, data.
            let rr = buf[pos + 1];
            let nes_cid = (rr >> 7) & 1;
            let ofs = rr & 0x7F;
            if ofs == 0x3F {
                vgm.writes.push(ChipWrite {
                    sample,
                    chip: "fds",
                    chip_id: nes_cid,
                    reg: 0x23,
                    val: buf[pos + 2] as u32,
                });
            } else if (0x20..=0x3F).contains(&ofs) {
                vgm.writes.push(ChipWrite {
                    sample,
                    chip: "fds",
                    chip_id: nes_cid,
                    reg: (0x80 | (ofs & 0x1F)) as u32,
                    val: buf[pos + 2] as u32,
                });
            } else if (0x40..=0x7F).contains(&ofs) {
                vgm.writes.push(ChipWrite {
                    sample,
                    chip: "fds",
                    chip_id: nes_cid,
                    reg: ofs as u32,
                    val: buf[pos + 2] as u32,
                });
            } else {
                if nes_cid != 0 && !nes_dual {
                    vgm.warnings.push(
                        "0xB4 NES writes with register bit 7 set but no dual-instance header bit; they were treated as the second APU".to_string(),
                    );
                }
                vgm.writes.push(ChipWrite {
                    sample,
                    chip: "nes",
                    chip_id: nes_cid,
                    reg: ofs as u32,
                    val: buf[pos + 2] as u32,
                });
            }
            pos += 3;
            continue;
        }
        if cmd == CMD_HUC6280 && pos + 3 <= n {
            // 0xB9: register, data.
            let rr = buf[pos + 1];
            let huc_cid = (rr >> 7) & 1;
            if huc_cid != 0 && !huc6280_dual {
                vgm.warnings.push(
                    "0xB9 HuC6280 writes with register bit 7 set but no dual-instance header bit; they were treated as the second chip".to_string(),
                );
            }
            vgm.writes.push(ChipWrite {
                sample,
                chip: "huc6280",
                chip_id: huc_cid,
                reg: (rr & 0x7F) as u32,
                val: buf[pos + 2] as u32,
            });
            pos += 3;
            continue;
        }
        if (cmd == CMD_RF5C68 || cmd == CMD_RF5C164) && pos + 3 <= n {
            // 0xB0/0xB1: (register, data).
            let name = if cmd == CMD_RF5C68 {
                "rf5c68"
            } else {
                "rf5c164"
            };
            let rr = buf[pos + 1];
            let cid = (rr >> 7) & 1;
            let reg = rr & 0x7F;
            let val = buf[pos + 2];
            if cid != 0 {
                stray_rf5c_second = true;
            }
            if reg == 0x07 && (val & 0x40) == 0 {
                rf5c_wbank.insert((name, cid), ((val & 0x0F) as u32) << 12);
            }
            vgm.writes.push(ChipWrite {
                sample,
                chip: name,
                chip_id: cid,
                reg: reg as u32,
                val: val as u32,
            });
            pos += 3;
            continue;
        }
        if (cmd == CMD_RF5C68_MEM || cmd == CMD_RF5C164_MEM) && pos + 4 <= n {
            // 0xC1/0xC2: 16-bit LE offset, then data.
            let name = if cmd == CMD_RF5C68_MEM {
                "rf5c68"
            } else {
                "rf5c164"
            };
            let offset = le_u16_at(&buf, pos + 1) as u32;
            let addr = (rf5c_wbank.get(&(name, 0)).copied().unwrap_or(0) | offset) & 0xFFFF;
            let chip = if cmd == CMD_RF5C68_MEM {
                "rf5c68_mem"
            } else {
                "rf5c164_mem"
            };
            vgm.writes.push(ChipWrite {
                sample,
                chip,
                chip_id: 0,
                reg: addr,
                val: buf[pos + 3] as u32,
            });
            pos += 4;
            continue;
        }
        if cmd == CMD_SCC && pos + 4 <= n {
            // 0xD2 pp aa dd: SCC "port" (register bank), register, data.
            let pp = buf[pos + 1];
            let reg = (((pp & 0x7F) as u32) << 8) | buf[pos + 2] as u32;
            let cid = (pp >> 7) & 1;
            if cid != 0 && !k051649_dual {
                stray_scc_second = true;
            }
            vgm.writes.push(ChipWrite {
                sample,
                chip: "k051649",
                chip_id: cid,
                reg,
                val: buf[pos + 3] as u32,
            });
            pos += 4;
            continue;
        }
        if cmd == CMD_K054539 && pos + 4 <= n {
            // 0xD3: 16-bit big-endian offset, then data.
            let addr = be_u16_at(&buf, pos + 1);
            let cid = ((addr >> 15) & 1) as u8;
            if cid != 0 && !k054539_dual {
                stray_k054539_second = true;
            }
            vgm.writes.push(ChipWrite {
                sample,
                chip: "k054539",
                chip_id: cid,
                reg: (addr & 0x7FFF) as u32,
                val: buf[pos + 3] as u32,
            });
            pos += 4;
            continue;
        }
        if cmd == CMD_C140 && pos + 4 <= n {
            // 0xD4: 16-bit big-endian address, then data.
            let addr = be_u16_at(&buf, pos + 1);
            let cid = ((addr >> 15) & 1) as u8;
            if cid != 0 && !c140_dual {
                stray_c140_second = true;
            }
            vgm.writes.push(ChipWrite {
                sample,
                chip: if c140_type == 2 { "c219" } else { "c140" },
                chip_id: cid,
                reg: (addr & 0x1FF) as u32,
                val: buf[pos + 3] as u32,
            });
            pos += 4;
            continue;
        }
        if cmd == CMD_ES5503 && pos + 4 <= n {
            // 0xD5 pp aa dd: pp bit 7 is the chip id.
            let port = buf[pos + 1];
            vgm.writes.push(ChipWrite {
                sample,
                chip: "es5503",
                chip_id: (port >> 7) & 1,
                reg: buf[pos + 2] as u32,
                val: buf[pos + 3] as u32,
            });
            pos += 4;
            continue;
        }
        if cmd == CMD_C352 && pos + 5 <= n {
            // 0xE1: uint16 BE address, uint16 BE data.
            let addr = be_u16_at(&buf, pos + 1);
            let data = be_u16_at(&buf, pos + 3);
            vgm.writes.push(ChipWrite {
                sample,
                chip: "c352",
                chip_id: ((addr >> 15) & 1) as u8,
                reg: (addr & 0x7FFF) as u32,
                val: data as u32,
            });
            pos += 5;
            continue;
        }
        if cmd == CMD_K053260 && pos + 3 <= n {
            // 0xBA: register, data.
            let aa = buf[pos + 1];
            let cid = (aa >> 7) & 1;
            if cid != 0 && !k053260_dual {
                stray_k053260_second = true;
            }
            vgm.writes.push(ChipWrite {
                sample,
                chip: "k053260",
                chip_id: cid,
                reg: (aa & 0x3F) as u32,
                val: buf[pos + 2] as u32,
            });
            pos += 3;
            continue;
        }
        if cmd == CMD_GA20 && pos + 3 <= n {
            // 0xBF: register, data.
            let aa = buf[pos + 1];
            let cid = (aa >> 7) & 1;
            if cid != 0 && !ga20_dual {
                stray_ga20_second = true;
            }
            vgm.writes.push(ChipWrite {
                sample,
                chip: "ga20",
                chip_id: cid,
                reg: (aa & 0x7F) as u32,
                val: buf[pos + 2] as u32,
            });
            pos += 3;
            continue;
        }
        if cmd == CMD_MSM5205 && pos + 2 <= n {
            // 0x32: (register << 4) | data, bit 7 selects the second chip.
            let aa = buf[pos + 1];
            let cid = (aa >> 7) & 1;
            if cid != 0 && !msm5205_dual {
                stray_msm5205_second = true;
            }
            vgm.writes.push(ChipWrite {
                sample,
                chip: "msm5205",
                chip_id: cid,
                reg: ((aa >> 4) & 0x07) as u32,
                val: (aa & 0x0F) as u32,
            });
            pos += 2;
            continue;
        }
        if (cmd == CMD_MSM5232 || cmd == CMD_MSM5232_LEGACY) && pos + 3 <= n {
            // 0x43 (and the older libvgm 0x2D spelling): register, data.
            let aa = buf[pos + 1];
            let cid = (aa >> 7) & 1;
            if cid != 0 && !msm5232_dual {
                stray_msm5232_second = true;
            }
            vgm.writes.push(ChipWrite {
                sample,
                chip: "msm5232",
                chip_id: cid,
                reg: (aa & 0x7F) as u32,
                val: buf[pos + 2] as u32,
            });
            pos += 3;
            continue;
        }
        if cmd == CMD_STREAM_SETUP {
            vgm.stream_setups += 1;
        }
        if cmd == 0x68 && pos + 12 <= n {
            // 68 tt dd dd dd aa aa aa ll ll ll.
            let typ = buf[pos + 2];
            let src = le_u32_at(&buf, pos + 3) as i64;
            let dest = le_u32_at(&buf, pos + 6) as i64;
            let length = le_u32_at(&buf, pos + 9) as i64;
            vgm.pcm_ram_writes.push(PcmRamWrite {
                sample,
                type_: typ & 0x7F,
                chip_id: (typ >> 7) & 1,
                src,
                dest,
                length: if length != 0 { length } else { 0x1000000 },
            });
            pos += 12;
            continue;
        }
        if !is_known_cmd(cmd) {
            // never skip a guessed number of operands and desync the stream
            vgm.warnings.push(format!(
                "unsupported VGM command {cmd:02X} at 0x{pos:X}; stopped parsing"
            ));
            break;
        }
        if unconverted_name(cmd).is_some() {
            skipped_sample_cmds.insert(cmd);
        }
        pos += cmd_size(cmd);
    }

    vgm.loop_sample = loop_sample;
    // The 0x66 command's sample is the timeline the file really covers.
    if saw_end {
        if total != sample {
            if total != 0 && (total - sample).abs() > VGM_RATE / 10 {
                vgm.warnings.push(format!(
                    "header length {total} samples, stream ends at {sample}; using the stream length"
                ));
            }
            vgm.total_samples = sample;
        }
    } else if total == 0 {
        vgm.total_samples = sample;
    }
    if !skipped_sample_cmds.is_empty() {
        let names: BTreeSet<&str> = skipped_sample_cmds
            .iter()
            .filter_map(|c| unconverted_name(*c))
            .collect();
        let first = *skipped_sample_cmds.iter().min().unwrap();
        let joined = names.into_iter().collect::<Vec<_>>().join(", ");
        vgm.warnings.push(format!(
            "VGM writes to a sample chip this tool does not convert ({joined}; first command {first:02X}); its samples are missing"
        ));
    }
    if saw_adpcm_compressed {
        vgm.warnings
            .push("compressed OKIM6258 data block (type 44) is not converted".to_string());
    }
    if has_chip(&vgm.writes, "x1_010") && vgm.x1_010_clock == 0 {
        vgm.warnings.push(
            "X1-010 clock is missing or out of range; using 16000000".to_string(),
        );
    }
    if has_chip(&vgm.writes, "ga20") && vgm.ga20_clock == 0 {
        vgm.warnings.push(format!(
            "GA20 clock is missing or out of range; using {GA20_DEFAULT_CLOCK}"
        ));
    }
    if has_chip(&vgm.writes, "msm5205") && vgm.msm5205_clock == 0 {
        vgm.warnings.push(format!(
            "MSM5205 clock is missing or out of range; using {MSM5205_DEFAULT_CLOCK}"
        ));
    }
    if has_chip(&vgm.writes, "msm5232") && vgm.msm5232_clock == 0 {
        vgm.warnings.push(format!(
            "MSM5232 clock is missing or out of range; using {MSM5232_DEFAULT_CLOCK}"
        ));
    }
    if has_chip(&vgm.writes, "es5503") && vgm.es5503_clock == 0 {
        vgm.warnings.push(format!(
            "ES5503 clock is missing or out of range; using {ES5503_DEFAULT_CLOCK}"
        ));
    }
    if has_chip(&vgm.writes, "c352") && vgm.c352_clock == 0 {
        vgm.warnings
            .push("C352 clock is missing or out of range; using 24192000".to_string());
    }
    if has_chip(&vgm.writes, "ay8910") && vgm.ay8910_clock == 0 {
        vgm.warnings
            .push("AY-3-8910 clock is missing or out of range; using 1789772".to_string());
    }
    if has_chip(&vgm.writes, "k051649") && vgm.k051649_clock == 0 {
        vgm.warnings
            .push("K051649 clock is missing or out of range; using 1789772".to_string());
    }
    if has_chip(&vgm.writes, "k005289") {
        if vgm.k005289_clock == 0 {
            vgm.warnings.push(format!(
                "K005289 clock is missing or out of range; using {K005289_DEFAULT_CLOCK}"
            ));
        }
        if !vgm.roms.iter().any(|r| r.kind == "k005289") {
            vgm.warnings.push(
                "K005289 waveform PROM data block is missing; the WSG voices have no waveform and stay silent".to_string(),
            );
        }
    }
    if has_chip(&vgm.writes, "huc6280") && vgm.huc6280_clock == 0 {
        vgm.warnings.push(format!(
            "HuC6280 clock is missing or out of range; using {HUC6280_DEFAULT_CLOCK}"
        ));
    }
    if has_chip(&vgm.writes, "k053260") && vgm.k053260_clock == 0 {
        vgm.warnings
            .push("K053260 clock is missing or out of range; using 3579545".to_string());
    }
    if has_chip(&vgm.writes, "rf5c68") && vgm.rf5c68_clock == 0 {
        vgm.warnings.push(format!(
            "RF5C68 clock is missing or out of range; using {RF5C68_DEFAULT_CLOCK}"
        ));
    }
    if has_chip(&vgm.writes, "rf5c164") && vgm.rf5c164_clock == 0 {
        vgm.warnings.push(format!(
            "RF5C164 clock is missing or out of range; using {RF5C164_DEFAULT_CLOCK}"
        ));
    }
    if has_chip(&vgm.writes, "y8950") && vgm.y8950_clock == 0 {
        vgm.warnings.push(format!(
            "Y8950 clock is missing or out of range; using {Y8950_DEFAULT_CLOCK}"
        ));
    }
    if has_chip(&vgm.writes, "ymf278b") && vgm.ymf278b_clock == 0 {
        vgm.warnings.push(format!(
            "YMF278B clock is missing or out of range; using {YMF278B_DEFAULT_CLOCK}"
        ));
    }
    if has_chip_any(&vgm.writes, &["ym2203", "ym2203_ssg"]) && vgm.ym2203_clock == 0 {
        vgm.warnings
            .push("YM2203 clock is missing or out of range; using 3579545".to_string());
    }
    if has_chip_any(&vgm.writes, &["ym2610", "ym2610_ssg"]) && vgm.ym2610_clock == 0 {
        vgm.warnings.push(format!(
            "YM2610 clock is missing or out of range; using {YM2610_DEFAULT_CLOCK}"
        ));
    }
    let ym2610_adpcm_a = vgm.writes.iter().any(|w| {
        w.chip == "ym2610" && (w.reg & 0x100) != 0 && (w.reg & 0xFF) <= 0x2D
    });
    let ym2610_adpcm_b = vgm.writes.iter().any(|w| {
        w.chip == "ym2610" && (w.reg & 0x100) == 0 && (0x10..=0x1B).contains(&(w.reg & 0xFF))
    });
    if (ym2610_adpcm_a || ym2610_adpcm_b)
        && !vgm.roms.iter().any(|r| r.kind == "ym2610_adpcm")
    {
        vgm.warnings.push(
            "YM2610 ADPCM writes but no ADPCM ROM block (0x82); the sample channels will be silent".to_string(),
        );
    }
    if stray_ym2610_second {
        vgm.warnings.push(
            "0xA8/0xA9 YM2610 writes without the dual-instance header bit; they were treated as the first chip".to_string(),
        );
    }
    if stray_ym2608_second {
        vgm.warnings.push(
            "0xA6/0xA7 YM2608 writes without the dual-instance header bit; they were treated as the first chip".to_string(),
        );
    }
    if stray_opll_second {
        vgm.warnings.push(
            "0xA1 YM2413 writes without the dual-instance header bit; they were treated as the first chip".to_string(),
        );
    }
    if has_chip_any(&vgm.writes, &["ym2608", "ym2608_ssg"]) && vgm.ym2608_clock == 0 {
        vgm.warnings.push(
            "YM2608 clock is missing or out of range; using the FM Towns clock".to_string(),
        );
    }
    if has_chip(&vgm.writes, "opll") && vgm.ym2413_clock == 0 {
        vgm.warnings.push(
            "YM2413 clock is missing or out of range; using the standard clock".to_string(),
        );
    }
    if has_chip(&vgm.writes, "gb") && vgm.gb_dmg_clock == 0 {
        vgm.warnings.push(format!(
            "Game Boy DMG clock is missing or out of range; using {GB_DMG_DEFAULT_CLOCK}"
        ));
    }
    if has_chip(&vgm.writes, "nes") && vgm.nes_clock == 0 {
        vgm.warnings.push(format!(
            "NES APU clock is missing or out of range; using {NES_DEFAULT_CLOCK}"
        ));
    }
    if has_chip(&vgm.writes, "ym2612") && vgm.ym2612_clock == 0 {
        vgm.warnings.push(format!(
            "YM2612 clock is missing or out of range; using {YM2612_DEFAULT_CLOCK}"
        ));
    }
    // OPL family.
    if has_chip(&vgm.writes, "opl") && vgm.opl_clock == 0 {
        vgm.warnings.push(format!(
            "YM3526 clock is missing or out of range; using {OPL_DEFAULT_CLOCK}"
        ));
    }
    if has_chip(&vgm.writes, "opl2") && vgm.opl2_clock == 0 {
        vgm.warnings.push(format!(
            "YM3812 clock is missing or out of range; using {OPL2_DEFAULT_CLOCK}"
        ));
    }
    if has_chip(&vgm.writes, "opl3") && vgm.opl3_clock == 0 {
        vgm.warnings.push(format!(
            "YMF262 clock is missing or out of range; using {OPL3_DEFAULT_CLOCK}"
        ));
    }
    for (off, name) in [(OPL_CLOCK_OFF, "YM3526"), (OPL3_CLOCK_OFF, "YMF262")] {
        let raw = if ver >= 0x151 {
            _header_raw(&buf, off)
        } else {
            0
        };
        if raw & OPL_DUAL_BIT != 0 {
            vgm.warnings.push(format!(
                "header declares a second {name}; only the first chip's register stream is converted"
            ));
        }
    }
    if has_chip(&vgm.writes, "sn76489") && vgm.sn76489_clock == 0 {
        if vgm.sn76489_t6w28 {
            vgm.sn76489_clock = 3072000;
            vgm.warnings
                .push("T6W28 clock is missing or out of range; using 3072000".to_string());
        } else {
            vgm.warnings.push(format!(
                "SN76489 clock is missing or out of range; using {SN76489_DEFAULT_CLOCK}"
            ));
        }
    }
    if !vgm.dac_data.is_empty()
        && vgm.dac_events.is_empty()
        && vgm.dac_block_plays.is_empty()
        && vgm.dac_stream_plays.is_empty()
    {
        vgm.warnings.push(
            "YM2612 PCM data block present but no DAC play stream; the DAC data is unused".to_string(),
        );
    }
    if !vgm.dac_events.is_empty()
        && vgm.dac_data.is_empty()
        && vgm.dac_events.iter().any(|(_, kind, _)| *kind == 1)
    {
        vgm.warnings.push(
            "YM2612 DAC play commands present but no PCM data block; the DAC stream cannot be reconstructed".to_string(),
        );
    }
    if vgm.dac_data.is_empty() && vgm.dac_events.iter().any(|(_, kind, _)| *kind == 2) {
        vgm.warnings
            .push("YM2612 direct 0x2A DAC writes are not converted (no PCM bank)".to_string());
    }
    if vgm
        .dac_stream_plays
        .iter()
        .any(|p| p.chip_type == DAC_STREAM_YM2612)
        && vgm.dac_data.is_empty()
    {
        vgm.warnings.push(
            "DAC stream start commands present but no PCM data block; the stream cannot be reconstructed".to_string(),
        );
    }
    if stray_ym2612_second {
        vgm.warnings.push(
            "0xA2/0xA3 YM2612 writes without the dual-instance header bit; they were treated as the first chip".to_string(),
        );
    }
    if stray_ym2203_second {
        vgm.warnings.push(
            "0xA5 YM2203 writes without the dual-instance header bit; they were treated as the first chip".to_string(),
        );
    }
    if stray_opl2_second {
        vgm.warnings.push(
            "0xAA YM3812 writes without the dual-instance header bit; they were treated as the first chip".to_string(),
        );
    }
    if stray_ym2151_second {
        vgm.warnings.push(
            "0xA4 YM2151 writes without the dual-instance header bit; they were treated as the first chip".to_string(),
        );
    }
    if stray_opl_second {
        vgm.warnings.push(
            "0xAB YM3526 writes without the dual-instance header bit; they were treated as the first chip".to_string(),
        );
    }
    if stray_opl3_second {
        vgm.warnings.push(
            "0xAE/0xAF YMF262 writes without the dual-instance header bit; they were treated as the first chip".to_string(),
        );
    }
    if stray_y8950_second {
        vgm.warnings.push(
            "0xAC Y8950 writes without the dual-instance header bit; they were treated as the first chip".to_string(),
        );
    }
    if stray_opl4_second {
        vgm.warnings.push(
            "0xD0 YMF278B writes with port bit 7 set but no dual-instance header bit; they were treated as the second chip".to_string(),
        );
    }
    if stray_sn_second {
        vgm.warnings.push(
            "0x30/0x3F SN76489 writes without the dual-instance header bit; they were treated as the second chip".to_string(),
        );
    }
    if vgm.sn76489_dual && !vgm.sn76489_t6w28 {
        vgm.warnings
            .push("dual SN76489 log: both chips are converted as separate systems".to_string());
    }
    if sn76489_raw & SN76489_T6W28_BIT != 0 && !vgm.sn76489_dual {
        vgm.warnings.push(
            "T6W28 flag without the dual-instance header bit; the SN76489 stream was converted as mono".to_string(),
        );
    }
    if stray_c140_second {
        vgm.warnings.push(
            "0xD4 C140 address bit 15 selects a second chip without the dual-instance header bit; it was still treated as chip 1".to_string(),
        );
    }
    if stray_k054539_second {
        vgm.warnings.push(
            "0xD3 K054539 offset bit 15 selects a second chip without the dual-instance header bit; it was still treated as chip 1".to_string(),
        );
    }
    if stray_k053260_second {
        vgm.warnings.push(
            "0xBA K053260 register bit 7 selects a second chip without the dual-instance header bit; it was still treated as chip 1".to_string(),
        );
    }
    if stray_ay_second {
        vgm.warnings.push(
            "0xA0 AY-8910 register bit 7 selects a second chip without the dual-instance header bit; it was still treated as chip 1".to_string(),
        );
    }
    if stray_pokey_second {
        vgm.warnings.push(
            "0xBB POKEY register bit 7 selects a second chip without the dual-instance header bit; it was still treated as chip 1".to_string(),
        );
    }
    if has_chip(&vgm.writes, "pokey") && vgm.pokey_clock == 0 {
        vgm.warnings
            .push("POKEY writes without a header clock; using 1789772 Hz".to_string());
        vgm.pokey_clock = POKEY_DEFAULT_CLOCK;
    }
    if has_chip(&vgm.writes, "k054539")
        && (_header_raw(&buf, K054539_CLOCK_OFF) & 0x3FFFFFFF) == 0
        && clocks.get(&EXTRA_K054539).copied().unwrap_or(0) == 0
    {
        vgm.warnings
            .push("K054539 writes without a header clock; using 18432000 Hz".to_string());
    }
    if stray_scc_second {
        vgm.warnings.push(
            "0xD2 K051649 port bit 7 selects a second chip without the dual-instance header bit; it was still treated as chip 1".to_string(),
        );
    }
    if stray_msm6295_second {
        vgm.warnings.push(
            "0xB8 MSM6295 register bit 7 selects a second chip without the dual-instance header bit; it was still treated as chip 1".to_string(),
        );
    }
    if stray_msm6258_second {
        vgm.warnings.push(
            "0xB7 MSM6258 register bit 7 selects a second chip without the dual-instance header bit; it was still treated as chip 1".to_string(),
        );
    }
    if stray_ga20_second {
        vgm.warnings.push(
            "0xBF GA20 register bit 7 selects a second chip without the dual-instance header bit; it was still treated as chip 1".to_string(),
        );
    }
    if stray_msm5205_second {
        vgm.warnings.push(
            "0x32 MSM5205 bit 7 selects a second chip without the dual-instance header bit; it was still treated as chip 1".to_string(),
        );
    }
    if stray_msm5232_second {
        vgm.warnings.push(
            "MSM5232 register bit 7 selects a second chip without the dual-instance header bit; it was still treated as chip 1".to_string(),
        );
    }
    if stray_ymz_second {
        vgm.warnings.push(
            "0xAD YMZ280B writes without the dual-instance header bit; they were treated as the second chip".to_string(),
        );
    }
    if has_chip(&vgm.writes, "ymz280b") && vgm.ymz280b_clock == 0 {
        vgm.warnings
            .push("YMZ280B writes without a header clock; using 16934400 Hz".to_string());
        vgm.ymz280b_clock = YMZ280B_DEFAULT_CLOCK;
    }
    if stray_vsu_second {
        vgm.warnings.push(
            "0xC7 VSU writes with register bit 15 set but no dual-instance header bit; they were treated as the second chip".to_string(),
        );
    }
    if has_chip(&vgm.writes, "vsu") && vgm.vsu_clock == 0 {
        vgm.warnings
            .push("VSU writes without a header clock; using 5000000 Hz".to_string());
        vgm.vsu_clock = VSU_DEFAULT_CLOCK;
    }
    if stray_saa_second {
        vgm.warnings.push(
            "0xBD SAA1099 register bit 7 selects a second chip without the dual-instance header bit; it was still treated as chip 1".to_string(),
        );
    }
    if has_chip(&vgm.writes, "saa1099") && vgm.saa1099_clock == 0 {
        vgm.warnings
            .push("SAA1099 writes without a header clock; using 8000000 Hz".to_string());
        vgm.saa1099_clock = SAA1099_DEFAULT_CLOCK;
    }
    vgm.extra_clocks = clocks;
    vgm.extra_volumes = volumes;
    vgm.buf = buf;
    // Python tracks these three flags but never turns them into a warning.
    let _ = (
        stray_rf5c_second,
        stray_segapcm_second,
        stray_k007232_second,
    );
    Ok(vgm)
}

/// Stitch every ROM slice of a kind into one buffer.
pub fn assemble_rom(
    vgm: &VgmFile,
    kind: &str,
    chip_id: u8,
    fill: u8,
    swap16: bool,
    declared_size: bool,
) -> Vec<u8> {
    let slices: Vec<&RomSlice> = vgm
        .roms
        .iter()
        .filter(|s| s.chip == chip_id && s.kind == kind)
        .collect();
    if slices.is_empty() {
        return Vec::new();
    }
    let rom_size = slices.iter().map(|s| s.rom_size).max().unwrap_or(0);
    let end = slices
        .iter()
        .map(|s| s.start + s.data.len() as i64)
        .max()
        .unwrap_or(0);
    let size_i = if declared_size {
        rom_size.max(end)
    } else {
        end
    };
    let size = size_i.max(0) as usize;
    let mut rom = if fill != 0 {
        vec![fill & 0xFF; size]
    } else {
        vec![0u8; size]
    };
    for s in slices {
        let mut data = s.data.clone();
        if swap16 {
            let mut out = vec![0u8; data.len() & !1];
            let mut i = 0usize;
            while i < out.len() {
                out[i] = data[i + 1];
                out[i + 1] = data[i];
                i += 2;
            }
            data = out;
        }
        let start = s.start.max(0) as usize;
        if let Some(dst) = rom.get_mut(start..start + data.len()) {
            dst.copy_from_slice(&data);
        }
    }
    rom
}

pub fn assemble_k007232_rom(vgm: &VgmFile, chip_id: u8) -> Vec<u8> {
    assemble_rom(vgm, "k007232", chip_id, 0, false, true)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header(version: u32, rel: u32) -> Vec<u8> {
        let mut buf = vec![0u8; 0x40];
        buf[0..4].copy_from_slice(b"Vgm ");
        buf[8..12].copy_from_slice(&version.to_le_bytes());
        buf[0x34..0x38].copy_from_slice(&rel.to_le_bytes());
        buf
    }

    #[test]
    fn cmd_size_matches_the_python_table() {
        // 1-byte commands: waits, end, and the 0x70-0x8F block.
        for cmd in [0x62u8, 0x63, 0x66, 0x70, 0x7F, 0x8F] {
            assert_eq!(cmd_size(cmd), 1, "cmd {cmd:#04x}");
        }
        // 2-byte commands.
        for cmd in [0x4Fu8, 0x50, 0x94, 0x30, 0x3F] {
            assert_eq!(cmd_size(cmd), 2, "cmd {cmd:#04x}");
        }
        // 3-byte commands.
        for cmd in [0x2Du8, 0x61, 0x40, 0x4E, 0x51, 0x5F, 0xA0, 0xBF] {
            assert_eq!(cmd_size(cmd), 3, "cmd {cmd:#04x}");
        }
        // 4-byte commands.
        for cmd in [0xC0u8, 0xDF] {
            assert_eq!(cmd_size(cmd), 4, "cmd {cmd:#04x}");
        }
        // 5-byte commands.
        for cmd in [0x90u8, 0x91, 0x95, 0xE0, 0xFF] {
            assert_eq!(cmd_size(cmd), 5, "cmd {cmd:#04x}");
        }
        assert_eq!(cmd_size(0x92), 6);
        assert_eq!(cmd_size(0x93), 11);
        assert_eq!(cmd_size(0x68), 12);
        // The Python fallback covers everything else.
        for cmd in [0x00u8, 0x67] {
            assert_eq!(cmd_size(cmd), 2, "cmd {cmd:#04x}");
        }
    }

    #[test]
    fn data_offset_follows_the_header_field_with_fallback() {
        // Version >= 1.50 and a zero relative offset falls back to 0x40.
        assert_eq!(data_offset(&header(0x161, 0)).unwrap(), 0x40);
        // A non-zero relative offset is measured from 0x34.
        assert_eq!(data_offset(&header(0x161, 0x0C)).unwrap(), 0x40);
        assert_eq!(data_offset(&header(0x161, 0x20)).unwrap(), 0x54);
        // Versions before 1.50 always use the 0x40 command offset.
        assert_eq!(data_offset(&header(0x140, 0x20)).unwrap(), 0x40);
        // Not a VGM, or too short for a header.
        assert!(data_offset(b"RIFF").is_err());
        assert!(data_offset(&[0u8; 0x20]).is_err());
    }

    #[test]
    fn load_vgm_parses_a_minimal_ym2151_track() {
        // Same layout as tests/_synth.py raw_vgm: 0x40-byte header, version
        // 0x161, 44100 Hz, YM2151 at 3579545, data offset field 0x0C.
        let mut buf = header(0x161, 0x0C);
        buf[0x18..0x1C].copy_from_slice(&44100u32.to_le_bytes());
        buf[0x30..0x34].copy_from_slice(&3579545u32.to_le_bytes());
        // 16-sample wait, one YM2151 write (reg 0x28 = 0x00), end of data.
        buf.extend_from_slice(&[0x61, 0x10, 0x00, 0x54, 0x28, 0x00, 0x66]);
        let len = (buf.len() - 4) as u32;
        buf[4..8].copy_from_slice(&len.to_le_bytes());

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("minimal.vgm");
        std::fs::write(&path, &buf).unwrap();

        let vgm = load_vgm(&path).unwrap();
        assert_eq!(vgm.writes.len(), 1);
        let write = &vgm.writes[0];
        assert_eq!(write.chip, "ym2151");
        assert_eq!(write.chip_id, 0);
        assert_eq!(write.sample, 16);
        assert_eq!(write.reg, 0x28);
        assert_eq!(write.val, 0x00);
        // The 0x66 end command sets the timeline the stream covers.
        assert_eq!(vgm.total_samples, 16);
        assert_eq!(vgm.loop_samples, 0);
        assert_eq!(vgm.loop_file_pos, None);
        assert_eq!(vgm.ym2151_clock, 3_579_545);
    }

    // --- header readers / ROM stitching -----------------------------------
    // Covers the header half of the module: guarded field reads, sane-clock
    // filtering, the extra-header clock list, the header census and the ROM
    // stitcher.

    /// A synthetic header `data_start` bytes long with a matching 0x34 field.
    fn synth_header(version: u32, data_start: usize) -> Vec<u8> {
        let size = data_start.max(0x100);
        let mut buf = vec![0u8; size];
        buf[0..4].copy_from_slice(b"Vgm ");
        buf[8..12].copy_from_slice(&version.to_le_bytes());
        if version >= 0x150 {
            let rel = (data_start - 0x34) as u32;
            buf[0x34..0x38].copy_from_slice(&rel.to_le_bytes());
        }
        buf
    }

    fn put_u32(buf: &mut [u8], off: usize, val: u32) {
        buf[off..off + 4].copy_from_slice(&val.to_le_bytes());
    }

    #[test]
    fn head_bytes_reads_want_and_less_on_a_short_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("short.vgm");
        let data: Vec<u8> = (0..=255u8).collect();
        std::fs::write(&path, &data).unwrap();
        assert_eq!(head_bytes(&path, 10).unwrap(), data[..10]);
        assert_eq!(head_bytes(&path, 1000).unwrap(), data);
        assert_eq!(head_bytes(&path, 0).unwrap(), Vec::<u8>::new());
    }

    #[test]
    fn head_bytes_inflates_a_vgz() {
        use flate2::write::GzEncoder;
        use std::io::Write as _;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x.vgz");
        let data: Vec<u8> = (0..=255u8).collect();
        let mut enc = GzEncoder::new(Vec::new(), flate2::Compression::default());
        enc.write_all(&data).unwrap();
        std::fs::write(&path, enc.finish().unwrap()).unwrap();
        assert_eq!(head_bytes(&path, 16).unwrap(), data[..16]);
        assert_eq!(head_bytes(&path, 1000).unwrap(), data);
    }

    #[test]
    fn field_in_header_boundaries() {
        // Not a VGM: anything that fits counts as "in the header".
        assert!(_field_in_header(&[0u8; 8], 0, 4));
        assert!(!_field_in_header(&[0u8; 8], 6, 4));
        // A VGM header ends at data_offset.
        let buf = synth_header(0x150, 0x40);
        assert!(_field_in_header(&buf, 0x3C, 4));
        assert!(!_field_in_header(&buf, 0x3D, 4));
        assert!(!_field_in_header(&buf, 0x100, 4));
        // Below 0x40 the magic test is skipped, like the Python short-circuit.
        let short = &buf[..0x20];
        assert!(_field_in_header(short, 0x1C, 4));
        assert!(!_field_in_header(short, 0x1D, 4));
    }

    #[test]
    fn sane_clock_rejects_out_of_range() {
        assert_eq!(_sane_clock(CLOCK_MIN - 1), 0);
        assert_eq!(_sane_clock(CLOCK_MIN), CLOCK_MIN);
        assert_eq!(_sane_clock(CLOCK_MAX), CLOCK_MAX);
        assert_eq!(_sane_clock(CLOCK_MAX + 1), 0);
        // Bit 31 is stripped before the range test.
        assert_eq!(_sane_clock(3_579_545 | 0x8000_0000), 3_579_545);
    }

    #[test]
    fn parse_clock_list_decodes_a_synthetic_list() {
        let mut buf = vec![2u8];
        buf.push(0x02);
        buf.extend_from_slice(&3_579_545u32.to_le_bytes());
        buf.push(0x2A);
        buf.extend_from_slice(&4_000_000u32.to_le_bytes());
        let clocks = _parse_clock_list(&buf, 0);
        assert_eq!(clocks.len(), 2);
        assert_eq!(clocks.get(&0x02), Some(&3_579_545));
        assert_eq!(clocks.get(&0x2A), Some(&4_000_000));

        // Bad count byte, out-of-range clock, truncation, past the end.
        assert!(_parse_clock_list(&[0u8], 0).is_empty());
        assert!(_parse_clock_list(&[65u8], 0).is_empty());
        assert!(_parse_clock_list(&[1u8, 0x02, 0x10, 0, 0, 0], 0).is_empty());
        assert!(_parse_clock_list(&[1u8, 0x02, 0, 0], 0).is_empty());
        assert!(_parse_clock_list(&[], 0).is_empty());
        // Volume list: (chip, flags, u16 value).
        let volumes = _parse_volume_list(&[1u8, 0x12, 0x00, 0x00, 0x01], 0);
        assert_eq!(volumes.get(&0x12), Some(&(0x00, 0x0100)));
    }

    #[test]
    fn chips_in_head_core_families() {
        // YM2151 at 0x30 has no version gate.
        let mut buf = synth_header(0x150, 0x100);
        put_u32(&mut buf, 0x30, 3_579_545);
        assert!(chips_in_head(&buf).contains("ym2151"));

        // YM2612 at 0x2C, with the dual bit.
        let mut buf = synth_header(0x150, 0x100);
        put_u32(&mut buf, 0x2C, 7_670_453 | 0x4000_0000);
        let kinds = chips_in_head(&buf);
        assert!(kinds.contains("ym2612"));
        assert!(kinds.contains("ym2612_dual"));

        // OPL3 at 0x5C (1.51+).
        let mut buf = synth_header(0x151, 0x100);
        put_u32(&mut buf, 0x5C, 14_318_180);
        assert!(chips_in_head(&buf).contains("opl3"));

        // NES APU at 0x84 (1.51+), FDS on bit 31.
        let mut buf = synth_header(0x151, 0x100);
        put_u32(&mut buf, 0x84, 1_789_772 | 0x8000_0000);
        let kinds = chips_in_head(&buf);
        assert!(kinds.contains("nes"));
        assert!(kinds.contains("fds"));

        // No magic: empty. A short magic-only buffer still reads 0x30.
        assert!(chips_in_head(b"Nope. VGM").is_empty());
        let mut small = vec![0u8; 0x34];
        small[0..4].copy_from_slice(b"Vgm ");
        put_u32(&mut small, 0x30, 3_579_545);
        assert!(chips_in_head(&small).contains("ym2151"));
    }

    #[test]
    fn chips_in_head_c140_type_selects_c219() {
        let mut buf = synth_header(0x161, 0x100);
        put_u32(&mut buf, C140_CLOCK_OFF, 12_288_000);
        assert!(chips_in_head(&buf).contains("c140"));
        buf[C140_TYPE_OFF] = 2;
        let kinds = chips_in_head(&buf);
        assert!(kinds.contains("c219"));
        assert!(!kinds.contains("c140"));
    }

    #[test]
    fn header_effective_clocks() {
        assert_eq!(_c140_effective_clock(21390), C140_DEFAULT_CLOCK);
        assert_eq!(_c140_effective_clock(12_288_000), 12_288_000);
        // Below CLOCK_MIN the legacy rate form is not multiplied.
        assert_eq!(_c140_effective_clock(24_000), C140_DEFAULT_CLOCK);
        // The K054539 legacy form multiplies by 384.
        assert_eq!(_k054539_effective_clock(100_000), 38_400_000);
        // The C219 has a wider ceiling, so its 576x form survives.
        assert_eq!(_c219_effective_clock(100_000), 57_600_000);
        assert_eq!(c352_effective_clock(24_192_000, 72), 24_192_000);
        assert_eq!(c352_effective_clock(336_000, 1), 24_192_000);
        assert_eq!(c352_effective_clock(0, 72), 0);
    }

    #[test]
    fn k007232_clock_fallbacks() {
        let mut buf = synth_header(0x171, 0x100);
        put_u32(&mut buf, K007232_EXTRA_CLOCK_OFF, 3_579_545);
        let empty = IndexMap::new();
        assert_eq!(_k007232_clock(&buf, &empty, 0, 0x171), 3_579_545);
        // Header ignored before 1.71; the extra-header value wins.
        let mut extra = IndexMap::new();
        extra.insert(EXTRA_K007232, 6_000_000);
        assert_eq!(_k007232_clock(&buf, &extra, 0, 0x170), 6_000_000);
        // Neither header nor extra: the YM clock, else 3579545.
        put_u32(&mut buf, K007232_EXTRA_CLOCK_OFF, 0);
        assert_eq!(_k007232_clock(&buf, &empty, 8_000_000, 0x171), 8_000_000);
        assert_eq!(_k007232_clock(&buf, &empty, 0, 0x171), 3_579_545);
    }

    fn rom_slice(chip: u8, rom_size: i64, start: i64, data: &[u8], kind: &'static str) -> RomSlice {
        RomSlice {
            chip,
            rom_size,
            start,
            data: data.to_vec(),
            kind,
            time: 0,
        }
    }

    #[test]
    fn assemble_rom_stitches_two_slices_with_fill_and_declared_size() {
        let mut vgm = VgmFile::default();
        vgm.roms = vec![
            rom_slice(0, 16, 4, &[1, 2, 3, 4], "c140"),
            rom_slice(0, 16, 8, &[5, 6, 7, 8], "c140"),
        ];
        let rom = assemble_rom(&vgm, "c140", 0, 0xFF, false, true);
        assert_eq!(rom.len(), 16);
        assert_eq!(&rom[..4], &[0xFFu8; 4]);
        assert_eq!(&rom[4..8], &[1, 2, 3, 4]);
        assert_eq!(&rom[8..12], &[5, 6, 7, 8]);
        assert_eq!(&rom[12..], &[0xFFu8; 4]);

        // declared_size = false allocates only the dumped span.
        let rom = assemble_rom(&vgm, "c140", 0, 0, false, false);
        assert_eq!(rom.len(), 12);
        assert_eq!(&rom[4..12], &[1, 2, 3, 4, 5, 6, 7, 8]);
    }

    #[test]
    fn assemble_rom_swap16_drops_the_odd_tail() {
        let mut vgm = VgmFile::default();
        vgm.roms = vec![rom_slice(0, 8, 0, &[1, 2, 3, 4, 5], "c219")];
        let rom = assemble_rom(&vgm, "c219", 0, 0, true, true);
        assert_eq!(rom.len(), 8);
        assert_eq!(&rom[..4], &[2u8, 1, 4, 3]);
        assert_eq!(&rom[4..], &[0u8; 4]);
    }

    #[test]
    fn assemble_k007232_rom_delegates() {
        let mut vgm = VgmFile::default();
        vgm.roms = vec![
            rom_slice(0, 8, 0, &[1, 2, 3, 4], "k007232"),
            rom_slice(0, 8, 4, &[5, 6, 7, 8], "k007232"),
        ];
        assert_eq!(
            assemble_k007232_rom(&vgm, 0),
            vec![1, 2, 3, 4, 5, 6, 7, 8]
        );
        assert_eq!(
            assemble_k007232_rom(&vgm, 0),
            assemble_rom(&vgm, "k007232", 0, 0, false, true)
        );
        assert!(assemble_k007232_rom(&vgm, 1).is_empty());
    }
}
