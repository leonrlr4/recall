# recall phases 1 to 3 implementation plan

> Historical record, frozen 2026-10-27. Source code is authoritative; where
> this document and the code disagree, the code wins.

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A Rust daemon that detects meetings on PipeWire, records mic and system audio as two tracks, transcribes them with Breeze-ASR-25 into Traditional Chinese and English, diarizes remote speakers with voiceprints, summarizes with Ollama or Claude, and serves a searchable local web archive.

**Architecture:** Cargo workspace of six crates. `recall-store` owns the on-disk format and SQLite index and has no audio or network dependency, so every other crate tests against it with temp dirs. `recall-audio` owns PipeWire. `recall-pipeline` owns the six processing steps behind traits so tests use fakes. `recall-llm` is one trait with two backends. `recall-server` is axum over the store. `recall` is the binary that wires them into a daemon and CLI.

**Tech Stack:** Rust 2024 edition, tokio 1, pipewire 0.10.1, opus 0.4 + ogg 0.9, whisper-rs 0.16 (feature `vulkan` optional), sherpa-onnx 2.0.1 (static), opencc 0.3 (system libopencc), rusqlite 0.41 (bundled, FTS5), axum 0.8, rust-embed 8, notify-rust 4, clap 4, serde, toml 1, tracing. Frontend Svelte 5 + Vite 7, TypeScript.

**Spec:** `docs/decisions/specs/2026-10-27-recall-design.md`

## Global constraints

- Everything that reaches the repo is in English: code, comments, docs, commit messages. No AI attribution anywhere.
- Audio on disk: `audio.opus`, Ogg Opus, stereo 48 kHz, left = mic, right = system.
- Meeting directory name: `YYYY-MM-DD HH:MM <title>` under the configured meetings dir (default `~/Meetings`).
- Chinese output is Traditional; every transcript string passes OpenCC `s2tw` before it is stored.
- Server binds `127.0.0.1:7474` only.
- Detection constants: Armed debounce 8 s, end grace 30 s, minimum kept length 120 s, cache chunk 20 s.
- `whisper-rs` feature `vulkan` is opt-in (`--features vulkan`); the default build is CPU because the dev machine lacks `vulkan-headers` at the time of writing.
- Models are downloaded on first use into `~/.local/share/recall/models/`, never bundled.
- Every crate has `#![deny(warnings)]` in CI via `RUSTFLAGS="-D warnings"`; clippy must be clean.

## Review focus

1. A meeting directory whose title contains `/` or is empty: `MeetingDir::create` must sanitize to a safe name and fall back to `Meeting HH:MM`. Test in Task 2.
2. Two meetings starting in the same minute with the same title: the second gets a ` (2)` suffix rather than overwriting. Test in Task 2.
3. The daemon restarting with an unfinished `.cache/`: the recording is finalized and queued, not lost or recorded over. Test in Task 7.
4. The default sink changing mid-recording: the system track reconnects and the two tracks stay the same length. Test in Task 6.
5. The LLM returning malformed JSON or JSON wrapped in a code fence: the parser strips fences and reports a step failure with the raw text saved, never a panic. Test in Task 11.

---

## File structure

```
recall/
├── Cargo.toml                      workspace
├── rust-toolchain.toml
├── .github/workflows/ci.yml
├── README.md
├── crates/
│   ├── recall-store/src/
│   │   ├── lib.rs                  re-exports
│   │   ├── meeting.rs              Meeting, Segment, Step, StepState; meeting.json read/write
│   │   ├── dir.rs                  MeetingDir: naming, create, list, cache
│   │   ├── markdown.rs             render meeting.md from Meeting + speaker names
│   │   ├── speakers.rs             Speakers (speakers.json): Person, embeddings, rename, merge
│   │   ├── index.rs                SQLite index + FTS5 trigram; rebuild from dirs; job queue
│   │   └── config.rs               Config (config.toml) with defaults and paths
│   ├── recall-audio/src/
│   │   ├── lib.rs
│   │   ├── detect.rs               Detector: PipeWire registry watcher → DetectEvent
│   │   ├── state.rs                Session state machine (pure, no PipeWire)
│   │   ├── capture.rs              two capture streams → f32 chunks with silence fill
│   │   ├── writer.rs               ChunkWriter (.cache/*.f32) and finalize → audio.opus
│   │   └── opus.rs                 OggOpusWriter
│   ├── recall-llm/src/
│   │   ├── lib.rs                  Llm trait, Backend enum, from_config
│   │   ├── ollama.rs
│   │   ├── claude.rs
│   │   └── prompts.rs              summary + ask prompts, SummaryJson schema, parse_summary
│   ├── recall-pipeline/src/
│   │   ├── lib.rs                  run_step(meeting, step) dispatcher, Engines struct
│   │   ├── decode.rs               audio.opus → (left, right) Vec<f32> 16 kHz
│   │   ├── vad.rs                  Vad trait, SileroVad, Region, echo rule
│   │   ├── transcribe.rs           Transcriber trait, WhisperTranscriber (Breeze), to_traditional
│   │   ├── diarize.rs              Diarizer trait, SherpaDiarizer, Cluster
│   │   ├── naming.rs               match clusters to people (cosine), vote
│   │   ├── merge.rs                merge two channel transcripts into one timeline
│   │   ├── summarize.rs            chunk by chapter, call Llm, fill Meeting.summary
│   │   └── models.rs               model registry: names, URLs, download with progress
│   ├── recall-server/src/
│   │   ├── lib.rs                  build_router(AppState) → axum::Router
│   │   ├── meetings.rs             /api/meetings handlers
│   │   ├── people.rs               /api/people handlers
│   │   ├── events.rs               SSE broadcaster
│   │   ├── audio.rs                Range streaming
│   │   ├── ask.rs                  /ask SSE
│   │   └── static_files.rs         rust-embed of web/dist
│   └── recall/src/
│       ├── main.rs                 clap: daemon, start, stop, discard, list, show, rerun, selftest, index rebuild
│       ├── daemon.rs               wires detector, recorder, queue worker, server
│       ├── control.rs              control.sock server/client (newline JSON)
│       ├── notify.rs               desktop notifications + state.json
│       └── worker.rs               queue worker: pops jobs, runs pipeline steps
├── web/                            Svelte 5 + Vite; dist/ committed
│   └── src/
│       ├── lib/api.ts, lib/i18n.ts, lib/player.ts
│       ├── i18n/zh-TW.json, i18n/en.json
│       └── routes: List.svelte, Meeting.svelte, People.svelte, Settings.svelte
├── bench/                          fixtures, truth.json, thresholds.json, run.py
└── packaging/recall.service        systemd user unit
```

