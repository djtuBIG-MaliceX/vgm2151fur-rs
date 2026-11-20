#!/usr/bin/env python3
"""Compare the reference vgm2151fur package with the Rust port.

Stage 0 prints the reference version and the size of the sample corpus.  Later
stages run the cases registered in CASES plus any corpus files selected with
--pack or --full, convert each with both implementations, decompress the .fur
files with zlib, and compare those bytes and the `report` output.

Only the Python standard library is used.

Every case converts with the default options only: no ``--variants`` and no
``--optimize`` (both default to off), plus ``--no-normalize --workers 1`` so
Furnace and thread scheduling cannot vary.  The variant/optimise ladder is a
separate check, not part of the byte-parity gate.
"""

from __future__ import annotations

import argparse
import hashlib
import os
import re
import shutil
import subprocess
import sys
import tempfile
import zlib
from pathlib import Path

# The reference tree is read-only for this port; never write its bytecode.
sys.dont_write_bytecode = True

REF_ROOT = Path("D:/Projects/forks/vgm2151fur")
REF_TESTS = REF_ROOT / "tests"
WORKSPACE = Path(__file__).resolve().parent.parent
DEFAULT_RUST_BIN = WORKSPACE / "target" / "debug" / "vgm2fur.exe"
DEFAULT_SAMPLES = Path("D:/Downloads/moreVGMs")
FAIL_ROOT = WORKSPACE / "target" / "parity" / "fail"
VGM_SUFFIXES = (".vgm", ".vgz")
RUN_TIMEOUT = 1800

# Stage -> [(case name, [source, extra convert args...])].  A relative source
# is resolved against --samples.  Stages add entries as the port grows; the
# harness runs whatever is present and reports the totals.
CASES: dict[int, list[tuple[str, list[str]]]] = {
    1: [],
    2: [],
    3: [],
    4: [],
    5: [],
    6: [],
    7: [],
    8: [],
    9: [],
    10: [],
    11: [],
}


def _reconfigure_stdio() -> None:
    for stream in (sys.stdout, sys.stderr):
        try:
            stream.reconfigure(encoding="utf-8", errors="replace")
        except (AttributeError, ValueError):
            pass


def _exe(path: Path) -> Path:
    if path.exists():
        return path
    if os.name == "nt" and path.suffix == "":
        candidate = path.with_suffix(".exe")
        if candidate.exists():
            return candidate
    return path


def _rust_binary(explicit: str | None) -> Path:
    if explicit:
        return _exe(Path(explicit))
    return _exe(DEFAULT_RUST_BIN)


def _reference_version() -> str:
    result = subprocess.run(
        [sys.executable, "-c", "import vgm2151fur; print(vgm2151fur.__version__)"],
        cwd=str(REF_ROOT),
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        timeout=120,
        check=False,
    )
    if result.returncode != 0:
        text = result.stderr.decode("utf-8", "replace").strip()
        raise RuntimeError(text or f"python exited {result.returncode}")
    return result.stdout.decode("utf-8", "replace").strip()


def _iter_corpus(root: Path) -> list[Path]:
    files: list[Path] = []
    if not root.is_dir():
        return files
    for dirpath, dirnames, filenames in os.walk(root):
        dirnames.sort(key=str.lower)
        for name in sorted(filenames, key=str.lower):
            if Path(name).suffix.lower() in VGM_SUFFIXES:
                files.append(Path(dirpath) / name)
    return files


def _pack_matches(rel: Path, pack: str) -> bool:
    wanted = pack.replace("\\", "/").strip("/").lower()
    if not wanted:
        return False
    parts = [part.lower() for part in rel.parts[:-1]]
    if not parts:
        return False
    if "/" in wanted:
        return "/".join(parts).endswith(wanted) or wanted in "/".join(parts)
    return wanted in parts


def _sanitize(text: str) -> str:
    cleaned = re.sub(r"[^A-Za-z0-9._-]+", "_", text).strip("._-")
    return cleaned[:100] or "case"


def _unique_sanitized(rel: str) -> str:
    base = _sanitize(rel)
    digest = hashlib.sha1(rel.encode("utf-8", "replace")).hexdigest()[:8]
    return f"{base}-{digest}" if base != rel else base


def _first_diff(a: bytes, b: bytes) -> int | None:
    limit = min(len(a), len(b))
    for index in range(limit):
        if a[index] != b[index]:
            return index
    if len(a) != len(b):
        return limit
    return None


def _first_diff_line(a: str, b: str) -> int | None:
    left = a.replace("\r\n", "\n").split("\n")
    right = b.replace("\r\n", "\n").split("\n")
    limit = min(len(left), len(right))
    for index in range(limit):
        if left[index] != right[index]:
            return index + 1
    if len(left) != len(right):
        return limit + 1
    return None


