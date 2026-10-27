//! SQLite index over the meeting directories, plus the processing job queue.
//!
//! The directories are the source of truth: `meetings` and `segments` can be
//! dropped and rebuilt from them at any time with [`Index::rebuild`]. `jobs`
//! exists only here and survives a rebuild.

use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use chrono::{DateTime, Utc};
use rusqlite::types::Value;
use rusqlite::{Connection, OptionalExtension, Row, params, params_from_iter};

use crate::dir::MeetingDir;
use crate::markdown::segment_speaker;
use crate::meeting::{Meeting, MeetingId, Step, StepState};
use crate::speakers::Speakers;

const SCHEMA: &str = "
PRAGMA journal_mode=WAL;
CREATE TABLE IF NOT EXISTS meetings (
    id TEXT PRIMARY KEY,
    title TEXT NOT NULL,
    started INTEGER NOT NULL,      -- Unix milliseconds
    duration_ms INTEGER NOT NULL,
    source_app TEXT,
    one_line TEXT,
    participants TEXT NOT NULL,    -- JSON array of display names
    state TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS meetings_started ON meetings(started);
CREATE VIRTUAL TABLE IF NOT EXISTS segments USING fts5(
    meeting UNINDEXED, start_ms UNINDEXED, speaker, text, tokenize='trigram'
);
CREATE TABLE IF NOT EXISTS jobs (
    id INTEGER PRIMARY KEY,
    meeting TEXT NOT NULL,
    from_step TEXT NOT NULL,
    state TEXT NOT NULL,           -- 'pending' | 'running' | 'done' | 'failed'
    attempts INTEGER NOT NULL DEFAULT 0,
    error TEXT,
    created INTEGER NOT NULL       -- Unix milliseconds
);
CREATE INDEX IF NOT EXISTS jobs_state ON jobs(state, created);
";

/// The daemon, the web server and the CLI may open the index at once.
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);
/// The FTS5 trigram tokenizer cannot match queries shorter than this many
/// characters; those fall back to `LIKE`.
const TRIGRAM: usize = 3;
/// Characters of context on each side of a match in a `LIKE` snippet, so
/// fallback snippets are about as long as FTS5's 64-token ones.
const SNIPPET_CONTEXT: usize = 32;
/// Speaker name for an unattributed remote segment.
const THEM: &str = "Them";

pub struct Index {
    conn: Connection,
}