Task order and parallelism: Tasks 1 to 3 (store, config) first. Then Tasks 4 to 7 (audio + daemon phase 1), Tasks 8 to 12 (pipeline + llm) and Tasks 13 to 16 (server + web) are three independent tracks that only consume `recall-store` types; run them in parallel. Task 17 (CLI wiring and end-to-end) and Task 18 (bench, CI, README) go last.

---

### Task 1: Workspace scaffold and store types

**Files:**
- Create: `Cargo.toml`, `rust-toolchain.toml`, `.gitignore`, `crates/recall-store/Cargo.toml`, `crates/recall-store/src/lib.rs`, `crates/recall-store/src/meeting.rs`
- Test: `crates/recall-store/src/meeting.rs` (unit tests in module)

**Interfaces:**
- Produces:
  ```rust
  pub struct MeetingId(pub String);              // dir name, e.g. "2026-10-27 14:30 Weekly sync"
  pub enum Step { Segment, Transcribe, Diarize, Name, Summarize, Write }
  pub enum StepState { Pending, Running, Done { at: DateTime<Utc> }, Failed { at: DateTime<Utc>, error: String } }
  pub enum Side { Me, Them }
  pub struct Segment { pub start_ms: u32, pub end_ms: u32, pub side: Side, pub speaker: Option<SpeakerId>, pub text: String }
  pub struct SpeakerId(pub String);              // "me" or uuid
  pub struct Cluster { pub label: String, pub embedding: Vec<f32>, pub speaker: Option<SpeakerId> }
  pub struct ActionItem { pub text: String, pub owner: Option<String>, pub due: Option<String> }
  pub struct Chapter { pub start_ms: u32, pub title: String }
  pub struct Summary { pub one_line: String, pub body: String, pub decisions: Vec<String>, pub actions: Vec<ActionItem>, pub chapters: Vec<Chapter> }
  pub struct Meeting { pub id: MeetingId, pub title: String, pub started: DateTime<Utc>, pub duration_ms: u32, pub source_app: Option<String>, pub steps: BTreeMap<Step, StepState>, pub segments: Vec<Segment>, pub clusters: Vec<Cluster>, pub summary: Option<Summary>, pub models: BTreeMap<String, String> }
  impl Meeting { pub fn load(dir: &Path) -> Result<Self>; pub fn save(&self, dir: &Path) -> Result<()>; pub fn participants(&self) -> Vec<SpeakerId>; }
  ```
  `Result` is `anyhow::Result` throughout the workspace.

- [ ] **Step 1: Write the failing test** `meeting_json_round_trips` in `meeting.rs`: build a `Meeting` with two segments (one `Me`, one `Them` with a speaker), one cluster, a summary with one action item, `steps` with `Transcribe: Done`, save to a tempdir, load, assert equal. Also `missing_meeting_json_is_an_error`.
- [ ] **Step 2: Run** `cargo test -p recall-store` → fails to compile.
- [ ] **Step 3: Implement** the types with `serde` derives (`#[serde(rename_all = "snake_case")]`, `Step` as a string key), `load`/`save` writing pretty JSON atomically (write `meeting.json.tmp`, rename).
- [ ] **Step 4: Run** `cargo test -p recall-store` → PASS.
- [ ] **Step 5: Commit** `Add workspace and the meeting record type`.

### Task 2: Meeting directories and cache

**Files:**
- Create: `crates/recall-store/src/dir.rs`
- Test: unit tests in module

**Interfaces:**
- Produces:
  ```rust
  pub struct MeetingDir { pub root: PathBuf, pub id: MeetingId }
  impl MeetingDir {
      pub fn create(root: &Path, started: DateTime<Local>, title: &str) -> Result<Self>;  // sanitizes, suffixes " (2)" on collision
      pub fn open(root: &Path, id: &MeetingId) -> Result<Self>;
      pub fn list(root: &Path) -> Result<Vec<MeetingId>>;          // newest first, only dirs with meeting.json
      pub fn path(&self) -> PathBuf;
      pub fn audio(&self) -> PathBuf;      // audio.opus
      pub fn markdown(&self) -> PathBuf;   // meeting.md
      pub fn cache(&self) -> PathBuf;      // .cache
      pub fn has_unfinished_cache(&self) -> bool;
      pub fn clear_cache(&self) -> Result<()>;
      pub fn rename(self, title: &str) -> Result<Self>;            // renames the directory, keeps the time prefix
  }
  pub fn sanitize_title(title: &str) -> String;   // strips `/`, `\0`, trims, collapses whitespace, max 80 chars, empty → "Meeting"
  ```

- [ ] **Step 1: Tests** `create_names_dir_by_time_and_title` (`2026-10-27 14:30 Weekly sync`), `create_sanitizes_slash_and_empty` (`a/b` → `a b`; `""` → `Meeting`), `create_suffixes_on_collision` (second call → `... (2)`), `list_is_newest_first_and_skips_dirs_without_json`, `rename_keeps_time_prefix`.
- [ ] **Step 2: Run** → FAIL.
- [ ] **Step 3: Implement** `dir.rs`.
- [ ] **Step 4: Run** → PASS.
- [ ] **Step 5: Commit** `Add meeting directories with safe names`.

### Task 3: Speakers, markdown, config, index

**Files:**
- Create: `crates/recall-store/src/speakers.rs`, `markdown.rs`, `config.rs`, `index.rs`
- Test: unit tests in each module

