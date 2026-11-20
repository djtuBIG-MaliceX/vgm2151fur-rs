#!/usr/bin/env python3
"""Diff the Python and Rust VGM loader dumps over a set of files.

    python tools/dump_diff.py FILE...
    python tools/dump_diff.py --pack "Gradius VGM Rip" --pack "fds"

Exit code 0 when every file's dump is identical, 1 otherwise.  The first
mismatching line of a file is printed with both sides.
"""

from __future__ import annotations

import argparse
import subprocess
import sys
from pathlib import Path

sys.dont_write_bytecode = True

WORKSPACE = Path(__file__).resolve().parent.parent
REF = WORKSPACE / "tools" / "dump_vgm.py"
RUST_BIN = WORKSPACE / "target" / "debug" / "parity_dump.exe"
DEFAULT_SAMPLES = Path("D:/Downloads/moreVGMs")


def _run(cmd: list[str]) -> str:
    env = {"PYTHONDONTWRITEBYTECODE": "1", "PATH": "/usr/bin:/bin"}
    return subprocess.run(
        cmd, capture_output=True, text=True, encoding="utf-8", errors="replace"
    ).stdout


def _corpus(samples: Path, packs: list[str], limit: int) -> list[Path]:
    files: list[Path] = []
    roots = [samples / p for p in packs] if packs else [samples]
    for root in roots:
        if root.is_file():
            files.append(root)
            continue
        for suffix in ("*.vgm", "*.vgz"):
            files.extend(root.rglob(suffix))
    files.sort()
    return files[:limit] if limit else files


def main(argv: list[str]) -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("files", nargs="*")
    ap.add_argument("--pack", action="append", default=[])
    ap.add_argument("--samples", default=str(DEFAULT_SAMPLES))
    ap.add_argument("--limit", type=int, default=0)
    args = ap.parse_args(argv)

    if not RUST_BIN.exists():
        print(f"missing {RUST_BIN}; run cargo build --bin parity_dump", file=sys.stderr)
        return 2

    files = [Path(f) for f in args.files] or _corpus(
        Path(args.samples), args.pack, args.limit
    )
    if not files:
        print("no files selected", file=sys.stderr)
        return 2

    ok = bad = 0
    for path in files:
        want = _run([sys.executable, str(REF), str(path)]).splitlines()
        got = _run([str(RUST_BIN), str(path)]).splitlines()
        if want == got:
            ok += 1
            continue
        bad += 1
        print(f"FAIL {path}")
        for i, (a, b) in enumerate(zip(want, got)):
            if a != b:
                print(f"  line {i + 1}:\n    py   {a}\n    rust {b}")
                break
        else:
            print(f"  length: py={len(want)} rust={len(got)}")
            tail = want[len(got):] or got[len(want):]
            for line in tail[:3]:
                print(f"    extra {line}")
    print(f"compared={len(files)} ok={ok} mismatch={bad}")
    return 0 if bad == 0 else 1


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
