# recall phase 4 design

> Historical record, frozen 2026-10-28. Source code is authoritative; where
> this document and the code disagree, the code wins.

## Purpose

Phase 4 of `2026-10-27-recall-design.md` lists four deliverables: screen
recording, a Google Meet extension, an MCP server and a TEA-ASR spike. They
share no code paths and can be built in any order. The order below follows
what Leon needs most: remote speakers named from their first meeting.

What Leon chose during design:

- The Meet extension reports who is speaking and the participant list. The
  list becomes one-click naming choices on the meeting page.
- Screen recording is a global setting; a single meeting can turn it off,
  which deletes that meeting's video while audio keeps recording.
- The portal picker appears once; later recordings reuse its restore token.
- MCP covers reading meetings, listing action items, controlling recording
  and editing (meeting titles, speaker names).

### Success criteria

- In a Google Meet call with the extension installed, every remote speaker
  who talks for at least 30 seconds alone is named automatically in that
  same meeting, with no voiceprint on file.
- A person named through Meet is recognised by voiceprint in a later Slack
  or Discord call.
- Killing the daemon with SIGKILL mid-meeting leaves a playable video up to
  the kill point after the next start.
- Screen recording adds under 5% of one core at 30 fps on the Intel Arc B390.
- Claude Code can answer "what did I agree with Tom last week" through
  `recall mcp` without reading files directly.
- The TEA-ASR spike ends in a written go/no-go with measured numbers.

### Out of scope

- Publishing the extension to the Chrome Web Store.
- Meet closed captions as a data source (speaker labels are not stable or
  machine-readable, and captions lag by seconds).
- An HTTP `/mcp` endpoint on the daemon. stdio works in every client.
- Switching the default ASR engine. The spike only produces a recommendation.

## Meet extension

An unpacked Manifest V3 extension in `extension/`, loaded with "Load
unpacked". The manifest carries a fixed `key`, so the extension ID is the
same on every machine and the daemon can allow exactly that origin.

### Who is speaking

Meet's speaking glow uses obfuscated CSS classes that change between Meet
releases, and a changed class fails silently: tiles read as "not speaking"
and the vote receives nothing. The primary signal is therefore WebRTC:

- A MAIN-world content script at `document_start` wraps `RTCPeerConnection`
  once (a second wrap mirrors every track twice) and, for each audio
  receiver, polls `getContributingSources()` every 100 ms. Each remote
  participant has a stable CSRC; the script emits `speaking_start` and
  `speaking_stop` per CSRC when its `audioLevel` crosses a threshold.
  Meet's audio receivers are a rotating pool, so the CSRC, not the receiver,
  identifies a participant.
- An isolated-world content script reads the DOM only to name CSRCs: while a
  single CSRC is active and a single tile glows, that pair is counted, and a
  CSRC takes the name it co-occurred with most. Tiles are
  `div[data-participant-id]`, the name is in `span.notranslate`, and the
  user's own tile (`[data-self-name]`) is excluded. The user's speech always
  comes from the microphone track.
- The participant list comes from the tiles and the People panel.

When no speaking event arrives for 5 minutes while audio shows remote
speech, the extension reports `signal_lost`; the status chip shows it. When
the CSRC hook fails to install, the extension falls back to glow-only
detection and says so in the same status.

### Transport

The service worker POSTs batches to `http://127.0.0.1:7474/api/meet/events`
with `host_permissions: ["http://127.0.0.1:7474/*"]`. Content scripts never
fetch; their requests would carry the `meet.google.com` origin and hit the
browser's local-network prompt. The server's Origin guard additionally
allows `chrome-extension://<fixed id>` on this one route.

Chrome exempts extensions with host permissions from Local Network Access;
Brave has its own localhost permission whose effect on extension service
workers is undocumented. The first implementation task verifies the fetch in
Leon's Brave. If Brave blocks it, the transport becomes native messaging (a
`recall meet-host` subcommand registered under Brave's
`NativeMessagingHosts`), and the rest of this section is unchanged.

### Event format

```json
{"meeting_code": "abc-defg-hij",
 "events": [
   {"t": 1793170000123, "kind": "speaking_start", "participant": "csrc:12345"},
   {"t": 1793170004456, "kind": "speaking_stop", "participant": "csrc:12345"},
   {"t": 1793170000500, "kind": "name", "participant": "csrc:12345", "name": "Tom Lin"},
   {"t": 1793170000600, "kind": "roster", "names": ["Tom Lin", "Amy Chen"]},
   {"t": 1793170300000, "kind": "status", "status": "signal_lost"}]}
```

`t` is Unix milliseconds (`performance.timeOrigin + performance.now()`).
The extension keeps the last 60 seconds of events and resends them on every
batch, because the daemon starts recording 8 seconds after the microphone
opens; the daemon drops duplicates and events older than the recording.

The daemon appends accepted events to `meet-events.jsonl` in the meeting
directory with `t` converted to milliseconds from the recording origin, using
the wall-clock time it stores next to the origin when recording starts.
Events arriving while nothing records are discarded.

### Naming with Meet events

The naming step gains a second source after the voiceprint match:

- For each `Remote N` cluster, collect the Meet speaking intervals that
  overlap its segments. Intervals where two or more participants speak at
  once are dropped. A name that covers at least 60% of the cluster's
  remaining overlapped speech, and at least 30 seconds of it, wins.
- When the voiceprint match and the Meet vote disagree, Meet wins, because
  it is direct evidence from this meeting; the voiceprint is not updated
  from that cluster.
