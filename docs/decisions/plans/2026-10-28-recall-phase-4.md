# recall phase 4 implementation plan

> Historical record, frozen 2026-10-28. Source code is authoritative; where
> this document and the code disagree, the code wins.

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Meet extension with automatic speaker naming, screen recording, a stdio MCP server, a code-switched bench case and a TEA-ASR go/no-go.

**Architecture:** The extension posts speaking/roster events to the daemon, which stores them per meeting; the Name step votes them against diarization clusters. The daemon supervises `gpu-screen-recorder` beside the audio capture and remuxes on stop or recovery. `recall mcp` is a stateless rmcp stdio server that calls the daemon's HTTP API.

**Tech Stack:** Rust 2024 (axum, rmcp 3.8, rustix), Svelte 5 + Vitest, MV3 extension in TypeScript built with Vite, gpu-screen-recorder 6.1.4, ffmpeg.

**Spec:** `docs/decisions/specs/2026-10-28-recall-phase-4-design.md`

## Global Constraints

- All repository text in English. No AI attribution anywhere (commits, code, docs).
- Rust: `cargo fmt --all --check` and `cargo clippy --workspace --all-targets -- -D warnings` clean; `cargo test --workspace` green. Web: `npm test` and `npm run build` in `web/`, and the committed `web/dist` matches the build.
- Older `meeting.json` and `speakers.json` files must load unchanged (new fields use serde defaults).
- The server stays loopback-only; the only new cross-origin exception is `chrome-extension://oiejafpeoacoblcpomkoloaibfmdfnhd` on `POST /api/meet/events`.
- Extension manifest `key` (gives the ID above):
  `MIIBIjANBgkqhkiG9w0BAQEFAAOCAQ8AMIIBCgKCAQEAzNvyhMsxPx0/o88bfY+CqcvNcK7ahPr5GsO17QNOFT2i1KN7zV61fg+vwmtNgENniXbGJcK4GlDHKLKfVJMsOQRLYj1iSzIttSPTgF7CdQzWOPko4+1ssP178A1854NqCpcc7Zkuqypf7jflcmj1GQoi7S6Z1dtZqUoCxFiLv71MfYf/m3iWFnOLh3iIwO8xV1tVQQ5WOMAzJMQIKh4yt9o8jVHlRIbrCnG4SE/QVMC2IvRCGuf5xSybCGw3t8/H59yOiY1IWVDsJn5A0gdrOIsTQz3M3NT8ZiMDk/ZTsgrOQT5ETZhLoKvXN+Vql75ACl6JZvKrsXat/RuP8kY1EwIDAQAB`
- Screen recorder command line exactly as in the spec, including `-f 30`.
- Meet vote thresholds: 60% of the cluster's non-overlapping overlapped speech and at least 30,000 ms. Signal-lost after 300,000 ms. Event resend window 60,000 ms. CSRC poll 100 ms.
- MCP transcript page cap: 20,000 tokens, estimated as `chars / 2` (CJK-heavy text).
- No discard or delete tool in MCP.

## Review Focus

1. A Meet call that starts before recording does (8 s debounce): the first 60 s of events must still land, converted to recording-relative ms; events before the recording origin are dropped, not stored negative. Test in Task 4.
2. Two Meet tabs or a rejoin (new CSRCs, same names): names must map by name, not CSRC, in the vote. Test in Task 5.
3. A meeting where screen recording was on but the daemon was SIGKILLed: recovery must remux `.cache/screen.mkv` before `clear_cache` deletes it. Test in Task 7.
4. The extension posting while nothing records, or posting garbage: 204 with nothing written for the former, 400 for the latter, never a panic. Test in Task 4.
5. `get_transcript` on a 2-hour meeting: pages must cut on segment boundaries, never mid-segment, and `next_cursor` must reach the end exactly once. Test in Task 9.

---

### Task 1: Verify extension-to-localhost fetch in Brave

Decides the transport before any extension code exists.

**Files:**
- Create (scratchpad only, not committed): a minimal MV3 extension with `host_permissions: ["http://127.0.0.1:7474/*"]` whose service worker POSTs `{}` to `http://127.0.0.1:7474/probe` on install, and a Python HTTP server on 7474 logging requests.

