//! Publishing chosen meetings to the team's Obsidian vault, a git
//! repository, and keeping the published notes in step with the meetings.
//!
//! A meeting is published only while its `sync` switch is on, and only once
//! processing has finished; recall records every call, private ones too.
//! The note is the meeting's Markdown ([`markdown::render`], the same text
//! as `meeting.md`) at `<vault.folder>/<note name>.md`, see [`note_name`].
//! No audio or video leaves the machine.
//!
//! The git work runs on its own thread (see [`Syncer`]), so a slow push
//! never holds up the recorder, the worker or a request. It drives a local
//! clone at [`Config::vault_dir`] with the git CLI: every change is one
//! commit made on top of the freshly fetched branch, touching files only
//! under the folder, then pushed; a rejected push is rebased onto the new
//! upstream and pushed again. Nothing is ever force-pushed.
//!
//! The ledger at [`Config::vault_state_path`] records what was last pushed
//! for each meeting: the note's path, a hash of its text, when, and the last
//! error. The thread compares a meeting with its ledger entry and runs git
//! only on a difference, so every trigger (an edit, a finished job, the
//! periodic sweep) can be repeated freely, and a failed change is simply
//! looked at again later, with backoff.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use chrono::{DateTime, Utc};
use recall_server::{SyncWhen, Vault, VaultStatus};
use recall_store::{Config, Event, Index, Meeting, MeetingDir, MeetingId, Speakers, markdown};
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;

/// Quiet time after an edit before it is pushed, so typing through a
/// transcript makes one commit rather than one per line.
pub const EDIT_DEBOUNCE: Duration = Duration::from_secs(20);
/// How often every synced meeting is compared with the ledger. It catches
/// what no trigger reported: a missed event, a meeting folder renamed or
/// deleted by hand.
const SWEEP_INTERVAL: Duration = Duration::from_secs(10 * 60);
/// Wait before the first retry of a failed change; it doubles with every
/// further failure up to [`RETRY_MAX`].
const RETRY_FIRST: Duration = Duration::from_secs(30);
const RETRY_MAX: Duration = Duration::from_secs(30 * 60);
/// Pushes tried per change, each after rebasing onto what was pushed in the
/// meantime.
const PUSH_ATTEMPTS: usize = 3;
/// A git command running longer than this (a network stall) is killed and
/// the change fails, to be retried; the thread is never stuck on one.
const GIT_TIMEOUT: Duration = Duration::from_secs(120);
/// Credential helper for an `https` remote: the gh CLI hands git the token
/// of the account it is logged in as. Under the systemd user service gh
/// must find that token without a terminal: either in
/// `~/.config/gh/hosts.yml`, or in the Secret Service keyring, which needs
/// the session bus (systemd passes `DBUS_SESSION_BUS_ADDRESS` to user
/// units) and a keyring unlocked by the login. `gh` must also be on the
/// unit's `PATH`. When it is not, the push fails and the error shows on the
/// meeting page.
const GH_HELPER: &str = "!gh auth git-credential";
/// File in the clone's `.git` that marks it as made by recall.
const MARKER: &str = "recall-vault";

/// What was last pushed for one meeting, plus a switch value waiting for a
/// job to end.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
struct Entry {
    /// The note's path in the vault, relative to its root; `None` when it
    /// is not there.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    path: Option<String>,
    /// The meeting title, for the removal's commit message.
    #[serde(default)]
    title: String,
    /// [`content_hash`] of the pushed text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    hash: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    synced_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    /// Failures in a row, for the retry backoff.
    #[serde(default, skip_serializing_if = "is_zero")]
    failures: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    requested: Option<bool>,
}

fn is_zero(n: &u32) -> bool {
    *n == 0
}