#[derive(Clone, Debug, PartialEq)]
pub struct MeetingRow {
    pub id: MeetingId,
    pub title: String,
    pub started: DateTime<Utc>,
    pub duration_ms: u32,
    pub source_app: Option<String>,
    pub one_line: Option<String>,
    pub participants: Vec<String>,
    /// `"recording"`, `"processing:<step>"`, `"done"` or `"failed:<step>"`.
    pub state: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Hit {
    pub meeting: MeetingId,
    pub start_ms: u32,
    pub speaker: String,
    /// Text around the match, with the match wrapped in `[` `]`.
    pub snippet: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Job {
    pub id: i64,
    pub meeting: MeetingId,
    pub from_step: Step,
    /// Times this job has been started, including the current run.
    pub attempts: u32,
}

/// Conditions for [`Index::list`]; every set field must hold.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ListFilter {
    /// Substring of the title or of any transcript segment.
    pub q: Option<String>,
    /// Source app, compared case-insensitively.
    pub app: Option<String>,
    /// A participant's display name, compared case-insensitively.
    pub participant: Option<String>,
    /// Inclusive lower bound on the start time.
    pub from: Option<DateTime<Utc>>,
    /// Exclusive upper bound on the start time.
    pub to: Option<DateTime<Utc>>,
}

/// The list state of a meeting. Without `audio.opus` it is still
/// `"recording"`. Otherwise: `"failed:<step>"` for the first failed step,
/// else `"processing:<step>"` for the first running step, else `"done"` if
/// every step is done, else `"processing:<step>"` for the first step that is
/// not done. Steps absent from `m.steps` count as pending.
pub fn state_of(m: &Meeting, has_audio: bool) -> String {
    if !has_audio {
        return "recording".into();
    }
    let state = |step: &Step| m.steps.get(step).unwrap_or(&StepState::Pending);
    if let Some(step) = Step::ALL
        .iter()
        .find(|s| matches!(state(s), StepState::Failed { .. }))
    {
        return format!("failed:{}", step_name(*step));
    }
    let running = Step::ALL
        .iter()
        .find(|s| matches!(state(s), StepState::Running));
    let not_done = Step::ALL
        .iter()
        .find(|s| !matches!(state(s), StepState::Done { .. }));
    match running.or(not_done) {
        Some(step) => format!("processing:{}", step_name(*step)),
        None => "done".into(),
    }
}

/// The step's `snake_case` name, as in `meeting.json`.
fn step_name(step: Step) -> String {
    match serde_json::to_value(step) {
        Ok(serde_json::Value::String(s)) => s,
        _ => unreachable!("Step serializes as a string"),
    }
}

fn parse_step(name: &str) -> Result<Step> {
    serde_json::from_value(serde_json::Value::String(name.into()))
        .with_context(|| format!("unknown step {name:?}"))
}

/// `q` as an FTS5 phrase restricted to the `text` column, so FTS5 syntax in
/// user input matches literally.
fn fts_phrase(q: &str) -> String {
    format!("text : \"{}\"", q.replace('"', "\"\""))
}

/// `q` as a `LIKE` substring pattern with `\` as the escape character.
fn like_pattern(q: &str) -> String {
    let mut out = String::with_capacity(q.len() + 2);
    out.push('%');
    for c in q.chars() {
        if matches!(c, '%' | '_' | '\\') {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('%');
    out
}

/// A snippet like FTS5's for a `LIKE` match: up to [`SNIPPET_CONTEXT`]
/// characters each side of the first case-insensitive match of `q`.
fn like_snippet(text: &str, q: &str) -> String {
    // ASCII lowercasing keeps byte offsets, and LIKE folds ASCII case only.
    let Some(start) = text.to_ascii_lowercase().find(&q.to_ascii_lowercase()) else {
        return text.to_string();
    };
    let end = start + q.len();
    let before: Vec<char> = text[..start].chars().collect();
    let after: Vec<char> = text[end..].chars().collect();
    let lead = &before[before.len().saturating_sub(SNIPPET_CONTEXT)..];
    let tail = &after[..after.len().min(SNIPPET_CONTEXT)];
    let ellipsis = |cut: bool| if cut { "…" } else { "" };
    format!(
        "{}{}[{}]{}{}",
        ellipsis(lead.len() < before.len()),
        lead.iter().collect::<String>(),
        &text[start..end],
        tail.iter().collect::<String>(),
        ellipsis(tail.len() < after.len()),
    )
}

fn millis(t: DateTime<Utc>) -> i64 {
    t.timestamp_millis()
}

fn meeting_row(row: &Row) -> rusqlite::Result<MeetingRow> {
    let started: i64 = row.get("started")?;
    let participants: String = row.get("participants")?;
    Ok(MeetingRow {
        id: MeetingId(row.get("id")?),
        title: row.get("title")?,
        started: DateTime::from_timestamp_millis(started).unwrap_or_default(),
        duration_ms: row.get("duration_ms")?,
        source_app: row.get("source_app")?,
        one_line: row.get("one_line")?,
        participants: serde_json::from_str(&participants).unwrap_or_default(),
        state: row.get("state")?,
    })
}

fn hit(row: &Row) -> rusqlite::Result<Hit> {
    Ok(Hit {
        meeting: MeetingId(row.get(0)?),
        start_ms: row.get(1)?,
        speaker: row.get(2)?,
        snippet: row.get(3)?,
    })
}

/// Replaces the meeting's row and segments. Runs on whatever transaction
/// `conn` is in.
fn upsert_on(conn: &Connection, m: &Meeting, speakers: &Speakers, has_audio: bool) -> Result<()> {
    let participants: Vec<String> = m
        .participants()
        .iter()
        .map(|id| speakers.name_of(id))
        .collect();
    conn.execute(
        "INSERT OR REPLACE INTO meetings
         (id, title, started, duration_ms, source_app, one_line, participants, state)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![
            m.id.0,
            m.title,
            millis(m.started),
            m.duration_ms,
            m.source_app,
            m.summary.as_ref().map(|s| &s.one_line),
            serde_json::to_string(&participants)?,
            state_of(m, has_audio),
        ],
    )?;
    conn.execute("DELETE FROM segments WHERE meeting = ?1", [&m.id.0])?;
    let mut insert = conn.prepare_cached(
        "INSERT INTO segments (meeting, start_ms, speaker, text) VALUES (?1, ?2, ?3, ?4)",
    )?;
    for segment in &m.segments {
        insert.execute(params![
            m.id.0,
            segment.start_ms,
            segment_speaker(segment, speakers, THEM),
            segment.text,
        ])?;
    }
    Ok(())
}

impl Index {
    /// Opens or creates the index at `path`, creating its parent directory
    /// and its tables.
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        let conn = Connection::open(path).with_context(|| format!("opening {}", path.display()))?;
        conn.busy_timeout(BUSY_TIMEOUT)?;
        conn.execute_batch(SCHEMA)
            .with_context(|| format!("creating schema in {}", path.display()))?;
        Ok(Self { conn })
    }

    /// Inserts or replaces the meeting and its segments. `has_audio` is
    /// whether `audio.opus` exists yet; see [`state_of`].
    pub fn upsert(&self, m: &Meeting, speakers: &Speakers, has_audio: bool) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        upsert_on(&tx, m, speakers, has_audio)?;
        tx.commit()?;
        Ok(())
    }

    pub fn remove(&self, id: &MeetingId) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        tx.execute("DELETE FROM meetings WHERE id = ?1", [&id.0])?;
        tx.execute("DELETE FROM segments WHERE meeting = ?1", [&id.0])?;
        tx.commit()?;
        Ok(())
    }

