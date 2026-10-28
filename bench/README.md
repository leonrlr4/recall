# Transcription bench

`run.py` feeds each case's two tracks to the hidden `recall transcribe-file`
subcommand and scores the transcript against the case's `truth.json`.

```sh
cargo build --release -p recall
python3 bench/run.py                  # every case, prints a table
python3 bench/run.py --case silence   # one case
python3 bench/run.py --check          # exit 1 on an error or a missed threshold
python3 bench/run.py --json out.json  # results with the transcripts
python3 bench/run.py --bin ~/.local/bin/recall
python3 -m unittest bench/test_run.py # tests for the scoring
```

## Metrics

| Metric | Meaning |
|---|---|
| `found` | Fraction of truth tokens that appear in a transcript line within 2 s of the truth line, on either side. Each transcript token matches at most one truth token, so a repeated 的 needs as many 的 in the transcript. Matching tries the truth line's side first, then either side, nearest in time first |
| `side` | Of the found tokens, the fraction that appear on the truth line's side (`me` or `them`) |
| `leaked` | Transcript lines on `me` whose tokens mostly (at least half) belong to a `them` truth line in the same window, and not to a `me` truth line there: the remote voice echoing into the microphone |
| `lines` | Number of transcript lines |

Chinese counts one token per character, other text one per lowercase word,
punctuation stripped. `thresholds.json` sets a `min` or `max` per metric per
case; `--check` fails a case that misses one.

## Cases

Each case is a directory under `fixtures/` with:

- `mic.ogg`: what the microphone heard (the `me` side)
- `computer.ogg`: what the system played (the `them` side)
- `truth.json`: `{"lines": [{"start_ms", "end_ms", "side", "text"}]}`, with
  `"skip": true` to leave the case out

A case marked `skip`, or missing either audio file, is reported as skipped.

- `silence`: 20 s of white noise at about -60 dBFS on both tracks. Nothing
  should be transcribed (`lines` max 0). Generated with
  `ffmpeg -f lavfi -i "anoisesrc=d=20:c=white:r=48000:a=0.002:s=1" -c:a libopus -b:a 24k mic.ogg`
  (and `s=2` for `computer.ogg`).

- `zh-en`: five Mandarin-English code-switched utterances from the CSZS
  test split (files 2650, 953, 363, 20 and 92), 49.7 s with one second of
  silence between them, on `computer.ogg` only; `mic.ogg` is digital
  silence of the same length. English words sit inside Mandarin sentences
  ("overall失效的possibility可以透過system設計的提升來降低"). `truth.json` holds the
  dataset's references converted to Traditional Chinese with OpenCC `s2tw`,
  all on `them`. The case measures how well code-switched speech is
  recognised (`found`). With nothing on the microphone it does not test
  separating two speakers or echo into the mic, so `side` and `leaked` only
  catch remote speech landing on the wrong side. Source and licence (MIT)
  are in `NOTICE`.

The other cases have no audio yet. To record one, two people hold a call:
person A reads the `me` lines into the microphone while their side captures
the mic into `mic.ogg`; person B reads the `them` lines from the other end
while person A's side captures the system output into `computer.ogg`. Both
files start at the same moment. Write each spoken line into `truth.json` with
its side and times, then remove `"skip": true` and add thresholds.

- `zh`: 60 s of Taiwanese Mandarin only, alternating between A and B.
- `en-tw`: 60 s of English spoken by Taiwanese speakers, alternating
  between A and B.

On PipeWire, `pw-record mic.wav` captures the default microphone and
`pw-record -P '{ stream.capture.sink = true }' computer.wav` the default output;
convert each with `ffmpeg -i mic.wav -c:a libopus mic.ogg`.