impl Entry {
    /// Nothing worth keeping: not in the vault, no error, no request.
    fn is_empty(&self) -> bool {
        self.path.is_none() && self.error.is_none() && self.requested.is_none()
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Ledger {
    #[serde(default)]
    meetings: BTreeMap<MeetingId, Entry>,
}

impl Ledger {
    /// Reads the ledger; a missing file is an empty one.
    fn load(path: &Path) -> Result<Self> {
        match fs::read(path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .with_context(|| format!("parsing {}", path.display())),
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
        }
    }

    /// Writes the ledger through a temporary file and a rename.
    fn save(&self, path: &Path) -> Result<()> {
        let parent = path.parent().unwrap_or(Path::new("."));
        fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
        let mut tmp = tempfile::NamedTempFile::new_in(parent)
            .with_context(|| format!("creating a temporary file in {}", parent.display()))?;
        tmp.write_all(&serde_json::to_vec_pretty(self)?)
            .and_then(|()| tmp.as_file().sync_all())
            .with_context(|| format!("writing {}", tmp.path().display()))?;
        tmp.persist(path)
            .with_context(|| format!("renaming to {}", path.display()))?;
        Ok(())
    }
}

/// State shared by the handle the server holds and the sync thread. Lock
/// order: `speakers`, then `index`, then this.
#[derive(Debug, Default)]
struct Shared {
    ledger: Ledger,
    /// Meetings a user action is waiting on; shown as syncing.
    queued: BTreeSet<MeetingId>,
    /// Renames since the thread finished its last change, old id to new.
    /// The change in flight records its result under the new id.
    moved: BTreeMap<MeetingId, MeetingId>,
}

enum Msg {
    Touch(MeetingId, Duration),
    JobEnded(MeetingId),
    Sweep,
}

/// The server's handle on the vault sync; see [`Vault`].
pub struct VaultSync {
    shared: Arc<Mutex<Shared>>,
    tx: Sender<Msg>,
    state_path: PathBuf,
}

impl VaultSync {
    /// Loads the ledger and returns the handle with the [`Syncer`] to run.
    /// Errors when the `vault` config is invalid or the ledger unreadable.
    pub fn new(
        config: Arc<Config>,
        speakers: Arc<Mutex<Speakers>>,
        index: Arc<Mutex<Index>>,
    ) -> Result<(Arc<Self>, Syncer)> {
        config.vault.validate().map_err(|e| anyhow!(e))?;
        let state_path = config.vault_state_path();
        let shared = Arc::new(Mutex::new(Shared {
            ledger: Ledger::load(&state_path)?,
            ..Shared::default()
        }));
        let (tx, rx) = mpsc::channel();
        let repo = Repo::new(&config);
        let syncer = Syncer {
            config,
            speakers,
            index,
            shared: shared.clone(),
            state_path: state_path.clone(),
            repo,
            rx,
            due: BTreeMap::new(),
            next_sweep: Instant::now(),
        };
        let handle = Arc::new(Self {
            shared,
            tx,
            state_path,
        });
        Ok((handle, syncer))
    }

    /// Forwards the end of every job to the sync thread until `events`
    /// closes: a finished meeting may be due for publishing, and a switch
    /// flipped while it was processing can now be written. A lagged
    /// receiver missed some, so it asks for a sweep.
    pub async fn follow(self: Arc<Self>, mut events: broadcast::Receiver<Event>) {
        loop {
            let msg = match events.recv().await {
                Ok(Event::Done { meeting } | Event::Failed { meeting, .. }) => {
                    Msg::JobEnded(meeting)
                }
                Ok(_) => continue,
                Err(broadcast::error::RecvError::Lagged(_)) => Msg::Sweep,
                Err(broadcast::error::RecvError::Closed) => return,
            };
            if self.tx.send(msg).is_err() {
                return;
            }
        }
    }

    fn save(&self, shared: &Shared) {
        if let Err(err) = shared.ledger.save(&self.state_path) {
            tracing::error!(err = format!("{err:#}"), "saving the vault ledger");
        }
    }
}

impl Vault for VaultSync {
    fn touch(&self, id: &MeetingId, when: SyncWhen) {
        let delay = match when {
            SyncWhen::Now => Duration::ZERO,
            SyncWhen::Debounced => EDIT_DEBOUNCE,
        };
        lock(&self.shared).queued.insert(id.clone());
        let _ = self.tx.send(Msg::Touch(id.clone(), delay));
    }

    fn renamed(&self, from: &MeetingId, to: &MeetingId) {
        let mut shared = lock(&self.shared);
        if let Some(entry) = shared.ledger.meetings.remove(from) {
            shared.ledger.meetings.insert(to.clone(), entry);
            self.save(&shared);
        }
        if shared.queued.remove(from) {
            shared.queued.insert(to.clone());
        }
        shared.moved.insert(from.clone(), to.clone());
    }

    fn request(&self, id: &MeetingId, sync: Option<bool>) {
        let mut shared = lock(&self.shared);
        let entry = shared.ledger.meetings.entry(id.clone()).or_default();
        if entry.requested == sync {
            return;
        }
        entry.requested = sync;
        if entry.is_empty() {
            shared.ledger.meetings.remove(id);
        }
        self.save(&shared);
    }

    fn status(&self, id: &MeetingId) -> VaultStatus {
        let shared = lock(&self.shared);
        let entry = shared.ledger.meetings.get(id).cloned().unwrap_or_default();
        VaultStatus {
            requested: entry.requested,
            queued: shared.queued.contains(id),
            path: entry.path,
            synced_at: entry.synced_at,
            error: entry.error,
        }
    }
}

/// The note a meeting should have in the vault right now.
#[derive(Debug, PartialEq)]
enum Desired {
    /// The switch is off or the meeting is gone: no note.
    Absent,
    /// The switch is on but processing has not finished: leave whatever is
    /// there (nothing, or the note from before a rerun).
    Keep,
    Note {
        title: String,
        text: String,
    },
}

/// The sync thread's side: the queue of meetings to look at and the clone.
pub struct Syncer {
    config: Arc<Config>,
    speakers: Arc<Mutex<Speakers>>,
    index: Arc<Mutex<Index>>,
    shared: Arc<Mutex<Shared>>,
    state_path: PathBuf,
    repo: Repo,
    rx: Receiver<Msg>,
    /// When each meeting is next looked at, and whether that is a retry
    /// (which does not show the meeting as syncing).
    due: BTreeMap<MeetingId, (Instant, bool)>,
    next_sweep: Instant,
}

impl Syncer {
    /// Runs the sync on a new thread until the [`VaultSync`] is dropped. It
    /// starts with a sweep, which picks up anything left from before a
    /// restart.
    pub fn spawn(self) -> Result<JoinHandle<()>> {
        std::thread::Builder::new()
            .name("recall-vault".into())
            .spawn(move || self.run())
            .context("spawning the vault thread")
    }

    fn run(mut self) {
        loop {
            let now = Instant::now();
            if now >= self.next_sweep {
                self.sweep();
                self.next_sweep = now + SWEEP_INTERVAL;
            }
            self.run_due(Instant::now());
            let wake = self
                .due
                .values()
                .map(|(at, _)| *at)
                .min()
                .unwrap_or(self.next_sweep)
                .min(self.next_sweep);
            match self
                .rx
                .recv_timeout(wake.saturating_duration_since(Instant::now()))
            {
                Ok(msg) => self.handle(msg),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return,
            }
            self.drain();
        }
    }

    fn handle(&mut self, msg: Msg) {
        let now = Instant::now();
        match msg {
            Msg::Touch(id, delay) => {
                self.due.insert(id, (now + delay, false));
            }
            Msg::JobEnded(id) => {
                self.apply_request(&id);
                self.due.insert(id, (now, false));
            }
            Msg::Sweep => self.next_sweep = now,
        }
    }

    /// Handles every message already sent.
    fn drain(&mut self) {
        while let Ok(msg) = self.rx.try_recv() {
            self.handle(msg);
        }
    }

    /// Looks at every meeting due by `now` (or by the present, if later).
    pub fn run_due(&mut self, now: Instant) {
        self.drain();
        let now = now.max(Instant::now());
        let ready: Vec<MeetingId> = self
            .due
            .iter()
            .filter(|(_, (at, _))| *at <= now)
            .map(|(id, _)| id.clone())
            .collect();
        for id in ready {
            self.due.remove(&id);
            self.sync(&id);
            self.drain();
            let waiting = self.due.get(&id).is_some_and(|(_, retry)| !retry);
            if !waiting {
                lock(&self.shared).queued.remove(&id);
            }
        }
    }

    /// Schedules every meeting with the switch on or a ledger entry, and
    /// writes switch values left waiting for a job that has ended.
    fn sweep(&mut self) {
        let mut ids: BTreeSet<MeetingId> =
            lock(&self.shared).ledger.meetings.keys().cloned().collect();
        let requested: Vec<MeetingId> = lock(&self.shared)
            .ledger
            .meetings
            .iter()
            .filter(|(_, e)| e.requested.is_some())
            .map(|(id, _)| id.clone())
            .collect();
        for id in requested {
            self.apply_request(&id);
        }
        match MeetingDir::list(&self.config.meetings_dir) {
            Ok(all) => {
                for id in all {
                    let path = self.config.meetings_dir.join(&id.0);
                    if Meeting::load(&path).is_ok_and(|m| m.sync) {
                        ids.insert(id);
                    }
                }
            }
            Err(err) => tracing::warn!(err = format!("{err:#}"), "listing meetings for the vault"),
        }
        let now = Instant::now();
        for id in ids {
            self.due.entry(id).or_insert((now, false));
        }
    }

    /// Writes a switch value asked for while a job ran into `meeting.json`,
    /// once no job is pending or running for the meeting. The `speakers`
    /// lock is held from the check to the save, as the PATCH handler holds
    /// it from its own check to recording the request.
    fn apply_request(&self, id: &MeetingId) {
        let result = (|| -> Result<()> {
            let _speakers = lock(&self.speakers);
            let Some(sync) = lock(&self.shared)
                .ledger
                .meetings
                .get(id)
                .and_then(|e| e.requested)
            else {
                return Ok(());
            };
            if lock(&self.index).active_job(id)?.is_some() {
                return Ok(());
            }
            if let Ok(dir) = MeetingDir::open(&self.config.meetings_dir, id) {
                let mut meeting = Meeting::load(&dir.path())?;
                if meeting.sync != sync {
                    meeting.sync = sync;
                    meeting.save(&dir.path())?;
                }
            }
            let mut shared = lock(&self.shared);
            if let Some(entry) = shared.ledger.meetings.get_mut(id) {
                entry.requested = None;
                if entry.is_empty() {
                    shared.ledger.meetings.remove(id);
                }
            }
            self.save(&shared);
            Ok(())
        })();
        if let Err(err) = result {
            tracing::error!(
                err = format!("{err:#}"),
                id = id.0,
                "applying the vault switch"
            );
        }
    }

    /// Brings the meeting's note in the vault in line with the meeting, and
    /// records the outcome. A failure is retried with backoff.
    fn sync(&mut self, id: &MeetingId) {
        let result = self.try_sync(id);
        let mut shared = lock(&self.shared);
        let key = resolve(&shared.moved, id);
        shared.moved.clear();
        match result {
            Ok(None) => {}
            Ok(Some(done)) => {
                let entry = shared.ledger.meetings.entry(key.clone()).or_default();
                entry.path = done.path;
                entry.hash = done.hash;
                if let Some(title) = done.title {
                    entry.title = title;
                }
                if done.pushed {
                    entry.synced_at = Some(Utc::now());
                }
                entry.error = None;
                entry.failures = 0;
                if entry.is_empty() {
                    shared.ledger.meetings.remove(&key);
                }
                self.save(&shared);
            }
            Err(err) => {
                let error = format!("{err:#}");
                tracing::warn!(id = key.0, err = error, "publishing to the vault failed");
                let entry = shared.ledger.meetings.entry(key.clone()).or_default();
                entry.error = Some(error);
                entry.failures = entry.failures.saturating_add(1);
                let delay = backoff(entry.failures);
                self.save(&shared);
                drop(shared);
                self.due.insert(key, (Instant::now() + delay, true));
            }
        }
    }

    /// The git side of [`Syncer::sync`]. Returns `None` when nothing
    /// changed, else the ledger values to record.
    fn try_sync(&self, id: &MeetingId) -> Result<Option<Done>> {
        // The rename handler holds `speakers` from renaming the directory
        // to rekeying the ledger, so under it both agree on the id.
        let (desired, entry, path) = {
            let speakers = lock(&self.speakers);
            let desired = self.desired(id, &speakers)?;
            let shared = lock(&self.shared);
            let entry = shared.ledger.meetings.get(id).cloned().unwrap_or_default();
            let path = match &desired {
                Desired::Note { .. } => Some(self.note_path(&shared.ledger, id)),
                _ => None,
            };
            (desired, entry, path)
        };
        match desired {
            Desired::Keep => Ok(entry.error.is_some().then(|| Done::unchanged(&entry))),
            Desired::Absent => {
                let Some(old) = &entry.path else {
                    // Only a stale error or nothing at all to clear.
                    return Ok((entry.error.is_some()).then(Done::default));
                };
                let message = format!("Remove meeting: {}", entry.title);
                let pushed = self.repo.commit(&[], &[old.as_str()], &message)?;
                Ok(Some(Done {
                    pushed,
                    ..Done::default()
                }))
            }
            Desired::Note { title, text } => {
                let path = path.expect("set for a note");
                let hash = content_hash(&text);
                if entry.path.as_deref() == Some(path.as_str())
                    && entry.hash.as_deref() == Some(hash.as_str())
                {
                    return Ok(entry.error.is_some().then(|| Done::unchanged(&entry)));
                }
                let verb = if entry.path.is_some() {
                    "Update"
                } else {
                    "Add"
                };
                let message = format!("{verb} meeting: {title}");
                let removed: Vec<&str> = entry
                    .path
                    .iter()
                    .map(String::as_str)
                    .filter(|p| *p != path)
                    .collect();
                let pushed =
                    self.repo
                        .commit(&[(path.as_str(), text.as_str())], &removed, &message)?;
                Ok(Some(Done {
                    path: Some(path),
                    hash: Some(hash),
                    title: Some(title),
                    pushed,
                }))
            }
        }
    }

    /// What the vault should hold for the meeting. Call with the `speakers`
    /// lock held. A folder that is gone (deleted, or renamed by hand) is
    /// [`Desired::Absent`]; one that cannot be read is an error, so a
    /// broken file never takes a note down.
    fn desired(&self, id: &MeetingId, speakers: &Speakers) -> Result<Desired> {
        let Ok(dir) = MeetingDir::open(&self.config.meetings_dir, id) else {
            return Ok(Desired::Absent);
        };
        let meeting = Meeting::load(&dir.path())?;
        let requested = lock(&self.shared)
            .ledger
            .meetings
            .get(id)
            .and_then(|e| e.requested);
        if !requested.unwrap_or(meeting.sync) {
            return Ok(Desired::Absent);
        }
        if recall_store::state_of(&meeting, dir.audio().exists()) != "done" {
            return Ok(Desired::Keep);
        }
        Ok(Desired::Note {
            text: markdown::render(&meeting, speakers, &self.config.summary_lang),
            title: meeting.title,
        })
    }

    /// `<folder>/<note name>.md`, with ` (2)`, ` (3)`, ... appended if
    /// another meeting's note already has that path.
    fn note_path(&self, ledger: &Ledger, id: &MeetingId) -> String {
        let folder = self.config.vault.folder();
        let name = note_name(id);
        let taken = |path: &str| {
            ledger
                .meetings
                .iter()
                .any(|(other, e)| other != id && e.path.as_deref() == Some(path))
        };
        (1..)
            .map(|n| match n {
                1 => format!("{folder}/{name}.md"),
                n => format!("{folder}/{name} ({n}).md"),
            })
            .find(|p| !taken(p))
            .expect("the candidates are endless")
    }

    fn save(&self, shared: &Shared) {
        if let Err(err) = shared.ledger.save(&self.state_path) {
            tracing::error!(err = format!("{err:#}"), "saving the vault ledger");
        }
    }
}

/// A finished change: the ledger values to record.
#[derive(Debug, Default)]
struct Done {
    path: Option<String>,
    hash: Option<String>,
    title: Option<String>,
    /// A commit was pushed. False when the vault already held the change.
    pushed: bool,
}

impl Done {
    /// Records the entry as it is, which clears its error.
    fn unchanged(entry: &Entry) -> Self {
        Self {
            path: entry.path.clone(),
            hash: entry.hash.clone(),
            title: None,
            pushed: false,
        }
    }
}

/// `id` after the renames in `moved`.
fn resolve(moved: &BTreeMap<MeetingId, MeetingId>, id: &MeetingId) -> MeetingId {
    let mut id = id.clone();
    // A chain is at most as long as the map; the bound stops a cycle.
    for _ in 0..=moved.len() {
        match moved.get(&id) {
            Some(next) => id = next.clone(),
            None => break,
        }
    }
    id
}

/// [`RETRY_FIRST`] doubled for every failure after the first, at most
/// [`RETRY_MAX`].
fn backoff(failures: u32) -> Duration {
    let doublings = failures.saturating_sub(1).min(16);
    RETRY_FIRST.saturating_mul(1 << doublings).min(RETRY_MAX)
}

/// A hash of a note's text, to tell whether the vault already has it. The
/// std hasher may change between Rust releases; that costs one look at the
/// vault per meeting, which then finds nothing to commit.
fn content_hash(text: &str) -> String {
    let mut hasher = DefaultHasher::new();
    text.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

/// The note's file name, without `.md`, for a meeting: its id made valid on
/// every OS Obsidian runs on and safe in Obsidian links. `:` becomes `.`
/// (so `12:37` reads `12.37`); `* " \ / < > | ?`, which Windows or Android
/// refuse, `# ^ [ ]`, which break `[[links]]`, and control characters
/// become spaces; runs of spaces collapse; leading dots and trailing dots
/// and spaces go. `"2026-10-29 12:37 Meeting 12:37"` gives
/// `"2026-10-29 12.37 Meeting 12.37"`. The id changes with the title, and
/// the ledger keeps the old path so a rename moves the note.
pub fn note_name(id: &MeetingId) -> String {
    let replaced: String =
        id.0.chars()
            .map(|c| match c {
                ':' => '.',
                '*' | '"' | '\\' | '/' | '<' | '>' | '|' | '?' | '#' | '^' | '[' | ']' => ' ',
                c if c.is_control() => ' ',
                c => c,
            })
            .collect();
    let collapsed = replaced.split_whitespace().collect::<Vec<_>>().join(" ");
    let trimmed = collapsed
        .trim_start_matches('.')
        .trim_end_matches(['.', ' '])
        .trim_start();
    if trimmed.is_empty() {
        "Meeting".into()
    } else {
        trimmed.into()
    }
}

/// recall's clone of the vault, driven with the git CLI.
struct Repo {
    dir: PathBuf,
    remote: String,
    branch: String,
    folder: String,
    /// Extra environment for every git command; tests set an identity.
    env: Vec<(String, String)>,
    /// Runs between the commit and the first push; tests push a teammate's
    /// commit there.
    #[cfg(test)]
    before_push: Option<Box<dyn Fn() + Send>>,
}

impl Repo {
    fn new(config: &Config) -> Self {
        Self {
            dir: config.vault_dir(),
            remote: config.vault.remote.trim().to_owned(),
            branch: config.vault.branch.trim().to_owned(),
            folder: config.vault.folder().to_owned(),
            env: Vec::new(),
            #[cfg(test)]
            before_push: None,
        }
    }

    fn https(&self) -> bool {
        self.remote.starts_with("https://")
    }

    /// Makes one commit on top of the remote branch that writes `writes`
    /// and deletes `removes` (paths relative to the vault root, all under
    /// the folder), and pushes it. Returns false when the branch already
    /// matched and there was nothing to commit.
    ///
    /// The clone is recall's alone: it is reset to the fetched branch
    /// first, which drops anything a failed attempt left behind (its
    /// change is still unrecorded in the ledger, so it is made again).
    fn commit(&self, writes: &[(&str, &str)], removes: &[&str], message: &str) -> Result<bool> {
        for path in writes
            .iter()
            .map(|(p, _)| *p)
            .chain(removes.iter().copied())
        {
            if !self.inside_folder(path) {
                bail!("{path:?} is outside the vault folder {:?}", self.folder);
            }
        }
        self.ensure_clone()?;
        self.git(&["fetch", "--quiet", "origin", &self.branch])?;
        self.git(&[
            "checkout",
            "--quiet",
            "--force",
            "-B",
            &self.branch,
            "FETCH_HEAD",
        ])?;
        self.git(&["clean", "-d", "--force", "--quiet", "--", &self.folder])?;
        for (path, text) in writes {
            let file = self.dir.join(path);
            if let Some(parent) = file.parent() {
                fs::create_dir_all(parent)
                    .with_context(|| format!("creating {}", parent.display()))?;
            }
            fs::write(&file, text).with_context(|| format!("writing {}", file.display()))?;
        }
        for path in removes {
            let file = self.dir.join(path);
            match fs::remove_file(&file) {
                Ok(()) => {}
                // Someone took it out of the vault already.
                Err(e) if e.kind() == ErrorKind::NotFound => {}
                Err(e) => return Err(e).with_context(|| format!("removing {}", file.display())),
            }
        }
        self.git(&["add", "--all", "--", &self.folder])?;
        if self.run(&["diff", "--cached", "--quiet"])?.status.success() {
            return Ok(false);
        }
        self.git(&["commit", "--quiet", "--message", message])?;
        #[cfg(test)]
        if let Some(hook) = &self.before_push {
            hook();
        }
        let refspec = format!("HEAD:refs/heads/{}", self.branch);
        for attempt in 1..=PUSH_ATTEMPTS {
            let err = match self.git(&["push", "--quiet", "origin", &refspec]) {
                Ok(_) => return Ok(true),
                Err(err) => err,
            };
            if attempt == PUSH_ATTEMPTS {
                return Err(err.context(format!("pushing after {PUSH_ATTEMPTS} attempts")));
            }
            tracing::info!(
                err = format!("{err:#}"),
                "the vault push was rejected; rebasing"
            );
            if let Err(err) = self.git(&["pull", "--rebase", "--quiet", "origin", &self.branch]) {
                let _ = self.run(&["rebase", "--abort"]);
                return Err(err);
            }
        }
        unreachable!("the last attempt returns")
    }

    /// `path` is a plain relative path inside the folder.
    fn inside_folder(&self, path: &str) -> bool {
        path.strip_prefix(&self.folder)
            .and_then(|rest| rest.strip_prefix('/'))
            .is_some_and(|rest| {
                !rest.is_empty()
                    && rest
                        .split('/')
                        .all(|part| !part.is_empty() && part != "." && part != "..")
            })
    }

    /// Clones the vault if the directory has no clone yet, points `origin`
    /// at the configured remote, and for an https remote makes the gh CLI
    /// the clone's only credential helper.
    ///
    /// A clone recall did not make (no [`MARKER`]) is refused: every change
    /// resets the working tree, which would discard someone's own work in,
    /// say, the vault they open in Obsidian.
    fn ensure_clone(&self) -> Result<()> {
        let git_dir = self.dir.join(".git");
        if git_dir.exists() && !git_dir.join(MARKER).exists() {
            bail!(
                "{} is a git clone recall did not make; point vault.dir at an empty or missing folder",
                self.dir.display()
            );
        }
        if !git_dir.exists() {
            let empty = match fs::read_dir(&self.dir) {
                Ok(mut entries) => entries.next().is_none(),
                Err(e) if e.kind() == ErrorKind::NotFound => true,
                Err(e) => return Err(e).with_context(|| format!("reading {}", self.dir.display())),
            };
            if !empty {
                bail!(
                    "{} is not empty and not a git clone; point vault.dir elsewhere",
                    self.dir.display()
                );
            }
            let parent = self.dir.parent().unwrap_or(Path::new("."));
            fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
            let dir = self.dir.to_string_lossy();
            let helper = format!("credential.helper={GH_HELPER}");
            let mut args = Vec::new();
            if self.https() {
                // An empty value drops the helpers from the user's global
                // config for this command.
                args.extend(["-c", "credential.helper=", "-c", &helper]);
            }
            args.extend([
                "clone",
                "--quiet",
                "--branch",
                &self.branch,
                "--",
                &self.remote,
                &dir,
            ]);
            self.git_in(parent, &args)?;
            let marker = git_dir.join(MARKER);
            fs::write(&marker, "Made and reset by recall's vault sync.\n")
                .with_context(|| format!("writing {}", marker.display()))?;
        }
        let url = self.git(&["remote", "get-url", "origin"])?;
        if url.trim() != self.remote {
            self.git(&["remote", "set-url", "origin", &self.remote])?;
        }
        if self.https() {
            let helpers = self.run(&["config", "--local", "--get-all", "credential.helper"])?;
            let helpers = String::from_utf8_lossy(&helpers.stdout);
            if helpers.lines().collect::<Vec<_>>() != ["", GH_HELPER] {
                // Exit code 5 when there is nothing to unset.
                let _ = self.run(&["config", "--local", "--unset-all", "credential.helper"]);
                self.git(&["config", "--local", "--add", "credential.helper", ""])?;
                self.git(&["config", "--local", "--add", "credential.helper", GH_HELPER])?;
            }
        }
        Ok(())
    }

    /// Runs git in the clone; errors with its stderr unless it succeeds.
    fn git(&self, args: &[&str]) -> Result<String> {
        self.git_in(&self.dir, args)
    }

    fn git_in(&self, cwd: &Path, args: &[&str]) -> Result<String> {
        let output = self.run_in(cwd, args)?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            bail!("git {} failed: {}", args.join(" "), stderr.trim());
        }
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }

    fn run(&self, args: &[&str]) -> Result<Output> {
        self.run_in(&self.dir, args)
    }

    /// Runs git with no terminal to prompt on, killing it after
    /// [`GIT_TIMEOUT`].
    fn run_in(&self, cwd: &Path, args: &[&str]) -> Result<Output> {
        let mut command = Command::new("git");
        command
            .args(args)
            .current_dir(cwd)
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GH_PROMPT_DISABLED", "1")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for (key, value) in &self.env {
            command.env(key, value);
        }
        let child = command.spawn().context("running git")?;
        let pid = child.id();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(child.wait_with_output());
        });
        match rx.recv_timeout(GIT_TIMEOUT) {
            Ok(output) => output.context("running git"),
            Err(_) => {
                // SAFETY: plain kill(2) on the child's pid; the waiting
                // thread has not reaped it, as it has not returned.
                unsafe { libc::kill(pid as libc::pid_t, libc::SIGKILL) };
                bail!(
                    "git {} took longer than {} s",
                    args.join(" "),
                    GIT_TIMEOUT.as_secs()
                )
            }
        }
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use chrono::{Local, TimeZone};
    use recall_store::{Step, StepState};
    use serde_json::json;

