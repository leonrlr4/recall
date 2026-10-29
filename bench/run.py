#!/usr/bin/env python3
"""Transcription bench for recall.

Runs `recall transcribe-file --mic <mic.ogg> --computer <computer.ogg> --json`
on every case under bench/fixtures, scores the transcript against the case's
truth.json and prints a table. Standard library only.

Metrics per case:
  found   fraction of truth tokens present in a transcript line that overlaps
          the truth line widened by WINDOW_MS on each side (any side); each
          transcript token matches at most one truth token: first on the
          truth line's side, then on either side, nearest in time first
  side    of the found tokens, the fraction also present in such a line on
          the truth line's side ("me" or "them")
  leaked  transcript lines on "me" that match a "them" truth line in the
          same window and no "me" truth line there (the remote voice echoing
          into the microphone)
  lines   number of transcript lines
  mer     mixed error rate: the edit distance (substitutions, deletions,
          insertions) from all truth tokens to all transcript tokens, each
          in time order and ignoring side, over the number of truth tokens

Chinese is scored per character, everything else per lowercase word, with
punctuation stripped.

Each run gets its own config: the user's config file (RECALL_CONFIG or
~/.config/recall/config.toml) with an empty speaker registry and a
[glossary] of only the case's glossary.txt (one term per line) when
--glossary is given, and no glossary otherwise, so the user's notes and
named voices never change a score.
"""

import argparse
import json
import os
import re
import subprocess
import sys
import tempfile
import time
import tomllib
from collections import Counter
from pathlib import Path

BENCH = Path(__file__).resolve().parent
REPO = BENCH.parent
WINDOW_MS = 2000
# A transcript line matches a truth line when at least this fraction of its
# tokens belong to it; see `leaked`.
LEAK_OVERLAP = 0.5
METRICS = ("found", "side", "leaked", "lines", "mer")

_CJK = re.compile(r"([\u3400-\u4dbf\u4e00-\u9fff\uf900-\ufaff\U00020000-\U0002fa1f])")
# Words, keeping inner apostrophes ("it's").
_WORD = re.compile(r"[^\W_]+(?:'[^\W_]+)*")


def tokenize(text):
    """Lowercase tokens: one per CJK character, one per other word."""
    return _WORD.findall(_CJK.sub(r" \1 ", text.lower()))


def edit_distance(a, b):
    """Levenshtein distance between token lists `a` and `b`."""
    prev = list(range(len(b) + 1))
    for i, x in enumerate(a, 1):
        cur = [i]
        for j, y in enumerate(b, 1):
            cur.append(min(prev[j] + 1, cur[j - 1] + 1, prev[j - 1] + (x != y)))
        prev = cur
    return prev[-1]


