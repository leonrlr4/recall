#!/usr/bin/env python3
"""Transcription bench for recall.

Runs `recall transcribe-file --mic <mic.ogg> --computer <computer.ogg> --json`
on every case under bench/fixtures, scores the transcript against the case's
truth.json and prints a table. Standard library only.

Metrics per case:
  found   fraction of truth tokens present in a transcript line that overlaps
          the truth line widened by WINDOW_MS on each side (any side); each
          transcript token matches at most one truth token
  side    of the found tokens, the fraction also present in such a line on
          the truth line's side ("me" or "them")
  leaked  transcript lines on "me" that match a "them" truth line in the
          same window (the remote voice echoing into the microphone)
  lines   number of transcript lines

Chinese is scored per character, everything else per lowercase word, with
punctuation stripped.
"""

import argparse
import json
import re
import subprocess
import sys
import time
from collections import Counter
from pathlib import Path

BENCH = Path(__file__).resolve().parent
REPO = BENCH.parent
WINDOW_MS = 2000
# A "me" line leaks when at least this fraction of its tokens belong to one
# "them" truth line in the window.
LEAK_OVERLAP = 0.5
METRICS = ("found", "side", "leaked", "lines")

_CJK = re.compile(r"([\u3400-\u4dbf\u4e00-\u9fff\uf900-\ufaff\U00020000-\U0002fa1f])")
# Words, keeping inner apostrophes ("it's").
_WORD = re.compile(r"[^\W_]+(?:'[^\W_]+)*")


def tokenize(text):
    """Lowercase tokens: one per CJK character, one per other word."""
    return _WORD.findall(_CJK.sub(r" \1 ", text.lower()))


def _overlaps(a, lo, hi):
    return a["start_ms"] <= hi and a["end_ms"] >= lo


def score(truth, transcript):
    """Scores `transcript` (a list of transcribe-file lines) against `truth`.

    `found` and `side` are None when there is nothing to divide by.
    """
    tokens = [(t, set(tokenize(t["text"]))) for t in transcript]
    # Tokens not yet matched to a truth token, per transcript line, so a
    # repeated truth token (的 的 的) needs as many occurrences to be found.
    unused = [Counter(tokenize(t["text"])) for t in transcript]
    total = found = right_side = 0
    them_truth = []
    for tl in truth.get("lines", []):
        lo, hi = tl["start_ms"] - WINDOW_MS, tl["end_ms"] + WINDOW_MS
        near = [i for i, t in enumerate(transcript) if _overlaps(t, lo, hi)]
        # Lines on the truth line's side are consumed first.
        near.sort(key=lambda i: transcript[i]["side"] != tl["side"])
        for tok in tokenize(tl["text"]):
            total += 1
            hit = next((i for i in near if unused[i][tok] > 0), None)
            if hit is None:
                continue
            unused[hit][tok] -= 1
            found += 1
            if transcript[hit]["side"] == tl["side"]:
                right_side += 1
        if tl["side"] == "them":
            them_truth.append((lo, hi, set(tokenize(tl["text"]))))

    leaked = 0
    for t, toks in tokens:
        if t["side"] != "me" or not toks:
            continue
        if any(
            _overlaps(t, lo, hi) and len(toks & ttoks) / len(toks) >= LEAK_OVERLAP
            for lo, hi, ttoks in them_truth
        ):
            leaked += 1

    return {
        "found": found / total if total else None,
        "side": right_side / found if found else None,
        "leaked": leaked,
        "lines": len(transcript),
    }


def check(result, limits):
    """Returns one message per metric in `limits` that `result` misses.

    `limits` maps a metric to {"min": x} and/or {"max": y}.
    """
    failures = []
    for metric, bounds in limits.items():
        value = result.get(metric)
        if value is None:
            failures.append(f"{metric}: no value")
            continue
        if "min" in bounds and value < bounds["min"]:
            failures.append(f"{metric}: {value:.3g} < min {bounds['min']}")
        if "max" in bounds and value > bounds["max"]:
            failures.append(f"{metric}: {value:.3g} > max {bounds['max']}")
    return failures


LINE_KEYS = ("start_ms", "end_ms", "side", "text")