**Interfaces:**
- Produces:
  ```rust
  // speakers.rs
  pub struct Person { pub id: SpeakerId, pub name: String, pub aliases: Vec<String>, pub embeddings: Vec<Vec<f32>> }
  pub struct Speakers { people: Vec<Person> }
  impl Speakers {
      pub fn load(path: &Path) -> Result<Self>;   // missing file → one Person { id: "me", name: "Me" }
      pub fn save(&self, path: &Path) -> Result<()>;
      pub fn me(&self) -> &Person;
      pub fn get(&self, id: &SpeakerId) -> Option<&Person>;
      pub fn name_of(&self, id: &SpeakerId) -> String;   // falls back to id
      pub fn add(&mut self, name: &str, embedding: Option<Vec<f32>>) -> SpeakerId;
      pub fn rename(&mut self, id: &SpeakerId, name: &str) -> Result<()>;
      pub fn add_embedding(&mut self, id: &SpeakerId, e: Vec<f32>, max_per_person: usize /* 10 */);
      pub fn merge(&mut self, keep: &SpeakerId, drop: &SpeakerId) -> Result<()>;  // moves embeddings + aliases
      pub fn best_match(&self, e: &[f32], threshold: f32) -> Option<(SpeakerId, f32)>;  // max cosine over all embeddings
  }
  pub fn cosine(a: &[f32], b: &[f32]) -> f32;

  // markdown.rs
  pub fn render(meeting: &Meeting, speakers: &Speakers, lang: &str /* "zh-TW" | "en" */) -> String;
  // frontmatter keys: title, date (RFC3339), duration (mm:ss), participants (names), source_app, tags: []
  // headings zh-TW: ## 摘要 / ## 決策 / ## 待辦 / ## 章節 / ## 逐字稿 ; en: ## Summary / ## Decisions / ## Action items / ## Chapters / ## Transcript
  // transcript line: "[mm:ss] **Name** text"; action item: "- [ ] text (owner, due)"
  pub fn clock(ms: u32) -> String;   // "01:32"; hours shown as "1:02:03" past 60 min

  // config.rs
  pub struct Config { pub meetings_dir: PathBuf, pub data_dir: PathBuf, pub record_screen: bool, pub allowlist: Vec<String>, pub debounce_secs: u64, pub end_grace_secs: u64, pub min_length_secs: u64, pub llm: LlmConfig, pub summary_lang: String, pub ui_lang: Option<String>, pub stt: SttConfig, pub port: u16 }
  pub struct LlmConfig { pub backend: String /* "ollama" | "claude" */, pub model: String, pub endpoint: String }
  pub struct SttConfig { pub engine: String /* "breeze" */, pub threads: usize }
  impl Config { pub fn load() -> Result<Self>; pub fn load_from(path: &Path) -> Result<Self>; pub fn path() -> PathBuf; pub fn speakers_path(&self) -> PathBuf; pub fn index_path(&self) -> PathBuf; pub fn models_dir(&self) -> PathBuf; }
  impl Default for Config  // meetings_dir ~/Meetings, data_dir ~/.local/share/recall, allowlist [brave, chromium, chrome, firefox, zoom, slack, discord, teams], debounce 8, grace 30, min 120, llm ollama qwen3.6:35b-a3b http://127.0.0.1:11434, summary_lang zh-TW, stt breeze threads 8, port 7474

  // index.rs
  pub struct Index { conn: rusqlite::Connection }
  pub struct MeetingRow { pub id: MeetingId, pub title: String, pub started: DateTime<Utc>, pub duration_ms: u32, pub source_app: Option<String>, pub one_line: Option<String>, pub participants: Vec<String>, pub state: String /* "recording"|"processing:<step>"|"done"|"failed:<step>" */ }
  pub struct Hit { pub meeting: MeetingId, pub start_ms: u32, pub speaker: String, pub snippet: String }
  pub struct Job { pub id: i64, pub meeting: MeetingId, pub from_step: Step, pub attempts: u32 }
  impl Index {
      pub fn open(path: &Path) -> Result<Self>;        // creates schema; FTS5 table segments(meeting, start_ms, speaker, text) tokenize='trigram'
      pub fn upsert(&self, m: &Meeting, speakers: &Speakers) -> Result<()>;
      pub fn remove(&self, id: &MeetingId) -> Result<()>;
      pub fn list(&self, filter: &ListFilter) -> Result<Vec<MeetingRow>>;
      pub fn search(&self, q: &str, limit: usize) -> Result<Vec<Hit>>;
      pub fn rebuild(&self, root: &Path, speakers: &Speakers) -> Result<usize>;
      pub fn enqueue(&self, meeting: &MeetingId, from: Step) -> Result<i64>;
      pub fn next_job(&self) -> Result<Option<Job>>;   // oldest pending, marks running
      pub fn finish_job(&self, id: i64, ok: bool, error: Option<&str>) -> Result<()>;
      pub fn reset_running_jobs(&self) -> Result<()>;  // on daemon start
  }
  pub struct ListFilter { pub q: Option<String>, pub app: Option<String>, pub participant: Option<String>, pub from: Option<DateTime<Utc>>, pub to: Option<DateTime<Utc>> }
  ```

- [ ] **Step 1: Tests**: `speakers_default_has_me`, `rename_and_merge_move_embeddings`, `best_match_picks_highest_cosine_above_threshold` (two people, embeddings `[1,0]` and `[0,1]`, query `[0.9,0.1]` with threshold 0.6 → first; threshold 0.95 → None); `markdown_renders_frontmatter_and_zh_headings`, `markdown_en_headings`, `clock_formats_hours`; `config_defaults_when_file_missing`, `config_parses_partial_toml` (only `[llm] backend = "claude"` set → rest default); `index_search_finds_chinese_substring` (segment text `這段 export 會 timeout`, query `export 會` → one hit), `index_list_filters_by_app`, `index_rebuild_counts_dirs`, `queue_pops_oldest_and_resets_running`.
- [ ] **Step 2: Run** → FAIL.
- [ ] **Step 3: Implement** the four modules. Schema in `index.rs` as a `const SCHEMA: &str` with `meetings`, `segments` (FTS5), `jobs` tables and `PRAGMA journal_mode=WAL`. `upsert` deletes and reinserts that meeting's segments.
- [ ] **Step 4: Run** → PASS. Also `cargo clippy -p recall-store -- -D warnings`.
- [ ] **Step 5: Commit** `Add speakers, markdown rendering, config and the search index`.

### Task 4: Session state machine

**Files:**
- Create: `crates/recall-audio/Cargo.toml`, `src/lib.rs`, `src/state.rs`
- Test: unit tests in module

**Interfaces:**
- Produces:
  ```rust
  pub struct MicUser { pub node_id: u32, pub binary: String, pub app_name: String }
  pub enum DetectEvent { MicOpened(MicUser), MicClosed(u32), HotkeyStart, HotkeyStop, HotkeyDiscard, Tick(Instant) }
  pub enum Phase { Idle, Armed { since: Instant }, Recording { since: Instant, users: Vec<MicUser>, closed_at: Option<Instant> }, }
  pub enum Action { StartCache, StartRecording { source_app: String }, Finish, Discard, None }
  pub struct Session { phase: Phase, allowlist: Vec<String>, debounce: Duration, grace: Duration }
  impl Session {
      pub fn new(allowlist: Vec<String>, debounce: Duration, grace: Duration) -> Self;
      pub fn handle(&mut self, ev: DetectEvent) -> Action;
      pub fn phase(&self) -> &Phase;
  }
  ```
  Matching is `allowlist.iter().any(|a| binary.eq_ignore_ascii_case(a) || app_name.to_lowercase().contains(a))`.

- [ ] **Step 1: Tests**: `allowlisted_mic_arms_then_records_after_debounce` (MicOpened brave → `StartCache`, Tick +9 s → `StartRecording { source_app: "brave" }`), `probe_closed_within_debounce_discards` (open, close at +3 s → `Discard`), `non_allowlisted_app_is_ignored`, `second_app_joins_current_recording` (no new action), `close_then_grace_finishes` (MicClosed, Tick +31 s → `Finish`; Tick +10 s → `None`), `reopen_within_grace_cancels_finish`, `hotkey_start_from_idle_records_immediately`, `hotkey_discard_while_recording`.
- [ ] **Step 2: Run** → FAIL.
- [ ] **Step 3: Implement** `state.rs` as a pure function of events and `Instant`s.
- [ ] **Step 4: Run** → PASS.
- [ ] **Step 5: Commit** `Add the recording session state machine`.

