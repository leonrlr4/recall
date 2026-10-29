//! The processing worker: takes jobs from the index queue and runs the
//! pipeline on them, one at a time, on its own thread.
//!
//! The pipeline is CPU-bound and blocks for minutes, so it runs on a plain
//! thread with a small current-thread tokio runtime of its own (the LLM
//! client is async), never on the daemon's runtime. Neither the shared
//! `Index` nor the shared `Speakers` mutex is held while a step runs: the
//! HTTP server takes both. The run works on a clone of `Speakers`; when it
//! ends, successful or not, the people it added and the voices it enrolled
//! are carried into the shared registry, which is then saved over what the
//! run's Name step wrote.
//! Edits the server made to the shared registry meanwhile are kept.

use std::any::Any;
use std::future::Future;
use std::panic::{self, AssertUnwindSafe};
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use chrono::Utc;
use recall_pipeline::runner::{Engines, MAX_VOICES, run_from};
use recall_store::{
    Config, Event, Index, Job, Meeting, MeetingDir, MeetingId, Speakers, Step, StepState,
};
use tokio::sync::broadcast;

use crate::notify;

/// How long the worker sleeps without a wake before looking at the queue
/// again. Every enqueue in the daemon sends a wake, so this only matters for
/// jobs queued by another process, such as `recall rerun`.
const IDLE_POLL: Duration = Duration::from_secs(30);
/// Error recorded on a step a stopped daemon left running.
pub const INTERRUPTED: &str = "interrupted";

/// Loads the engines; called once, before the first job. Failing leaves the
/// job failed and the next job tries again.
pub type LoadEngines =
    Box<dyn FnMut(&Config) -> Pin<Box<dyn Future<Output = Result<Engines>>>> + Send>;

/// The real engines, downloading missing models with a log line every 10 %.
pub fn load_real_engines() -> LoadEngines {
    Box::new(|config: &Config| {
        let config = config.clone();
        Box::pin(async move {
            let logged = AtomicU64::new(u64::MAX);
            Engines::load(&config, |file, done, total| {
                let tenth = (done * 10).checked_div(total).unwrap_or(0);
                if logged.swap(tenth, Ordering::Relaxed) != tenth {
                    tracing::info!(file, done, total, "downloading a model");
                }
            })
            .await
        })
    })
}

pub struct Worker {
    pub config: Arc<Config>,
    pub index: Arc<Mutex<Index>>,
    pub speakers: Arc<Mutex<Speakers>>,
    pub events: broadcast::Sender<Event>,
    pub load: LoadEngines,
    /// Desktop notifications for finished and failed jobs; off in tests.
    pub notifications: bool,
}

impl Worker {
    /// Runs the worker on a new thread until every sender of `wake` is
    /// dropped. Send `()` on `wake` after queueing a job.
    pub fn spawn(self, wake: Receiver<()>) -> Result<JoinHandle<()>> {
        std::thread::Builder::new()
            .name("recall-worker".into())
            .spawn(move || self.run(&wake))
            .context("spawning the worker thread")
    }