def _decompress_fur(path: Path) -> bytes:
    return zlib.decompress(path.read_bytes())


def _find_fur(out_dir: Path, stem: str) -> Path | None:
    direct = out_dir / (stem + ".fur")
    if direct.is_file():
        return direct
    hits = sorted(out_dir.rglob("*.fur"))
    if len(hits) == 1:
        return hits[0]
    return None


def _run(command: list[str], cwd: Path) -> subprocess.CompletedProcess:
    env = os.environ.copy()
    env["PYTHONDONTWRITEBYTECODE"] = "1"
    return subprocess.run(
        command,
        cwd=str(cwd),
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        timeout=RUN_TIMEOUT,
        env=env,
        check=False,
    )


def _error_text(result: subprocess.CompletedProcess) -> str:
    stderr = result.stderr.decode("utf-8", "replace").strip()
    stdout = result.stdout.decode("utf-8", "replace").strip()
    text = stderr or stdout or "no output"
    return text[-800:]


def _write_synth(name: str, out_dir: Path) -> Path:
    """Write one synthetic VGM through the reference tests/_synth.py."""
    for entry in (str(REF_ROOT), str(REF_TESTS)):
        if entry not in sys.path:
            sys.path.insert(0, entry)
    import _synth  # noqa: E402  (path set above, reference tests only)

    if name == "staircase":
        writes = _synth.note_song(pitch_writes=_synth.staircase(60000)).writes
    elif name == "climb":
        writes = _synth.note_song(pitch_writes=_synth.climb(60030)).writes
    else:  # note_song and raw_vgm share the same keyed note
        writes = _synth.note_song().writes

    cmds = bytearray()
    now = 0
    for write in sorted(writes, key=lambda w: int(w.sample)):
        sample = int(write.sample)
        if sample > now:
            delta = sample - now
            while delta > 0:
                chunk = min(delta, 0xFFFF)
                cmds += bytes([0x61, chunk & 0xFF, (chunk >> 8) & 0xFF])
                delta -= chunk
            now = sample
        if write.chip != "ym2151":
            raise ValueError(f"synth writer cannot encode {write.chip!r} writes")
        command = 0x54 + (1 if write.chip_id else 0)
        cmds += bytes([command, int(write.reg) & 0xFF, int(write.val) & 0xFF])

    data = _synth.raw_vgm(bytes(cmds))
    path = out_dir / f"{name}.vgm"
    path.write_bytes(data)
    return path


def _registered_cases(stage: int, samples_root: Path) -> list[tuple[str, Path, list[str]]]:
    cases: list[tuple[str, Path, list[str]]] = []
    for name, args in CASES.get(stage, []):
        if isinstance(args, str):
            args = [args]
        if not args:
            continue
        source = Path(args[0])
        if not source.is_absolute():
            source = samples_root / source
        extra = [str(arg) for arg in args[1:]]
        if source.is_dir():
            for path in _iter_corpus(source):
                try:
                    rel = path.relative_to(source)
                except ValueError:
                    rel = Path(path.name)
                cases.append((f"{name}/{rel}", path, extra))
            continue
        cases.append((name, source, extra))
    return cases


def _corpus_cases(files: list[Path], samples_root: Path) -> list[tuple[str, Path, list[str]]]:
    cases: list[tuple[str, Path, list[str]]] = []
    for path in files:
        try:
            rel = path.relative_to(samples_root)
        except ValueError:
            rel = Path(path.name)
        cases.append((_unique_sanitized(str(rel)), path, []))
    return cases


def _convert(
    command_base: list[str],
    cwd: Path,
    source: Path,
    extra: list[str],
    out_dir: Path,
) -> tuple[Path | None, bytes | None, str | None, subprocess.CompletedProcess]:
    out_dir.mkdir(parents=True, exist_ok=True)
    command = command_base + ["convert", str(source)] + extra + [
        "-o", str(out_dir), "--no-normalize", "--workers", "1",
    ]
    result = _run(command, cwd)
    fur = _find_fur(out_dir, source.stem)
    if result.returncode != 0:
        return fur, None, f"exit {result.returncode}: {_error_text(result)}", result
    if fur is None:
        return None, None, "no .fur produced: " + _error_text(result), result
    try:
        return fur, _decompress_fur(fur), None, result
    except (OSError, zlib.error) as exc:
        return fur, None, f"cannot decompress {fur.name}: {exc}", result


def _report(command_base: list[str], cwd: Path, source: Path) -> subprocess.CompletedProcess:
    return _run(
        command_base + ["report", str(source), "--workers", "1"],
        cwd,
    )


