# recall

recall is a meeting recorder for Linux that notices when a call starts, records
your microphone and the call audio as two tracks, and afterwards writes a
transcript with named speakers and a summary into a folder per meeting. It is
built for Taiwanese Mandarin mixed with English (transcripts come out in
Traditional Chinese with English words kept as spoken), runs as one Rust
daemon with a local web archive, and keeps audio on your machine.

Status: phases 1 to 4 of the design are built: recording, transcription,
speakers, summaries, the web archive, screen recording, a Google Meet
extension that names remote speakers, and an MCP server. Live captions are
not built yet.

## Requirements

- Linux with PipeWire. Detection and capture use PipeWire directly; there is
  no PulseAudio-only mode.
- `ffmpeg` at run time (decoding recordings) and OpenCC with its data files
  (Simplified to Traditional conversion).
- To build: Rust 1.89 or newer, `clang`, `cmake`, `pkg-config` and the
  PipeWire headers. The first build downloads the prebuilt sherpa-onnx shared
  libraries; the web UI is prebuilt in `web/dist`, so Node is not needed.
- About 1.7 GB of disk for the models, downloaded once.
- For summaries, a running [Ollama](https://ollama.com) or an Anthropic API key.
- For screen recording (optional), `gpu-screen-recorder` and an
  xdg-desktop-portal backend with screen casting.
- For Meet speaker names (optional), Chrome, Chromium or Brave.

Arch Linux:

```sh
sudo pacman -S --needed pipewire ffmpeg opencc clang cmake pkgconf rustup
```

Debian and Ubuntu:

```sh
sudo apt install libpipewire-0.3-dev libspa-0.2-dev clang cmake pkg-config \
  libopencc-dev opencc ffmpeg
```

## Install

From a clone of this repository:

```sh
packaging/install.sh            # add --vulkan for GPU transcription, see below
```

The script is short; read it first, or run it with `--dry-run` to print every
step without doing anything. It builds `recall` in release mode and installs:

| Path | What |
|---|---|
| `~/.local/bin/recall` | the binary |
| `~/.local/lib/recall/` | `libsherpa-onnx-c-api.so` and `libonnxruntime*.so*`, which the binary finds through its `$ORIGIN/../lib/recall` rpath |
| `~/.config/systemd/user/recall.service` | the user service, running `recall daemon` |

and then runs `recall models download`. `packaging/install.sh --uninstall`
removes those three paths and leaves your recordings and
`~/.local/share/recall` alone.

## First run

```sh
systemctl --user enable --now recall
recall status
```

Open <http://127.0.0.1:7474>. Join a call in an allowlisted app and recording
starts by itself; a desktop notification says so, another says when the summary
is ready.

To record or stop by hand, bind the CLI commands to keys. Hyprland
(`~/.config/hypr/hyprland.conf`):

```
bind = SUPER ALT, R, exec, recall start
bind = SUPER ALT, S, exec, recall stop
bind = SUPER ALT, D, exec, recall discard
```

`stop` keeps the recording and queues it for processing; `discard` stops and
deletes it. Other desktops can bind the same commands. They talk to the daemon
over `$XDG_RUNTIME_DIR/recall/control.sock`. The daemon also writes its state
to `$XDG_RUNTIME_DIR/recall/state.json` for status bars.

Other commands:

```sh
recall list                       # meetings: id, duration, state, one line of summary
recall show <id>                  # print a meeting's meeting.md
recall rerun <id> summarize       # rerun from a step: segment, transcribe, diarize, name, summarize, write
recall index rebuild              # rebuild the search index from the meeting folders
recall selftest                   # record 5 s and show the level of both tracks
recall screen off                 # stop and delete this meeting's screen video
recall mcp                        # MCP server on stdin/stdout, see below
recall models download
```

## How detection works

recall watches PipeWire for applications that open the microphone.

- An app whose binary or application name matches the allowlist (`brave`,
  `chromium`, `chrome`, `firefox`, `zoom`, `slack`, `discord`, `teams` by
  default) opening the mic arms the recorder. Audio is cached from that moment.
- If the app keeps the mic open for 8 seconds, the recording starts, keeping
  those first 8 seconds. Browsers open and close the mic to probe permissions;
  those probes end within the 8 seconds and are dropped.
- The recording ends once every allowlisted app has closed the mic for 30
  seconds, so a reconnect does not split a meeting in two.
- Detected recordings shorter than 2 minutes are deleted. A recording started
  with `recall start` is kept whatever its length.
- If the capture cannot start, a notification says so and recall tries again
  every 10 seconds while the app keeps the mic open.
- A notification warns when the meetings folder has less than 2 GB free at
  the start of a recording. If the disk fills up, the recording stops and
  keeps what was written; saving it is retried every minute.
- One meeting records at a time; a second app joining the mic joins the
  current recording.
- Audio is written to disk every 20 seconds. After a crash the daemon finishes
  and processes the unfinished recording on its next start.

## What happens after a meeting

The recording goes through six steps, one meeting at a time:

1. `segment`: voice activity detection (Silero VAD) on each track. A
   stretch of microphone speech that falls almost entirely inside call speech
   is dropped as echo of the call.
2. `transcribe`: Breeze ASR 25 through whisper.cpp, then OpenCC to Traditional
   Chinese.
3. `diarize`: pyannote segmentation and CAM++ voice embeddings split the call
   side into speakers.
4. `name`: speakers are matched against the voices recall has seen before.
5. `summarize`: the LLM writes a summary, decisions, action items and chapters.
6. `write`: `meeting.md` is written and the search index updated.

A failed step keeps the audio; rerun it from the web page or with
`recall rerun <id> <step>`.

## Where files live

Each meeting is a folder under `~/Meetings/`, named by start time and title:

```
~/Meetings/2026-10-27 14:30 Weekly sync/
├── meeting.md         summary, decisions, action items, chapters, transcript
├── audio.opus         stereo: left is your microphone, right is the call
├── meeting.json       times, source app, speaker ids, segments, step states
├── screen.mp4         the screen, with record_screen on
├── meet-events.jsonl  who Meet said was speaking, with the Meet extension
└── .cache/            raw 20 s chunks while recording; removed when done
```

`meeting.md` has YAML front matter and plain Markdown, so Obsidian and other
note tools can read the folder directly. The folders are the source of truth;
the index can be rebuilt from them.

Shared data lives in `~/.local/share/recall/`:

```
~/.local/share/recall/
├── speakers.json   names, aliases and voice embeddings of everyone recorded
├── index.sqlite    full-text search index
└── models/         speech, VAD, segmentation and speaker models
```

## Configuration

`~/.config/recall/config.toml` (or the file named by `RECALL_CONFIG`). Every
key is optional. `RECALL_HOME` changes the default `data_dir`. The settings page
in the web UI edits the same file; restart the daemon
(`systemctl --user restart recall`) to apply changes.

```toml
meetings_dir = "/home/you/Meetings"    # absolute; ~ is not expanded
data_dir = "/home/you/.local/share/recall"
record_screen = false                  # record the screen too; see below
allowlist = ["brave", "chromium", "chrome", "firefox", "zoom", "slack", "discord", "teams"]
debounce_secs = 8                      # mic must stay open this long to record
end_grace_secs = 30                    # mic closed this long ends the recording
min_length_secs = 120                  # shorter detected recordings are deleted
summary_lang = "zh-TW"                 # "zh-TW" or "en"
# ui_lang = "en"                       # unset: follow the browser; a choice made in the UI wins
port = 7474

[llm]
backend = "ollama"                     # "ollama" or "claude"
model = "qwen3.6:35b-a3b"
endpoint = "http://127.0.0.1:11434"    # Ollama only
context_tokens = 32768                 # model context; see below

[stt]
engine = "breeze"                      # the only engine; anything else is rejected
threads = 8
```

## LLM backends

Summaries and the "ask this meeting" box use one of two backends.

- `ollama` (default): talks to Ollama at `endpoint`. Pull the model first:
  `ollama pull qwen3.6:35b-a3b`, or set `model` to one you have.
- `claude`: the Anthropic API. Set `backend = "claude"` and export
  `ANTHROPIC_API_KEY` in the service's environment (for example with
  `systemctl --user edit recall` and an `Environment=` line). A `model`
  that is empty or starts with `qwen` (such as the Ollama default) is replaced
  by `claude-sonnet-5-5`; set `model` to another Claude model name to use that.