### Task 5: Opus writer and chunk cache

**Files:**
- Create: `crates/recall-audio/src/opus.rs`, `src/writer.rs`
- Test: unit tests; `writer.rs` tests use synthetic sine data

**Interfaces:**
- Produces:
  ```rust
  pub struct OggOpusWriter { .. }
  impl OggOpusWriter {
      pub fn create(path: &Path, sample_rate: u32 /* 48000 */, channels: u8 /* 2 */, bitrate: i32 /* 64000 */) -> Result<Self>;
      pub fn write_interleaved(&mut self, pcm: &[f32]) -> Result<()>;   // buffers to 20 ms frames
      pub fn finish(self) -> Result<()>;
  }
  pub struct ChunkWriter { .. }   // writes .cache/mic-NNNNN.f32 and .cache/sys-NNNNN.f32 every 20 s of samples (960 000 f32)
  impl ChunkWriter {
      pub fn new(cache: &Path) -> Result<Self>;
      pub fn push(&mut self, track: Track, samples: &[f32]) -> Result<()>;
      pub fn flush(&mut self) -> Result<()>;
  }
  pub enum Track { Mic, Sys }
  pub fn finalize(cache: &Path, out: &Path) -> Result<Duration>;   // reads chunks in order, pads the shorter track with zeros, writes stereo Opus, returns duration
  pub fn cache_duration(cache: &Path) -> Result<Duration>;
  ```

- [ ] **Step 1: Tests**: `opus_file_decodes_with_ffprobe_as_stereo_48k` (skip if `ffprobe` missing; assert `channels=2`, `sample_rate=48000`), `chunks_roll_over_every_20s`, `finalize_pads_shorter_track` (mic 3 s, sys 2 s → duration 3 s, file exists), `finalize_on_empty_cache_is_error`.
- [ ] **Step 2: Run** → FAIL.
- [ ] **Step 3: Implement** with `opus::Encoder` (`Application::Voip`) and `ogg::PacketWriter`; OpusHead/OpusTags per RFC 7845 (pre-skip 312, granule position in 48 kHz units).
- [ ] **Step 4: Run** → PASS.
- [ ] **Step 5: Commit** `Add the Opus writer and the recording chunk cache`.

### Task 6: PipeWire detection and capture

**Files:**
- Create: `crates/recall-audio/src/detect.rs`, `src/capture.rs`
- Test: `crates/recall-audio/tests/pipewire.rs`, gated behind env `RECALL_PW_TESTS=1` (CI sets it with a null sink; skipped otherwise)

**Interfaces:**
- Consumes: `DetectEvent`, `MicUser`, `Track`, `ChunkWriter`.
- Produces:
  ```rust
  pub struct Detector { .. }
  impl Detector {
      pub fn spawn(tx: std::sync::mpsc::Sender<DetectEvent>, ignore_app_name: &str /* "recall" */) -> Result<Detector>;  // own thread with its own MainLoop
      pub fn stop(self);
  }
  pub struct Capture { .. }
  impl Capture {
      pub fn start(writer: Arc<Mutex<ChunkWriter>>, levels: Arc<Levels>) -> Result<Capture>;  // two streams, follows default sink via metadata
      pub fn stop(self) -> Result<()>;
  }
  pub struct Levels { pub mic: AtomicU32 /* f32 bits of peak */, pub sys: AtomicU32 }
  ```
  Detector emits `MicOpened` for globals of type Node with `media.class == "Stream/Input/Audio"` and `application.name != ignore_app_name`, `MicClosed(id)` on `global_remove`. Capture: mic stream props `MEDIA_TYPE=Audio, MEDIA_CATEGORY=Capture, MEDIA_ROLE=Communication, NODE_NAME=recall-mic, APP_NAME=recall`; sys stream adds `STREAM_CAPTURE_SINK=true`. Format F32LE 48 kHz; mic downmixed to mono, sys downmixed to mono. Silence fill: a `process` callback tracks expected samples from wall clock; a gap over 100 ms pushes zeros.

- [ ] **Step 1: Test** (gated) `capture_records_tone_played_to_null_sink`: create `pw-cli create-node adapter { factory.name=support.null-audio-sink node.name=recall-test-sink media.class=Audio/Sink }`, set it default with `wpctl set-default`, play a 440 Hz tone with `pw-play` for 2 s, assert the sys chunk RMS > 0.05 and mic chunk exists. `detector_sees_pw_record` spawns `pw-record --target 0 /dev/null`, expects `MicOpened` with binary `pw-record` within 2 s, kills it, expects `MicClosed`.
- [ ] **Step 2: Run** `RECALL_PW_TESTS=1 cargo test -p recall-audio --test pipewire` → FAIL.
- [ ] **Step 3: Implement** following `pipewire` crate examples `audio-capture.rs` and `pw-mon.rs`. Default-sink following: listen on the `default` metadata object for key `default.audio.sink`; on change, disconnect and reconnect the sys stream with `TARGET_OBJECT` set to the new sink name.
- [ ] **Step 4: Run** → PASS on this machine; ungated `cargo test -p recall-audio` still passes with tests skipped.
- [ ] **Step 5: Commit** `Detect meetings and capture both tracks from PipeWire`.

### Task 7: Daemon phase 1: recorder, control socket, notifications

**Files:**
- Create: `crates/recall/Cargo.toml`, `src/main.rs`, `src/daemon.rs`, `src/control.rs`, `src/notify.rs`, `src/recorder.rs`, `packaging/recall.service`
- Test: `crates/recall/src/control.rs` unit tests; `crates/recall/tests/recovery.rs`

