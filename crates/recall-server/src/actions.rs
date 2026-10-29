//! `GET /api/action-items`: the action items of every meeting's summary.

use axum::Json;
use axum::extract::{Query, State};
use chrono::{DateTime, Utc};
use recall_store::{ListFilter, Meeting, MeetingDir, MeetingId};
use serde::{Deserialize, Serialize};

use crate::AppState;
use crate::error::ApiResult;
use crate::meetings::{SyncedSet, non_empty};

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
    /// `true`: only meetings marked for sync.
    #[serde(default)]
    synced: bool,
}

#[derive(Serialize)]
pub struct ActionDto {
    meeting: MeetingId,
    title: String,
    started: DateTime<Utc>,
    text: String,
    owner: Option<String>,
    due: Option<String>,
}

/// Newest meeting first, each meeting's items in summary order. A meeting
/// whose `meeting.json` does not load is left out with a warning.
pub async fn list(
    State(state): State<AppState>,
    Query(query): Query<ActionQuery>,
) -> ApiResult<Json<Vec<ActionDto>>> {
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
        for action in summary.actions {
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
            items.push(ActionDto {
                meeting: row.id.clone(),
                title: meeting.title.clone(),
                started: meeting.started,
                text: action.text,
                owner: action.owner,
                due: action.due,
            });
        }
    }
    Ok(Json(items))
}