    use super::*;

    /// A vault in a local bare repository, seeded with files recall must
    /// not touch, and a sync whose git runs with a fixed identity and
    /// without the user's git config.
    struct Fixture {
        tmp: tempfile::TempDir,
        config: Arc<Config>,
        bare: PathBuf,
        vault: Arc<VaultSync>,
        syncer: Syncer,
        index: Arc<Mutex<Index>>,
    }

    fn git_env(tmp: &Path) -> Vec<(String, String)> {
        let global = tmp.join("gitconfig");
        fs::write(&global, "").unwrap();
        [
            ("GIT_CONFIG_GLOBAL", global.to_str().unwrap()),
            ("GIT_CONFIG_NOSYSTEM", "1"),
            ("GIT_AUTHOR_NAME", "Recall Test"),
            ("GIT_AUTHOR_EMAIL", "recall@example.com"),
            ("GIT_COMMITTER_NAME", "Recall Test"),
            ("GIT_COMMITTER_EMAIL", "recall@example.com"),
        ]
        .map(|(k, v)| (k.to_owned(), v.to_owned()))
        .to_vec()
    }

    fn git(env: &[(String, String)], cwd: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .args(args)
            .current_dir(cwd)
            .envs(env.iter().map(|(k, v)| (k, v)))
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap()
    }