**Interfaces:**
- Consumes: `Session`, `Detector`, `Capture`, `ChunkWriter`, `finalize`, `MeetingDir`, `Meeting`, `Index::enqueue`, `Config`.
- Produces:
  ```rust
  // control.rs: newline-delimited JSON over $XDG_RUNTIME_DIR/recall/control.sock
  pub enum Command { Start, Stop, Discard, Status }
  pub enum Reply { Ok, Status { phase: String, since_ms: u64, source_app: Option<String>, mic_peak: f32, sys_peak: f32 } }
  pub fn socket_path() -> PathBuf;
  pub async fn serve(path: &Path, tx: mpsc::Sender<DetectEvent>, status: Arc<RwLock<StatusSnapshot>>) -> Result<()>;
  pub fn send(cmd: Command) -> Result<Reply>;   // blocking client for the CLI

  // notify.rs
  pub fn recording_started(app: &str); pub fn processing(title: &str); pub fn summary_ready(title: &str, url: &str); pub fn failed(title: &str, step: &str);
  pub fn write_state(path: &Path, snapshot: &StatusSnapshot) -> Result<()>;   // $XDG_RUNTIME_DIR/recall/state.json

  // recorder.rs
  pub struct Recorder { .. }
  impl Recorder {
      pub fn new(config: Arc<Config>, index: Arc<Mutex<Index>>) -> Self;
      pub fn apply(&mut self, action: Action, now: DateTime<Local>) -> Result<()>;  // StartCache → MeetingDir::create(now, "Meeting HH:MM") + Capture::start; StartRecording → record source_app; Finish → stop, finalize, Meeting::save with steps Pending, enqueue(Segment), or delete dir if under min_length; Discard → stop + remove dir
      pub fn recover(config: &Config, index: &Index) -> Result<Vec<MeetingId>>;   // for each dir with unfinished cache: finalize + enqueue, or delete if under min length
  }

  // daemon.rs
  pub async fn run(config: Config) -> Result<()>;   // spawns Detector, control server, ticker (1 s), recorder loop, worker (Task 12), server (Task 13)
  ```

- [ ] **Step 1: Tests**: `control_round_trip_over_unix_socket` (serve on a tempdir path, send Status, get a Reply); `recovery_finalizes_unfinished_cache` (make a dir with 3 s of chunks → after `recover`, `audio.opus` exists, job enqueued) and `recovery_deletes_short_cache` (30 s of chunks with `min_length_secs = 120` → dir gone).
- [ ] **Step 2: Run** → FAIL.
- [ ] **Step 3: Implement**. `main.rs` subcommands: `daemon`, `start`, `stop`, `discard`, `status`. The service file: `[Service] ExecStart=%h/.cargo/bin/recall daemon`, `Restart=on-failure`, `[Install] WantedBy=default.target`.
- [ ] **Step 4: Run** tests → PASS. Manual smoke: `cargo run -- daemon` in one terminal, `pw-record --target 0 /dev/null` in another for 10 s, see the "Recording" notification and a `~/Meetings/... /.cache` directory.
- [ ] **Step 5: Commit** `Add the daemon with recording, hotkeys and notifications`.

### Task 8: LLM backends and summary prompt

**Files:**
- Create: `crates/recall-llm/Cargo.toml`, `src/lib.rs`, `src/ollama.rs`, `src/claude.rs`, `src/prompts.rs`
- Test: unit tests with a local `axum` mock server in `tests/backends.rs`; `prompts.rs` unit tests

**Interfaces:**
- Consumes: `LlmConfig`, `Summary`, `ActionItem`, `Chapter`.
- Produces:
  ```rust
  #[async_trait]
  pub trait Llm: Send + Sync {
      async fn complete(&self, system: &str, user: &str) -> Result<String>;
      async fn stream(&self, system: &str, user: &str, tx: mpsc::Sender<String>) -> Result<()>;
      fn name(&self) -> String;   // "ollama/qwen3.6:35b-a3b"
  }
  pub fn from_config(cfg: &LlmConfig) -> Result<Arc<dyn Llm>>;   // claude reads ANTHROPIC_API_KEY; missing key → error
  pub fn summary_prompt(lang: &str) -> String;     // system prompt; zh-TW demands Traditional Chinese, keep English terms and names verbatim, output JSON only
  pub fn transcript_text(segments: &[(String /*speaker*/, u32 /*start_ms*/, String)]) -> String;   // "[01:32] Tom: text" lines
  pub fn parse_summary(raw: &str) -> Result<Summary>;   // strips ``` fences and leading prose before the first `{`
  pub fn ask_prompt(lang: &str) -> String;
  ```
  Ollama uses `POST {endpoint}/api/chat` with `stream`, `think: false`, `keep_alive: "30m"`, `format: "json"` for `complete`. Claude uses `https://api.anthropic.com/v1/messages`, model from config (default `claude-sonnet-5-5`), `max_tokens 4096`, header `anthropic-version: 2023-06-01`.

- [ ] **Step 1: Tests**: `ollama_complete_posts_chat_and_returns_content` (mock returns `{"message":{"content":"hi"}}`), `claude_complete_sends_api_key_header`, `claude_without_key_errors`, `parse_summary_strips_code_fence`, `parse_summary_rejects_garbage_with_raw_in_error`, `transcript_text_formats_lines`.
- [ ] **Step 2: Run** → FAIL.
- [ ] **Step 3: Implement** with `reqwest` (rustls).
- [ ] **Step 4: Run** → PASS.
- [ ] **Step 5: Commit** `Add Ollama and Claude backends behind one trait`.

### Task 9: Audio decode, VAD and the echo rule

**Files:**
- Create: `crates/recall-pipeline/Cargo.toml`, `src/lib.rs`, `src/decode.rs`, `src/vad.rs`, `src/models.rs`
- Test: unit tests; `vad.rs` echo-rule tests are pure; Silero test gated on model presence

**Interfaces:**
- Produces:
  ```rust
  pub fn decode_stereo(path: &Path) -> Result<(Vec<f32>, Vec<f32>)>;   // 16 kHz mono per channel via `ffmpeg -i audio.opus -f f32le -ac 2 -ar 16000 -` then deinterleave
  pub struct Region { pub start_ms: u32, pub end_ms: u32 }
  pub trait Vad { fn regions(&self, samples16k: &[f32]) -> Result<Vec<Region>>; }
  pub struct SileroVad { .. }  impl SileroVad { pub fn load(models: &Path, threads: usize) -> Result<Self>; }
  pub fn drop_echo(mic: &[Region], sys: &[Region], tolerance_ms: u32 /* 150 */) -> Vec<Region>;   // keep a mic region unless ≥ 80% of it overlaps sys regions
  pub fn merge_close(regions: &[Region], gap_ms: u32 /* 300 */, max_len_ms: u32 /* 30_000 */) -> Vec<Region>;
  // models.rs
  pub enum ModelId { BreezeGgml, SileroVad, PyannoteSeg, CamPlusPlus }
  pub struct ModelSpec { pub id: ModelId, pub file: &'static str, pub url: &'static str, pub bytes: u64, pub sha256: Option<&'static str> }
  pub fn spec(id: ModelId) -> ModelSpec;
  pub fn path(models_dir: &Path, id: ModelId) -> PathBuf;
  pub async fn ensure(models_dir: &Path, id: ModelId, progress: impl Fn(u64, u64)) -> Result<PathBuf>;   // download to .part then rename
  ```
  Model URLs: Breeze `https://huggingface.co/shdennlin/breeze-asr-25-ggml/resolve/main/ggml-breeze-asr-25-q8_0.bin`; Silero `https://github.com/k2-fsa/sherpa-onnx/releases/download/asr-models/silero_vad.onnx`; pyannote `https://github.com/k2-fsa/sherpa-onnx/releases/download/speaker-segmentation-models/sherpa-onnx-pyannote-segmentation-3-0.tar.bz2` (extract `model.onnx`); CAM++ `https://github.com/k2-fsa/sherpa-onnx/releases/download/speaker-recongition-models/3dspeaker_speech_campplus_sv_zh_en_16k-common_advanced.onnx`. The implementer verifies each URL with `curl -sI` and fixes the file name if the listing differs.

