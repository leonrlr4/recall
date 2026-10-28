//! `/api/meetings`: listing and search, one meeting, edits, deletion and
//! reprocessing.

use std::collections::BTreeMap;
use std::fs;
use std::io::ErrorKind;

use anyhow::Context;
use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use chrono::{DateTime, Utc};
use recall_store::{
    Config, Hit, Index, ListFilter, Meeting, MeetingDir, MeetingId, MeetingRow, SpeakerId,
    Speakers, Step, markdown,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::AppState;
use crate::error::{ApiError, ApiResult};

/// Most transcript hits a search returns.
const SEARCH_LIMIT: usize = 100;

#[derive(Serialize)]
pub struct RowDto {
    id: MeetingId,
    title: String,
    started: DateTime<Utc>,
    duration_ms: u32,
    source_app: Option<String>,
    one_line: Option<String>,
    participants: Vec<String>,
    state: String,
}

impl From<MeetingRow> for RowDto {
    fn from(r: MeetingRow) -> Self {
        Self {
            id: r.id,
            title: r.title,
            started: r.started,
            duration_ms: r.duration_ms,
            source_app: r.source_app,
            one_line: r.one_line,
            participants: r.participants,
            state: r.state,
        }
    }
}

#[derive(Serialize)]
pub struct HitDto {
    meeting: MeetingId,
    start_ms: u32,
    speaker: String,
    snippet: String,
}

impl From<Hit> for HitDto {
    fn from(h: Hit) -> Self {
        Self {
            meeting: h.meeting,
            start_ms: h.start_ms,
            speaker: h.speaker,
            snippet: h.snippet,
        }
    }
}

#[derive(Deserialize)]
pub struct ListQuery {
    q: Option<String>,
    app: Option<String>,
    participant: Option<String>,
    from: Option<DateTime<Utc>>,
    to: Option<DateTime<Utc>>,
}

#[derive(Serialize)]
pub struct ListResponse {
    meetings: Vec<RowDto>,
    /// Transcript matches for `q`; empty without `q`.
    hits: Vec<HitDto>,
}

/// An empty query parameter (`?app=`) means no filter.
pub(crate) fn non_empty(s: Option<String>) -> Option<String> {
    s.filter(|s| !s.trim().is_empty())
}

pub async fn list(
    State(state): State<AppState>,
    Query(query): Query<ListQuery>,
) -> ApiResult<Json<ListResponse>> {
    let filter = ListFilter {
        q: non_empty(query.q),
        app: non_empty(query.app),
        participant: non_empty(query.participant),
        from: query.from,
        to: query.to,
    };
    let index = state.index.lock().expect("index lock poisoned");
    let meetings = index.list(&filter)?;
    let hits = match &filter.q {
        Some(q) => index.search(q, SEARCH_LIMIT)?,
        None => Vec::new(),
    };
    Ok(Json(ListResponse {
        meetings: meetings.into_iter().map(RowDto::from).collect(),
        hits: hits.into_iter().map(HitDto::from).collect(),
    }))
}

/// The meeting's directory; any id that does not name one is a 404.
pub(crate) fn open_dir(config: &Config, id: &str) -> ApiResult<MeetingDir> {
    MeetingDir::open(&config.meetings_dir, &MeetingId(id.to_string()))
        .map_err(|e| ApiError::not_found(format!("{e:#}")))
}

/// 409 while the meeting has a pending or running job: the job saves
/// `meeting.json` after every step and would overwrite an edit, or recreate
/// a deleted directory's files.
fn ensure_idle(index: &Index, id: &MeetingId) -> ApiResult<()> {
    match index.active_job(id)? {
        Some(_) => Err(ApiError::conflict("meeting is being processed")),
        None => Ok(()),
    }
}

pub(crate) fn load(dir: &MeetingDir) -> ApiResult<Meeting> {
    Ok(Meeting::load(&dir.path())?)
}

/// Rewrites `meeting.md` and the index row from `meeting`, which must
/// already be saved in `dir`.
pub(crate) fn refresh(
    config: &Config,
    index: &Index,
    speakers: &Speakers,
    meeting: &Meeting,
    dir: &MeetingDir,
) -> anyhow::Result<()> {
    let md = dir.markdown();
    fs::write(
        &md,
        markdown::render(meeting, speakers, &config.summary_lang),
    )
    .with_context(|| format!("writing {}", md.display()))?;
    index.upsert(meeting, speakers, dir.audio().exists())
}

#[derive(Serialize)]
pub struct MeetingResponse {
    meeting: Meeting,
    /// Display name for every speaker id the meeting mentions, plus `me`.
    names: BTreeMap<SpeakerId, String>,
    markdown: String,
}

pub async fn get(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Json<MeetingResponse>> {
    let dir = open_dir(&state.config, &id)?;
    let meeting = load(&dir)?;
    let speakers = state.speakers.lock().expect("speakers lock poisoned");
    let mut names = BTreeMap::new();
    let me = speakers.me();
    names.insert(me.id.clone(), me.name.clone());
    let ids = meeting
        .segments
        .iter()
        .filter_map(|s| s.speaker.as_ref())
        .chain(meeting.clusters.iter().filter_map(|c| c.speaker.as_ref()));
    for id in ids {
        names
            .entry(id.clone())
            .or_insert_with(|| speakers.name_of(id));
    }
    let markdown = match fs::read_to_string(dir.markdown()) {
        Ok(text) => text,
        Err(e) if e.kind() == ErrorKind::NotFound => {
            markdown::render(&meeting, &speakers, &state.config.summary_lang)
        }
        Err(e) => return Err(anyhow::Error::new(e).context("reading meeting.md").into()),
    };
    Ok(Json(MeetingResponse {
        meeting,
        names,
        markdown,
    }))
}

#[derive(Deserialize)]
pub struct PatchMeeting {
    title: Option<String>,
    #[serde(default)]
    segments: Vec<SegmentEdit>,
}

/// An edit to `meeting.segments[index]`. Indexes refer to the segments as
/// they were before the request, whatever else the request deletes.
#[derive(Deserialize)]
pub struct SegmentEdit {
    index: usize,
    text: Option<String>,
    speaker: Option<SpeakerId>,
    #[serde(default)]
    delete: bool,
}

/// Applies the edits, renames the directory for a new title, then saves
/// `meeting.json`, rewrites `meeting.md` and the index row. Returns the
/// meeting, whose `id` changes with the title.
pub async fn patch(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<PatchMeeting>,
) -> ApiResult<Json<Meeting>> {
    // Held from load to save so a concurrent people edit, which also
    // rewrites meeting files, cannot interleave with this one.
    let speakers = state.speakers.lock().expect("speakers lock poisoned");
    let dir = open_dir(&state.config, &id)?;
    ensure_idle(&state.index.lock().expect("index lock poisoned"), &dir.id)?;
    let mut meeting = load(&dir)?;

    let len = meeting.segments.len();
    for edit in &body.segments {
        if edit.index >= len {
            return Err(ApiError::bad_request(format!(
                "segment index {} out of range; the meeting has {len}",
                edit.index
            )));
        }
        if let Some(sid) = &edit.speaker
            && speakers.get(sid).is_none()
        {
            return Err(ApiError::bad_request(format!("no speaker {:?}", sid.0)));
        }
    }
    let title = match &body.title {
        Some(t) if t.trim().is_empty() => return Err(ApiError::bad_request("empty title")),
        Some(t) => Some(t.trim().to_string()),
        None => None,
    };

    let mut deleted = Vec::new();
    for edit in body.segments {
        let segment = &mut meeting.segments[edit.index];
        if let Some(text) = edit.text {
            segment.text = text;
        }
        if let Some(sid) = edit.speaker {
            segment.speaker = Some(sid);
        }
        if edit.delete {
            deleted.push(edit.index);
        }
    }
    deleted.sort_unstable();
    deleted.dedup();
    for i in deleted.into_iter().rev() {
        meeting.segments.remove(i);
    }

    // Save before renaming, so a failed rename loses no edits. A rename
    // then saves again to record the new id.
    let old_id = dir.id.clone();
    if let Some(title) = title {
        meeting.title = title;
    }
    meeting.save(&dir.path())?;
    let dir = if body.title.is_some() {
        let title = meeting.title.clone();
        let dir = dir.rename(&title)?;
        if dir.id != old_id {
            meeting.id = dir.id.clone();
            meeting.save(&dir.path())?;
        }
        dir
    } else {
        dir
    };
    let index = state.index.lock().expect("index lock poisoned");
    if old_id != dir.id {
        index.remove(&old_id)?;
    }
    refresh(&state.config, &index, &speakers, &meeting, &dir)?;
    Ok(Json(meeting))
}

/// Removes the meeting's directory, audio included, and its index row.
pub async fn delete(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<StatusCode> {
    let dir = open_dir(&state.config, &id)?;
    ensure_idle(&state.index.lock().expect("index lock poisoned"), &dir.id)?;
    let path = dir.path();
    fs::remove_dir_all(&path).with_context(|| format!("removing {}", path.display()))?;
    state
        .index
        .lock()
        .expect("index lock poisoned")
        .remove(&dir.id)?;
    Ok(StatusCode::NO_CONTENT)
}

/// Queues the meeting for reprocessing from `step` (snake_case, e.g.
/// `summarize`).
pub async fn rerun(
    State(state): State<AppState>,
    Path((id, step)): Path<(String, String)>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let step: Step = serde_json::from_value(Value::String(step.clone()))
        .map_err(|_| ApiError::bad_request(format!("unknown step {step:?}")))?;
    let dir = open_dir(&state.config, &id)?;
    // Held through the enqueue, so a PATCH of this meeting, which holds it
    // from load to save, cannot be between its busy check and its save.
    let _speakers = state.speakers.lock().expect("speakers lock poisoned");
    ensure_idle(&state.index.lock().expect("index lock poisoned"), &dir.id)?;
    let job = (state.rerun)(&dir.id, step)?;
    Ok((StatusCode::ACCEPTED, Json(json!({ "job": job }))))
}
