//! The MCP tools. Each one is a call to the daemon's HTTP API; errors come
//! back as tool results with `isError`, so the model reads them.

use std::collections::BTreeMap;
use std::fmt::Write;

use recall_store::markdown::clock;
use recall_store::{Meeting, Side, SpeakerId};
use reqwest::Url;
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, ContentBlock};
use rmcp::{Json, ServerHandler, tool, tool_handler, tool_router};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::client::{Daemon, ToolResult};

/// Most estimated tokens one `get_transcript` page holds.
pub const PAGE_TOKENS: usize = 20_000;

/// The token estimate the page cap uses: two characters per token, which
/// errs high for English and is about right for Chinese.
fn estimated_tokens(chars: usize) -> usize {
    chars / 2
}

/// Name for a remote segment no speaker has been assigned to, as in
/// `meeting.md`.
const THEM: &str = "Them";

#[derive(Deserialize, JsonSchema)]
pub struct SearchArgs {
    /// Words to look for in titles, summaries and transcripts.
    pub query: String,
    /// Most meetings and most transcript matches to return.
    #[serde(default = "default_limit")]
    #[schemars(default = "default_limit")]
    pub limit: usize,
}

fn default_limit() -> usize {
    20
}

#[derive(Deserialize, JsonSchema)]
pub struct MeetingArgs {
    /// Meeting id, as `search_meetings` returns it.
    pub id: String,
}

#[derive(Deserialize, JsonSchema)]
pub struct TranscriptArgs {
    /// Meeting id, as `search_meetings` returns it.
    pub id: String,
    /// `next_cursor` from the previous page; omit for the first page.
    pub cursor: Option<String>,
    /// Only segments that end after this many milliseconds into the meeting.
    pub from_ms: Option<u32>,
    /// Only segments that start before this many milliseconds into the
    /// meeting.
    pub to_ms: Option<u32>,
}

#[derive(Deserialize, Serialize, JsonSchema)]
pub struct ActionArgs {
    /// Part of the owner's name, case-insensitive.
    pub owner: Option<String>,
    /// Meetings that started at or after this RFC 3339 time, such as
    /// `2026-10-20T00:00:00Z`.
    pub from: Option<String>,
    /// Meetings that started before this RFC 3339 time, such as
    /// `2026-10-27T00:00:00Z`.
    pub to: Option<String>,
    /// Items due on or after this date (YYYY-MM-DD); leaves out items
    /// without a due date.
    pub due_from: Option<String>,
    /// Items due on or before this date (YYYY-MM-DD); leaves out items
    /// without a due date.
    pub due_to: Option<String>,
}

/// A processing step, in pipeline order. Rerunning one reruns every step
/// after it too.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RerunStep {
    Segment,
    Transcribe,
    Diarize,
    Name,
    Summarize,
    Write,
}

impl RerunStep {
    fn as_str(self) -> &'static str {
        match self {
            Self::Segment => "segment",
            Self::Transcribe => "transcribe",
            Self::Diarize => "diarize",
            Self::Name => "name",
            Self::Summarize => "summarize",
            Self::Write => "write",
        }
    }
}

#[derive(Deserialize, JsonSchema)]
pub struct RerunArgs {
    /// Meeting id, as `search_meetings` returns it.
    pub id: String,
    pub step: RerunStep,
}

#[derive(Deserialize, JsonSchema)]
pub struct RenameArgs {
    /// Meeting id, as `search_meetings` returns it.
    pub id: String,
    pub title: String,
}

#[derive(Deserialize, JsonSchema)]
pub struct NameSpeakerArgs {
    /// Speaker id, as `get_meeting` lists it under participants.
    pub person_id: String,
    pub name: String,
}

#[derive(Deserialize, Serialize, JsonSchema)]
pub struct ActionItem {
    /// Id of the meeting whose summary lists the item.
    pub meeting: String,
    /// That meeting's title.
    pub title: String,
    /// When that meeting started (RFC 3339).
    pub started: String,
    pub text: String,
    pub owner: Option<String>,
    pub due: Option<String>,
}

#[derive(Deserialize, Serialize, JsonSchema)]
pub struct ActionItems {
    /// Newest meeting first, each meeting's items in summary order.
    pub items: Vec<ActionItem>,
}

/// `GET /api/status`, the daemon's `StatusSnapshot`.
#[derive(Deserialize, Serialize, JsonSchema)]
pub struct RecordingStatus {
    /// `idle`, `armed`, `recording` or another recorder phase.
    pub phase: String,
    /// Unix milliseconds when `phase` began.
    pub since_ms: u64,
    /// The app whose call started the recording, if one did.
    pub source_app: Option<String>,
    /// Latest microphone peak, 0 to 1.
    pub mic_peak: f32,
    /// Latest system audio peak, 0 to 1.
    pub sys_peak: f32,
    /// Latest status from the Meet extension during this recording.
    #[serde(default)]
    pub meet: Option<String>,
    /// The screen recording's state.
    #[serde(default)]
    pub screen: Option<String>,
}