- [ ] **Step 1: Tests**: `drop_echo_removes_region_fully_inside_sys`, `drop_echo_keeps_region_mostly_alone`, `merge_close_joins_gaps_under_300ms_and_splits_at_30s`, `decode_stereo_splits_channels` (generate a 1 s file with ffmpeg: left 440 Hz, right silence; assert left RMS > 0.1, right < 0.01; skip without ffmpeg), `silero_finds_speech_in_fixture` (gated: skip when model missing).
- [ ] **Step 2: Run** → FAIL.
- [ ] **Step 3: Implement**. `SileroVad` wraps `sherpa_onnx::VoiceActivityDetector` with `threshold 0.5, min_silence 0.25, min_speech 0.25, window 512`.
- [ ] **Step 4: Run** → PASS.
- [ ] **Step 5: Commit** `Add audio decoding, VAD and the echo rule`.

### Task 10: Transcription with Breeze and Traditional Chinese output

**Files:**
- Create: `crates/recall-pipeline/src/transcribe.rs`, `src/merge.rs`
- Test: `merge.rs` pure tests; `to_traditional` test; whisper test gated on model presence

**Interfaces:**
- Consumes: `Region`, `Segment`, `Side`.
- Produces:
  ```rust
  pub struct Word { pub start_ms: u32, pub end_ms: u32, pub text: String }
  pub struct Utterance { pub start_ms: u32, pub end_ms: u32, pub text: String, pub words: Vec<Word> }
  pub trait Transcriber: Send { fn transcribe(&mut self, samples16k: &[f32], regions: &[Region], lang: &str) -> Result<Vec<Utterance>>; fn model_name(&self) -> String; }
  pub struct WhisperTranscriber { .. }
  impl WhisperTranscriber { pub fn load(model: &Path, threads: usize) -> Result<Self>; }   // FullParams Greedy{best_of:1}, language Some(lang), token_timestamps true, no_context true, initial_prompt "繁體中文，English terms as is。"
  pub fn to_traditional(text: &str) -> String;   // OpenCC "s2tw.json", cached in a OnceLock
  pub fn merge_sides(me: Vec<Utterance>, them: Vec<Utterance>) -> Vec<Segment>;   // sorted by start; side Me/Them; speaker None
  ```

- [ ] **Step 1: Tests**: `to_traditional_converts_simplified` (`软件测试` → `軟體測試` is s2twp; with s2tw assert `测试` → `測試`), `merge_sides_interleaves_by_start`, `merge_sides_keeps_overlap_both` (one Me and one Them overlapping → two segments), `whisper_transcribes_fixture` (gated on `models/ggml-breeze-asr-25-q8_0.bin`; use `bench/fixtures/zh-en/mic.ogg`; assert output contains `export`).
- [ ] **Step 2: Run** → FAIL.
- [ ] **Step 3: Implement**. Regions are transcribed one at a time with 200 ms of padding each side; timestamps offset back to the file timeline.
- [ ] **Step 4: Run** → PASS.
- [ ] **Step 5: Commit** `Transcribe both tracks with Breeze into Traditional Chinese`.

### Task 11: Diarization, naming and summary steps

**Files:**
- Create: `crates/recall-pipeline/src/diarize.rs`, `src/naming.rs`, `src/summarize.rs`
- Test: `naming.rs` and `summarize.rs` pure tests; sherpa test gated

**Interfaces:**
- Consumes: `Llm`, `summary_prompt`, `parse_summary`, `Speakers`, `Cluster`, `Segment`.
- Produces:
  ```rust
  pub struct Turn { pub start_ms: u32, pub end_ms: u32, pub cluster: usize }
  pub trait Diarizer: Send { fn diarize(&mut self, samples16k: &[f32]) -> Result<(Vec<Turn>, Vec<Vec<f32>> /* centroid per cluster */)>; }
  pub struct SherpaDiarizer { .. }  impl SherpaDiarizer { pub fn load(models: &Path, threads: usize) -> Result<Self>; }  // clustering threshold 0.5, min_duration_on 0.3, off 0.5
  pub fn assign_clusters(segments: &mut [Segment], turns: &[Turn]) -> Vec<Cluster>;   // Them segments get the cluster with most overlap; labels "Remote 1".. in first-speech order; Me segments get speaker "me"
  pub fn name_clusters(clusters: &mut [Cluster], speakers: &Speakers, threshold: f32 /* 0.6 */);   // sets cluster.speaker by best_match
  pub fn apply_names(segments: &mut [Segment], clusters: &[Cluster]);
  pub async fn summarize(llm: &dyn Llm, segments: &[Segment], speakers: &Speakers, lang: &str, max_chars: usize /* 60_000 */) -> Result<Summary>;   // over max_chars: split at chapter-sized chunks of max_chars, summarize each, then one merge call
  pub async fn ask(llm: &dyn Llm, segments: &[Segment], speakers: &Speakers, question: &str, lang: &str, tx: mpsc::Sender<String>) -> Result<()>;
  ```

- [ ] **Step 1: Tests**: `assign_clusters_picks_max_overlap_and_labels_in_order`, `name_clusters_matches_known_voice_and_leaves_unknown`, `summarize_single_chunk_parses` (fake `Llm` returning fixed JSON), `summarize_chunks_long_transcript_and_merges` (fake counts calls: 3 chunks + 1 merge), `summarize_reports_bad_json_as_error_with_raw`.
- [ ] **Step 2: Run** → FAIL.
- [ ] **Step 3: Implement**. Diarizer embedding centroid: mean of `SpeakerEmbeddingExtractor::compute` over the three longest turns per cluster.
- [ ] **Step 4: Run** → PASS.
- [ ] **Step 5: Commit** `Add diarization, speaker naming and summaries`.

### Task 12: Step runner and queue worker

**Files:**
- Create: `crates/recall-pipeline/src/runner.rs`, `crates/recall/src/worker.rs`
- Test: `crates/recall-pipeline/tests/runner.rs` with fake engines