    fn fixture() -> Fixture {
        let tmp = tempfile::tempdir().unwrap();
        let env = git_env(tmp.path());
        let bare = tmp.path().join("remote.git");
        git(
            &env,
            tmp.path(),
            &["init", "--quiet", "--bare", "-b", "main", "remote.git"],
        );
        let seed = tmp.path().join("seed");
        git(
            &env,
            tmp.path(),
            &["clone", "--quiet", "remote.git", "seed"],
        );
        fs::write(seed.join("README.md"), "Team vault\n").unwrap();
        fs::create_dir(seed.join(".obsidian")).unwrap();
        fs::write(seed.join(".obsidian/app.json"), "{}\n").unwrap();
        git(&env, &seed, &["add", "--all"]);
        git(&env, &seed, &["commit", "--quiet", "-m", "Start the vault"]);
        git(&env, &seed, &["push", "--quiet", "origin", "HEAD:main"]);

        let mut config = Config {
            meetings_dir: tmp.path().join("Meetings"),
            data_dir: tmp.path().join("data"),
            summary_lang: "en".into(),
            ..Config::default()
        };
        config.vault.remote = bare.to_str().unwrap().to_owned();
        let config = Arc::new(config);
        fs::create_dir_all(&config.data_dir).unwrap();
        let index = Arc::new(Mutex::new(Index::open(&config.index_path()).unwrap()));
        let speakers = Arc::new(Mutex::new(Speakers::default()));
        let (vault, mut syncer) = VaultSync::new(config.clone(), speakers, index.clone()).unwrap();
        syncer.repo.env = env;
        Fixture {
            tmp,
            config,
            bare,
            vault,
            syncer,
            index,
        }
    }