def _write_note(
    case_dir: Path,
    case: str,
    source: Path,
    ref_error: str | None,
    rust_error: str | None,
    byte_offset: int | None,
    byte_lengths: tuple[int, int] | None,
    report_line: int | None,
) -> None:
    case_dir.mkdir(parents=True, exist_ok=True)
    lines = [f"case: {case}", f"source: {source}"]
    if ref_error:
        lines.append(f"reference convert: FAILED ({ref_error})")
    if rust_error:
        lines.append(f"rust convert: FAILED ({rust_error})")
    if byte_offset is not None:
        lines.append(f"byte mismatch: first difference at offset {byte_offset} (0x{byte_offset:x})")
        if byte_lengths is not None:
            lines.append(f"decompressed lengths: reference {byte_lengths[0]}, rust {byte_lengths[1]}")
    elif ref_error is None and rust_error is None:
        lines.append("byte mismatch: none")
    if report_line is not None:
        lines.append(f"report mismatch: first difference at line {report_line}")
    else:
        lines.append("report mismatch: none")
    (case_dir / "note.txt").write_text("\n".join(lines) + "\n", encoding="utf-8")


def _run_case(
    case: str,
    source: Path,
    extra: list[str],
    rust_bin: Path,
) -> dict:
    """Run one case; returns counts and writes failure artifacts."""
    ref_base = [sys.executable, "-m", "vgm2151fur"]
    rust_base = [str(rust_bin)]
    artifact_dir = FAIL_ROOT / _sanitize(case)

    with tempfile.TemporaryDirectory(prefix="vgm2fur_ref_") as ref_tmp, \
            tempfile.TemporaryDirectory(prefix="vgm2fur_rust_") as rust_tmp:
        ref_out = Path(ref_tmp)
        rust_out = Path(rust_tmp)
        ref_fur, ref_bytes, ref_error, _ref_convert = _convert(
            ref_base, REF_ROOT, source, extra, ref_out
        )
        rust_fur, rust_bytes, rust_error, _rust_convert = _convert(
            rust_base, WORKSPACE, source, extra, rust_out
        )

        ref_report = _report(ref_base, REF_ROOT, source)
        rust_report = _report(rust_base, WORKSPACE, source)
        ref_text = ref_report.stdout.decode("utf-8", "replace")
        rust_text = rust_report.stdout.decode("utf-8", "replace")
        if ref_report.returncode != 0:
            ref_error = ref_error or (
                f"report exit {ref_report.returncode}: {_error_text(ref_report)}"
            )
        if rust_report.returncode != 0:
            rust_error = rust_error or (
                f"report exit {rust_report.returncode}: {_error_text(rust_report)}"
            )

        byte_offset = None
        byte_lengths = None
        if ref_bytes is not None and rust_bytes is not None:
            byte_offset = _first_diff(ref_bytes, rust_bytes)
            byte_lengths = (len(ref_bytes), len(rust_bytes))
        report_line = _first_diff_line(ref_text, rust_text)

        byte_mismatch = bool(ref_error or rust_error or byte_offset is not None)
        report_mismatch = bool(
            ref_report.returncode != 0
            or rust_report.returncode != 0
            or report_line is not None
        )

        if byte_mismatch or report_mismatch:
            if artifact_dir.exists():
                shutil.rmtree(artifact_dir, ignore_errors=True)
            artifact_dir.mkdir(parents=True, exist_ok=True)
            if ref_bytes is not None:
                (artifact_dir / "ref.decompressed.fur").write_bytes(ref_bytes)
            if rust_bytes is not None:
                (artifact_dir / "rust.decompressed.fur").write_bytes(rust_bytes)
            (artifact_dir / "ref.report.txt").write_text(ref_text, encoding="utf-8")
            (artifact_dir / "rust.report.txt").write_text(rust_text, encoding="utf-8")
            if ref_fur is not None and ref_fur.is_file():
                shutil.copyfile(ref_fur, artifact_dir / "ref.fur")
            if rust_fur is not None and rust_fur.is_file():
                shutil.copyfile(rust_fur, artifact_dir / "rust.fur")
            _write_note(
                artifact_dir, case, source, ref_error, rust_error,
                byte_offset, byte_lengths, report_line,
            )
            detail = []
            if ref_error:
                detail.append(f"reference: {ref_error.splitlines()[0]}")
            if rust_error:
                detail.append(f"rust: {rust_error.splitlines()[0]}")
            if byte_offset is not None:
                detail.append(f"bytes differ at offset {byte_offset} (0x{byte_offset:x})")
            if report_line is not None:
                detail.append(f"report differs at line {report_line}")
            print(f"FAIL {case}: " + "; ".join(detail))

        return {
            "case": case,
            "byte_mismatch": byte_mismatch,
            "report_mismatch": report_mismatch,
            "artifact": str(artifact_dir) if (byte_mismatch or report_mismatch) else None,
        }