**Interfaces:**
- Consumes: everything in Tasks 8 to 11, `Index` job API, `markdown::render`, notifications.
- Produces:
  ```rust
  pub struct Engines { pub vad: Box<dyn Vad>, pub transcriber: Box<dyn Transcriber>, pub diarizer: Box<dyn Diarizer>, pub llm: Arc<dyn Llm> }
  impl Engines { pub async fn load(config: &Config, progress: impl Fn(&str, u64, u64)) -> Result<Self>; }   // downloads models via ensure()
  pub async fn run_from(engines: &mut Engines, config: &Config, dir: &MeetingDir, speakers: &mut Speakers, from: Step, on_progress: impl Fn(Step, f32)) -> Result<()>;
  // runs steps from `from` to Write in order; before each step sets StepState::Running and saves; on error sets Failed and returns Err; Write renders meeting.md and upserts the index
  // worker.rs
  pub async fn run(config: Arc<Config>, index: Arc<Mutex<Index>>, speakers: Arc<Mutex<Speakers>>, events: broadcast::Sender<Event>) -> Result<()>;   // loop: next_job → run_from → finish_job; sleeps 2 s when idle; Event::Progress { meeting, step, pct } and Event::Done / Event::Failed
  pub enum Event { Recording { app: String }, Stopped, Progress { meeting: MeetingId, step: Step, pct: f32 }, Done { meeting: MeetingId }, Failed { meeting: MeetingId, step: Step, error: String } }
  ```

- [ ] **Step 1: Tests**: `run_from_segment_reaches_write_with_fakes` (fixture 2 s stereo opus; fake transcriber returns one utterance per region; fake diarizer one cluster; fake llm fixed JSON → `meeting.md` exists, all steps Done, index has one row), `run_from_summarize_only_reruns_last_steps`, `failed_step_is_recorded_and_later_steps_stay_pending`.
- [ ] **Step 2: Run** → FAIL.
- [ ] **Step 3: Implement**.
- [ ] **Step 4: Run** → PASS.
- [ ] **Step 5: Commit** `Run pipeline steps from the job queue`.

### Task 13: HTTP API

**Files:**
- Create: `crates/recall-server/Cargo.toml`, `src/lib.rs`, `src/meetings.rs`, `src/people.rs`, `src/events.rs`, `src/audio.rs`, `src/ask.rs`, `src/static_files.rs`
- Test: `crates/recall-server/tests/api.rs` with `axum::serve` on an ephemeral port and `reqwest`

**Interfaces:**
- Consumes: `Index`, `Speakers`, `Meeting`, `MeetingDir`, `Event`, `ask`, `Llm`, `Config`.
- Produces:
  ```rust
  pub struct AppState { pub config: Arc<Config>, pub index: Arc<Mutex<Index>>, pub speakers: Arc<Mutex<Speakers>>, pub events: broadcast::Sender<Event>, pub llm: Arc<dyn Llm>, pub status: Arc<RwLock<StatusSnapshot>> }
  pub fn build_router(state: AppState) -> axum::Router;
  ```
  Routes and JSON shapes:
  - `GET /api/meetings?q&app&participant&from&to` → `Vec<MeetingRow>`; with `q` also `hits: Vec<Hit>` (response `{ meetings, hits }`).
  - `GET /api/meetings/:id` → `{ meeting: Meeting, names: {speaker_id: name}, markdown: String }`.
  - `PATCH /api/meetings/:id` body `{ title?: String, segments?: [{ index, text?, speaker?, delete? }] }` → updated `Meeting`; rewrites meeting.md and index.
  - `DELETE /api/meetings/:id` → 204; removes dir.
  - `POST /api/meetings/:id/rerun/:step` → 202 `{ job: i64 }`.
  - `GET /api/meetings/:id/audio` → `audio/ogg` with Range support (`tower_http::services::ServeFile`).
  - `POST /api/meetings/:id/ask` body `{ question }` → `text/event-stream`, events `data: <token>` then `event: done`.
  - `GET /api/people` → `Vec<{ id, name, aliases, samples: usize, meetings: usize }>`; `PATCH /api/people/:id` body `{ name?, merge_into? }`.
  - `GET /api/status` → `StatusSnapshot`; `POST /api/record/{start|stop|discard}` → forwards to the control channel.
  - `GET /api/config` and `PUT /api/config` (writes config.toml, returns `{ restart_required: bool }`).
  - `POST /api/selftest` → records 5 s into a temp cache, returns `{ mic_peaks: [f32; 50], sys_peaks: [f32; 50] }`.
  - `GET /api/events` → SSE of `Event` as JSON.
  - `/` and unknown paths → embedded `index.html`; `/assets/*` from `web/dist`.

- [ ] **Step 1: Tests**: `list_and_search_return_rows_and_hits`, `patch_title_renames_dir_and_md`, `patch_segment_text_rewrites_md`, `people_rename_applies_to_meeting_markdown`, `rerun_enqueues_job`, `audio_supports_range` (206 with `Content-Range`), `events_stream_delivers_progress`, `spa_fallback_serves_index_html`.
- [ ] **Step 2: Run** → FAIL.
- [ ] **Step 3: Implement**. `static_files.rs` embeds `../../web/dist` with `rust-embed` and a build-time check that `index.html` exists (clear error telling you to run `npm run build` in `web/`).
- [ ] **Step 4: Run** → PASS.
- [ ] **Step 5: Commit** `Add the HTTP API and SSE events`.

### Task 14: Web app: list and meeting pages

**Files:**
- Create: `web/package.json`, `web/vite.config.ts`, `web/src/main.ts`, `web/src/App.svelte`, `web/src/lib/api.ts`, `web/src/lib/i18n.ts`, `web/src/lib/router.ts`, `web/src/lib/player.ts`, `web/src/i18n/zh-TW.json`, `web/src/i18n/en.json`, `web/src/routes/List.svelte`, `web/src/routes/Meeting.svelte`, `web/src/components/{Transcript,Summary,Waveform,Ask,MeetingCard,SearchBox}.svelte`, `web/src/app.css`
- Test: `web/src/lib/*.test.ts` with Vitest (api URL building, i18n key parity between the two JSON files, clock formatting); `web/dist/` committed after `npm run build`

**Interfaces:**
- Consumes: the API in Task 13.
- Produces: a built `web/dist/index.html` + `assets/`. Router is hash-free history routing with four paths; `api.ts` exports `listMeetings(filter)`, `getMeeting(id)`, `patchMeeting(id, body)`, `rerun(id, step)`, `ask(id, q, onToken)`, `people()`, `patchPerson(id, body)`, `events(onEvent)`.

