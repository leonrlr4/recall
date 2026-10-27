"""Tests for bench/run.py. Run with `python3 -m unittest bench/test_run.py`."""

import json
import os
import stat
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

import run  # noqa: E402


def line(start, end, side, text, speaker="S1"):
    return {"start_ms": start, "end_ms": end, "side": side, "speaker": speaker, "text": text}


def truth_line(start, end, side, text):
    return {"start_ms": start, "end_ms": end, "side": side, "text": text}


class TokenizeTest(unittest.TestCase):
    def test_chinese_per_character_english_per_word(self):
        self.assertEqual(
            run.tokenize("這段 Export 會 timeout，對吧?"),
            ["這", "段", "export", "會", "timeout", "對", "吧"],
        )

    def test_cjk_next_to_latin_splits(self):
        self.assertEqual(run.tokenize("export超時OK"), ["export", "超", "時", "ok"])

    def test_punctuation_and_case_are_ignored(self):
        self.assertEqual(run.tokenize("Hello, WORLD! It's fine."), ["hello", "world", "it's", "fine"])


class ScoreTest(unittest.TestCase):
    def test_english_found_and_side(self):
        truth = {"lines": [
            truth_line(0, 2000, "me", "the deploy is done"),
            truth_line(3000, 5000, "them", "great thanks"),
        ]}
        transcript = [
            line(500, 2500, "me", "The deploy is done."),
            # Right words, wrong side.
            line(3000, 5000, "me", "great thanks"),
        ]
        s = run.score(truth, transcript)
        self.assertAlmostEqual(s["found"], 1.0)
        self.assertAlmostEqual(s["side"], 4 / 6)
        self.assertEqual(s["leaked"], 1)
        self.assertEqual(s["lines"], 2)

    def test_chinese_scored_per_character(self):
        truth = {"lines": [truth_line(0, 3000, "them", "這段會超時")]}
        transcript = [line(0, 3000, "them", "這段會")]
        s = run.score(truth, transcript)
        self.assertAlmostEqual(s["found"], 3 / 5)
        self.assertAlmostEqual(s["side"], 1.0)
        self.assertEqual(s["leaked"], 0)

    def test_words_outside_two_second_window_are_not_found(self):
        truth = {"lines": [truth_line(0, 1000, "me", "hello there")]}
        transcript = [line(3100, 4000, "me", "hello there")]
        s = run.score(truth, transcript)
        self.assertEqual(s["found"], 0.0)
        self.assertIsNone(s["side"])

    def test_empty_truth_counts_lines_only(self):
        s = run.score({"lines": []}, [line(0, 1000, "me", "uh")])
        self.assertIsNone(s["found"])
        self.assertIsNone(s["side"])
        self.assertEqual(s["leaked"], 0)
        self.assertEqual(s["lines"], 1)


class CheckTest(unittest.TestCase):
    def test_max_and_min_thresholds(self):
        limits = {"lines": {"max": 0}, "found": {"min": 0.8}}
        self.assertEqual(run.check({"lines": 0, "found": 0.9}, limits), [])
        failures = run.check({"lines": 2, "found": 0.5}, limits)
        self.assertEqual(len(failures), 2)
        self.assertIn("lines", failures[0])
        self.assertIn("found", failures[1])

    def test_missing_metric_fails(self):
        self.assertEqual(len(run.check({"found": None}, {"found": {"min": 0.8}})), 1)


class RunCaseTest(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.root = Path(self.tmp.name)
        self.fixtures = self.root / "fixtures"
        self.bin = self.root / "recall"
        self.bin.write_text("#!/bin/sh\necho '[]'\n")
        self.bin.chmod(self.bin.stat().st_mode | stat.S_IXUSR)

    def tearDown(self):
        self.tmp.cleanup()

    def make_case(self, name, truth, audio=True):
        d = self.fixtures / name
        d.mkdir(parents=True)
        (d / "truth.json").write_text(json.dumps(truth))
        if audio:
            (d / "mic.ogg").write_bytes(b"")
            (d / "computer.ogg").write_bytes(b"")
        return d

    def test_skip_flag_and_missing_audio_skip(self):
        flagged = self.make_case("flagged", {"skip": True, "lines": []})
        silent = self.make_case("noaudio", {"lines": []}, audio=False)
        for d in (flagged, silent):
            result = run.run_case(d, self.bin)
            self.assertEqual(result["status"], "skipped")

    def test_runs_binary_and_scores(self):
        d = self.make_case("silence", {"lines": []})
        result = run.run_case(d, self.bin)
        self.assertEqual(result["status"], "ok")
        self.assertEqual(result["lines"], 0)

    def test_main_check_exit_codes(self):
        self.make_case("silence", {"lines": []})
        self.make_case("todo", {"skip": True, "lines": []})
        thresholds = self.root / "thresholds.json"
        thresholds.write_text(json.dumps({"silence": {"lines": {"max": 0}}, "todo": {"found": {"min": 1}}}))
        out = self.root / "out.json"
        args = ["--bin", str(self.bin), "--fixtures", str(self.fixtures),
                "--thresholds", str(thresholds), "--check", "--json", str(out)]
        with open(os.devnull, "w") as devnull:
            stdout, sys.stdout = sys.stdout, devnull
            try:
                self.assertEqual(run.main(args), 0)
                thresholds.write_text(json.dumps({"silence": {"lines": {"max": -1}}}))
                self.assertEqual(run.main(args), 1)
            finally:
                sys.stdout = stdout
        results = json.loads(out.read_text())
        self.assertEqual({r["case"]: r["status"] for r in results}, {"silence": "ok", "todo": "skipped"})


if __name__ == "__main__":
    unittest.main()