    impl Fixture {
        fn env(&self) -> Vec<(String, String)> {
            self.syncer.repo.env.clone()
        }

        /// Creates a processed meeting at 12:37 with one transcript line.
        fn meeting(&self, title: &str, sync: bool) -> MeetingId {
            let started = Local.with_ymd_and_hms(2026, 10, 29, 12, 37, 0).unwrap();
            let dir = MeetingDir::create(&self.config.meetings_dir, started, title).unwrap();
            let done = json!({ "done": { "at": "2026-10-29T05:00:00Z" } });
            let meeting: Meeting = serde_json::from_value(json!({
                "id": dir.id,
                "title": title,
                "started": started.with_timezone(&Utc),
                "duration_ms": 60_000,
                "source_app": "chrome",
                "steps": {
                    "segment": done, "transcribe": done, "diarize": done,
                    "name": done, "summarize": done, "write": done,
                },
                "segments": [{
                    "start_ms": 0, "end_ms": 1_000, "side": "them",
                    "speaker": null, "text": "We ship on Friday.",
                }],
                "clusters": [],
                "summary": null,
                "models": {},
                "sync": sync,
            }))
            .unwrap();
            meeting.save(&dir.path()).unwrap();
            fs::write(dir.audio(), b"opus").unwrap();
            dir.id
        }