#[derive(Deserialize)]
struct MeetingList {
    meetings: Vec<MeetingRow>,
    hits: Vec<Hit>,
}

#[derive(Deserialize)]
struct MeetingRow {
    id: String,
    title: String,
    started: String,
    duration_ms: u32,
    one_line: Option<String>,
    participants: Vec<String>,
    state: String,
}

#[derive(Deserialize)]
struct Hit {
    meeting: String,
    start_ms: u32,
    speaker: String,
    snippet: String,
}

/// `GET /api/meetings/{id}`.
#[derive(Deserialize)]
struct MeetingResponse {
    meeting: Meeting,
    names: BTreeMap<SpeakerId, String>,
}

/// One entry of `GET /api/people`, or the `PATCH` reply.
#[derive(Deserialize)]
struct Person {
    id: String,
    name: String,
    #[serde(default)]
    aliases: Vec<String>,
    #[serde(default)]
    named: bool,
}

#[derive(Clone)]
pub struct Recall {
    daemon: Daemon,
    tool_router: ToolRouter<Self>,
}

impl Recall {
    pub fn new(base: Url) -> anyhow::Result<Self> {
        Ok(Self {
            daemon: Daemon::new(base)?,
            tool_router: Self::tool_router(),
        })
    }

    async fn meeting(&self, id: &str) -> ToolResult<MeetingResponse> {
        self.daemon.get(&["api", "meetings", id], &[]).await
    }
}

#[tool_router]
impl Recall {
    /// Search meetings by title, summary and transcript. Returns matching
    /// meetings and transcript snippets with their timestamps.
    #[tool(annotations(read_only_hint = true, open_world_hint = false))]
    async fn search_meetings(
        &self,
        Parameters(args): Parameters<SearchArgs>,
    ) -> ToolResult<String> {
        let list: MeetingList = self
            .daemon
            .get(&["api", "meetings"], &[("q", args.query)])
            .await?;
        Ok(render_search(&list, args.limit))
    }

    /// A meeting's details and notes: participants, summary, decisions,
    /// action items and chapters. Use get_transcript for what was said.
    #[tool(annotations(read_only_hint = true, open_world_hint = false))]
    async fn get_meeting(&self, Parameters(args): Parameters<MeetingArgs>) -> ToolResult<String> {
        Ok(render_meeting(&self.meeting(&args.id).await?))
    }

    /// A meeting's transcript as `[mm:ss] Name: text` lines (`[h:mm:ss]`
    /// from one hour on), at most about 20,000 tokens per call. When more
    /// remains, a second content block gives `next_cursor`; pass it as
    /// `cursor` for the next page.
    #[tool(annotations(read_only_hint = true, open_world_hint = false))]
    async fn get_transcript(
        &self,
        Parameters(args): Parameters<TranscriptArgs>,
    ) -> ToolResult<CallToolResult> {
        let start = match &args.cursor {
            Some(c) => c
                .parse::<usize>()
                .map_err(|_| format!("invalid cursor {c:?}; pass next_cursor as it was given"))?,
            None => 0,
        };
        let response = self.meeting(&args.id).await?;
        let page = transcript_page(&response, start, args.from_ms, args.to_ms);
        let mut content = vec![ContentBlock::text(page.text)];
        if let Some(next) = page.next_cursor {
            content.push(ContentBlock::text(
                json!({ "next_cursor": next.to_string() }).to_string(),
            ));
        }
        Ok(CallToolResult::success(content))
    }

    /// Action items from every meeting's summary, filtered by owner, due
    /// date range and meeting date range.
    // The return type is spelled out: `#[tool]` derives `outputSchema` only
    // from a literal `Result<Json<_>, _>`, not from the `ToolResult` alias.
    #[tool(annotations(read_only_hint = true, open_world_hint = false))]
    async fn list_action_items(
        &self,
        Parameters(args): Parameters<ActionArgs>,
    ) -> Result<Json<ActionItems>, String> {
        let query: Vec<(&str, String)> = [
            ("owner", args.owner),
            ("from", args.from),
            ("to", args.to),
            ("due_from", args.due_from),
            ("due_to", args.due_to),
        ]
        .into_iter()
        .filter_map(|(k, v)| Some((k, v?)))
        .collect();
        let items = self.daemon.get(&["api", "action-items"], &query).await?;
        Ok(Json(ActionItems { items }))
    }