`context_tokens` is the model's context window. Ollama gets it as `num_ctx`
(Ollama's own default is a few thousand tokens and silently drops the start of
a longer transcript), so the Ollama machine needs the memory for it. A
transcript longer than `context_tokens * 0.6` characters is summarized in parts
that are then merged, and a question about such a meeting is refused with
"this meeting is too long for the current context; raise llm.context_tokens".

## GPU transcription

`packaging/install.sh --vulkan` builds whisper.cpp with its Vulkan backend
(`cargo build --release -p recall --features recall-pipeline/vulkan`). It needs
the Vulkan headers and the `glslc` shader compiler: `vulkan-headers` and
`shaderc` on Arch. Without it, transcription runs on the CPU with
`stt.threads` threads.

## Google Meet speaker names

The extension in `extension/` tells recall who is speaking in a Google Meet
call, so the `name` step can put real names on the call-side speakers
instead of matching voices alone. It is not in a store. To install it, open
`chrome://extensions`, turn on Developer mode, choose "Load unpacked" and
pick `extension/dist` (built and committed; rebuild with `npm ci && npm run
build` in `extension/` after changing `src/`). Its id is fixed by the key in
the manifest, and the daemon accepts events only from that id.

On `meet.google.com` it reads:

- the audio level of each remote participant's contributing source (CSRC)
  on Meet's WebRTC receivers, polled every 100 ms, to tell who is speaking;