def _stage_zero(samples_root: Path) -> int:
    try:
        version = _reference_version()
    except (OSError, subprocess.SubprocessError, RuntimeError) as exc:
        print(f"ERROR: cannot read the reference version: {exc}", file=sys.stderr)
        return 1
    files = _iter_corpus(samples_root)
    print(f"reference version: {version}")
    print(f"sample corpus: {len(files)} files under {samples_root}")
    if not samples_root.is_dir():
        print(f"WARNING: sample root {samples_root} does not exist", file=sys.stderr)
    return 0


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        prog="parity.py",
        description="Compare reference vgm2151fur output with the Rust port.",
    )
    parser.add_argument(
        "--stage", type=int, choices=range(0, 12), default=None,
        help="stage 0 prints the reference version and corpus count; "
             "1..11 run the cases registered for that stage",
    )
    parser.add_argument(
        "--synth", choices=("note_song", "staircase", "climb", "raw_vgm"),
        default=None, help="write a synthetic VGM and compare it",
    )
    parser.add_argument(
        "--pack", action="append", default=[], metavar="NAME",
        help="corpus pack folder to run (repeatable)",
    )
    parser.add_argument(
        "--samples", default=str(DEFAULT_SAMPLES), metavar="ROOT",
        help=f"sample corpus root (default: {DEFAULT_SAMPLES})",
    )
    parser.add_argument(
        "--full", action="store_true",
        help="run every VGM/VGZ under --samples",
    )
    parser.add_argument(
        "--rust-bin", default=None,
        help=f"Rust binary (default: {DEFAULT_RUST_BIN})",
    )
    return parser


def main(argv: list[str] | None = None) -> int:
    _reconfigure_stdio()
    args = build_parser().parse_args(argv)
    samples_root = Path(args.samples)

    if args.stage == 0:
        return _stage_zero(samples_root)

    rust_bin = _rust_binary(args.rust_bin)
    if not rust_bin.is_file():
        print(
            f"ERROR: Rust binary not found at {rust_bin}; run `cargo build` first",
            file=sys.stderr,
        )
        return 2

    cases: list[tuple[str, Path, list[str]]] = []
    synth_dir: tempfile.TemporaryDirectory | None = None
    if args.synth is not None:
        synth_dir = tempfile.TemporaryDirectory(prefix="vgm2fur_synth_")
        try:
            synth_path = _write_synth(args.synth, Path(synth_dir.name))
        except (OSError, ValueError, ImportError) as exc:
            print(f"ERROR: cannot write synth {args.synth}: {exc}", file=sys.stderr)
            synth_dir.cleanup()
            return 2
        cases.append((f"synth-{args.synth}", synth_path, []))

    if args.stage is not None:
        cases.extend(_registered_cases(args.stage, samples_root))

    if args.full:
        selected = _iter_corpus(samples_root)
    else:
        selected = _iter_corpus(samples_root)
        if args.pack:
            selected = [
                path for path in selected
                if any(
                    _pack_matches(path.relative_to(samples_root), pack)
                    for pack in args.pack
                )
            ]
            missing = [
                pack for pack in args.pack
                if not any(
                    _pack_matches(path.relative_to(samples_root), pack)
                    for path in _iter_corpus(samples_root)
                )
            ]
            for pack in missing:
                print(f"WARNING: no corpus files for pack {pack!r}", file=sys.stderr)
        else:
            selected = []
    cases.extend(_corpus_cases(selected, samples_root))

    if not cases:
        print("no cases selected", file=sys.stderr)

    compared = ok = byte_mismatch = report_mismatch = 0
    failures = 0
    try:
        for case, source, extra in cases:
            if not source.is_file():
                print(f"SKIP {case}: missing {source}", file=sys.stderr)
                continue
            compared += 1
            result = _run_case(case, source, extra, rust_bin)
            if result["byte_mismatch"]:
                byte_mismatch += 1
            if result["report_mismatch"]:
                report_mismatch += 1
            if not result["byte_mismatch"] and not result["report_mismatch"]:
                ok += 1
            else:
                failures += 1
    finally:
        if synth_dir is not None:
            synth_dir.cleanup()

    print(
        f"compared={compared} ok={ok} byte_mismatch={byte_mismatch} "
        f"report_mismatch={report_mismatch}"
    )
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())