        fn edit(&self, id: &MeetingId, f: impl FnOnce(&mut Meeting)) {
            let path = self.config.meetings_dir.join(&id.0);
            let mut meeting = Meeting::load(&path).unwrap();
            f(&mut meeting);
            meeting.save(&path).unwrap();
        }

        /// Touches the meeting and runs everything due within the hour.
        fn sync(&mut self, id: &MeetingId) {
            self.vault.touch(id, SyncWhen::Now);
            self.run();
        }

        fn run(&mut self) {
            self.syncer
                .run_due(Instant::now() + Duration::from_secs(3600));
        }

        fn remote(&self, args: &[&str]) -> String {
            let mut all = vec!["--git-dir", self.bare.to_str().unwrap()];
            all.extend(args);
            git(&self.env(), self.tmp.path(), &all)
        }

        fn files(&self) -> Vec<String> {
            let out = self.remote(&["ls-tree", "-r", "--name-only", "main"]);
            out.lines().map(str::to_owned).collect()
        }

        fn subjects(&self) -> Vec<String> {
            let out = self.remote(&["log", "--format=%s", "main"]);
            out.lines().map(str::to_owned).collect()
        }

        fn note(&self, path: &str) -> String {
            self.remote(&["show", &format!("main:{path}")])
        }
    }

