//! `/api/people`: the speaker registry, with per-person meeting counts,
//! renames and merges.

use std::collections::HashMap;

use axum::Json;
use axum::extract::{Path, State};
use recall_store::{Meeting, MeetingDir, Person, Side, SpeakerId, Speakers};
use serde::{Deserialize, Serialize};

use crate::AppState;
use crate::error::{ApiError, ApiResult};
use crate::meetings::refresh;

#[derive(Serialize)]
pub struct PersonDto {
    id: SpeakerId,
    name: String,
    aliases: Vec<String>,
    /// Stored voice embeddings.
    samples: usize,
    /// Meetings in which the person speaks.
    meetings: usize,
}

fn dto(person: &Person, counts: &HashMap<SpeakerId, usize>) -> PersonDto {
    PersonDto {
        id: person.id.clone(),
        name: person.name.clone(),
        aliases: person.aliases.clone(),
        samples: person.embeddings.len(),
        meetings: counts.get(&person.id).copied().unwrap_or(0),
    }
}

/// Every meeting that loads, with its directory. Unreadable ones are logged
/// and skipped so one bad file does not hide the rest.
fn all_meetings(state: &AppState) -> anyhow::Result<Vec<(MeetingDir, Meeting)>> {
    let root = &state.config.meetings_dir;
    let mut out = Vec::new();
    for id in MeetingDir::list(root)? {
        let dir = MeetingDir {
            root: root.clone(),
            id,
        };
        match Meeting::load(&dir.path()) {
            Ok(meeting) => out.push((dir, meeting)),
            Err(e) => tracing::warn!("skipping {}: {e:#}", dir.id.0),
        }
    }
    Ok(out)
}

fn meeting_counts<'a>(meetings: impl Iterator<Item = &'a Meeting>) -> HashMap<SpeakerId, usize> {
    let mut counts = HashMap::new();
    for meeting in meetings {
        for id in meeting.participants() {
            *counts.entry(id).or_insert(0) += 1;
        }
    }
    counts
}

pub async fn list(State(state): State<AppState>) -> ApiResult<Json<Vec<PersonDto>>> {
    let meetings = all_meetings(&state)?;
    let counts = meeting_counts(meetings.iter().map(|(_, m)| m));
    let speakers = state.speakers.lock().expect("speakers lock poisoned");
    Ok(Json(
        speakers.people().iter().map(|p| dto(p, &counts)).collect(),
    ))
}

#[derive(Deserialize)]
pub struct PatchPerson {
    name: Option<String>,
    /// Fold this person into the one with this id.
    merge_into: Option<SpeakerId>,
}

/// Renames and/or merges, saves `speakers.json`, then rewrites `meeting.md`
/// and the index row of every meeting the change shows up in. A merge also
/// moves the dropped id's segments and clusters to the kept id in
/// `meeting.json`. Returns the surviving person.
pub async fn patch(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<PatchPerson>,
) -> ApiResult<Json<PersonDto>> {
    let id = SpeakerId(id);
    let mut speakers = state.speakers.lock().expect("speakers lock poisoned");
    // A running job renders names into its meeting and rewrites
    // speakers.json; an edit now would be lost or half applied.
    if state
        .index
        .lock()
        .expect("index lock poisoned")
        .running_job()?
        .is_some()
    {
        return Err(ApiError::conflict(
            "a meeting is being processed; try again when it finishes",
        ));
    }
    if speakers.get(&id).is_none() {
        return Err(ApiError::not_found(format!("no speaker {:?}", id.0)));
    }
    if body.name.is_some() && body.merge_into.is_some() {
        return Err(ApiError::bad_request(
            "send either name or merge_into, not both",
        ));
    }
    let mut next: Speakers = speakers.clone();
    if let Some(name) = &body.name {
        let name = name.trim();
        if name.is_empty() {
            return Err(ApiError::bad_request("empty name"));
        }
        next.rename(&id, name)?;
    }
    let survivor = match &body.merge_into {
        Some(keep) => {
            next.merge(keep, &id)
                .map_err(|e| ApiError::bad_request(format!("{e:#}")))?;
            keep.clone()
        }
        None => id.clone(),
    };
    next.save(&state.config.speakers_path())?;
    *speakers = next;

    let is_me = speakers.me().id == survivor;
    let mut meetings = all_meetings(&state)?;
    let index = state.index.lock().expect("index lock poisoned");
    let (mut updated, mut failed) = (0usize, Vec::new());
    for (dir, meeting) in &mut meetings {
        let mut moved = false;
        if body.merge_into.is_some() {
            let speaker_slots = meeting
                .segments
                .iter_mut()
                .map(|s| &mut s.speaker)
                .chain(meeting.clusters.iter_mut().map(|c| &mut c.speaker));
            for slot in speaker_slots.filter(|s| s.as_ref() == Some(&id)) {
                *slot = Some(survivor.clone());
                moved = true;
            }
        }
        // Unattributed microphone segments are shown with `me`'s name.
        let shows_me = is_me
            && meeting
                .segments
                .iter()
                .any(|s| s.speaker.is_none() && s.side == Side::Me);
        let mentions = meeting.participants().contains(&survivor)
            || meeting
                .clusters
                .iter()
                .any(|c| c.speaker.as_ref() == Some(&survivor));
        if !(moved || shows_me || mentions) {
            continue;
        }
        let result = if moved {
            meeting.save(&dir.path())
        } else {
            Ok(())
        };
        match result.and_then(|()| refresh(&state.config, &index, &speakers, meeting, dir)) {
            Ok(()) => updated += 1,
            Err(e) => {
                tracing::warn!("updating {} after a people edit: {e:#}", dir.id.0);
                failed.push(dir.id.0.clone());
            }
        }
    }
    // The registry is already saved; a few stale meetings are reported in the
    // log, and only a total failure is an error.
    if updated == 0 && !failed.is_empty() {
        return Err(anyhow::anyhow!(
            "speakers.json is saved, but no meeting could be updated: {}",
            failed.join(", ")
        )
        .into());
    }

    let counts = meeting_counts(meetings.iter().map(|(_, m)| m));
    let person = speakers
        .get(&survivor)
        .expect("survivor is in the registry");
    Ok(Json(dto(person, &counts)))
}