def mer(truth_lines, transcript):
    """Mixed error rate of `transcript` against `truth_lines`, both read
    in time order whatever their side; None without truth tokens."""
    def ordered(lines):
        key = lambda t: (t["start_ms"], t["end_ms"], t["side"], t["text"])
        return [tok for t in sorted(lines, key=key) for tok in tokenize(t["text"])]

    ref = ordered(truth_lines)
    if not ref:
        return None
    return edit_distance(ref, ordered(transcript)) / len(ref)


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
    lines = truth.get("lines", [])
    # Per truth line: its tokens, and the transcript lines in its window,
    # nearest midpoint first (ties broken by content, so the transcript's
    # order does not matter).
    plans = []
    for tl in lines:
        lo, hi = tl["start_ms"] - WINDOW_MS, tl["end_ms"] + WINDOW_MS
        mid = (tl["start_ms"] + tl["end_ms"]) / 2
        near = sorted(
            (i for i, t in enumerate(transcript) if _overlaps(t, lo, hi)),
            key=lambda i: (abs((transcript[i]["start_ms"] + transcript[i]["end_ms"]) / 2 - mid),
                           transcript[i]["start_ms"], transcript[i]["end_ms"],
                           transcript[i]["side"], transcript[i]["text"]),
        )
        plans.append((tl, tokenize(tl["text"]), near))

    def consume(tok, candidates):
        hit = next((i for i in candidates if unused[i][tok] > 0), None)
        if hit is not None:
            unused[hit][tok] -= 1
        return hit is not None

    # Pass 1 matches on the truth line's side only, across all lines, so a
    # wrong-side truth line cannot take the token a right-side line needs.
    # Pass 2 matches what is left against either side.
    total = found = right_side = 0
    leftover = []
    for tl, toks, near in plans:
        same = [i for i in near if transcript[i]["side"] == tl["side"]]
        for tok in toks:
            total += 1
            if consume(tok, same):
                found += 1
                right_side += 1
            else:
                leftover.append((tok, near))
    for tok, near in leftover:
        if consume(tok, near):
            found += 1

    windows = [
        (tl["side"], tl["start_ms"] - WINDOW_MS, tl["end_ms"] + WINDOW_MS, set(toks))
        for tl, toks, _ in plans
    ]

    def matches(t, toks, side):
        return any(
            wside == side and _overlaps(t, lo, hi) and len(toks & ttoks) / len(toks) >= LEAK_OVERLAP
            for wside, lo, hi, ttoks in windows
        )

    # A "me" line that also matches a "me" truth line nearby is the speaker's
    # own words (both sides said the same thing), not an echo.
    leaked = sum(
        1
        for t, toks in tokens
        if t["side"] == "me" and toks and matches(t, toks, "them") and not matches(t, toks, "me")
    )

    return {
        "found": found / total if total else None,
        "side": right_side / found if found else None,
        "leaked": leaked,
        "lines": len(transcript),
        "mer": mer(lines, transcript),
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
    def is_time(v):
        return isinstance(v, (int, float)) and not isinstance(v, bool)

    ok = all(
        isinstance(line, dict)
        and isinstance(line.get("text"), str)
        and line.get("side") in ("me", "them")
        and is_time(line.get("start_ms"))
        and is_time(line.get("end_ms"))
        for line in data
    )
    return data if ok else None


def _toml_value(v):
    if isinstance(v, bool):
        return "true" if v else "false"
    if isinstance(v, (int, float)):
        return repr(v)
    if isinstance(v, str):
        return json.dumps(v, ensure_ascii=False)
    if isinstance(v, list):
        return "[" + ", ".join(_toml_value(x) for x in v) + "]"
    raise TypeError(f"cannot write {v!r} as TOML")


def to_toml(config):
    """`config` (scalars, lists and one level of tables) as TOML text."""
    out = [f"{k} = {_toml_value(v)}" for k, v in config.items() if not isinstance(v, dict)]
    for name, table in config.items():
        if isinstance(table, dict):
            out.append(f"\n[{name}]")
            out += [f"{k} = {_toml_value(v)}" for k, v in table.items()]
    return "\n".join(out) + "\n"


def case_config(base_path, terms, tmp):
    """Writes the config for one run into the directory `tmp` and returns
    its path: the config at `base_path` (if any) with `data_dir` moved into
    `tmp`, its models linked from the real one, and the glossary set to
    `terms` alone (None turns it off)."""
    base = {}
    if base_path.is_file():
        base = tomllib.loads(base_path.read_text(encoding="utf-8"))
    home = os.environ.get("RECALL_HOME") or str(Path.home() / ".local/share/recall")
    models = Path(base.get("data_dir", home)).expanduser() / "models"
    data = Path(tmp) / "data"
    data.mkdir()
    (data / "models").symlink_to(models, target_is_directory=True)
    base["data_dir"] = str(data)
    base["glossary"] = {"terms": list(terms or []), "sources": [], "max_terms": 30 if terms else 0}
    path = Path(tmp) / "config.toml"
    path.write_text(to_toml(base), encoding="utf-8")
    return path


def read_glossary(case_dir):
    """The terms in a case's glossary.txt, one per line; [] without one."""
    path = Path(case_dir) / "glossary.txt"
    if not path.is_file():
        return []
    lines = path.read_text(encoding="utf-8").splitlines()
    return [t.strip() for t in lines if t.strip() and not t.lstrip().startswith("#")]


def run_case(case_dir, binary, glossary=False, base_config=None):
    """Runs and scores one case. Status is "ok", "skipped" or "error".

    With `glossary`, the case's glossary.txt is the run's glossary.
    `base_config` is the user's config file the run's config starts from.
    """
    case_dir = Path(case_dir)
    result = {"case": case_dir.name}
    truth = json.loads((case_dir / "truth.json").read_text(encoding="utf-8"))
    mic, computer = case_dir / "mic.ogg", case_dir / "computer.ogg"
    if truth.get("skip"):
        return {**result, "status": "skipped", "reason": "marked skip"}
    if not (mic.is_file() and computer.is_file()):
        return {**result, "status": "skipped", "reason": "audio missing"}

    cmd = [str(binary), "transcribe-file", "--mic", str(mic), "--computer", str(computer), "--json"]
    if base_config is None:
        base_config = Path(os.environ.get("RECALL_CONFIG") or Path.home() / ".config/recall/config.toml")
    terms = read_glossary(case_dir) if glossary else []
    if glossary:
        result["glossary"] = terms
    started = time.monotonic()
    with tempfile.TemporaryDirectory() as tmp:
        env = {**os.environ, "RECALL_CONFIG": str(case_config(base_config, terms, tmp))}
        try:
            proc = subprocess.run(cmd, capture_output=True, text=True, env=env)
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
    parser.add_argument("--glossary", action="store_true",
                        help="give each case its glossary.txt as the glossary (default: none)")
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
        r = run_case(d, args.bin, glossary=args.glossary)
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