- [ ] **Step 1:** Run `brave-origin --user-data-dir=<tmp> --load-extension=<probe dir> --no-first-run` (headless `--headless=new` if a display is unavailable) and wait up to 20 s for the request.
- [ ] **Step 2:** Record in the report: whether the POST arrived, the `Origin` header it carried, and any permission prompt in stderr.
- [ ] **Step 3:** If it did not arrive, also try with `--enable-features`/`--disable-features` flags for LocalNetworkAccess to tell a Brave block from a probe bug.

Outcome feeds Task 4 and Task 6: fetch works → as specified. Blocked → the controller rules native messaging in, and Tasks 4 and 6 swap the transport (`recall meet-host` reading Chrome native messaging frames on stdin and forwarding to the same handler). No commit.

### Task 2: Store types for Meet events and screen metadata

**Files:**
- Create: `crates/recall-store/src/meet.rs`
- Modify: `crates/recall-store/src/meeting.rs`, `crates/recall-store/src/dir.rs`, `crates/recall-store/src/lib.rs`

**Interfaces (produces):**
- `meet.rs`:
  - `#[serde(tag = "kind", rename_all = "snake_case")] pub enum MeetEventKind { SpeakingStart { participant: String }, SpeakingStop { participant: String }, Name { participant: String, name: String }, Roster { names: Vec<String> }, Status { status: String } }`
  - `pub struct MeetEvent { pub t: i64, #[serde(flatten)] pub kind: MeetEventKind }` (`t`: Unix ms on the wire, recording-relative ms on disk)
  - `pub struct MeetBatch { pub meeting_code: String, pub events: Vec<MeetEvent> }`
  - `pub fn append_events(path: &Path, events: &[MeetEvent]) -> Result<()>` (JSON Lines, appends, creates the file)
  - `pub fn read_events(path: &Path) -> Result<Vec<MeetEvent>>` (missing file → empty; a malformed line is skipped with a `tracing::warn!`)
- `meeting.rs`: `Meeting` gains `#[serde(default)] participants_hint: Vec<String>`, `#[serde(default, skip_serializing_if = "Option::is_none")] screen_offset_ms: Option<i64>`, `#[serde(default, skip_serializing_if = "Option::is_none")] screen_error: Option<String>`. `Cluster` gains `#[serde(default, skip_serializing_if = "Option::is_none")] named_by: Option<NamedBy>` with `#[serde(rename_all = "snake_case")] pub enum NamedBy { Voiceprint, Meet, User }`.
- `dir.rs`: `MeetingDir::meet_events() -> PathBuf` (`meet-events.jsonl`), `screen() -> PathBuf` (`screen.mp4`), `screen_cache() -> PathBuf` (`.cache/screen.mkv`), `clock() -> PathBuf` (`.cache/clock.json`).
- `dir.rs`: `pub struct CaptureClock { pub wall_ms: i64, pub monotonic_us: i64 }` with `pub fn write(&self, path: &Path) -> Result<()>` and `pub fn read(path: &Path) -> Result<Option<Self>>` (missing file → `None`), re-exported from `lib.rs`.

- [ ] **Step 1: Tests** in each module:
  - `meet_event_round_trips_wire_format`: the spec's example JSON parses into `MeetBatch` and re-serializes to the same values.
  - `read_events_skips_bad_lines_and_missing_file`.
  - `old_meeting_json_loads_with_new_fields_defaulted`: a `meeting.json` without the new keys loads; saving it again does not emit `screen_offset_ms`/`screen_error`/`named_by`.
  - `clock_round_trips`.
- [ ] **Step 2:** Run `cargo test -p recall-store` and see them fail; implement; see them pass.
- [ ] **Step 3:** Fix every `Meeting { .. }` / `Cluster { .. }` literal in the workspace (add the new fields). `cargo test --workspace` green.
- [ ] **Step 4:** Commit "Add Meet event and screen metadata types to the store".

### Task 3: Record the capture clock

**Files:**
- Modify: `crates/recall-audio/src/capture.rs`, `crates/recall/src/recorder.rs`