    /// What the recorder is doing now: idle, armed or recording, since
    /// when, and audio levels.
    #[tool(annotations(read_only_hint = true, open_world_hint = false))]
    async fn recording_status(&self) -> Result<Json<RecordingStatus>, String> {
        Ok(Json(self.daemon.get(&["api", "status"], &[]).await?))
    }

    /// Start recording now.
    #[tool(annotations(
        read_only_hint = false,
        destructive_hint = false,
        open_world_hint = false
    ))]
    async fn start_recording(&self) -> ToolResult<String> {
        self.daemon.post(&["api", "record", "start"]).await?;
        Ok("Recording started.".into())
    }

    /// Stop the current recording and keep it; processing starts on its own.
    #[tool(annotations(
        read_only_hint = false,
        destructive_hint = false,
        open_world_hint = false
    ))]
    async fn stop_recording(&self) -> ToolResult<String> {
        self.daemon.post(&["api", "record", "stop"]).await?;
        Ok("Recording stopped; the meeting is queued for processing.".into())
    }

    /// Process a meeting again from a step, and every step after it. A rerun
    /// of transcribe or diarize replaces transcript edits.
    #[tool(annotations(
        read_only_hint = false,
        destructive_hint = true,
        open_world_hint = false
    ))]
    async fn rerun_step(&self, Parameters(args): Parameters<RerunArgs>) -> ToolResult<String> {
        let step = args.step.as_str();
        let reply = self
            .daemon
            .post(&["api", "meetings", &args.id, "rerun", step])
            .await?;
        Ok(format!(
            "Queued {} for processing from {step} (job {}).",
            args.id, reply["job"]
        ))
    }

    /// Change a meeting's title. The meeting's id follows its title, so the
    /// reply gives the new id.
    #[tool(annotations(
        read_only_hint = false,
        destructive_hint = false,
        idempotent_hint = true,
        open_world_hint = false
    ))]
    async fn rename_meeting(&self, Parameters(args): Parameters<RenameArgs>) -> ToolResult<String> {
        let meeting: Meeting = self
            .daemon
            .patch(
                &["api", "meetings", &args.id],
                &json!({ "title": args.title }),
            )
            .await?;
        Ok(format!(
            "Renamed to {:?}; the meeting id is now {}.",
            meeting.title, meeting.id.0
        ))
    }

    /// Set a speaker's display name everywhere it appears. When another
    /// named person already goes by that name or alias, the speaker is
    /// merged into them instead, so one person never appears twice.
    #[tool(annotations(
        read_only_hint = false,
        destructive_hint = false,
        idempotent_hint = true,
        open_world_hint = false
    ))]
    async fn name_speaker(
        &self,
        Parameters(args): Parameters<NameSpeakerArgs>,
    ) -> ToolResult<String> {
        // The web UI's rule: a rename onto an existing person's name would
        // leave two named people with the same name.
        let people: Vec<Person> = self.daemon.get(&["api", "people"], &[]).await?;
        let key = |s: &str| s.trim().to_lowercase();
        let wanted = key(&args.name);
        let existing = people.iter().find(|p| {
            p.id != args.person_id
                && p.named
                && std::iter::once(&p.name)
                    .chain(&p.aliases)
                    .any(|n| key(n) == wanted)
        });
        let body = match existing {
            Some(p) => json!({ "merge_into": p.id }),
            None => json!({ "name": args.name }),
        };
        let person: Person = self
            .daemon
            .patch(&["api", "people", &args.person_id], &body)
            .await?;
        Ok(if existing.is_some() {
            format!(
                "Speaker {} is merged into {} ({:?}), who already had that name.",
                args.person_id, person.id, person.name
            )
        } else {
            format!("Speaker {} is now named {:?}.", person.id, person.name)
        })
    }
}

#[tool_handler(
    router = self.tool_router,
    name = "recall",
    instructions = "Meetings recorded by recall on this machine: search them, read notes and \
                    transcripts, list action items, and control the recorder."
)]
impl ServerHandler for Recall {}

fn render_search(list: &MeetingList, limit: usize) -> String {
    let mut out = String::new();
    if list.meetings.is_empty() {
        out.push_str("No meetings match.\n");
    } else {
        out.push_str("Meetings (id: title, started, length, state):\n");
        for m in list.meetings.iter().take(limit) {
            let _ = write!(
                out,
                "- {}: {}, {}, {}, {}",
                m.id,
                m.title,
                m.started,
                clock(m.duration_ms),
                m.state
            );
            if !m.participants.is_empty() {
                let _ = write!(out, "; with {}", m.participants.join(", "));
            }
            if let Some(line) = m.one_line.as_deref().filter(|l| !l.is_empty()) {
                let _ = write!(out, "; {line}");
            }
            out.push('\n');
        }
    }
    if !list.hits.is_empty() {
        out.push_str("\nTranscript matches (meeting id [time] speaker: snippet):\n");
        for h in list.hits.iter().take(limit) {
            let _ = writeln!(
                out,
                "- {} [{}] {}: {}",
                h.meeting,
                clock(h.start_ms),
                h.speaker,
                flatten(&h.snippet)
            );
        }
    }
    out
}

