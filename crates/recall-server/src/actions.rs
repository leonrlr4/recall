//! `GET /api/action-items`: the action items of every meeting's summary;
//! `PATCH /api/meetings/{id}/actions/{index}`: checking one off.

use axum::Json;
use axum::extract::{Path, Query, State};
use chrono::{DateTime, Utc};
use recall_store::{ActionItem, ListFilter, Meeting, MeetingDir, MeetingId, Priority, Speakers};
use serde::{Deserialize, Serialize};

use crate::error::{ApiError, ApiResult};
use crate::meetings::{SyncedSet, ensure_idle, load, non_empty, open_dir, refresh};
use crate::{AppState, SyncWhen};

#[derive(Deserialize)]
pub struct ActionQuery {
    /// Substring of the owner, compared case-insensitively.
    owner: Option<String>,
    /// Bounds on the meeting's start time, as in `GET /api/meetings`.
    from: Option<DateTime<Utc>>,
    to: Option<DateTime<Utc>>,
    /// Inclusive bounds on `due`, compared as strings, so ISO dates order
    /// correctly. Either bound leaves out items without a due date.
    due_from: Option<String>,
    due_to: Option<String>,
    /// Only done (`true`) or only open (`false`) items.
    done: Option<bool>,
    /// `true`: only meetings marked for sync.
    #[serde(default)]
    synced: bool,
}

#[derive(Serialize)]
pub struct ActionDto {
    meeting: MeetingId,
    title: String,
    started: DateTime<Utc>,
    /// Position in the meeting's `summary.actions`, for `PATCH`.
    index: usize,
    text: String,
    owner: Option<String>,
    due: Option<String>,
    priority: Option<Priority>,
    source_ms: Option<u32>,
    done: bool,
    done_at: Option<DateTime<Utc>>,
    /// `owner` names the user: their name or an alias in the speaker
    /// registry, or "me".
    mine: bool,
}

/// Owner spellings that mean the user, lowercased.
fn my_names(speakers: &Speakers) -> Vec<String> {
    let me = speakers.me();
    std::iter::once(&me.name)
        .chain(&me.aliases)
        .map(|n| n.trim().to_lowercase())
        .chain(["me".to_string(), "我".to_string()])
        .collect()
}

/// Newest meeting first, each meeting's items in summary order. A meeting
/// whose `meeting.json` does not load is left out with a warning.
pub async fn list(
    State(state): State<AppState>,
    Query(query): Query<ActionQuery>,
) -> ApiResult<Json<Vec<ActionDto>>> {
    let me = my_names(&state.speakers.lock().expect("speakers lock poisoned"));
    let filter = ListFilter {
        from: query.from,
        to: query.to,
        ..ListFilter::default()
    };
    let rows = state
        .index
        .lock()
        .expect("index lock poisoned")
        .list(&filter)?;
    let owner = non_empty(query.owner).map(|o| o.to_lowercase());
    let due_from = non_empty(query.due_from);
    let due_to = non_empty(query.due_to);
    let synced = SyncedSet::new(&state);
    let mut items = Vec::new();
    for row in rows {
        if query.synced && !synced.contains(&row.id) {
            continue;
        }
        let meeting = match MeetingDir::open(&state.config.meetings_dir, &row.id)
            .and_then(|dir| Meeting::load(&dir.path()))
        {
            Ok(meeting) => meeting,
            Err(e) => {
                tracing::warn!(id = row.id.0, "listing action items: {e:#}");
                continue;
            }
        };
        let Some(summary) = meeting.summary else {
            continue;
        };
        for (index, action) in summary.actions.into_iter().enumerate() {
            if let Some(owner) = &owner
                && !action
                    .owner
                    .as_ref()
                    .is_some_and(|o| o.to_lowercase().contains(owner))
            {
                continue;
            }
            if due_from.is_some() || due_to.is_some() {
                let Some(due) = &action.due else { continue };
                if due_from.as_ref().is_some_and(|f| due < f)
                    || due_to.as_ref().is_some_and(|t| due > t)
                {
                    continue;
                }
            }
            if query.done.is_some_and(|done| done != action.done) {
                continue;
            }
            let mine = action
                .owner
                .as_ref()
                .is_some_and(|o| me.contains(&o.trim().to_lowercase()));
            items.push(ActionDto {
                meeting: row.id.clone(),
                title: meeting.title.clone(),
                started: meeting.started,
                index,
                text: action.text,
                owner: action.owner,
                due: action.due,
                priority: action.priority,
                source_ms: action.source_ms,
                done: action.done,
                done_at: action.done_at,
                mine,
            });
        }
    }
    Ok(Json(items))
}

#[derive(Deserialize)]
pub struct PatchAction {
    done: bool,
    /// The item's text as the client last saw it. When given and different,
    /// the request is a 409: a summary rerun has replaced the items since,
    /// and `index` may name another one now.
    text: Option<String>,
}

/// Checks the item off (`done_at` now) or reopens it (`done_at` cleared),
/// then saves `meeting.json`, rewrites `meeting.md` and the index row, and
/// tells the vault sync. Marking an item done again keeps its `done_at`.
///
/// 409 while a job processes the meeting, as for other edits: the job saves
/// its own copy of `meeting.json` and would undo this one. A summary rerun
/// carries done marks over; see `recall_pipeline::summarize::carry_done`.
pub async fn patch(
    State(state): State<AppState>,
    Path((id, index)): Path<(String, usize)>,
    Json(body): Json<PatchAction>,
) -> ApiResult<Json<ActionItem>> {
    // Held from load to save, as in `PATCH /api/meetings/{id}`, so a rerun
    // cannot be queued between the busy check and the save.
    let speakers = state.speakers.lock().expect("speakers lock poisoned");
    let dir = open_dir(&state.config, &id)?;
    ensure_idle(&state.index.lock().expect("index lock poisoned"), &dir.id)?;
    let mut meeting = load(&dir)?;
    let action = meeting
        .summary
        .as_mut()
        .and_then(|s| s.actions.get_mut(index))
        .ok_or_else(|| ApiError::not_found(format!("no action item {index}")))?;
    if body.text.as_ref().is_some_and(|t| *t != action.text) {
        return Err(ApiError::conflict(
            "the action items have changed; reload them",
        ));
    }
    if action.done != body.done {
        action.done = body.done;
        action.done_at = body.done.then(Utc::now);
    }
    let action = action.clone();
    meeting.save(&dir.path())?;
    let index = state.index.lock().expect("index lock poisoned");
    refresh(&state.config, &index, &speakers, &meeting, &dir)?;
    if let Some(vault) = &state.vault {
        vault.touch(&dir.id, SyncWhen::Debounced);
    }
    Ok(Json(action))
}
