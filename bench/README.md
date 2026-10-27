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
| `found` | Fraction of truth tokens that appear in a transcript line within 2 s of the truth line, on either side. Each transcript token matches at most one truth token, so a repeated 的 needs as many 的 in the transcript |
| `side` | Of the found tokens, the fraction that appear on the truth line's side (`me` or `them`) |
| `leaked` | Transcript lines on `me` whose tokens mostly (at least half) belong to a `them` truth line in the same window: the remote voice echoing into the microphone |
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

The other cases have no audio yet. To record one, two people hold a call:
person A reads the `me` lines into the microphone while their side captures
the mic into `mic.ogg`; person B reads the `them` lines from the other end
while person A's side captures the system output into `computer.ogg`. Both
files start at the same moment. Write each spoken line into `truth.json` with
its side and times, then remove `"skip": true` and add thresholds.

- `zh-en`: 60 s of Taiwanese Mandarin with English words mid-sentence
  ("這段 export 會 timeout"), alternating between A and B.
- `zh`: 60 s of Taiwanese Mandarin only, alternating between A and B.
- `en-tw`: 60 s of English spoken by Taiwanese speakers, alternating
  between A and B.

On PipeWire, `pw-record mic.wav` captures the default microphone and
`pw-record -P '{ stream.capture.sink = true }' computer.wav` the default output;
convert each with `ffmpeg -i mic.wav -c:a libopus mic.ogg`.