fn render_meeting(response: &MeetingResponse) -> String {
    let m = &response.meeting;
    let mut out = String::new();
    let _ = writeln!(out, "# {}", m.title);
    let _ = writeln!(out, "id: {}", m.id.0);
    let _ = writeln!(out, "started: {}", m.started.to_rfc3339());
    let _ = writeln!(out, "length: {}", clock(m.duration_ms));
    if let Some(app) = &m.source_app {
        let _ = writeln!(out, "source app: {app}");
    }
    let participants: Vec<String> = m
        .participants()
        .iter()
        .map(|id| format!("{} (speaker id {})", name_of(response, id), id.0))
        .collect();
    if !participants.is_empty() {
        let _ = writeln!(out, "participants: {}", participants.join(", "));
    }
    let _ = writeln!(
        out,
        "transcript: {} segments; read them with get_transcript",
        m.segments.len()
    );
    let Some(summary) = &m.summary else {
        out.push_str("\nNo summary yet.\n");
        return out;
    };
    if !summary.one_line.is_empty() || !summary.body.is_empty() {
        let _ = write!(out, "\n## Summary\n\n");
        if !summary.one_line.is_empty() {
            let _ = writeln!(out, "{}\n", summary.one_line);
        }
        if !summary.body.is_empty() {
            let _ = writeln!(out, "{}", summary.body.trim_end());
        }
    }
    if !summary.decisions.is_empty() {
        let _ = write!(out, "\n## Decisions\n\n");
        for d in &summary.decisions {
            let _ = writeln!(out, "- {}", flatten(d));
        }
    }
    if !summary.actions.is_empty() {
        let _ = write!(out, "\n## Action items\n\n");
        for a in &summary.actions {
            let _ = write!(out, "- {}", flatten(&a.text));
            if let Some(owner) = &a.owner {
                let _ = write!(out, "; owner: {owner}");
            }
            if let Some(due) = &a.due {
                let _ = write!(out, "; due: {due}");
            }
            out.push('\n');
        }
    }
    if !summary.chapters.is_empty() {
        let _ = write!(out, "\n## Chapters\n\n");
        for c in &summary.chapters {
            let _ = writeln!(out, "- [{}] {}", clock(c.start_ms), flatten(&c.title));
        }
    }
    out
}

fn name_of(response: &MeetingResponse, id: &SpeakerId) -> String {
    response
        .names
        .get(id)
        .cloned()
        .unwrap_or_else(|| id.0.clone())
}

/// One page of `get_transcript`.
#[derive(Debug, PartialEq)]
pub struct Page {
    pub text: String,
    /// Index into `meeting.segments` of the first segment the page left out.
    pub next_cursor: Option<usize>,
}

/// The transcript lines of the segments from index `start` on that overlap
/// `[from_ms, to_ms)`, cut before the segment that would take the page past
/// [`PAGE_TOKENS`]. A page always holds at least one segment, however long.
fn transcript_page(
    response: &MeetingResponse,
    start: usize,
    from_ms: Option<u32>,
    to_ms: Option<u32>,
) -> Page {
    let me = response
        .names
        .get(&SpeakerId(recall_store::speakers::ME.to_string()));
    let mut text = String::new();
    let mut chars = 0;
    let in_range = |s: &recall_store::Segment| {
        from_ms.is_none_or(|f| s.end_ms > f) && to_ms.is_none_or(|t| s.start_ms < t)
    };
    let segments = response.meeting.segments.iter().enumerate().skip(start);
    for (i, segment) in segments.filter(|(_, s)| in_range(s)) {
        let name = match (&segment.speaker, segment.side) {
            (Some(id), _) => name_of(response, id),
            (None, Side::Me) => me.cloned().unwrap_or_else(|| "Me".into()),
            (None, Side::Them) => THEM.to_string(),
        };
        let line = format!(
            "[{}] {name}: {}\n",
            clock(segment.start_ms),
            flatten(segment.text.trim())
        );
        let line_chars = line.chars().count();
        if !text.is_empty() && estimated_tokens(chars + line_chars) > PAGE_TOKENS {
            return Page {
                text,
                next_cursor: Some(i),
            };
        }
        chars += line_chars;
        text.push_str(&line);
    }
    Page {
        text,
        next_cursor: None,
    }
}

/// `s` on one line, so a newline in a segment cannot start a fake one.
fn flatten(s: &str) -> String {
    s.replace("\r\n", " ").replace(['\n', '\r'], " ")
}