    /// Meetings matching `filter`, newest first.
    pub fn list(&self, filter: &ListFilter) -> Result<Vec<MeetingRow>> {
        let mut sql = String::from("SELECT * FROM meetings WHERE 1");
        let mut args: Vec<Value> = Vec::new();
        let mut arg = |sql: &mut String, clause: &str, value: Value| {
            args.push(value);
            sql.push_str(&clause.replace('?', &format!("?{}", args.len())));
        };
        if let Some(q) = filter.q.as_deref().map(str::trim).filter(|q| !q.is_empty()) {
            arg(
                &mut sql,
                " AND (title LIKE ? ESCAPE '\\'",
                Value::Text(like_pattern(q)),
            );
            if q.chars().count() < TRIGRAM {
                arg(
                    &mut sql,
                    " OR id IN (SELECT meeting FROM segments WHERE text LIKE ? ESCAPE '\\'))",
                    Value::Text(like_pattern(q)),
                );
            } else {
                arg(
                    &mut sql,
                    " OR id IN (SELECT meeting FROM segments WHERE segments MATCH ?))",
                    Value::Text(fts_phrase(q)),
                );
            }
        }
        if let Some(app) = &filter.app {
            arg(
                &mut sql,
                " AND source_app = ? COLLATE NOCASE",
                Value::Text(app.clone()),
            );
        }
        if let Some(name) = &filter.participant {
            arg(
                &mut sql,
                " AND EXISTS (SELECT 1 FROM json_each(participants) WHERE value = ? COLLATE NOCASE)",
                Value::Text(name.clone()),
            );
        }
        if let Some(from) = filter.from {
            arg(&mut sql, " AND started >= ?", Value::Integer(millis(from)));
        }
        if let Some(to) = filter.to {
            arg(&mut sql, " AND started < ?", Value::Integer(millis(to)));
        }
        sql.push_str(" ORDER BY started DESC, id DESC");
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(params_from_iter(args), meeting_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Transcript segments containing `q`, best matches first, at most
    /// `limit`. An empty query has no hits.
    pub fn search(&self, q: &str, limit: usize) -> Result<Vec<Hit>> {
        let q = q.trim();
        if q.is_empty() {
            return Ok(Vec::new());
        }
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        if q.chars().count() >= TRIGRAM {
            let mut stmt = self.conn.prepare_cached(
                "SELECT meeting, start_ms, speaker,
                        snippet(segments, 3, '[', ']', '…', 64)
                 FROM segments WHERE segments MATCH ?1 ORDER BY rank LIMIT ?2",
            )?;
            let rows = stmt.query_map(params![fts_phrase(q), limit], hit)?;
            return Ok(rows.collect::<rusqlite::Result<_>>()?);
        }
        let mut stmt = self.conn.prepare_cached(
            "SELECT meeting, start_ms, speaker, text FROM segments
             WHERE text LIKE ?1 ESCAPE '\\'
             ORDER BY meeting DESC, start_ms LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![like_pattern(q), limit], hit)?;
        let mut hits = rows.collect::<rusqlite::Result<Vec<_>>>()?;
        for h in &mut hits {
            h.snippet = like_snippet(&h.snippet, q);
        }
        Ok(hits)
    }

    /// Replaces `meetings` and `segments` with what is on disk under `root`
    /// and returns the number of meetings indexed. Each meeting is indexed
    /// under its directory name, whatever id its `meeting.json` holds.
    /// Directories whose `meeting.json` does not parse are skipped.
    pub fn rebuild(&self, root: &Path, speakers: &Speakers) -> Result<usize> {
        let tx = self.conn.unchecked_transaction()?;
        tx.execute("DELETE FROM meetings", [])?;
        tx.execute("DELETE FROM segments", [])?;
        let mut count = 0;
        for id in MeetingDir::list(root)? {
            let dir = MeetingDir {
                root: root.to_path_buf(),
                id,
            };
            let Ok(mut meeting) = Meeting::load(&dir.path()) else {
                continue;
            };
            meeting.id = dir.id.clone();
            upsert_on(&tx, &meeting, speakers, dir.audio().exists())?;
            count += 1;
        }
        tx.commit()?;
        Ok(count)
    }

    /// Queues processing of `meeting` from step `from`; returns the job id.
    pub fn enqueue(&self, meeting: &MeetingId, from: Step) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO jobs (meeting, from_step, state, attempts, created)
             VALUES (?1, ?2, 'pending', 0, ?3)",
            params![meeting.0, step_name(from), millis(Utc::now())],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Takes the oldest pending job: marks it running, counts the attempt,
    /// and returns it. One statement, so two callers never get the same job.
    pub fn next_job(&self) -> Result<Option<Job>> {
        let row = self
            .conn
            .query_row(
                "UPDATE jobs SET state = 'running', attempts = attempts + 1
                 WHERE id = (SELECT id FROM jobs WHERE state = 'pending'
                             ORDER BY created, id LIMIT 1)
                 RETURNING id, meeting, from_step, attempts",
                [],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, u32>(3)?,
                    ))
                },
            )
            .optional()?;
        row.map(|(id, meeting, step, attempts)| {
            Ok(Job {
                id,
                meeting: MeetingId(meeting),
                from_step: parse_step(&step)?,
                attempts,
            })
        })
        .transpose()
    }

    /// Marks a job done (`ok`) or failed with `error`.
    pub fn finish_job(&self, id: i64, ok: bool, error: Option<&str>) -> Result<()> {
        let state = if ok { "done" } else { "failed" };
        let changed = self.conn.execute(
            "UPDATE jobs SET state = ?1, error = ?2 WHERE id = ?3",
            params![state, error, id],
        )?;
        if changed == 0 {
            return Err(anyhow!("no job with id {id}"));
        }
        Ok(())
    }

    /// Returns jobs left running by a daemon that stopped to pending. Call on
    /// daemon start, before taking jobs.
    pub fn reset_running_jobs(&self) -> Result<()> {
        self.conn.execute(
            "UPDATE jobs SET state = 'pending' WHERE state = 'running'",
            [],
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dir::MeetingDir;
    use crate::meeting::{Segment, Side, SpeakerId, StepState, Summary};
    use chrono::{Local, TimeZone};
    use std::collections::BTreeMap;

    fn meeting(id: &str, app: Option<&str>, texts: &[&str]) -> Meeting {
        let started = Utc.with_ymd_and_hms(2026, 10, 27, 6, 30, 0).unwrap();
        Meeting {
            id: MeetingId(id.into()),
            title: format!("Title of {id}"),
            started,
            duration_ms: 60_000,
            source_app: app.map(String::from),
            steps: BTreeMap::new(),
            segments: texts
                .iter()
                .enumerate()
                .map(|(i, t)| Segment {
                    start_ms: i as u32 * 1000,
                    end_ms: i as u32 * 1000 + 900,
                    side: Side::Me,
                    speaker: Some(SpeakerId("me".into())),
                    text: (*t).into(),
                })
                .collect(),
            clusters: vec![],
            summary: None,
            models: BTreeMap::new(),
        }
    }

    fn open() -> (tempfile::TempDir, Index) {
        let dir = tempfile::tempdir().unwrap();
        let index = Index::open(&dir.path().join("data").join("index.sqlite")).unwrap();
        (dir, index)
    }

    fn ids(rows: &[MeetingRow]) -> Vec<&str> {
        rows.iter().map(|r| r.id.0.as_str()).collect()
    }

    #[test]
    fn index_search_finds_chinese_substring() {
        let (_dir, index) = open();
        let speakers = Speakers::default();
        let m = meeting("a", None, &["早安", "這段 export 會 timeout", "好"]);
        index.upsert(&m, &speakers, true).unwrap();
        index
            .upsert(&meeting("b", None, &["沒有相關內容"]), &speakers, true)
            .unwrap();

        let hits = index.search("export 會", 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].meeting, MeetingId("a".into()));
        assert_eq!(hits[0].start_ms, 1000);
        assert_eq!(hits[0].speaker, "Me");
        assert!(
            hits[0].snippet.contains("[export 會]"),
            "{}",
            hits[0].snippet
        );

        // Shorter than a trigram: falls back to LIKE.
        let short = index.search("早安", 10).unwrap();
        assert_eq!(short.len(), 1);
        assert_eq!(short[0].snippet, "[早安]");
        let long = format!("{}早安{}", "x".repeat(40), "y".repeat(40));
        assert_eq!(
            like_snippet(&long, "早安"),
            format!("…{}[早安]{}…", "x".repeat(32), "y".repeat(32))
        );
        assert_eq!(like_snippet("Hello", "he"), "[He]llo");
        assert!(index.search("  ", 10).unwrap().is_empty());
        // FTS5 syntax in user input is matched literally.
        assert!(index.search("\"export", 10).unwrap().is_empty());
        assert!(index.search("a OR b*", 10).unwrap().is_empty());
        assert!(index.search("50%", 10).unwrap().is_empty());
    }

    #[test]
    fn upsert_replaces_segments() {
        let (_dir, index) = open();
        let speakers = Speakers::default();
        index
            .upsert(&meeting("a", None, &["old words here"]), &speakers, true)
            .unwrap();
        index
            .upsert(&meeting("a", None, &["new words here"]), &speakers, true)
            .unwrap();
        assert!(index.search("old", 10).unwrap().is_empty());
        assert_eq!(index.search("new", 10).unwrap().len(), 1);
        assert_eq!(index.list(&ListFilter::default()).unwrap().len(), 1);
        index.remove(&MeetingId("a".into())).unwrap();
        assert!(index.search("new", 10).unwrap().is_empty());
        assert!(index.list(&ListFilter::default()).unwrap().is_empty());
    }

    #[test]
    fn index_list_filters_by_app() {
        let (_dir, index) = open();
        let speakers = Speakers::default();
        let mut zoom = meeting("2026-10-27 09:00 a", Some("zoom"), &["budget review"]);
        zoom.started = Utc.with_ymd_and_hms(2026, 10, 27, 1, 0, 0).unwrap();
        zoom.summary = Some(Summary {
            one_line: "Budget.".into(),
            body: String::new(),
            decisions: vec![],
            actions: vec![],
            chapters: vec![],
        });
        let mut meet = meeting("2026-10-27 11:00 b", Some("Google Meet"), &["roadmap"]);
        meet.started = Utc.with_ymd_and_hms(2026, 10, 27, 3, 0, 0).unwrap();
        let mut tom = speakers.clone();
        let tom_id = tom.add("Tom", None);
        meet.segments[0].speaker = Some(tom_id);
        index.upsert(&zoom, &tom, true).unwrap();
        index.upsert(&meet, &tom, false).unwrap();

        let all = index.list(&ListFilter::default()).unwrap();
        assert_eq!(ids(&all), ["2026-10-27 11:00 b", "2026-10-27 09:00 a"]);
        assert_eq!(all[0].state, "recording");
        assert_eq!(all[0].participants, ["Tom"]);
        assert_eq!(all[1].one_line.as_deref(), Some("Budget."));
        assert_eq!(all[1].started, zoom.started);
        assert_eq!(all[1].source_app.as_deref(), Some("zoom"));

        let by_app = |app: &str| ListFilter {
            app: Some(app.into()),
            ..ListFilter::default()
        };
        assert_eq!(
            ids(&index.list(&by_app("zoom")).unwrap()),
            ["2026-10-27 09:00 a"]
        );
        assert_eq!(
            ids(&index.list(&by_app("ZOOM")).unwrap()),
            ["2026-10-27 09:00 a"]
        );
        assert!(index.list(&by_app("teams")).unwrap().is_empty());

        let by_person = ListFilter {
            participant: Some("Tom".into()),
            ..ListFilter::default()
        };
        assert_eq!(
            ids(&index.list(&by_person).unwrap()),
            ["2026-10-27 11:00 b"]
        );

        let range = ListFilter {
            from: Some(Utc.with_ymd_and_hms(2026, 10, 27, 2, 0, 0).unwrap()),
            to: Some(Utc.with_ymd_and_hms(2026, 10, 27, 4, 0, 0).unwrap()),
            ..ListFilter::default()
        };
        assert_eq!(ids(&index.list(&range).unwrap()), ["2026-10-27 11:00 b"]);

        let query = |q: &str| ListFilter {
            q: Some(q.into()),
            ..ListFilter::default()
        };
        assert_eq!(
            ids(&index.list(&query("budget")).unwrap()),
            ["2026-10-27 09:00 a"]
        );
        // Title match, with a short query.
        assert_eq!(index.list(&query("b")).unwrap().len(), 2);
        assert_eq!(
            ids(&index.list(&query("Title of 2026-10-27 11")).unwrap()),
            ["2026-10-27 11:00 b"]
        );
        assert!(index.list(&query("nothing like it")).unwrap().is_empty());
    }

    #[test]
    fn state_of_follows_steps() {
        let at = Utc::now();
        let mut m = meeting("a", None, &[]);
        assert_eq!(state_of(&m, false), "recording");
        assert_eq!(state_of(&m, true), "processing:segment");
        for step in Step::ALL {
            m.steps.insert(step, StepState::Done { at });
        }
        assert_eq!(state_of(&m, true), "done");
        m.steps.insert(Step::Name, StepState::Pending);
        assert_eq!(state_of(&m, true), "processing:name");
        m.steps.insert(Step::Summarize, StepState::Running);
        assert_eq!(state_of(&m, true), "processing:summarize");
        m.steps.insert(
            Step::Write,
            StepState::Failed {
                at,
                error: "x".into(),
            },
        );
        assert_eq!(state_of(&m, true), "failed:write");
    }

    #[test]
    fn index_rebuild_counts_dirs() {
        let (_dir, index) = open();
        let root = tempfile::tempdir().unwrap();
        let speakers = Speakers::default();
        let started = Local.with_ymd_and_hms(2026, 10, 27, 9, 0, 0).unwrap();
        for title in ["One", "Two", "Three"] {
            let dir = MeetingDir::create(root.path(), started, title).unwrap();
            let mut m = meeting(&dir.id.0, None, &["searchable text"]);
            // A stale id in meeting.json is replaced by the directory name.
            m.id = MeetingId("stale".into());
            m.save(&dir.path()).unwrap();
        }
        MeetingDir::create(root.path(), started, "Empty").unwrap();
        index
            .upsert(
                &meeting("gone", None, &["searchable text"]),
                &speakers,
                true,
            )
            .unwrap();

        assert_eq!(index.rebuild(root.path(), &speakers).unwrap(), 3);
        let rows = index.list(&ListFilter::default()).unwrap();
        assert_eq!(
            ids(&rows),
            [
                "2026-10-27 09:00 Two",
                "2026-10-27 09:00 Three",
                "2026-10-27 09:00 One"
            ]
        );
        assert!(rows.iter().all(|r| r.state == "recording"));
        assert_eq!(index.search("searchable", 10).unwrap().len(), 3);
        assert_eq!(index.search("searchable", 2).unwrap().len(), 2);
    }

    #[test]
    fn queue_pops_oldest_and_resets_running() {
        let (_dir, index) = open();
        let a = MeetingId("a".into());
        let b = MeetingId("b".into());
        let ja = index.enqueue(&a, Step::Segment).unwrap();
        let jb = index.enqueue(&b, Step::Summarize).unwrap();

        let first = index.next_job().unwrap().unwrap();
        assert_eq!((first.id, &first.meeting), (ja, &a));
        assert_eq!(first.from_step, Step::Segment);
        assert_eq!(first.attempts, 1);
        let second = index.next_job().unwrap().unwrap();
        assert_eq!((second.id, second.from_step), (jb, Step::Summarize));
        assert!(index.next_job().unwrap().is_none());

        index.finish_job(jb, true, None).unwrap();
        index.reset_running_jobs().unwrap();
        let again = index.next_job().unwrap().unwrap();
        assert_eq!((again.id, again.attempts), (ja, 2));
        index.finish_job(ja, false, Some("boom")).unwrap();
        assert!(index.next_job().unwrap().is_none());
        // Jobs survive a rebuild.
        let empty = tempfile::tempdir().unwrap();
        index.rebuild(empty.path(), &Speakers::default()).unwrap();
        let jc = index.enqueue(&a, Step::Write).unwrap();
        assert!(jc > jb);
    }
}