def parse_transcript(stdout):
    """The lines in transcribe-file's output, which is either a JSON array of
    lines or {"segments": [...]}; None for anything else."""
    try:
        data = json.loads(stdout)
    except json.JSONDecodeError:
        return None
    if isinstance(data, dict):
        data = data.get("segments")
    if not isinstance(data, list):
        return None
    ok = all(
        isinstance(line, dict)
        and isinstance(line.get("text"), str)
        and line.get("side") in ("me", "them")
        and all(isinstance(line.get(k), (int, float)) for k in ("start_ms", "end_ms"))
        for line in data
    )
    return data if ok else None


def run_case(case_dir, binary):
    """Runs and scores one case. Status is "ok", "skipped" or "error"."""
    case_dir = Path(case_dir)
    result = {"case": case_dir.name}
    truth = json.loads((case_dir / "truth.json").read_text(encoding="utf-8"))
    mic, computer = case_dir / "mic.ogg", case_dir / "computer.ogg"
    if truth.get("skip"):
        return {**result, "status": "skipped", "reason": "marked skip"}
    if not (mic.is_file() and computer.is_file()):
        return {**result, "status": "skipped", "reason": "audio missing"}

    cmd = [str(binary), "transcribe-file", "--mic", str(mic), "--computer", str(computer), "--json"]
    started = time.monotonic()
    try:
        proc = subprocess.run(cmd, capture_output=True, text=True)
    except OSError as e:
        return {**result, "status": "error", "reason": str(e)}
    seconds = time.monotonic() - started
    if proc.returncode != 0:
        reason = proc.stderr.strip().splitlines()[-1:] or [f"exit {proc.returncode}"]
        return {**result, "status": "error", "reason": reason[0], "seconds": seconds}
    transcript = parse_transcript(proc.stdout)
    if transcript is None:
        return {**result, "status": "error", "reason": "bad JSON from transcribe-file", "seconds": seconds}
    return {**result, "status": "ok", "seconds": seconds, **score(truth, transcript), "transcript": transcript}


def _fmt(value):
    if value is None:
        return "-"
    if isinstance(value, float):
        return f"{value:.3f}"
    return str(value)


def print_table(results):
    header = ("case", "status", *METRICS, "seconds", "note")
    rows = [header]
    for r in results:
        note = r.get("reason", "") or "; ".join(r.get("failures", []))
        rows.append((r["case"], r["status"], *(_fmt(r.get(m)) for m in METRICS),
                     _fmt(r.get("seconds")), note))
    widths = [max(len(row[i]) for row in rows) for i in range(len(header) - 1)]
    for row in rows:
        print("  ".join(c.ljust(w) for c, w in zip(row, widths)) + "  " + row[-1])


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--bin", type=Path, default=REPO / "target/release/recall",
                        help="recall binary (default: target/release/recall)")
    parser.add_argument("--case", action="append", help="run only this case (repeatable)")
    parser.add_argument("--check", action="store_true",
                        help="exit 1 when a case errors or misses its thresholds")
    parser.add_argument("--json", type=Path, help="write results to this file")
    parser.add_argument("--fixtures", type=Path, default=BENCH / "fixtures", help=argparse.SUPPRESS)
    parser.add_argument("--thresholds", type=Path, default=BENCH / "thresholds.json",
                        help=argparse.SUPPRESS)
    args = parser.parse_args(argv)

    thresholds = json.loads(args.thresholds.read_text()) if args.thresholds.is_file() else {}
    cases = sorted(d for d in args.fixtures.iterdir() if (d / "truth.json").is_file())
    if args.case:
        unknown = set(args.case) - {d.name for d in cases}
        if unknown:
            parser.error(f"unknown case(s): {', '.join(sorted(unknown))}")
        cases = [d for d in cases if d.name in args.case]

    results = []
    failed = False
    for d in cases:
        r = run_case(d, args.bin)
        if r["status"] == "ok":
            r["failures"] = check(r, thresholds.get(d.name, {}))
        failed |= r["status"] == "error" or bool(r.get("failures"))
        results.append(r)

    print_table(results)
    if args.json:
        args.json.write_text(json.dumps(results, ensure_ascii=False, indent=2) + "\n")
    return 1 if args.check and failed else 0


if __name__ == "__main__":
    sys.exit(main())