    const NOTE: &str = "meetings/2026-10-29 12.37 Weekly sync.md";
    const SEED: [&str; 2] = [".obsidian/app.json", "README.md"];

    #[test]
    fn note_names_are_valid_file_names_everywhere() {
        let name = |s: &str| note_name(&MeetingId(s.into()));
        assert_eq!(
            name("2026-10-29 12:37 Meeting 12:37"),
            "2026-10-29 12.37 Meeting 12.37"
        );
        assert_eq!(
            name("2026-10-29 12:37 Q3? <plan> | \"A*B\" \\ [[x]] #1 ^2"),
            "2026-10-29 12.37 Q3 plan A B x 1 2"
        );
        assert_eq!(
            name("2026-10-29 12:37 Wrap up..."),
            "2026-10-29 12.37 Wrap up"
        );
        assert_eq!(name("2026-10-29 12:37 週會"), "2026-10-29 12.37 週會");
        assert_eq!(name("..."), "Meeting");
    }

    #[test]
    fn backoff_doubles_up_to_the_cap() {
        assert_eq!(backoff(1), RETRY_FIRST);
        assert_eq!(backoff(2), RETRY_FIRST * 2);
        assert_eq!(backoff(3), RETRY_FIRST * 4);
        assert_eq!(backoff(40), RETRY_MAX);
    }

    #[test]
    fn first_publish_creates_the_folder_and_note_and_nothing_else() {
        let mut f = fixture();
        let id = f.meeting("Weekly sync", true);
        f.sync(&id);
        assert_eq!(f.files(), [SEED[0], SEED[1], NOTE]);
        assert_eq!(
            f.subjects(),
            ["Add meeting: Weekly sync", "Start the vault"]
        );
        let note = f.note(NOTE);
        assert!(note.starts_with("---\ntitle: Weekly sync\n"), "{note}");
        assert!(note.contains("We ship on Friday."));
        let status = f.vault.status(&id);
        assert_eq!(status.path.as_deref(), Some(NOTE));
        assert!(status.synced_at.is_some());
        assert_eq!((status.error, status.queued), (None, false));

        // Nothing changed since: no commit.
        f.sync(&id);
        assert_eq!(f.subjects().len(), 2);
    }

    #[test]
    fn meetings_switched_off_or_unprocessed_are_not_published() {
        let mut f = fixture();
        let private = f.meeting("Private call", false);
        f.sync(&private);
        let id = f.meeting("Weekly sync", true);
        f.edit(&id, |m| {
            m.steps.insert(Step::Summarize, StepState::Running);
        });
        f.sync(&id);
        assert_eq!(f.files(), SEED);
        assert_eq!(f.vault.status(&private), VaultStatus::default());
        assert_eq!(f.vault.status(&id), VaultStatus::default());
    }

    #[test]
    fn an_edit_updates_the_note_after_the_debounce() {
        let mut f = fixture();
        let id = f.meeting("Weekly sync", true);
        f.sync(&id);
        f.edit(&id, |m| m.segments[0].text = "We ship on Monday.".into());
        f.vault.touch(&id, SyncWhen::Debounced);
        f.syncer.run_due(Instant::now());
        assert!(f.vault.status(&id).queued);
        assert_eq!(f.subjects().len(), 2);
        f.run();
        assert_eq!(f.subjects()[0], "Update meeting: Weekly sync");
        assert!(f.note(NOTE).contains("We ship on Monday."));
        assert!(!f.vault.status(&id).queued);
    }

    #[test]
    fn a_rename_moves_the_note() {
        let mut f = fixture();
        let id = f.meeting("Weekly sync", true);
        f.sync(&id);
        let dir = MeetingDir::open(&f.config.meetings_dir, &id).unwrap();
        let renamed = dir.rename("Release: go/no-go").unwrap();
        f.edit(&renamed.id, |m| {
            m.id = renamed.id.clone();
            m.title = "Release: go/no-go".into();
        });
        f.vault.renamed(&id, &renamed.id);
        f.sync(&renamed.id);
        let moved = "meetings/2026-10-29 12.37 Release. go no-go.md";
        assert_eq!(f.files(), [SEED[0], SEED[1], moved]);
        assert_eq!(f.subjects()[0], "Update meeting: Release: go/no-go");
        assert_eq!(f.vault.status(&id), VaultStatus::default());
        assert_eq!(f.vault.status(&renamed.id).path.as_deref(), Some(moved));
    }