Design: one `app.css` with CSS variables for light and dark (`prefers-color-scheme`), system font stack plus `Noto Sans TC`, 16 px gutter, max width 1280 px. List: left sidebar 280 px with search box and filters, main column of cards grouped by date; card shows title, `mm:ss`, participant initials in coloured circles, one-line summary, state chip. Meeting: three columns 320 / 1fr / 360 px collapsing to one column under 1024 px; transcript lines `[time] Name text` with speaker colour from a 8-colour palette by speaker id hash; current line highlighted while playing; hover reveals edit / next speaker / delete; waveform drawn on a `<canvas>` from peaks computed client-side via `AudioContext.decodeAudioData` (me above the midline in blue, them below in orange, chapter ticks on top); Ask panel streams tokens. Keyboard: `/`, `j`, `k`, `Enter` on list; `space`, `[`, `]`, `e`, `s` on meeting.

- [ ] **Step 1: Tests** `i18n_files_have_the_same_keys`, `api_list_builds_query_string`, `clock_formats`.
- [ ] **Step 2: Run** `npm test` → FAIL.
- [ ] **Step 3: Implement** the pages. Every visible string goes through `t('key')`.
- [ ] **Step 4: Run** `npm test` → PASS; `npm run build` → `dist/` ; `cargo test -p recall-server` still passes (SPA test sees the real index.html).
- [ ] **Step 5: Commit** `Add the web app with list and meeting pages` (include `web/dist`).

### Task 15: Web app: people and settings pages

**Files:**
- Create: `web/src/routes/People.svelte`, `web/src/routes/Settings.svelte`, `web/src/components/{PersonRow,Meters}.svelte`
- Modify: `web/src/lib/api.ts` (config, selftest, status endpoints), i18n files

Settings form fields mirror `Config` exactly; a "Test recording" button calls `/api/selftest` and draws both peak arrays in `Meters`. People page: table of name, aliases, samples, meetings; inline rename; "merge into" select; renaming calls `PATCH /api/people/:id` and refreshes.

- [ ] **Step 1: Tests** extend `i18n` parity; `api_put_config_returns_restart_flag` against a mocked fetch.
- [ ] **Step 2: Run** → FAIL. **Step 3: Implement.** **Step 4: Run** → PASS, rebuild `dist`.
- [ ] **Step 5: Commit** `Add the people and settings pages`.

### Task 16: Recording status in the web app and notifications link

**Files:**
- Modify: `web/src/App.svelte` (header status chip fed by `/api/events` and `/api/status`: idle / armed / recording with elapsed time and app name / processing with step and percent; buttons start, stop, discard), `crates/recall/src/notify.rs` (summary_ready opens `http://127.0.0.1:<port>/m/<id>` via the notification's default action)

- [ ] **Step 1: Test** `status_chip_text_for_each_phase` (pure function in `web/src/lib/status.ts`).
- [ ] **Step 2–4:** FAIL → implement → PASS, rebuild `dist`.
- [ ] **Step 5: Commit** `Show recording state in the web app`.

### Task 17: CLI commands and end-to-end run

**Files:**
- Modify: `crates/recall/src/main.rs` (`list`, `show <id>`, `rerun <id> <step>`, `index rebuild`, `selftest`, `models download`), `crates/recall/src/daemon.rs` (start server + worker)
- Test: `crates/recall/tests/cli.rs` using `assert_cmd` against a temp `RECALL_CONFIG` pointing at a temp meetings dir

`recall list` prints `id  duration  state  one-line`; `recall show <id>` prints `meeting.md`; `recall selftest` records 5 s and prints both peak bars; `RECALL_CONFIG=<path>` overrides the config file; `RECALL_HOME=<dir>` overrides data dir for tests.

- [ ] **Step 1: Tests** `list_prints_meetings_from_index`, `show_prints_markdown`, `rerun_enqueues`.
- [ ] **Step 2–4:** FAIL → implement → PASS.
- [ ] **Step 5: End-to-end on this machine**: `cargo run --release -- models download` (downloads the four models), then `cargo run --release -- daemon`; in another terminal play `bench/fixtures/zh-en/computer.ogg` with `pw-play` while `pw-record --target 0 /dev/null` holds the mic for 3 minutes; confirm the meeting appears in `recall list`, reaches `done`, and `http://127.0.0.1:7474` shows it with a Traditional Chinese transcript. Record the timings in the commit message.
- [ ] **Step 6: Commit** `Add CLI commands and wire the daemon end to end`.

### Task 18: Bench, CI, README

**Files:**
- Create: `bench/README.md`, `bench/run.py`, `bench/thresholds.json`, `bench/fixtures/zh-en/{mic.ogg,computer.ogg,truth.json}`, `bench/fixtures/zh/{...}`, `bench/fixtures/silence/{...}`, `.github/workflows/ci.yml`, `README.md`, `packaging/install.sh`

Fixtures: synthesize with a TTS available locally or record 60 s each with the user's own voice later; for now `zh-en` and `zh` come from `ffmpeg`-generated speech-like noise only if no TTS is available, and the bench marks them `"skip": true` in `truth.json` so CI does not fail on them. `run.py` calls `recall transcribe-file --mic <> --computer <> --json` (add this hidden subcommand in `main.rs`) and scores `found`, `side`, `leaked` like omarchy-meeting-recorder's bench; `thresholds.json` starts with `silence: lines max 0`. CI: `cargo fmt --check`, `cargo clippy --workspace -D warnings`, `cargo test --workspace` (PipeWire tests gated off), `cd web && npm ci && npm test && npm run build && git diff --exit-code dist`. README: what it does, install (`cargo install --path crates/recall`, `systemctl --user enable --now recall`, Hyprland bindings for `recall start|stop|discard`), the `vulkan` feature and `vulkan-headers`, config keys, where files live, privacy.

- [ ] **Step 1:** write `run.py` with a unit test `python -m pytest bench/test_run.py` for the scoring function (`score(truth, transcript)` on a tiny example).
- [ ] **Step 2–4:** FAIL → implement → PASS; run `bench/run.py --case silence` locally and see it pass.
- [ ] **Step 5: Commit** `Add the bench, CI and README`.

---

## Self-review notes

- Spec coverage: storage (1–3), state machine and detection (4, 6, 7), capture and cache (5, 6), notifications and state.json (7), pipeline six steps (9–12), LLM backends and JSON (8), web pages and API (13–16), CLI (17), bench/CI (18), i18n (14), selftest (13, 15, 17). Screen recording, Meet extension, MCP, live captions are phases 4–5 and out of this plan.
- Type names are consistent: `Segment`, `Cluster`, `Speakers`, `Index`, `MeetingDir`, `Step`, `Event` are defined once and consumed by name.
- Review focus items 1–2 → Task 2 tests; 3 → Task 7; 4 → Task 6 (manual on this machine, gated test asserts equal lengths after `finalize`); 5 → Tasks 8 and 11.
