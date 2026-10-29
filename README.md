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
starts by itself. One desktop notification follows each meeting: it says
recording, then saved, then summary ready (or which step failed), replacing
itself each time. Clicking it opens the meeting page.

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
- Every recording is kept whatever its length; `recall discard` throws one
  away.
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
5. `summarize`: the LLM writes a summary, decisions, action items and chapters
   in `summary_lang`, then the `llm` backend translates them into each of
   `summary_translations` (see below).
6. `write`: `meeting.md` is written and the search index updated.

A failed step keeps the audio; rerun it from the web page or with
`recall rerun <id> <step>`.

### Action items

Each action item has an owner, a due date, a priority and the moment in the
meeting it came from. The summary prompt carries the meeting's date, and
recall also resolves common relative dates itself (`明天`, `下週三`,
`next Friday`, `11/3`), so due dates are `YYYY-MM-DD` where they can be;
anything vaguer ("next sprint") keeps its wording. The meeting page lists its
items with a checkbox to mark them done and a link to the moment in the
recording. Rerunning `summarize` keeps your done marks: each one moves to the new item
with the most similar text, or stays as it was if none is close.

In `meeting.md` the items are [Obsidian Tasks](https://publish.obsidian.md/tasks/)
lines, so a vault with the Tasks plugin can query them:

```md
- [ ] Send the TRL6 report to Arlene 👤 Arlene ⏫ 📅 2026-11-03
- [x] Update the pricing page 👤 Leon 🔼 ✅ 2026-11-01
```

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
summary_lang = "zh-TW"                 # "zh-TW" or "en"
summary_translations = []              # e.g. ["en"]: extra languages for the web UI only
# ui_lang = "en"                       # unset: follow the browser; a choice made in the UI wins
port = 7474

[llm]
backend = "ollama"                     # "ollama" or "claude"
model = "qwen3.6:35b-a3b"
endpoint = "http://127.0.0.1:11434"    # Ollama only
context_tokens = 32768                 # model context; see below

[claude_code]                          # summaries of synced meetings; see below
enabled = false
command = "claude"
cwd = "~/Work"                         # ~ is expanded here; default: home directory
# model = "opus"                       # unset: Claude Code's own default
timeout_secs = 600

[calendar]                             # titles and attendees from Google Calendar; see below
enabled = false
slack_minutes = 5
timeout_secs = 120

[stt]
engine = "breeze"                      # the only engine; anything else is rejected
threads = 8

[vault]
remote = ""                            # git URL of a shared vault; empty: off
dir = ""                               # recall's clone; empty: <data_dir>/vault, ~ expands
folder = "meetings"                    # the only folder recall writes in the vault
branch = "main"

[glossary]
terms = ["Arlene", "BBINT-220", "TRL6"] # always included, first
sources = ["~/notes/people"]           # Markdown files or directories; ~ is expanded
max_terms = 30                         # 0 turns the glossary off
refresh_days = 7                       # Claude Code re-chooses the terms; 0: only by hand
```

### Glossary

Each processing run builds a glossary of names and project terms that the
transcriber and the summary model should spell right. It holds, in this order
and each once (ignoring case), until `max_terms`:

1. `terms`, as typed.
2. The names you gave to voices on the People page.
3. The terms Claude Code chose, from `<data_dir>/glossary.json`. Without
   that file, 4 and 5 instead.
4. One person per subdirectory of any `people/` directory under `sources`
   (`people/arlene/` gives `Arlene`).
5. Terms found in the `.md` files under `sources`: ticket ids (`BBINT-220`),
   acronyms (`TRL6`), CamelCase names (`BixBench`, `scRNA`) and words written
   capitalised mid-sentence in at least two notes (`Zenodo`), those in the
   most notes first. Front matter (except `description`), code, links and
   common English words and acronyms (`API`, `PDF`) are skipped, as are
   hidden directories and those starting with `_`.

The extraction lets in ordinary words written capitalised and cuts some
acronyms short (`TRL` for `TRL6`). `recall glossary refresh` runs Claude Code
in `[claude_code] cwd` with read-only tools (Read, Grep, Glob, and the
`sources` directories added), gives it the extracted candidates with their
counts, and saves the names and terms it picks, up to 60, each with a reason,
to `glossary.json`. With `[claude_code]` enabled the daemon does the same
once the file is `refresh_days` old, checking hourly and never while a
meeting is being processed. `recall glossary show` prints the list the next
run will use and where each term came from.

Breeze gets the glossary as a Traditional Chinese sentence naming the terms
(「這次會議提到 Arlene、BBINT-220、TRL6。」), cut to 120 tokens, before each
stretch of speech. The summary model gets the list as the known spellings, to
correct names the transcript misheard. Put the names you most need first in
`terms`. The prompt also changes words it does not name: on the code-switched
bench case the words stayed the same but Breeze wrote fewer, longer lines,
and on a real two-minute call with none of the terms spoken about 8% of the
tokens came out differently (no term was inserted).

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

### Summary translations

`summary_lang` is the summary's canonical language: `meeting.md`, the vault
note, the MCP tools and the questions use it alone. Each language in
`summary_translations` gets a translation of the summary's texts, made by the
`llm` backend right after the summary (also for summaries Claude Code wrote,
since only the summary is sent, not the transcript). Translations are kept in
`meeting.json` for the meeting page, whose language switch over the summary
shows one; done marks, owners, dues and times are shared with the canonical
items. A translation that fails, or whose lists do not line up item for item
with the summary's, is left out and logged. Rerunning `summarize` redoes them.

### Claude Code for synced meetings

With `[claude_code] enabled = true`, a meeting marked for sync is summarized by
Claude Code in print mode instead of the `llm` backend. It runs in `cwd`, so it
reads the CLAUDE.md files and Claude Code memory there and may look up a few
names, tickets or terms the transcript leaves unclear in the project notes and
repositories under it, within 10 turns. The
"ask this meeting" box of a synced meeting goes to Claude Code the same way.
Every other meeting keeps the `llm` backend. Install
Claude Code and log in (`claude`, then `/login`) as the user the daemon runs
as. Each summary runs:

```sh
claude -p --output-format json --no-session-persistence \
  --tools Read,Grep,Glob --allowedTools Read,Grep,Glob --permission-mode dontAsk \
  --strict-mcp-config --disable-slash-commands [--model <model>] --max-turns 10 \
  --append-system-prompt <summary instructions>
```

with the transcript on stdin. It can read and search files and nothing else: no
edits, no shell commands, no MCP servers, no web access. The session is not
saved, and the process (with anything it started) is killed after
`timeout_secs`. A transcript of up to 120,000 characters goes in one call.
When the run fails (`claude` missing, not logged in, timeout, a reply that is
not the summary JSON), recall logs the reason and summarizes that meeting with
the `llm` backend. `models.llm` in the meeting's `meeting.json` names the one
that wrote the summary, e.g. `claude-code/claude-opus-5-5`.

### Questions across meetings

The Ask page in the web UI (`POST /api/ask`) takes a question such as "What
did we decide with Arlene about TRL6 last week?" and has Claude Code answer it
from your synced meetings. The run is the one above with `--output-format stream-json`, plus
`recall mcp --synced-only` as its only MCP server (`--mcp-config` with
`--strict-mcp-config`) and its tools allowed. That server offers only the read
tools, and to them a meeting not marked for sync does not exist; the meetings
directory and recall's data directory are denied to the file tools as well.
The answer is in `summary_lang` whatever language the question is in (quotes
keep the language they were spoken in), and every claim cites a meeting and a
transcript time that the page links to. One question runs at a time, for at
most `timeout_secs` or 300 s, whichever is shorter. With `[claude_code]` off,
the page explains how to turn it on.

### Meeting titles and attendees from Google Calendar

With `[calendar] enabled = true`, the worker asks Claude Code (the `command`
and `model` of `[claude_code]`, which itself may stay disabled) for the event
on your Google Calendar at the meeting's start before the meeting is first
processed. It needs the claude.ai Google Calendar connector connected in that
Claude Code (`claude mcp list` shows it). Each lookup runs, in an empty
temporary directory:

```sh
claude -p --output-format json --no-session-persistence --tools "" \
  --permission-mode dontAsk --disable-slash-commands \
  --allowedTools <list_events, search_events, get_event> \
  --disallowedTools <create_event, update_event, delete_event, respond_to_event> \
  [--model <model>] --append-system-prompt <instructions>
```

Of the timed events you have not declined that overlap the start, give or take
`slack_minutes`, the one that overlaps the meeting longest wins. Its title,
attendees (name, email, response) and id go into `meeting.json` as `calendar`;
`meeting.md` gets `calendar_event` and the attendee names in its frontmatter,
the names join `participants_hint` (offered when renaming a speaker), and the
meeting page lists them under the title. A meeting still titled
"Meeting HH:MM" takes the event's title, which renames its directory; a title
you typed is kept. When the lookup is off, finds no event or fails (`claude`
missing, connector not connected, timeout), the meeting is left as it is and
the reason is logged.

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
and at least 5 s of it. Voices named this way join `speakers.json`, so the
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

## Publishing to a team vault

With `vault.remote` set to a git repository holding an Obsidian vault, each
meeting page gets a "Publish to vault" switch. It is off for every meeting
until you turn it on, since recall records private calls too. A published
meeting is its `meeting.md` (summary, decisions, action items, chapters,
transcript; never audio or video) at `<folder>/<note name>.md`, where the
note name is the meeting folder's name made valid on every OS:
`2026-10-29 12:37 Meeting 12:37` becomes
`meetings/2026-10-29 12.37 Meeting 12.37.md`.

recall keeps its own clone in `vault.dir` (it refuses a clone it did not
make, since it resets the clone before every change) and commits straight to
`vault.branch` with your git identity: "Add meeting: …" when the switch goes
on (or when processing finishes, if it was turned on earlier), "Update
meeting: …" about 20 s after edits to the title, transcript or speaker names
and after a rerun, and "Remove meeting: …" when the switch goes off or the
meeting is deleted. A removed note stays in the vault's git history. Renaming
a meeting moves its note. Edits made to the note in the vault are overwritten
by the next update. recall only writes inside `vault.folder`, rebases onto
teammates' pushes, and never force-pushes; a failed push shows on the meeting
page and is retried with backoff. What was last pushed for each meeting is in
`<data_dir>/vault.json`.

For an `https://` remote, the clone uses the GitHub CLI as its only
credential helper (`gh auth git-credential`), so run `gh auth login` as an
account with push access first. Under the systemd service, `gh` must be on
the unit's `PATH` and find its token without a terminal: in
`~/.config/gh/hosts.yml`, or in a keyring unlocked by your login.

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
- `list_action_items`: action items across meetings with priority, source
  time and done state, by owner, due date, meeting date and open or done.
- `recording_status`: whether the recorder is idle, armed or recording.
- `start_recording`, `stop_recording`: as `recall start` and `recall stop`.
- `rerun_step`: process a meeting again from a step.
- `rename_meeting`: change a meeting's title.
- `name_speaker`: set a speaker's name everywhere.

There is no tool to discard or delete a meeting. `recall mcp --synced-only`
serves only the four read tools, and only meetings marked for sync; the Ask
page runs it that way.

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
  question asked about a meeting, and the summary for each translation.
- With `[claude_code]` enabled, the transcript of every meeting marked for sync,
  with speaker names, goes to Anthropic through Claude Code, together with
  whatever files under `cwd` it reads while summarizing. A question on the Ask
  page sends Anthropic the transcripts and notes of the synced meetings Claude
  Code reads to answer it; meetings not marked for sync are never sent.
- With `[calendar]` enabled, the meeting's start and end times go to
  Anthropic through Claude Code, and the calendar events around them, with
  their attendees, pass through Claude Code's connector to get there. recall
  keeps attendee emails in `meeting.json` only; `meeting.md`, and so the vault,
  carries names.
- With `vault.remote` set, the notes of meetings you switch to "Publish to
  vault" are pushed to that repository, where everyone with access can read
  them and their history. Nothing else is pushed.
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