    #[test]
    fn switching_off_or_deleting_removes_the_note() {
        let mut f = fixture();
        let id = f.meeting("Weekly sync", true);
        f.sync(&id);
        f.edit(&id, |m| m.sync = false);
        f.sync(&id);
        assert_eq!(f.files(), SEED);
        assert_eq!(f.subjects()[0], "Remove meeting: Weekly sync");
        assert_eq!(f.vault.status(&id), VaultStatus::default());

        f.edit(&id, |m| m.sync = true);
        f.sync(&id);
        assert_eq!(f.files().len(), 3);
        fs::remove_dir_all(f.config.meetings_dir.join(&id.0)).unwrap();
        f.sync(&id);
        assert_eq!(f.files(), SEED);
        assert_eq!(
            f.subjects(),
            [
                "Remove meeting: Weekly sync",
                "Add meeting: Weekly sync",
                "Remove meeting: Weekly sync",
                "Add meeting: Weekly sync",
                "Start the vault"
            ]
        );
    }

    #[test]
    fn a_concurrent_upstream_commit_is_rebased_onto() {
        let mut f = fixture();
        let env = f.env();
        let teammate = f.tmp.path().join("teammate");
        git(
            &env,
            f.tmp.path(),
            &["clone", "--quiet", "remote.git", "teammate"],
        );
        f.syncer.repo.before_push = Some(Box::new(move || {
            fs::write(teammate.join("Ideas.md"), "Ideas\n").unwrap();
            git(&env, &teammate, &["add", "Ideas.md"]);
            git(&env, &teammate, &["commit", "--quiet", "-m", "Add ideas"]);
            git(&env, &teammate, &["push", "--quiet", "origin", "HEAD:main"]);
        }));
        let id = f.meeting("Weekly sync", true);
        f.sync(&id);
        assert_eq!(
            f.subjects(),
            ["Add meeting: Weekly sync", "Add ideas", "Start the vault"]
        );
        assert_eq!(f.files(), [SEED[0], "Ideas.md", SEED[1], NOTE]);
        assert_eq!(f.vault.status(&id).error, None);
    }

    #[test]
    fn a_failed_push_is_kept_and_retried_later() {
        let mut f = fixture();
        let hook = f.bare.join("hooks/pre-receive");
        fs::write(&hook, "#!/bin/sh\necho 'vault is read-only' >&2\nexit 1\n").unwrap();
        let mut perms = fs::metadata(&hook).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&hook, perms).unwrap();

        let id = f.meeting("Weekly sync", true);
        f.vault.touch(&id, SyncWhen::Now);
        f.syncer.run_due(Instant::now());
        let status = f.vault.status(&id);
        assert!(status.error.unwrap().contains("vault is read-only"));
        assert_eq!(
            (status.path, status.synced_at, status.queued),
            (None, None, false)
        );
        assert_eq!(f.subjects(), ["Start the vault"]);
        // A retry is scheduled for later, not run at once.
        let (at, retry) = f.syncer.due[&id];
        assert!(retry && at > Instant::now());
        // The failure is on disk, so it outlives a restart.
        let ledger = Ledger::load(&f.config.vault_state_path()).unwrap();
        assert_eq!(ledger.meetings[&id].failures, 1);

        fs::remove_file(&hook).unwrap();
        f.run();
        assert_eq!(f.subjects()[0], "Add meeting: Weekly sync");
        let status = f.vault.status(&id);
        assert_eq!(status.error, None);
        assert_eq!(status.path.as_deref(), Some(NOTE));
    }

    #[test]
    fn a_switch_flipped_during_processing_is_written_when_the_job_ends() {
        let mut f = fixture();
        let id = f.meeting("Weekly sync", false);
        let job = f
            .index
            .lock()
            .unwrap()
            .enqueue(&id, Step::Summarize)
            .unwrap();
        f.vault.request(&id, Some(true));
        assert_eq!(f.vault.status(&id).requested, Some(true));
        // Still queued: meeting.json is the job's to write.
        f.syncer.handle(Msg::JobEnded(id.clone()));
        assert_eq!(f.vault.status(&id).requested, Some(true));
        f.index.lock().unwrap().finish_job(job, true, None).unwrap();
        f.syncer.handle(Msg::JobEnded(id.clone()));
        f.run();
        let path = f.config.meetings_dir.join(&id.0);
        assert!(Meeting::load(&path).unwrap().sync);
        let status = f.vault.status(&id);
        assert_eq!(status.requested, None);
        assert_eq!(status.path.as_deref(), Some(NOTE));
    }

    #[test]
    fn the_sweep_finds_synced_meetings_nobody_touched() {
        let mut f = fixture();
        let id = f.meeting("Weekly sync", true);
        f.meeting("Private call", false);
        f.syncer.sweep();
        f.run();
        assert_eq!(f.files(), [SEED[0], SEED[1], NOTE]);
        assert_eq!(f.vault.status(&id).path.as_deref(), Some(NOTE));
    }

    #[test]
    fn an_https_remote_gets_gh_as_its_only_credential_helper() {
        let f = fixture();
        let dir = f.config.vault_dir();
        git(
            &f.env(),
            f.tmp.path(),
            &["clone", "--quiet", "remote.git", dir.to_str().unwrap()],
        );
        let repo = Repo {
            remote: "https://example.invalid/vault.git".into(),
            env: f.env(),
            ..Repo::new(&f.config)
        };
        // A clone recall did not make is someone's own; it is left alone.
        let err = repo.ensure_clone().unwrap_err();
        assert!(
            format!("{err:#}").contains("recall did not make"),
            "{err:#}"
        );
        fs::write(dir.join(".git").join(MARKER), "").unwrap();
        repo.ensure_clone().unwrap();
        let helpers = git(
            &f.env(),
            &dir,
            &["config", "--local", "--get-all", "credential.helper"],
        );
        assert_eq!(helpers, format!("\n{GH_HELPER}\n"));
        let url = git(&f.env(), &dir, &["remote", "get-url", "origin"]);
        assert_eq!(url.trim(), "https://example.invalid/vault.git");
    }
}