**Interfaces:**
- Consumes: `CaptureClock`, `MeetingDir::clock()`.
- Produces: `Capture::clock(&self) -> CaptureClock`, read at the moment `origin = Instant::now()` is taken: `wall_ms` from `SystemTime`, `monotonic_us` from `rustix::time::clock_gettime(ClockId::Monotonic)` (Rust's `Instant` uses the same clock on Linux). `CaptureControl` gains `fn clock(&self) -> CaptureClock`. `Recorder::start` writes it to `dir.clock()` right after the capture starts; `Recorder` exposes `pub fn live(&self) -> Option<(MeetingDir, CaptureClock)>`.

- [ ] **Step 1: Tests:** `capture_clock_is_monotonic_and_wall_consistent` (unit, no PipeWire: the helper that reads both clocks returns `wall_ms` within 1000 ms of `SystemTime::now()` and `monotonic_us` > 0); in `recorder.rs`, `start_writes_clock_file` using the fake capture (fake returns a fixed clock).
- [ ] **Step 2:** Implement; `cargo test -p recall-audio -p recall` green.
- [ ] **Step 3:** Commit "Record the capture's wall and monotonic origin".

### Task 4: Meet events endpoint and action-items endpoint

**Files:**
- Create: `crates/recall-server/src/meet.rs`, `crates/recall-server/src/actions.rs`, `crates/recall/src/meet_sink.rs`
- Modify: `crates/recall-server/src/lib.rs`, `crates/recall/src/daemon.rs`

**Interfaces:**
- Consumes: Task 2 types, `Recorder::live()`.
- Produces:
  - `pub const EXTENSION_ORIGIN: &str = "chrome-extension://oiejafpeoacoblcpomkoloaibfmdfnhd";`
  - `pub type MeetSinkFn = dyn Fn(MeetBatch) -> anyhow::Result<usize> + Send + Sync;` on `AppState` as `meet: Arc<MeetSinkFn>` (returns the number of events stored).
  - `POST /api/meet/events` → 204; 400 on a body that is not a `MeetBatch`. The Origin guard accepts `EXTENSION_ORIGIN` for this exact method and path only (Host must still be loopback).
  - `StatusSnapshot` gains `meet: Option<String>` (latest `Status` event value, cleared when a recording ends) and `screen: Option<String>` (filled by Task 7).
  - `GET /api/action-items?owner=&from=&to=&due_from=&due_to=` → `[{ "meeting": id, "title", "started", "text", "owner", "due" }]`, newest meeting first. It lists meetings through `Index::list` with `from`/`to`, loads each `meeting.json`, and filters owner case-insensitively by substring and due by string comparison of ISO dates (items without `due` are excluded only when a due bound is given).
  - `meet_sink::accept(batch, live: Option<(MeetingDir, CaptureClock)>, seen: &mut HashSet<(i64, String)>) -> Result<usize>`: no live recording → 0; converts `t` to `t - clock.wall_ms`; drops negatives and anything already in `seen` (key: wire `t` plus the event's JSON); appends the rest to `meet-events.jsonl`. `seen` resets when the live meeting changes.

- [ ] **Step 1: Tests:**
  - server: `extension_origin_allowed_only_on_meet_events` (204 there; 403 with the same Origin on `/api/record/start`; 403 for `chrome-extension://otherid`), `meet_events_rejects_bad_body` (400), `action_items_filters_by_owner_and_due`.
  - sink: `events_before_origin_are_dropped`, `resent_events_are_stored_once`, `nothing_stored_without_live_recording`, `status_event_updates_snapshot`.
- [ ] **Step 2:** Implement and wire `meet` in `daemon.rs` (the sink locks the recorder to read `live()`).
- [ ] **Step 3:** `cargo test --workspace` green; commit "Accept Meet events and list action items over the API".

### Task 5: Name clusters from Meet events

**Files:**
- Create: `crates/recall-pipeline/src/meet_vote.rs`
- Modify: `crates/recall-pipeline/src/naming.rs`, `crates/recall-pipeline/src/runner.rs`, `crates/recall-pipeline/src/lib.rs`

**Interfaces:**
- Consumes: `read_events`, `NamedBy`, `Meeting::participants_hint`.
- Produces:
  - `pub struct Interval { pub start_ms: i64, pub end_ms: i64, pub name: String }`
  - `pub fn speaking_intervals(events: &[MeetEvent]) -> Vec<Interval>`: pairs start/stop per participant, resolves the participant to its latest `Name` event (unnamed participants are dropped), closes an open interval at the participant's next start or at the last event time, then removes every span where two or more names overlap (splitting intervals as needed).
  - `pub fn vote(clusters: &[Cluster], segments: &[Segment], intervals: &[Interval]) -> Vec<Option<String>>` (one per cluster): a segment belongs to cluster `i` when its speaker is `cluster:{i}` or equals `clusters[i].speaker`; per name sum overlap ms with the cluster's segments; the winner needs ≥ 30,000 ms and ≥ 60% of the cluster's total overlapped ms.
  - `pub fn roster(events: &[MeetEvent]) -> Vec<String>`: union of `Roster` and `Name` names, first-seen order, deduplicated.
  - Runner `name()` order: `name_clusters` (sets `named_by = Voiceprint`), then Meet vote: for a cluster with a winning name, find a named person with that name or alias (case-insensitive) or `speakers.add(name, None)`, set `cluster.speaker` (overriding a voiceprint match) and `named_by = Meet`, and `add_embedding` the cluster's embedding; then `apply_names`; set `participants_hint = roster(..)`.

- [ ] **Step 1: Tests** in `meet_vote.rs`:
  - `overlapping_speech_is_dropped` (A 0–10 s, B 5–15 s → A 0–5 s, B 10–15 s).
  - `rejoin_with_new_csrc_maps_by_name` (csrc:1 and csrc:9 both named "Tom" → one name in the vote).
  - `vote_needs_30s_and_60_percent` (29 s alone → None; 40 s Tom + 30 s Amy → None; 40 s Tom + 10 s Amy → Tom).
  - `meet_overrides_voiceprint_and_enrols_embedding` (runner-level test with a fake meeting dir: the voiceprint matched Amy, Meet says Tom → Tom, `named_by = Meet`, Tom gains the embedding, Amy does not).
  - `no_events_file_keeps_old_behaviour`.
- [ ] **Step 2:** Implement; `cargo test -p recall-pipeline` green.
- [ ] **Step 3:** Commit "Name remote speakers from Meet speaking events".

### Task 6: The Meet extension

**Files:**
- Create: `extension/manifest.json`, `extension/package.json`, `extension/vite.config.ts`, `extension/src/hook.ts` (MAIN world), `extension/src/content.ts` (isolated world), `extension/src/worker.ts`, `extension/src/csrc.ts`, `extension/src/names.ts`, `extension/src/queue.ts`, tests beside each as `*.test.ts`, `extension/test/fixtures/*.html` (saved Meet DOM snippets: grid with self tile, speaking glow, People panel)
- Build output: `extension/dist/`, committed, loaded with "Load unpacked".

**Interfaces:**
- `manifest.json`: MV3, the Global Constraints `key`, `host_permissions: ["http://127.0.0.1:7474/*"]`, content scripts on `https://meet.google.com/*`: `hook.js` with `"world": "MAIN", "run_at": "document_start"`, `content.js` isolated at `document_idle`; background service worker `worker.js`.
- `hook.ts`: wraps `window.RTCPeerConnection` once (guard `window.__recallHooked`); every 100 ms polls each audio receiver's `getContributingSources()`; a CSRC is speaking while `audioLevel ≥ 0.02` and stops after 500 ms below; posts `{source: "recall", t, kind, participant: "csrc:<n>"}` via `window.postMessage`. Reports `status: "hook_failed"` if wrapping throws.
- `names.ts`: `correlate(samples)`: counts (CSRC, tile name) pairs only when exactly one CSRC is speaking and exactly one non-self tile glows; a CSRC takes the most counted name, emitted as a `name` event when it changes. Tile/name/self/glow selectors live in one exported `SELECTORS` object (`div[data-participant-id]`, `span.notranslate`, `[data-self-name]`, glow classes `Oaajhc HX2H7 wEsLMd OgVli`). Glow-only fallback when `hook_failed`: a glowing tile alone emits speaking events with `participant: "tile:<data-participant-id>"` and its name.
- `content.ts`: roster from tiles and the People panel every 10 s (emits `roster` on change); `signal_lost` after 300,000 ms with no speaking event while any remote CSRC has audio; forwards everything to the worker with `chrome.runtime.sendMessage`.
- `queue.ts`: keeps the last 60,000 ms of events; the worker POSTs `{meeting_code, events}` (code from the URL path) every 2 s and on page hide; failures are retried on the next tick.

- [ ] **Step 1: Tests (Vitest, jsdom):** `hook polls csrc and emits start/stop with 500 ms hangover` (fake `RTCPeerConnection` + fake timers), `hook wraps only once`, `correlate ignores ambiguous samples and picks majority`, `self tile excluded`, `queue resends only the last 60 s`, `signal_lost after 300 s of silence with remote audio`, `glow fallback after hook_failed`.
- [ ] **Step 2:** Implement; `npm test` and `npm run build` in `extension/`.
- [ ] **Step 3:** Add an `extension` job step to `.github/workflows/ci.yml` (`npm ci && npm test && npm run build && git diff --exit-code dist`).
- [ ] **Step 4:** Commit "Add the Google Meet extension".

### Task 7: Screen recording

**Files:**
- Create: `crates/recall/src/screen.rs`
- Modify: `crates/recall/src/recorder.rs`, `crates/recall/src/daemon.rs`, `crates/recall/src/control.rs`, `crates/recall/src/main.rs`, `crates/recall/src/notify.rs`, `crates/recall-server/src/lib.rs` (`ControlCommand::ScreenOff`)

**Interfaces:**
- Consumes: `MeetingDir::{screen, screen_cache, clock}`, `CaptureClock`, `Meeting::{screen_offset_ms, screen_error}`, `StatusSnapshot::screen`.
- Produces:
  - `pub struct ScreenRecorder` with `pub fn start(program: &Path, out: &Path, token: &Path) -> Result<Self>`, `pub fn is_running(&mut self) -> bool`, `pub fn stop(self) -> Result<()>` (SIGINT via `rustix::process::kill_process`, wait up to 10 s, then SIGKILL), `pub fn discard(self) -> Result<()>` (stop and delete `screen.mkv` and `screen.mkv.ts`).
  - `pub fn finish(cache_mkv: &Path, out_mp4: &Path, clock: &CaptureClock) -> Result<Option<i64>>`: remux with `ffmpeg -y -v error -i <mkv> -c copy -movflags +faststart <mp4>`, then compute `screen_offset_ms = (first_frame_monotonic_us - clock.monotonic_us) / 1000` from `screen.mkv.ts` (header line, then `<monotonic_us>\t<realtime_us>`; missing file → `None`).
  - `pub fn token_status(token: &Path) -> &'static str`: `"ok"` or `"picker"` (missing or empty file).
  - Program paths come from `RECALL_GSR` / `RECALL_FFMPEG` env vars, defaulting to `gpu-screen-recorder` / `ffmpeg`, so tests use fake scripts. Token at `$XDG_STATE_HOME/recall/portal-token` (default `~/.local/state/recall/portal-token`).
  - Recorder: starts the screen recorder after the audio capture when `config.record_screen`; a start failure or early exit sets `screen_error` and notifies, audio continues. `keep()` and `recover_dir()` call `finish` before `clear_cache`; a remux failure keeps the MKV by moving it to `screen.mkv` in the meeting directory and records `screen_error`.
  - `ControlCommand::ScreenOff` / `POST /api/record/screen_off` / `recall screen off`: discards the live screen recording; no-op when none.
  - `StatusSnapshot.screen`: `"recording"`, `"off"`, `"error"` while live; `"picker"` when idle and the token is missing.

- [ ] **Step 1: Tests** (fake `gpu-screen-recorder` shell script that writes a file, traps SIGINT and writes `.ts`; fake ffmpeg that copies input to output):
  - `stop_sends_sigint_and_keeps_output`, `stubborn_child_is_killed_after_timeout` (fake ignores SIGINT; use a 200 ms timeout via a test-only constructor).
  - `finish_remuxes_and_computes_offset` (`.ts` monotonic 5,250,000 µs, clock 5,000,000 µs → 250).
  - `recover_remuxes_before_clearing_cache`.
  - `start_failure_keeps_audio_and_records_error`.
  - `screen_off_deletes_video`.
  - `token_status_reports_picker_for_empty_token`.
- [ ] **Step 2:** Implement; `cargo test -p recall` green.
- [ ] **Step 3:** Manual check by the controller on this machine: `record_screen = true`, hotkey a 30 s recording, confirm `screen.mp4` plays and `screen_offset_ms` is set. Record the result in the ledger.
- [ ] **Step 4:** Commit "Record the screen beside the audio".

### Task 8: Web UI for video, naming hints and status

**Files:**
- Create: `web/src/components/ScreenVideo.svelte`
- Modify: `web/src/routes/Meeting.svelte`, `web/src/components/Transcript.svelte` (rename control), `web/src/components/RecordStatus.svelte`, `web/src/components/StateChip.svelte`, `web/src/lib/api.ts`, `web/src/lib/player.ts`, `web/src/i18n/en.json`, `web/src/i18n/zh-TW.json`, `crates/recall-server/src/meetings.rs` (serve `screen.mp4` at `GET /api/meetings/{id}/screen` with range support like audio)
- Rebuild `web/dist`.

**Interfaces:**
- Video shows only when the meeting has `screen.mp4`; seeking the waveform or clicking a transcript line sets `video.currentTime = (audio_ms - screen_offset_ms) / 1000` (clamped at 0); playing either keeps the other in sync within 300 ms.
- Rename control lists `participants_hint` names not yet used by another speaker in this meeting as one-click choices above the free-text field.
- Status chip: shows `meet === "signal_lost"` and `"hook_failed"` as a warning, `screen === "picker"` as "the screen picker will appear next time", and a "Stop screen recording" button while `screen === "recording"` (calls `POST /api/record/screen_off`).
- Cluster labels show a small "via Meet" marker when `named_by === "meet"`.

- [ ] **Step 1: Tests (Vitest):** `player maps audio time to video time with offset`, `rename suggestions exclude used names`, `status chip shows meet and screen warnings`; server test `screen_served_with_range`.
- [ ] **Step 2:** Implement, both locales; `npm test`, `npm run build`; `cargo test -p recall-server`.
- [ ] **Step 3:** Commit "Show screen video, Meet naming hints and status in the UI".

### Task 9: `recall mcp`

**Files:**
- Create: `crates/recall-mcp/` (`Cargo.toml`, `src/lib.rs`, `src/client.rs`, `src/tools.rs`, `tests/tools.rs`)
- Modify: workspace `Cargo.toml`, `crates/recall/src/main.rs` (`Mcp` subcommand), `crates/recall/Cargo.toml`

**Interfaces:**
- Consumes: daemon endpoints from the spec's MCP table plus Task 4's `/api/action-items`; base URL `http://127.0.0.1:{config.port}`.
- Produces: `pub async fn serve_stdio(base: Url) -> anyhow::Result<()>` built with `rmcp` 3.8 (`server`, `transport-io`, `macros` features) and `reqwest` (rustls).
  - Tools, names and annotations exactly as the spec table. Inputs: `search_meetings { query, limit? = 20 }`; `get_meeting { id }`; `get_transcript { id, cursor?, from_ms?, to_ms? }`; `list_action_items { owner?, from?, to?, due_from?, due_to? }`; `recording_status {}`; `start_recording {}`; `stop_recording {}`; `rerun_step { id, step }`; `rename_meeting { id, title }`; `name_speaker { person_id, name }`.
  - `get_transcript` returns lines `[mm:ss] Name: text`; pages end at a segment boundary when adding the next segment would pass 20,000 estimated tokens (`chars / 2`); `next_cursor` is the index of the next segment as a string, absent on the last page. A single segment over the cap is returned alone.
  - `list_action_items` and `recording_status` return `structuredContent` with an `outputSchema`.
  - Connection refused → tool error text `recall daemon is not running; start it with: systemctl --user start recall`. HTTP 409 → `meeting is being processed; try again when it is done`.

- [ ] **Step 1: Tests** (`tests/tools.rs`: start `recall_server::build_router` with a temp meetings dir and fake closures on an ephemeral port, connect an rmcp client over an in-process duplex transport): `lists_all_tools_with_annotations` (and no tool named like discard/delete), `transcript_pages_cut_on_segment_boundaries` (2-hour synthetic meeting; concatenated pages equal the full transcript; last page has no cursor), `action_items_structured`, `daemon_down_error_message` (closed port), `busy_meeting_maps_409`.
- [ ] **Step 2:** Implement; `cargo test -p recall-mcp`, clippy clean.
- [ ] **Step 3:** Commit "Add recall mcp, a stdio MCP server over the daemon API".

### Task 10: Code-switched bench case from CSZS

**Files:**
- Modify: `bench/fixtures/zh-en/` (add `mic.ogg` and `computer.ogg`, rewrite `truth.json` without `skip`), `bench/thresholds.json`, `NOTICE`

- [ ] **Step 1:** Download 5 clips (about 60 s total) from `ky552/cszs_zh_en` (MIT) into the scratchpad with `huggingface_hub`; concatenate with 1 s gaps into `computer.ogg` (48 kHz mono Opus) and write a matching-length `mic.ogg` of silence; write `truth.json` lines (`side: "them"`) from the dataset references converted to Traditional Chinese with OpenCC `s2tw`.
- [ ] **Step 2:** Run `python3 bench/run.py --case zh-en` and set the case's thresholds in `thresholds.json`: `found` min = measured minus 0.05 (rounded down to two decimals), `side` min 0.95, `leaked` max 0.
- [ ] **Step 3:** Add the CSZS attribution (dataset name, URL, MIT) to `NOTICE`; `python3 -m unittest bench/test_run.py` and `bench/run.py --check` pass.
- [ ] **Step 4:** Commit "Add a code-switched bench case from CSZS".

### Task 11: TEA-ASR spike

**Files:**
- Create: `docs/decisions/2026-10-28-tea-asr-spike.md` (the only committed file)
- Scratchpad: the spike program and data.

- [ ] **Step 1:** Build a scratch Rust binary using the `crispasr` crate (CPU) with `dseditor/TEA-ASR-1.1-CrispASR-GGUF` Q8_0 and Qwen3-ForcedAligner-0.6B. If it cannot build or load within an hour of effort, switch to `transcribe-cpp` with `mukiwu/TEA-ASR-1.1-transcribe-gguf` and record why.
- [ ] **Step 2:** Test set: 300 clips from the ASCEND test split and 300 from CSZS zh-en. Transcribe each with TEA and with the repo's Breeze path (`recall`'s transcribe code via a scratch harness or the bench runner), score mixed error rate the same way as `bench/run.py`.
- [ ] **Step 3:** Measure real-time factor, peak RSS, and run 1 hour of silence plus segments over 30 s for repetition loops or empty output.
- [ ] **Step 4:** Write the decision record: setup, numbers in a table, go/no-go against the spec's four criteria, and the ggml-clash note. Commit "Record the TEA-ASR spike result".

### Task 12: Documentation

**Files:**
- Modify: `README.md`, `packaging/install.sh` (no change unless the extension needs copying; it does not)

- [ ] **Step 1:** README sections: installing the extension (Load unpacked → `extension/dist`), what it reads and that data stays local; screen recording (setting, first-run picker, `screencopy:allow_token_by_default = true` for xdph, `recall screen off`); MCP setup (`claude mcp add recall -- recall mcp`, tool list, no delete tool); bench case attribution pointer. Known limitations: Meet DOM changes can stop naming (status chip warns), Slack/Discord have no extension.
- [ ] **Step 2:** Commit "Document the Meet extension, screen recording and MCP".

## Execution order

Task 1 first. Then tracks that touch disjoint files run in parallel:

- Daemon track: Task 2 → Task 3 → Task 4 → Task 7 → Task 9 (4, 7 and 9 all touch `daemon.rs`, `main.rs` or the server's `lib.rs`, so they run one after another).
- Pipeline track: Task 5 after Task 2.
- Extension track: Task 6 after Task 1.
- Independent: Task 10, Task 11.
- Task 8 after Tasks 5 and 7. Task 12 last.
