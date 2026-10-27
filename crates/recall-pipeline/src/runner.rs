//! Runs the processing steps on one meeting, recording each step's state in
//! `meeting.json` as it goes.

use std::io::Write as _;
use std::sync::Arc;

use anyhow::{Context, Result};
use chrono::Utc;
use recall_llm::Llm;
use recall_store::speakers::ME;
use recall_store::{
    Config, Index, Meeting, MeetingDir, Side, SpeakerId, Speakers, Step, StepState,
};
use tempfile::NamedTempFile;

use crate::decode::decode_stereo;
use crate::diarize::{Diarizer, SherpaDiarizer};
use crate::merge::merge_sides;
use crate::models::{self, ModelId};
use crate::naming::{apply_names, assign_clusters, name_clusters};
use crate::summarize::summarize;
use crate::transcribe::{Transcriber, WhisperTranscriber};
use crate::vad::{Region, SileroVad, Vad, drop_echo, merge_close};

/// Whisper language code for transcription.
const LANG: &str = "zh";
/// Playback latency allowed for when dropping the remote side's echo from
/// the microphone; see [`drop_echo`].
const ECHO_TOLERANCE_MS: u32 = 150;
/// Speech regions closer than this are joined; see [`merge_close`].
const MERGE_GAP_MS: u32 = 300;
/// Longest region handed to the transcriber.
const MAX_REGION_MS: u32 = 30_000;
/// Cosine similarity a cluster needs to be named after a known person.
const NAME_THRESHOLD: f32 = 0.6;
/// Longest transcript, in characters, summarized in one model call.
const SUMMARY_CHUNK_CHARS: usize = 60_000;

/// The models one pipeline run uses.
pub struct Engines {
    pub vad: Box<dyn Vad>,
    pub transcriber: Box<dyn Transcriber>,
    pub diarizer: Box<dyn Diarizer>,
    pub llm: Arc<dyn Llm>,
}

impl Engines {
    /// Downloads any missing model into `config.models_dir()` and loads the
    /// engines. An LLM backend that cannot be built does not fail the load;
    /// see [`recall_llm::from_config_or_unavailable`]. `progress` receives `(model file, downloaded, total)` bytes
    /// during downloads.
    pub async fn load(config: &Config, progress: impl Fn(&str, u64, u64)) -> Result<Self> {
        let dir = config.models_dir();
        for id in [
            ModelId::SileroVad,
            ModelId::BreezeGgml,
            ModelId::PyannoteSeg,
            ModelId::CamPlusPlus,
        ] {
            let name = models::spec(id).file;
            models::ensure(&dir, id, |done, total| progress(name, done, total)).await?;
        }
        let threads = config.stt.threads;
        Ok(Self {
            vad: Box::new(SileroVad::load(&dir, threads)?),
            transcriber: Box::new(WhisperTranscriber::load(
                &models::path(&dir, ModelId::BreezeGgml),
                threads,
            )?),
            diarizer: Box::new(SherpaDiarizer::load(&dir, threads)?),
            llm: recall_llm::from_config_or_unavailable(&config.llm),
        })
    }
}

/// Runs every step from `from` through [`Step::Write`] on the meeting in
/// `dir`, in [`Step::ALL`] order.
///
/// Those steps are first reset to pending, so a rerun never leaves an
/// earlier run's later results marked done. Each step is marked running and
/// saved before it starts, then done (and `on_progress` gets the step and the
/// fraction of all steps now finished) or failed with its error, in which
/// case the run stops and returns that error. Steps before `from` are taken
/// as done and only their saved results are used. `speakers` gains any new
/// remote voice and is saved to `config.speakers_path()`.
///
/// The work is CPU-bound and runs on the calling thread.
pub async fn run_from(
    engines: &mut Engines,
    config: &Config,
    dir: &MeetingDir,
    speakers: &mut Speakers,
    from: Step,
    on_progress: impl Fn(Step, f32),
) -> Result<()> {
    let path = dir.path();
    let mut meeting = Meeting::load(&path)?;
    for step in Step::ALL.into_iter().filter(|s| *s >= from) {
        meeting.steps.insert(step, StepState::Pending);
    }
    meeting.save(&path)?;

    let mut audio = None;
    for (i, step) in Step::ALL
        .into_iter()
        .enumerate()
        .filter(|(_, s)| *s >= from)
    {
        meeting.steps.insert(step, StepState::Running);
        meeting.save(&path)?;
        let mut run = Run {
            engines: &mut *engines,
            config,
            dir,
            speakers: &mut *speakers,
            meeting: &mut meeting,
            audio: &mut audio,
        };
        if let Err(e) = run.step(step).await {
            let e = e.context(format!("{step:?} step"));
            meeting.steps.insert(
                step,
                StepState::Failed {
                    at: Utc::now(),
                    error: format!("{e:#}"),
                },
            );
            return match meeting.save(&path) {
                Ok(()) => Err(e),
                Err(save) => Err(e.context(format!("recording the failure also failed: {save:#}"))),
            };
        }
        // `Write` marks itself done before indexing, so the index shows the
        // finished state.
        if meeting.steps.get(&step) == Some(&StepState::Running) {
            meeting
                .steps
                .insert(step, StepState::Done { at: Utc::now() });
        }
        meeting.save(&path)?;
        on_progress(step, (i + 1) as f32 / Step::ALL.len() as f32);
    }
    Ok(())
}

