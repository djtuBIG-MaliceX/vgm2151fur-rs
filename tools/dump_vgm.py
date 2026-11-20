#!/usr/bin/env python3
"""Stable line dump of a parsed VGM file, for diffing the Rust loader.

`parity_dump` emits the same lines from the Rust side.  The dump covers every
scalar header field (alphabetical), the extra-header lists, then the parsed
stream: warnings, chip writes, ROM slices, DAC events and stream plays, all in
file order.
"""

from __future__ import annotations

import sys
import zlib
from pathlib import Path

sys.dont_write_bytecode = True
REF_ROOT = Path("D:/Projects/forks/vgm2151fur")
sys.path.insert(0, str(REF_ROOT))

from vgm2151fur.vgm import load_vgm  # noqa: E402

# Scalar VgmFile fields, alphabetical.  Kept in sync with parity_dump.rs.
SCALARS = [
    "ay8910_clock", "ay8910_dual", "ay8910_flags", "ay8910_type",
    "c140_clock", "c140_dual", "c140_type",
"c352_clock", "c352_divider", "c352_mute_rear",
    "es5503_channels", "es5503_clock",
    "ga20_clock", "ga20_dual",
    "gb_dmg_clock", "gb_dmg_dual",
    "huc6280_clock", "huc6280_dual",
    "k005289_clock",
    "k007232_clock", "k007232_dual",
    "k051649_clock", "k051649_dual", "k051649_plus",
    "k053260_clock", "k053260_dual",
    "k054539_clock", "k054539_clock_2", "k054539_dual", "k054539_flags",
    "loop_file_pos", "loop_sample", "loop_samples",
    "main_volume",
    "mikey_clock", "mikey_dual",
    "msm5205_clock", "msm5205_dual", "msm5205_flags",
    "msm5232_clock", "msm5232_dual",
    "msm6258_clock", "msm6258_dual", "msm6258_flags",
    "msm6295_clock", "msm6295_dual", "msm6295_pin7",
    "multipcm_clock", "multipcm_clock_2", "multipcm_dual",
    "nes_clock", "nes_dual", "nes_fds",
    "opl3_clock", "opl3_dual",
    "opl2_clock", "opl2_dual", "opl2_pan",
    "opl_clock", "opl_dual",
    "ym2413_clock", "ym2413_dual", "ym2413_vrc7",
"pokey_clock", "pokey_clock_2", "pokey_dual",
    "pwm_clock", "pwm_clock_2", "pwm_dual",
    "qsound_clock",
    "rf5c164_clock", "rf5c164_dual", "rf5c68_clock", "rf5c68_dual",
    "saa1099_clock", "saa1099_dual",
    "segapcm_bankmask", "segapcm_bankshift", "segapcm_clock", "segapcm_dual",
    "sn76489_clock", "sn76489_clock_2", "sn76489_dual", "sn76489_stereo",
    "sn76489_t6w28",
    "stream_setups",
    "total_samples",
    "upd7759_clock", "upd7759_clock_2", "upd7759_dual", "upd7759_slave",
    "version",
    "vsu_clock", "vsu_clock_2", "vsu_dual",
    "wonderswan_clock", "wonderswan_dual",
    "x1_010_clock", "x1_010_dual",
    "y8950_clock", "y8950_dual",
    "ym2151_clock", "ym2151_dual",
    "ym2203_clock", "ym2203_dual",
    "ym2413_clock", "ym2413_dual", "ym2413_vrc7",
    "ym2608_clock", "ym2608_dual",
    "ym2610_b", "ym2610_clock", "ym2610_dual",
    "ym2612_clock", "ym2612_dual",
    "ymf278b_clock", "ymf278b_dual",
    "ymz280b_clock", "ymz280b_clock_2", "ymz280b_dual",
]


def _text(value) -> str:
    if value is None:
        return "None"
    if isinstance(value, bool):
        return "1" if value else "0"
    return str(value)


def dump(path: Path) -> list[str]:
    v = load_vgm(path)
    out = [f"path {v.path.name}"]
    for name in SCALARS:
        if not hasattr(v, name):
            out.append(f"hdr {name} <absent>")
            continue
        out.append(f"hdr {name} {_text(getattr(v, name))}")
    for chip, clock in sorted(v.extra_clocks.items()):
        out.append(f"extraclk {chip} {clock}")
    for chip, entry in sorted(v.extra_volumes.items()):
        out.append(f"extravol {chip} {entry[0]} {entry[1]}")
    for key, value in v.gd3.items():
        out.append(f"gd3 {key} {value}")
    for warning in v.warnings:
        out.append(f"warn {warning}")
    for w in v.writes:
        out.append(f"write {w.sample} {w.chip} {w.chip_id} {w.reg} {w.val}")
    for r in v.roms:
        digest = "%08x" % zlib.crc32(bytes(r.data))
        out.append(
            f"rom {r.chip} {r.kind} {r.rom_size} {r.start} {len(r.data)} {r.time} {digest}"
        )
    for sample, kind, value in v.dac_events:
        out.append(f"dacev {sample} {kind} {value}")
    for play in v.dac_stream_plays:
        out.append(
            "stream "
            f"{play.sample} {play.chip_type} {play.chip_id} {play.stream} {play.port} "
            f"{play.reg} {play.offset} {play.length} {play.mode} {play.rate} "
            f"{int(play.loop)} {int(play.reverse)} {play.step} {play.step_base} "
            f"{play.bank} {play.block} {play.stop_sample}"
        )
    for play in v.msm6258_plays:
        out.append(
            "msm6258 "
            f"{play.sample} {play.chip_id} {play.block} {play.rate} "
            f"{int(play.loop)} {int(play.reverse)} {play.offset} {play.nbytes} "
            f"{play.end} {play.step} {play.step_base} {play.length_mode}"
        )
    for w in v.pcm_ram_writes:
        out.append(
            f"pcmram {w.sample} {w.type} {w.chip_id} {w.src} {w.dest} {w.length}"
        )
    for d in v.dac_blocks:
        out.append(f"dacblock {len(d)} {"%08x" % zlib.crc32(d)}")
    for d in v.adpcm_blocks:
        out.append(f"adpcmblock {len(d)} {"%08x" % zlib.crc32(d)}")
    out.append(f"dacdata {len(v.dac_data)} {"%08x" % zlib.crc32(bytes(v.dac_data))}")
    return out


def main(argv: list[str]) -> int:
    if not argv:
        print("usage: dump_vgm.py FILE [FILE...]", file=sys.stderr)
        return 2
    for name in argv:
        try:
            print("\n".join(dump(Path(name))))
        except Exception as exc:  # noqa: BLE001 - report and continue
            print(f"error {type(exc).__name__}: {exc}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