    fn run(mut self, wake: &Receiver<()>) {
        lower_priority();
        let runtime = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(runtime) => runtime,
            Err(err) => {
                tracing::error!(%err, "building the worker runtime; no jobs will run");
                return;
            }
        };
        let mut engines = None;
        loop {
            match self.run_next(&runtime, &mut engines) {
                Ok(true) => continue,
                // The engines hold about 2 GB; load them again for the next
                // meeting rather than keep them between meetings.
                Ok(false) => engines = None,
                Err(err) => tracing::error!(err = format!("{err:#}"), "the job queue failed"),
            }
            match wake.recv_timeout(IDLE_POLL) {
                Ok(()) | Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return,
            }
        }
    }

    /// Takes the next job and runs it. Returns false when the queue is
    /// empty. Errors only when the index itself fails; a failed job is
    /// recorded and is not an error here.
    pub fn run_next(
        &mut self,
        runtime: &tokio::runtime::Runtime,
        engines: &mut Option<Engines>,
    ) -> Result<bool> {
        let Some(job) = lock(&self.index).next_job()? else {
            return Ok(false);
        };
        tracing::info!(id = job.meeting.0, from = ?job.from_step, job = job.id, "processing");
        let outcome = self.process(runtime, engines, &job);
        let title = outcome.title.clone();
        match outcome.result {
            Ok(()) => {
                lock(&self.index).finish_job(job.id, true, None)?;
                tracing::info!(id = job.meeting.0, "processed");
                let _ = self.events.send(Event::Done {
                    meeting: job.meeting.clone(),
                });
                if self.notifications {
                    notify::summary_ready(&job.meeting.0, &title, self.config.port);
                }
            }
            Err((step, err)) => {
                // The failed step is in meeting.json, but only the Write
                // step updates the index row.
                self.refresh_row(&job.meeting);
                let error = format!("{err:#}");
                lock(&self.index).finish_job(job.id, false, Some(&error))?;
                tracing::error!(id = job.meeting.0, ?step, err = error, "processing failed");
                let _ = self.events.send(Event::Failed {
                    meeting: job.meeting.clone(),
                    step,
                    error,
                });
                if self.notifications {
                    notify::processing_failed(
                        &job.meeting.0,
                        &title,
                        &step_name(step),
                        self.config.port,
                    );
                }
            }
        }
        Ok(true)
    }

    fn process(
        &mut self,
        runtime: &tokio::runtime::Runtime,
        engines: &mut Option<Engines>,
        job: &Job,
    ) -> Outcome {
        let mut outcome = Outcome {
            title: job.meeting.0.clone(),
            result: Ok(()),
        };
        let dir = match MeetingDir::open(&self.config.meetings_dir, &job.meeting) {
            Ok(dir) => dir,
            Err(err) => {
                outcome.result = Err((job.from_step, err));
                return outcome;
            }
        };
        if let Ok(meeting) = Meeting::load(&dir.path()) {
            outcome.title = meeting.title;
        }
        if engines.is_none() {
            tracing::info!("loading the models");
            // A panic in a model loader (a native library rejecting a
            // file) would otherwise end the worker thread with no job
            // failed and nothing processed after it.
            let loaded = panic::catch_unwind(AssertUnwindSafe(|| {
                runtime.block_on((self.load)(&self.config))
            }))
            .unwrap_or_else(|payload| {
                Err(anyhow!("loading panicked: {}", panic_message(&*payload)))
            });
            match loaded {
                Ok(loaded) => *engines = Some(loaded),
                Err(err) => {
                    let err = err.context("loading the models");
                    let step = self.record_failure(job, Some(job.from_step), &format!("{err:#}"));
                    outcome.result = Err((step, err));
                    return outcome;
                }
            }
        }
        let loaded = engines.as_mut().expect("loaded above");
        let mut speakers = lock(&self.speakers).clone();
        let events = &self.events;
        let meeting = &job.meeting;
        // Only the run is guarded, so that the speakers merge and the
        // failure record below happen after a panic too: a Name step that
        // finished has already saved its voices to speakers.json, and the
        // next save of the shared copy would drop them.
        let run = panic::catch_unwind(AssertUnwindSafe(|| {
            runtime.block_on(run_from(
                loaded,
                &self.config,
                &dir,
                &mut speakers,
                job.from_step,
                |step, pct| {
                    tracing::info!(id = meeting.0, ?step, pct, "step done");
                    let _ = events.send(Event::Progress {
                        meeting: meeting.clone(),
                        step,
                        pct,
                    });
                },
            ))
        }));
        let result = run.unwrap_or_else(|payload| {
            // The engines may be half way through a call; load them afresh.
            *engines = None;
            Err(anyhow!("pipeline panicked: {}", panic_message(&*payload)))
        });
        let merged = self.merge_speakers(&speakers);
        outcome.result = match result {
            Ok(()) => merged.map_err(|err| (Step::Name, err)),
            Err(err) => {
                if let Err(merge) = merged {
                    tracing::error!(err = format!("{merge:#}"), "saving the speakers");
                }
                // A panic leaves its step running in meeting.json.
                let step = match failed_step(&dir) {
                    Some(step) => step,
                    None => self.record_failure(job, None, &format!("{err:#}")),
                };
                Err((step, err))
            }
        };
        outcome
    }

    /// Inserts every person of the run's registry that the shared one lacks
    /// (the voices the run's Name step registered) into the shared registry,
    /// adds to people it has any embedding the run gave them that it lacks
    /// (a voice enrolled through Meet), keeping at most [`MAX_VOICES`], and
    /// saves it. Otherwise the shared copy wins, so a rename made while the
    /// run went on survives, on disk too: the Name step saved the run's
    /// older copy.
    fn merge_speakers(&self, run: &Speakers) -> Result<()> {
        let mut shared = lock(&self.speakers);
        let mut next = shared.clone();
        for person in run.people() {
            if next.insert_person(person.clone()) {
                continue;
            }
            for embedding in &person.embeddings {
                let known = next
                    .get(&person.id)
                    .is_some_and(|p| p.embeddings.contains(embedding));
                if !known {
                    next.add_embedding(&person.id, embedding.clone(), MAX_VOICES);
                }
            }
        }
        next.save(&self.config.speakers_path())?;
        *shared = next;
        Ok(())
    }

    /// Rewrites the meeting's index row from its `meeting.json`. Failing is
    /// only logged.
    fn refresh_row(&self, id: &MeetingId) {
        let result = (|| -> Result<()> {
            let dir = MeetingDir::open(&self.config.meetings_dir, id)?;
            let meeting = Meeting::load(&dir.path())?;
            let speakers = lock(&self.speakers);
            lock(&self.index).upsert(&meeting, &speakers, dir.audio().exists())
        })();
        if let Err(err) = result {
            tracing::error!(
                err = format!("{err:#}"),
                id = id.0,
                "updating the index row"
            );
        }
    }

    /// Marks `step`, or else the first step left running, or else the job's
    /// first step, failed with `error` in `meeting.json`, so the meeting does
    /// not show as still processing ([`Worker::run_next`] then refreshes the
    /// index row). Returns the step marked. Failing to record is only
    /// logged.
    fn record_failure(&self, job: &Job, step: Option<Step>, error: &str) -> Step {
        let mut marked = step.unwrap_or(job.from_step);
        let result = (|| -> Result<()> {
            let dir = MeetingDir::open(&self.config.meetings_dir, &job.meeting)?;
            let mut meeting = Meeting::load(&dir.path())?;
            if step.is_none()
                && let Some(running) = Step::ALL
                    .into_iter()
                    .find(|s| meeting.steps.get(s) == Some(&StepState::Running))
            {
                marked = running;
            }
            meeting.steps.insert(
                marked,
                StepState::Failed {
                    at: Utc::now(),
                    error: error.to_owned(),
                },
            );
            meeting.save(&dir.path())
        })();
        if let Err(err) = result {
            tracing::error!(
                err = format!("{err:#}"),
                id = job.meeting.0,
                "recording a failed step"
            );
        }
        marked
    }
}