- display names from the video tiles and the People panel, to put a name to
  each source.

It sends those speaking, name and roster events, with the meeting code, to
`http://127.0.0.1:7474/api/meet/events` and nowhere else. No audio or video
leaves the page. The daemon stores the events in the meeting's
`meet-events.jsonl` while a capture is running, which includes the
`debounce_secs` before a detected recording is confirmed (a capture that
ends there is deleted with its events), and drops them otherwise. The
extension's address is fixed, so it stops reaching the daemon if you change
`port`, and nothing warns you; keep `port = 7474` to use the extension.

After the call, a diarization cluster takes a Meet name when that name was
the only one speaking for at least 60% of the cluster's overlapping speech
and at least 30 s of it. Voices named this way join `speakers.json`, so the
next meeting recognises them without Meet.

## Screen recording

With `record_screen = true` (or "Record the screen too" in the settings
page), each recording also runs `gpu-screen-recorder` on the screen through
xdg-desktop-portal, and the meeting page plays the video in step with the
transcript. `ffmpeg` remuxes it into `screen.mp4` when the recording is kept;
if the remux fails the video stays as `screen.mkv`. The screen starts with
the recording proper, after `debounce_secs` (8 s by default), so the cached
first seconds have audio only.

The first recording shows the portal's picker to choose a screen. The portal
then hands back a restore token, saved in
`$XDG_STATE_HOME/recall/portal-token` (`~/.local/state/recall/portal-token`),
and later recordings reuse it. On Hyprland, xdg-desktop-portal-hyprland
shows the picker every time unless `~/.config/hypr/xdph.conf` has:

```
screencopy {
    allow_token_by_default = true
}
```

Restart the portal (`systemctl --user restart xdg-desktop-portal-hyprland`)
after changing it. Delete the token file to choose a different screen.

To stop recording the screen in the middle of a meeting, run
`recall screen off` or press "Stop screen recording" in the web UI. It stops
the video and deletes it; the audio goes on. If the screen recorder fails to
start or exits early, a notification says so and the audio goes on.

## MCP server

`recall mcp` is an MCP server on stdin/stdout that lets an assistant read and
control your meetings through the daemon's API. Add it to Claude Code with:

```sh
claude mcp add recall -- recall mcp
```

The daemon must be running; otherwise each tool replies that it is not.
Tools:

- `search_meetings`: search titles, summaries and transcripts.
- `get_meeting`: a meeting's participants, summary, decisions, action items
  and chapters.
- `get_transcript`: a meeting's transcript, about 20,000 tokens per call,
  with a cursor for the next page and an optional time range.
- `list_action_items`: action items across meetings, by owner, due date and
  meeting date.
- `recording_status`: whether the recorder is idle, armed or recording.
- `start_recording`, `stop_recording`: as `recall start` and `recall stop`.
- `rerun_step`: process a meeting again from a step.
- `rename_meeting`: change a meeting's title.
- `name_speaker`: set a speaker's name everywhere.

There is no tool to discard or delete a meeting.

## Privacy

- The web server listens on `127.0.0.1` only and rejects requests whose `Host`
  is not `127.0.0.1:<port>` or `localhost:<port>`, or whose `Origin` is another
  site, so web pages you visit cannot read your archive.
- Audio never leaves the machine. Transcription, speaker detection and voice
  matching run locally.
- The Meet extension posts only to the local daemon. The one cross-origin
  request the server accepts is that extension's `POST /api/meet/events`.
- With the `ollama` backend nothing leaves the machine unless you point
  `endpoint` at another host. With the `claude` backend, the transcript text
  with speaker names is sent to the Anthropic API for each summary and each
  question asked about a meeting.
- recall records other people's voices and keeps voice embeddings to recognise
  them. Getting their consent, and following the recording laws where you and
  they are, is up to you.

## Development

```sh
cargo test --workspace
RECALL_PW_TESTS=1 cargo test -p recall-audio --test pipewire -- --test-threads=1   # needs a PipeWire session
```

Tests that need downloaded models skip when the models are absent.

The web UI is Svelte in `web/`. With the daemon running:

```sh
cd web
npm ci
npm run dev        # Vite dev server; proxies /api to http://127.0.0.1:7474
npm test
npm run build      # writes web/dist, which is committed and embedded in the binary
```

CI checks that `web/dist` matches a fresh build, so commit it after changing the
UI.

## Bench

`bench/` scores transcription against recorded cases with known text:

```sh
cargo build --release -p recall
python3 bench/run.py
```

See [bench/README.md](bench/README.md) for the metrics and how to record new
cases. The `zh-en` case uses utterances from the CSZS dataset (MIT);
[NOTICE](NOTICE) has the attribution.

## Known limitations

- A sentence repeated back-to-back can come out only once when both copies
  fall in the same transcription window. Whisper decodes up to 28 s of glued
  speech at a time and drops the repeat. Giving each speech region its own
  window fixes this but doubles transcription time, so recall does not do it.
  The ignored test `whisper_keeps_repeated_speech` in
  `crates/recall-pipeline/src/transcribe.rs` reproduces it.
- The bench cases `zh` and `en-tw` have no recordings and are skipped until
  someone records them; [bench/README.md](bench/README.md) says how.
- The Meet extension reads Meet's page structure, which Google changes
  without notice; the selectors live in `extension/src/names.ts` and were
  written without a live call to check them against. The status chip
  catches two failures: "No speaker signal from Meet for 5 minutes"
  (`signal_lost`) means Meet audio plays but no speaker was detected for
  5 minutes, and "Meet speaker detection fell back to the video tiles"
  (`hook_failed`) means the WebRTC hook failed and speaking comes from the
  tiles' glow. It does not catch name selectors that stop matching: then
  speakers stay voice-matched and no "via Meet" names appear.
- Slack, Discord, Zoom and Teams have no extension; their speakers are named
  by voice only.
- Live captions are not built.

## License

MIT, see [LICENSE](LICENSE). The test clip
`crates/recall-pipeline/tests/fixtures/speech.opus` is from LibriSpeech
(CC BY 4.0); [NOTICE](NOTICE) has the attribution.