/// Decoded audio and speech regions of one recording. Not persisted: a run
/// that starts after `Segment` derives it again when a step needs it.
struct Audio {
    me: Vec<f32>,
    them: Vec<f32>,
    me_regions: Vec<Region>,
    them_regions: Vec<Region>,
}

/// One run's state, shared by its steps.
struct Run<'a> {
    engines: &'a mut Engines,
    config: &'a Config,
    dir: &'a MeetingDir,
    speakers: &'a mut Speakers,
    meeting: &'a mut Meeting,
    audio: &'a mut Option<Audio>,
}

impl Run<'_> {
    async fn step(&mut self, step: Step) -> Result<()> {
        match step {
            Step::Segment => self.audio().map(|_| ()),
            Step::Transcribe => self.transcribe(),
            Step::Diarize => self.diarize(),
            Step::Name => self.name(),
            Step::Summarize => self.summarize().await,
            Step::Write => self.write(),
        }
    }

    /// Decodes `audio.opus` and finds speech on both channels, once per run.
    /// Microphone regions that are the remote side's echo are dropped.
    fn audio(&mut self) -> Result<&Audio> {
        if self.audio.is_none() {
            let (me, them) = decode_stereo(&self.dir.audio())?;
            let vad = &self.engines.vad;
            let me_regions = merge_close(&vad.regions(&me)?, MERGE_GAP_MS, MAX_REGION_MS);
            let them_regions = merge_close(&vad.regions(&them)?, MERGE_GAP_MS, MAX_REGION_MS);
            let me_regions = drop_echo(&me_regions, &them_regions, ECHO_TOLERANCE_MS);
            *self.audio = Some(Audio {
                me,
                them,
                me_regions,
                them_regions,
            });
        }
        Ok(self.audio.as_ref().expect("set above"))
    }

    fn transcribe(&mut self) -> Result<()> {
        self.audio()?;
        let audio = self.audio.as_ref().expect("loaded above");
        let transcriber = &mut self.engines.transcriber;
        let me = transcriber.transcribe(&audio.me, &audio.me_regions, LANG)?;
        let them = transcriber.transcribe(&audio.them, &audio.them_regions, LANG)?;
        self.meeting.segments = merge_sides(me, them);
        self.meeting
            .models
            .insert("transcriber".into(), transcriber.model_name());
        Ok(())
    }

    /// Clusters the remote voices and gives microphone lines the speaker
    /// `me`. A meeting with no remote speech has no clusters, and the
    /// diarizer is not run.
    fn diarize(&mut self) -> Result<()> {
        if !self.meeting.segments.iter().any(|s| s.side == Side::Them) {
            for segment in &mut self.meeting.segments {
                segment.speaker = Some(SpeakerId(ME.into()));
            }
            self.meeting.clusters.clear();
            return Ok(());
        }
        self.audio()?;
        let audio = self.audio.as_ref().expect("loaded above");
        let (turns, centroids) = self.engines.diarizer.diarize(&audio.them)?;
        self.meeting.clusters = assign_clusters(&mut self.meeting.segments, &turns, &centroids);
        Ok(())
    }

    /// Names the clusters after known voices and registers new ones. Rerun
    /// on its own, this only relabels clusters: segments whose speaker was
    /// already resolved keep it, including any edit made since.
    fn name(&mut self) -> Result<()> {
        name_clusters(&mut self.meeting.clusters, self.speakers, NAME_THRESHOLD);
        apply_names(
            &mut self.meeting.segments,
            &mut self.meeting.clusters,
            self.speakers,
        );
        self.speakers.save(&self.config.speakers_path())
    }

    async fn summarize(&mut self) -> Result<()> {
        let llm = &self.engines.llm;
        let summary = summarize(
            llm.as_ref(),
            &self.meeting.segments,
            self.speakers,
            &self.config.summary_lang,
            SUMMARY_CHUNK_CHARS,
        )
        .await?;
        self.meeting.summary = Some(summary);
        self.meeting.models.insert("llm".into(), llm.name());
        Ok(())
    }

    /// Writes `meeting.md` (through a temporary file and a rename) and
    /// indexes the meeting.
    fn write(&mut self) -> Result<()> {
        let md =
            recall_store::markdown::render(self.meeting, self.speakers, &self.config.summary_lang);
        let target = self.dir.markdown();
        let mut tmp = NamedTempFile::new_in(self.dir.path())
            .with_context(|| format!("creating a temporary file beside {}", target.display()))?;
        tmp.write_all(md.as_bytes())
            .and_then(|()| tmp.as_file().sync_all())
            .with_context(|| format!("writing {}", target.display()))?;
        tmp.persist(&target)
            .with_context(|| format!("renaming to {}", target.display()))?;

        self.meeting
            .steps
            .insert(Step::Write, StepState::Done { at: Utc::now() });
        self.meeting.save(&self.dir.path())?;
        Index::open(&self.config.index_path())?.upsert(self.meeting, self.speakers, true)
    }
}