/// The message of a panic payload, as `panic!` formats it.
fn panic_message(payload: &(dyn Any + Send)) -> String {
    payload
        .downcast_ref::<&str>()
        .map(|s| (*s).to_owned())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "unknown panic".into())
}

struct Outcome {
    title: String,
    result: std::result::Result<(), (Step, anyhow::Error)>,
}

/// The first failed step recorded in the meeting's `meeting.json`.
fn failed_step(dir: &MeetingDir) -> Option<Step> {
    let meeting = Meeting::load(&dir.path()).ok()?;
    Step::ALL
        .into_iter()
        .find(|s| matches!(meeting.steps.get(s), Some(StepState::Failed { .. })))
}

/// Marks every step left running in a meeting under `config.meetings_dir`
/// failed with [`INTERRUPTED`], saves it and refreshes its index row. A
/// running step means the daemon stopped mid-run; its job is queued again
/// by [`Index::reset_running_jobs`]. Returns the meetings changed. A
/// meeting that cannot be read or saved is logged and skipped.
pub fn sweep_interrupted(
    config: &Config,
    index: &Index,
    speakers: &Speakers,
) -> Result<Vec<MeetingId>> {
    let mut swept = Vec::new();
    for id in MeetingDir::list(&config.meetings_dir)? {
        let Ok(dir) = MeetingDir::open(&config.meetings_dir, &id) else {
            continue;
        };
        let result = (|| -> Result<bool> {
            let mut meeting = Meeting::load(&dir.path())?;
            let mut changed = false;
            for state in meeting.steps.values_mut() {
                if *state == StepState::Running {
                    *state = StepState::Failed {
                        at: Utc::now(),
                        error: INTERRUPTED.into(),
                    };
                    changed = true;
                }
            }
            if changed {
                meeting.save(&dir.path())?;
                index.upsert(&meeting, speakers, dir.audio().exists())?;
            }
            Ok(changed)
        })();
        match result {
            Ok(true) => swept.push(id),
            Ok(false) => {}
            Err(err) => tracing::warn!(
                err = format!("{err:#}"),
                id = id.0,
                "checking for an interrupted run"
            ),
        }
    }
    Ok(swept)
}

/// The step's snake_case name, as the API and the CLI spell it.
pub fn step_name(step: Step) -> String {
    serde_json::to_value(step)
        .ok()
        .and_then(|v| v.as_str().map(str::to_owned))
        .unwrap_or_else(|| format!("{step:?}"))
}

/// Percent-encodes everything but RFC 3986 unreserved characters, so a
/// meeting id (spaces, `:`, CJK) fits in one URL path segment.
pub fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
            out.push(char::from(b));
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Puts the calling thread, and the transcription threads it starts, under
/// `SCHED_IDLE`: processing a meeting then uses only CPU time nothing else
/// wants, so the desktop stays responsive while all cores transcribe.
/// Recording runs on other threads and keeps its priority.
fn lower_priority() {
    let param = libc::sched_param { sched_priority: 0 };
    // SAFETY: `param` is a valid sched_param, and pid 0 is the calling thread.
    if unsafe { libc::sched_setscheduler(0, libc::SCHED_IDLE, &param) } != 0 {
        tracing::warn!(
            err = %std::io::Error::last_os_error(),
            "lowering the worker's priority"
        );
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percent_encode_keeps_only_unreserved() {
        assert_eq!(
            percent_encode("2026-10-27 09:00 週會"),
            "2026-10-27%2009%3A00%20%E9%80%B1%E6%9C%83"
        );
        assert_eq!(percent_encode("a/b?c"), "a%2Fb%3Fc");
    }

    #[test]
    fn step_names_are_snake_case() {
        assert_eq!(step_name(Step::Summarize), "summarize");
    }
}
