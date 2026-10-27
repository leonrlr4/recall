//! The meeting record stored as `meeting.json` in each meeting directory.

use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::Path;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

const FILE: &str = "meeting.json";
const TMP_FILE: &str = "meeting.json.tmp";

/// Meeting directory name, e.g. `"2026-10-27 14:30 Biobuild weekly"`.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct MeetingId(pub String);

/// Person id in the speaker registry: `"me"` or a uuid.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SpeakerId(pub String);

/// Processing steps. Declaration order is pipeline order, which `Ord` and
/// `ALL` both rely on.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Step {
    Segment,
    Transcribe,
    Diarize,
    Name,
    Summarize,
    Write,
}

impl Step {
    pub const ALL: [Step; 6] = [
        Step::Segment,
        Step::Transcribe,
        Step::Diarize,
        Step::Name,
        Step::Summarize,
        Step::Write,
    ];

    /// The step that runs after this one, or `None` after `Write`.
    pub fn next(self) -> Option<Step> {
        let i = Step::ALL.iter().position(|s| *s == self)?;
        Step::ALL.get(i + 1).copied()
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StepState {
    Pending,
    Running,
    Done { at: DateTime<Utc> },
    Failed { at: DateTime<Utc>, error: String },
}

/// Which audio channel a segment came from: the microphone or the remote side.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Side {
    Me,
    Them,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Segment {
    pub start_ms: u32,
    pub end_ms: u32,
    pub side: Side,
    pub speaker: Option<SpeakerId>,
    pub text: String,
}

/// A remote voice found by diarization, optionally matched to a known speaker.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Cluster {
    pub label: String,
    pub embedding: Vec<f32>,
    pub speaker: Option<SpeakerId>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ActionItem {
    pub text: String,
    pub owner: Option<String>,
    pub due: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Chapter {
    pub start_ms: u32,
    pub title: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Summary {
    pub one_line: String,
    pub body: String,
    pub decisions: Vec<String>,
    pub actions: Vec<ActionItem>,
    pub chapters: Vec<Chapter>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Meeting {
    pub id: MeetingId,
    pub title: String,
    pub started: DateTime<Utc>,
    pub duration_ms: u32,
    pub source_app: Option<String>,
    pub steps: BTreeMap<Step, StepState>,
    pub segments: Vec<Segment>,
    pub clusters: Vec<Cluster>,
    pub summary: Option<Summary>,
    /// Model name per role (e.g. `"asr"`), recorded for reproducibility.
    pub models: BTreeMap<String, String>,
}

impl Meeting {
    /// Reads `meeting.json` from `dir`. A missing file is an error.
    pub fn load(dir: &Path) -> Result<Self> {
        let path = dir.join(FILE);
        let bytes = fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
        serde_json::from_slice(&bytes).with_context(|| format!("parsing {}", path.display()))
    }

    /// Writes `meeting.json` into `dir` as pretty JSON. The data goes to
    /// `meeting.json.tmp` first and is renamed over the old file, so a crash
    /// mid-write never leaves a truncated record.
    pub fn save(&self, dir: &Path) -> Result<()> {
        let tmp = dir.join(TMP_FILE);
        let path = dir.join(FILE);
        let json = serde_json::to_vec_pretty(self)?;
        let mut file =
            fs::File::create(&tmp).with_context(|| format!("creating {}", tmp.display()))?;
        file.write_all(&json)
            .and_then(|()| file.sync_all())
            .with_context(|| format!("writing {}", tmp.display()))?;
        fs::rename(&tmp, &path).with_context(|| format!("renaming to {}", path.display()))
    }

    /// Distinct speakers attributed in the transcript, in order of first speech.
    pub fn participants(&self) -> Vec<SpeakerId> {
        let mut out: Vec<SpeakerId> = Vec::new();
        for id in self.segments.iter().filter_map(|s| s.speaker.as_ref()) {
            if !out.contains(id) {
                out.push(id.clone());
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn sample() -> Meeting {
        let at = Utc.with_ymd_and_hms(2026, 10, 27, 14, 30, 0).unwrap();
        let mut steps = BTreeMap::new();
        steps.insert(Step::Transcribe, StepState::Done { at });
        let mut models = BTreeMap::new();
        models.insert("asr".to_string(), "whisper-large-v3".to_string());
        Meeting {
            id: MeetingId("2026-10-27 14:30 Biobuild weekly".into()),
            title: "Biobuild weekly".into(),
            started: at,
            duration_ms: 1_800_000,
            source_app: Some("Google Meet".into()),
            steps,
            segments: vec![
                Segment {
                    start_ms: 0,
                    end_ms: 2_000,
                    side: Side::Me,
                    speaker: None,
                    text: "Morning.".into(),
                },
                Segment {
                    start_ms: 2_500,
                    end_ms: 4_000,
                    side: Side::Them,
                    speaker: Some(SpeakerId("3f1c".into())),
                    text: "Hi.".into(),
                },
            ],
            clusters: vec![Cluster {
                label: "Remote 1".into(),
                embedding: vec![0.25, -0.5],
                speaker: Some(SpeakerId("3f1c".into())),
            }],
            summary: Some(Summary {
                one_line: "Weekly sync.".into(),
                body: "Talked about the release.".into(),
                decisions: vec!["Ship Friday".into()],
                actions: vec![ActionItem {
                    text: "Write release notes".into(),
                    owner: Some("Leon".into()),
                    due: None,
                }],
                chapters: vec![Chapter {
                    start_ms: 0,
                    title: "Intro".into(),
                }],
            }),
            models,
        }
    }

    #[test]
    fn meeting_json_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let meeting = sample();
        meeting.save(dir.path()).unwrap();
        assert!(!dir.path().join("meeting.json.tmp").exists());
        let loaded = Meeting::load(dir.path()).unwrap();
        assert_eq!(loaded, meeting);
    }

    #[test]
    fn missing_meeting_json_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        assert!(Meeting::load(dir.path()).is_err());
    }

    #[test]
    fn json_uses_snake_case_and_string_step_keys() {
        let value = serde_json::to_value(sample()).unwrap();
        assert_eq!(value["id"], "2026-10-27 14:30 Biobuild weekly");
        assert_eq!(value["segments"][0]["side"], "me");
        assert_eq!(value["segments"][1]["speaker"], "3f1c");
        assert!(value["steps"]["transcribe"]["done"]["at"].is_string());
    }

    #[test]
    fn participants_are_distinct_speakers_in_first_speech_order() {
        let mut meeting = sample();
        meeting.segments.push(Segment {
            start_ms: 5_000,
            end_ms: 6_000,
            side: Side::Me,
            speaker: Some(SpeakerId("me".into())),
            text: "Sure.".into(),
        });
        meeting.segments.push(Segment {
            start_ms: 6_500,
            end_ms: 7_000,
            side: Side::Them,
            speaker: Some(SpeakerId("3f1c".into())),
            text: "Thanks.".into(),
        });
        assert_eq!(
            meeting.participants(),
            vec![SpeakerId("3f1c".into()), SpeakerId("me".into())]
        );
    }

    #[test]
    fn step_order_and_next() {
        assert_eq!(Step::ALL[0], Step::Segment);
        assert_eq!(Step::ALL[5], Step::Write);
        assert!(Step::ALL.windows(2).all(|w| w[0] < w[1]));
        assert_eq!(Step::Segment.next(), Some(Step::Transcribe));
        assert_eq!(Step::Summarize.next(), Some(Step::Write));
        assert_eq!(Step::Write.next(), None);
        assert_eq!(serde_json::to_value(Step::Diarize).unwrap(), "diarize");
    }
}