- A cluster named through Meet adds its embedding to that person in
  `speakers.json` (creating the person when the name is new), so the voice is
  recognised later in calls without the extension.
- The roster is stored in `meeting.json` as `participants_hint` and offered
  as one-click choices on the meeting page's rename control.

`meeting.json` gains `participants_hint: Vec<String>` and, per cluster,
`named_by: "voiceprint" | "meet" | "user"`; both default to empty/absent, so
older files load unchanged.

## Screen recording

The daemon spawns `gpu-screen-recorder` when recording starts and
`record_screen` is on:

```
gpu-screen-recorder -w portal -restore-portal-session yes
  -portal-session-token-filepath <state_dir>/portal-token
  -c mkv -k h264 -fm content -f 30 -bm qp -q medium -cursor yes
  -write-first-frame-ts yes -o <meeting>/.cache/screen.mkv
```

- MKV because MP4 writes its index last and a killed recorder leaves an
  unplayable MP4. On stop the daemon sends SIGINT, waits up to 10 seconds,
  then remuxes with `ffmpeg -c copy -movflags +faststart` to `screen.mp4`.
  Startup recovery runs the same remux on an orphaned `.cache/screen.mkv`.
- `-fm content` emits frames only when the screen changes, which keeps long
  meetings small.
- `screen.mkv.ts` holds the first frame's CLOCK_MONOTONIC time in
  microseconds. The capture origin records its own CLOCK_MONOTONIC time, and
  the difference is stored in `meeting.json` as `screen_offset_ms`.
- The portal restore token is single-use; `gpu-screen-recorder` writes the
  new one each time. A missing or empty token means the picker will appear;
  the status chip says so before the next meeting. README tells xdph users to
  set `screencopy:allow_token_by_default = true`.
- If the recorder exits early or fails to start, audio continues, the
  meeting records `screen_error`, and a notification explains it.
- Turning screen recording off for one meeting (`recall screen off` or the
  meeting page while live) stops the recorder and deletes the video files.

The meeting page shows the video above the transcript, kept in sync with the
waveform and transcript clicks through `screen_offset_ms`.

## MCP server

`recall mcp` is a stdio MCP server built on `rmcp` 3.8. It holds no state and
calls the daemon's HTTP API, so the daemon stays the only writer and the 409
rule for meetings under processing still applies. Setup:
`claude mcp add recall -- recall mcp`.

| Tool | Hint | Daemon endpoint |
|---|---|---|
| `search_meetings` | read-only | `GET /api/meetings?q=` (snippets with timestamps) |
| `get_meeting` | read-only | `GET /api/meetings/{id}` (summary, decisions, chapters) |
| `get_transcript` | read-only | same, paginated |
| `list_action_items` | read-only | new `GET /api/action-items` |
| `recording_status` | read-only | `GET /api/status` |
| `start_recording`, `stop_recording` | not read-only | `POST /api/record/{action}` |
| `rerun_step` | not read-only | `POST /api/meetings/{id}/rerun/{step}` |
| `rename_meeting` | idempotent | `PATCH /api/meetings/{id}` |
| `name_speaker` | idempotent | `PATCH /api/people/{id}` |

- `get_transcript` takes `cursor` and an optional time range and returns at
  most about 20,000 tokens per call with `next_cursor`.
- `list_action_items` filters by owner, due range and meeting date range and
  returns `structuredContent`.
- There is no discard or delete tool; deleting audio cannot be undone, and
  the web page covers it.
- When the daemon is not running, every tool returns an error naming
  `systemctl --user start recall`.

## TEA-ASR spike

The spike answers whether TEA-ASR-1.1 should replace Breeze-ASR-25. Its code
lives in the scratchpad, never in the repository; its result is a decision
record in `docs/decisions/`.

- Runtime: the `crispasr` crate with the published TEA-ASR-1.1 Q8_0 GGUF and
  Qwen3-ForcedAligner-0.6B for word timestamps; CPU only on this machine,
  because `vulkan-headers` is not installed. Fallback: `transcribe-cpp` with
  the same model (no timestamps).
- Test set: 300 clips from the ASCEND test split (CC BY-SA 4.0) and 300 from
  CSZS zh-en (MIT), cut by the existing VAD and scored the same way for both
  engines.
- Go when all hold: mixed error rate at least 5 points below Breeze-ASR-25
  q8_0; real-time factor at or below 1.0 on CPU; peak memory at or below 4 GB;
  no repetition loops or empty output on silence or on segments over 30 s.
- Known integration risk for a later phase: `crispasr` and `whisper-rs` both
  bundle ggml, so linking both into one binary may clash.

Separately, a few CSZS clips go into `bench/fixtures/` so the code-switched
bench case runs instead of skipping; the pure-Mandarin and Taiwanese-English
cases still need Leon's recordings.

## Testing

- Extension: Vitest over saved Meet DOM snippets and a fake
  `RTCPeerConnection`: CSRC transitions, CSRC-to-name correlation, own-tile
  exclusion, the 60-second resend window and `signal_lost`.
- Naming: Rust unit tests for interval alignment, overlap dropping, the 60%
  and 30 second thresholds, voiceprint/Meet disagreement and embedding
  enrolment.
- Server: the extension origin is accepted on `/api/meet/events` only;
  events outside a recording are discarded.
- Screen: a fake `gpu-screen-recorder` script drives start failure, early
  exit, SIGINT handling, remux, crash recovery and the per-meeting off
  switch. A real recording is checked by hand once.
- MCP: an rmcp client against a test daemon calls every tool, including
  pagination and the daemon-down error.
