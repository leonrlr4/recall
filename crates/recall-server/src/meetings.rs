//! `/api/meetings`: listing and search, one meeting, edits, deletion and
//! reprocessing.

use std::cell::RefCell;
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

use crate::error::{ApiError, ApiResult};
use crate::{AppState, SyncWhen, VaultStatus};

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
    /// `true`: only meetings marked for sync, rows and hits alike.
    #[serde(default)]
    synced: bool,
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
    let mut meetings = index.list(&filter)?;
    // Unsynced meetings' hits are dropped after the search, so it looks
    // further to leave room for them.
    let search_limit = if query.synced {
        SEARCH_LIMIT * 5
    } else {
        SEARCH_LIMIT
    };
    let mut hits = match &filter.q {
        Some(q) => index.search(q, search_limit)?,
        None => Vec::new(),
    };
    drop(index);
    if query.synced {
        let synced = SyncedSet::new(&state);
        meetings.retain(|m| synced.contains(&m.id));
        hits.retain(|h| synced.contains(&h.meeting));
        hits.truncate(SEARCH_LIMIT);
    }
    Ok(Json(ListResponse {
        meetings: meetings.into_iter().map(RowDto::from).collect(),
        hits: hits.into_iter().map(HitDto::from).collect(),
    }))
}

/// Answers whether a meeting is marked for sync (the user published it as
/// work), remembering each answer for the life of one request.
///
/// A switch value requested while a job processes the meeting wins over
/// `meeting.json`, as in `GET /api/meetings/{id}`. A meeting whose
/// `meeting.json` does not read counts as not synced.
pub(crate) struct SyncedSet<'a> {
    state: &'a AppState,
    seen: RefCell<BTreeMap<MeetingId, bool>>,
}

impl<'a> SyncedSet<'a> {
    pub(crate) fn new(state: &'a AppState) -> Self {
        Self {
            state,
            seen: RefCell::default(),
        }
    }

    pub(crate) fn contains(&self, id: &MeetingId) -> bool {
        if let Some(&known) = self.seen.borrow().get(id) {
            return known;
        }
        let requested = self
            .state
            .vault
            .as_ref()
            .and_then(|v| v.status(id).requested);
        let synced = requested.unwrap_or_else(|| sync_flag(&self.state.config, id));
        self.seen.borrow_mut().insert(id.clone(), synced);
        synced
    }
}

/// `meeting.json`'s `sync`, read without building the whole meeting.
fn sync_flag(config: &Config, id: &MeetingId) -> bool {
    #[derive(Deserialize)]
    struct Flag {
        #[serde(default)]
        sync: bool,
    }
    MeetingDir::open(&config.meetings_dir, id)
        .ok()
        .and_then(|dir| fs::read(dir.path().join("meeting.json")).ok())
        .and_then(|bytes| serde_json::from_slice::<Flag>(&bytes).ok())
        .is_some_and(|f| f.sync)
}

/// The meeting's directory; any id that does not name one is a 404.
pub(crate) fn open_dir(config: &Config, id: &str) -> ApiResult<MeetingDir> {
    MeetingDir::open(&config.meetings_dir, &MeetingId(id.to_string()))
        .map_err(|e| ApiError::not_found(format!("{e:#}")))
}

/// 409 while the meeting has a pending or running job: the job saves
/// `meeting.json` after every step and would overwrite an edit, or recreate
/// a deleted directory's files.
pub(crate) fn ensure_idle(index: &Index, id: &MeetingId) -> ApiResult<()> {
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
    /// `screen.mp4` exists, so `/api/meetings/{id}/screen` can play it.
    has_screen: bool,
    /// The meeting's place in the shared vault; `null` when publishing is
    /// off. `meeting.sync` already reflects `vault.requested`.
    vault: Option<VaultStatus>,
}

pub async fn get(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Json<MeetingResponse>> {
    let dir = open_dir(&state.config, &id)?;
    let mut meeting = load(&dir)?;
    let vault = state.vault.as_ref().map(|v| v.status(&dir.id));
    if let Some(requested) = vault.as_ref().and_then(|v| v.requested) {
        meeting.sync = requested;
    }
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
        has_screen: dir.screen().is_file(),
        meeting,
        names,
        markdown,
        vault,
    }))
}

#[derive(Deserialize)]
pub struct PatchMeeting {
    title: Option<String>,
    #[serde(default)]
    segments: Vec<SegmentEdit>,
    /// Publish the meeting to the shared vault, or take it out.
    sync: Option<bool>,
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
/// `meeting.json`, rewrites `meeting.md` and the index row, and tells the
/// vault sync. Returns the meeting, whose `id` changes with the title.
///
/// `sync` alone is accepted while a job processes the meeting: it is handed
/// to the vault sync, which writes it when the job ends, and the returned
/// meeting shows it already. Title and segment edits are 409 then.
pub async fn patch(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<PatchMeeting>,
) -> ApiResult<Json<Meeting>> {
    // Held from load to save so a concurrent people edit, which also
    // rewrites meeting files, cannot interleave with this one.
    let speakers = state.speakers.lock().expect("speakers lock poisoned");
    let dir = open_dir(&state.config, &id)?;
    if body.sync.is_some() && state.vault.is_none() {
        return Err(ApiError::bad_request(
            "publishing to the vault is off; set vault.remote first",
        ));
    }
    let edits = body.title.is_some() || !body.segments.is_empty();
    let busy = state
        .index
        .lock()
        .expect("index lock poisoned")
        .active_job(&dir.id)?
        .is_some();
    if busy && edits {
        return Err(ApiError::conflict("meeting is being processed"));
    }
    let mut meeting = load(&dir)?;
    if let (true, Some(sync), Some(vault)) = (busy, body.sync, &state.vault) {
        vault.request(&dir.id, Some(sync));
        vault.touch(&dir.id, SyncWhen::Now);
        meeting.sync = sync;
        return Ok(Json(meeting));
    }

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
    if let Some(sync) = body.sync {
        meeting.sync = sync;
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
    if let Some(vault) = &state.vault {
        if old_id != dir.id {
            vault.renamed(&old_id, &dir.id);
        }
        if body.sync.is_some() {
            // A request left from a job that has ended would otherwise be
            // applied over this newer choice.
            vault.request(&dir.id, None);
            vault.touch(&dir.id, SyncWhen::Now);
        } else {
            vault.touch(&dir.id, SyncWhen::Debounced);
        }
    }
    Ok(Json(meeting))
}

/// Removes the meeting's directory, audio included, and its index row. A
/// published note is then removed from the vault (it stays in the vault's
/// git history).
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
    if let Some(vault) = &state.vault {
        vault.touch(&dir.id, SyncWhen::Now);
    }
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
