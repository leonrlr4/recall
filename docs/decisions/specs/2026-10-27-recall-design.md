# recall design

> Historical record, frozen 2026-10-27. Source code is authoritative; where
> this document and the code disagree, the code wins.

## Purpose

recall records every online meeting held on this computer. It detects when a
meeting starts and ends, records the microphone and the system output as two
tracks, optionally records the screen, and after the meeting turns the audio
into a transcript in Traditional Chinese and English (mixed within sentences),
tells speakers apart, remembers each person's voice, and produces a summary,
decisions and action items with a local or cloud LLM. Everything lands in a
searchable local web archive. Audio and text stay on the machine unless the
user explicitly picks the Claude API as the summary backend.

The first user is Leon. Meetings happen in Google Meet (in Brave), Slack
huddles and Discord. Speech is Taiwanese Mandarin mixed with English. The
performance goal is a single Rust binary with no Python at runtime.

### Success criteria

- No key press needed during a meeting. For a 60 minute meeting, the
  transcript and summary are on the web page within 10 minutes of the end.
- On code-switched sentences the error rate is below half of Whisper
  large-v3 (measured on the bench's ASCEND-style cases).
- "Me" and "them" are attributed with 100% accuracy. Remote speakers are
  matched to names automatically from their second meeting on.
- The daemon uses 0% CPU when idle and under 2% of one core while recording.
- Any failed step keeps the audio and can be rerun from that step.

### Out of scope

- Calendar integration (discussed, deferred).
- Cloud sync, multiple users.
- A Tauri shell. The web page comes first; a thin shell pointing at the local
  URL can be added later without changing the daemon or the frontend.

## Where this sits among existing projects

Surveyed open-source projects (Meetily, Screenpipe, Anarlog, Vibe, Minutes,
omarchy-meeting-recorder, mnemosyne) and commercial products (Granola, Otter,
Fathom, Feishu Minutes). None ships a code-switching Chinese/English STT engine
or Traditional Chinese output. None detects meetings from PipeWire on Linux.
Every Linux option is a single-meeting workbench, not an archive of all
meetings.

omarchy-meeting-recorder (GTK4, Rust) is the closest. Its recording and
transcription are done carefully and are worth copying: live meters before
recording, the echo rule, crash recovery, an editable transcript, a bench
suite. It lacks a meeting list, search, summaries, auto-detection and Chinese.
Decision: build from scratch, using it and Anarlog's `crates/audio-sync` as
reference code.

## Storage format

One directory per meeting, by default under `~/Meetings/`, named by start time
and title:

```
~/Meetings/2026-10-27 14:30 Biobuild weekly/
├── meeting.md      summary and transcript; Obsidian reads it directly
├── audio.opus      stereo 48 kHz, left = microphone (me), right = system output (them)
├── screen.mp4      only when screen recording was on
├── meeting.json    machine data: times, source app, speaker ids, segment timestamps, step states, model versions
└── .cache/         20 second chunks while recording; cleared when done
```

`meeting.md` opens with YAML frontmatter (title, date, duration, participants,
source_app, tags), then `## Summary`, `## Decisions`, `## Action items` (each
with owner and due date), `## Chapters`, `## Transcript`. Each transcript line
is `[01:32] **Tom** 這段 export 會 timeout`. Chinese is always Traditional;
English words stay as spoken.

Speakers are global entities:

```
~/.local/share/recall/
├── speakers.json   id → name, aliases, several voice embeddings
├── index.sqlite    FTS5 full-text index and list queries; rebuildable from the directories at any time
└── models/         Breeze ggml, sherpa models
```

`meeting.json` stores only speaker ids. A rename happens once in
`speakers.json`, and the daemon rewrites every affected `meeting.md`. SQLite is
only an index; the directories are the source of truth, so rsync, git and
Obsidian all work on them directly.

Config at `~/.config/recall/config.toml`: meetings directory, screen recording
on/off, detection allowlist, debounce seconds, LLM backend (`ollama` or
`claude`) and model name, summary language, UI language, STT engine.

## Daemon state machine and detection

`recall daemon` runs as a systemd user service: one process with four tasks,
the PipeWire watcher, the recorder, the processing queue and HTTP.

```
Idle ──allowlisted app opens the mic──▶ Armed ──survives 8 s──▶ Recording
Idle ──hotkey start───────────────────────────────────────────▶ Recording
Armed ──closed within 8 s (permission probe)──▶ Idle
Recording ──mic closed for 30 s or hotkey stop──▶ Finishing ──audio.opus written──▶ Processing ──▶ Idle
Recording ──hotkey discard──▶ Idle (cache deleted)
```

Detection rules:

- Trigger: a `Stream/Input/Audio` node appears in the PipeWire registry whose
  `application.process.binary` is on the allowlist (default: brave, chromium,
  firefox, zoom, slack, discord, teams). recall's own nodes are ignored.
- The cache already records during Armed, so the first 8 seconds are not lost.
- End requires the node to be gone for more than 30 seconds, so a remote
  reconnect does not split one meeting in two.
- Recordings shorter than 2 minutes are discarded automatically.
- Only one meeting records at a time; a second app opening the mic joins the
  current one.
- PulseAudio-only systems use `libpulse-binding` source_output subscriptions
  with the same rules.

Hotkeys `recall start` / `stop` / `discard` send commands over
`$XDG_RUNTIME_DIR/recall/control.sock`, so any desktop can bind them.

While recording:

- Two `pipewire` crate streams: the mic captures the default source; system
  audio targets the default sink with `stream.capture.sink = true`. Both ask
  for F32 at 48 kHz and let PipeWire resample.
- Follows the `default.audio.sink` metadata, so switching to a headset keeps
  working. A track that briefly delivers nothing gets silence, keeping the two
  tracks aligned.
- Writes a chunk to `.cache/` every 20 seconds, so a crash loses at most 20
  seconds. On the next start an unfinished recording is processed.
- With screen recording on, spawns `gpu-screen-recorder -w portal` using a
  restore token to avoid the dialog. When the token fails, a notification asks
  for one click; audio keeps recording regardless.
- Desktop notifications: "Recording: Google Meet" on start, "Processing" on
  end, "Summary ready" with the web link on completion. State is also written
  to `$XDG_RUNTIME_DIR/recall/state.json` for bars.

The processing queue lives in SQLite, runs one job at a time and resumes after
a daemon restart.

## Processing pipeline

Six steps in order. Each writes its output to `meeting.json`; a failure stops
at that step, and the web page and CLI can rerun from it.

1. **Segment**: Silero VAD (sherpa-onnx) on each channel; only voiced regions
   go to transcription. Echo rule: a left-channel region that is voiced only
   while the right channel is voiced is echo and is dropped.
2. **Transcribe**: `whisper-rs` (Vulkan) with Breeze-ASR-25 ggml q8_0,
   channels transcribed separately, language hint `zh`, then OpenCC `s2tw`.
   Word timestamps from DTW. The engine is a trait; the second implementation
   is sherpa-onnx with TEA-ASR-1.1, to become the default only after a spike
   confirms the export works and is fast enough.
3. **Diarize**: right channel only, with sherpa-onnx's pyannote
   segmentation-3.0 plus CAM++ embeddings. The left channel is always "me".
   Output is Remote N plus one embedding per cluster.
4. **Name speakers**, in order: voiceprint match against `speakers.json`
   (cosine threshold starts at 0.6, calibrated on real recordings; several
   embeddings per person, added only on high-confidence matches); majority
   vote over active-speaker events from a Meet browser extension (not built in
   the first version, but the format reserves the field); otherwise Remote N
   stays, and renaming on the web page stores the embedding under that person.
5. **Summarize**: the speaker-labelled transcript goes to the LLM, which
   returns fixed JSON: one-line summary, summary paragraph, decisions, action
   items (item, owner, due), chapters (start, title). The prompt demands
   Traditional Chinese with English terms and names kept as is. Transcripts
   over the context window are summarized per chapter and merged. Backends:
   `ollama` (default `qwen3.6:35b-a3b`) or `claude` (API key from the
   environment).
6. **Write**: generate `meeting.md`, update the index, notify.

Performance targets (60 minute meeting, Intel Arc B390): transcription under
5 minutes, diarization under 2, local summary under 2.

Bench: `bench/fixtures/` holds each case's audio and `truth.json`;
`thresholds.json` sets the minimums; CI runs it. Cases copy
omarchy-meeting-recorder's ten and add three: pure Mandarin, code-switched
Mandarin/English, Taiwanese-accented English.

## Web page and API

`axum` on `127.0.0.1:7474`, static files embedded with `rust-embed`. Frontend
is Svelte 5 + Vite; `web/dist/` is committed so `cargo build` needs no Node.

UI strings live in `web/src/i18n/{zh-TW,en}.json`, follow the browser locale,
and can be pinned in settings. Summary language is a separate setting.

Pages:

1. `/` meeting list: search box, filters (source app, participant, date);
   cards grouped by date showing title, duration, participant initials,
   one-line summary, processing state.
2. `/m/:id` meeting page, three columns: left summary, decisions, action items,
   chapters; middle transcript with coloured speakers, click a line to play
   from it, hover for edit, next speaker, delete; right top player (waveform
   with me above and them below, chapters as ticks, replaced by the video when
   present), right bottom "ask this meeting".
3. `/people` speakers: name, aliases, meetings they appear in, number of voice
   samples; renaming applies everywhere immediately; two people can be merged.
4. `/settings` form view of the config, plus "test recording": record 5
   seconds and show both channels' waveforms.

API (JSON, bound to localhost only):

| Path | Purpose |
|---|---|
| `GET /api/meetings?q=&app=&from=` | list and full-text search |
| `GET/PATCH /api/meetings/:id` | read, rename, edit segments |
| `POST /api/meetings/:id/rerun/:step` | rerun from a step |
| `GET /api/meetings/:id/audio` | Range streaming |
| `POST /api/meetings/:id/ask` | Q&A, SSE |
| `GET/PATCH /api/people/:id` | rename, merge speakers |
| `GET /api/events` | SSE: recording state, processing progress |

The CLI uses the same API: `recall list`, `recall show <id>`,
`recall rerun <id> summary`.

Search uses FTS5 with the `trigram` tokenizer, so Chinese needs no word
segmentation. Results carry the matching line and timestamp; clicking jumps
to playback.

Keyboard: list `/` search, `j k` move, `Enter` open; meeting page `space`
play/pause, `[ ]` previous/next line, `e` edit current line, `s` next
speaker.

## Project layout

Cargo workspace:

```
recall/
├── crates/
│   ├── recall-audio      PipeWire detection, two-track capture, Opus writing
│   ├── recall-pipeline   VAD, transcription, diarization, naming, summary
│   ├── recall-store      meeting directories, speakers.json, SQLite index
│   ├── recall-llm        Ollama / Claude behind one trait
│   ├── recall-server     axum API, SSE, static files
│   └── recall            binary: daemon, CLI, systemd integration
├── web/                  Svelte; dist/ is committed
├── bench/
└── docs/decisions/
```

sherpa-onnx and whisper-rs each bring their own runtime (ONNX Runtime, ggml)
and do not collide. Do not add a second crate that vendors ONNX Runtime (such
as pyannote-rs): Minutes had to move sherpa into a dlopen'ed cdylib for that
reason.

## Testing

- Unit: store format round-trips, pure pipeline functions (echo rule,
  timeline merge, name voting), LLM JSON parsing, i18n key parity.
- Integration: the bench audio through the whole pipeline, without PipeWire.
- Recording: `recall selftest` plays a tone to the sink while capturing the
  monitor; CI uses a PipeWire null sink.
- Detection: fake capture nodes created with `pw-cli` drive the state machine.

## Phases

| Phase | Deliverable |
|---|---|
| 1 | daemon detection, two-track recording, hotkeys, notifications; produces audio.opus |
| 2 | transcription, me/them by track, meeting.md, list and meeting pages, zh-TW/en UI |
| 3 | diarization, voiceprints, people page, summary and action items, Q&A |
| 4 | screen recording, Meet extension, MCP server, TEA-ASR spike |
| 5 | live captions (sherpa-onnx streaming X-ASR, re-transcribed with the phase 2 engine afterwards) |

Each phase is usable on its own. After phase 1 audio accumulates, and later
model changes can reprocess everything.

## Known risks

- Diarization DER on overlapping Mandarin speech is around 12 to 20%, and
  similar voices get merged; the web page's next-speaker and merge actions are
  the fix.
- Browser WebRTC permission probes open and close the mic; the 8 second
  debounce handles that. Other mic uses in Brave (voice input) will be
  recorded by mistake; the 2 minute minimum and the discard hotkey handle that.
- xdph restore tokens may come back empty; the first screen recording may need
  one click.
- The `pipewire` crate is pre-1.0 (0.10) and its API changes.
- Breeze uses the large-v2 decoder and is slower than turbo; if the B390
  misses the target, switch to TEA-ASR or SenseVoice.
- Recording other people's voices and voiceprints needs their consent; that is
  on the user.
