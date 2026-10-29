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

/// Meeting directory name, e.g. `"2026-10-27 14:30 Weekly sync"`.
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
    /// How `speaker` was chosen; absent in records written before it existed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub named_by: Option<NamedBy>,
}

/// The source of a cluster's speaker name.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NamedBy {
    Voiceprint,
    Meet,
    User,
}

/// A follow-up task from the summary. The fields after `due` are absent in
/// records written before they existed and load as their defaults.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ActionItem {
    pub text: String,
    pub owner: Option<String>,
    /// `YYYY-MM-DD` when the meeting's date makes it computable, otherwise
    /// the wording used in the meeting (e.g. `"next sprint"`).
    pub due: Option<String>,
    #[serde(default)]
    pub priority: Option<Priority>,
    /// Milliseconds into the meeting where the task came up.
    #[serde(default)]
    pub source_ms: Option<u32>,
    /// Checked off by the user; the summary step never sets it.
    #[serde(default)]
    pub done: bool,
    #[serde(default)]
    pub done_at: Option<DateTime<Utc>>,
}

/// An action item's priority. Declaration order is urgency order, which
/// sorting relies on.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Priority {
    High,
    Medium,
    Low,
}

impl Priority {
    /// Reads a priority word case-insensitively; anything else is `None`, so
    /// a model writing `"urgent"` or `""` costs the priority, not the
    /// summary.
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "high" => Some(Self::High),
            "medium" => Some(Self::Medium),
            "low" => Some(Self::Low),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Chapter {
    pub start_ms: u32,
    pub title: String,
}

/// The meeting's summary in `summary_lang`, the canonical language:
/// `meeting.md`, the vault note, the MCP tools and done marks all use these
/// fields. The fields after `chapters` are absent in records written before
/// they existed.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Summary {
    pub one_line: String,
    pub body: String,
    pub decisions: Vec<String>,
    pub actions: Vec<ActionItem>,
    pub chapters: Vec<Chapter>,
    /// Language tag the summary was written in, e.g. `"en"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lang: Option<String>,
    /// The texts in other languages, keyed by language tag, for reading in
    /// the web UI only. Each fits this summary; see [`SummaryText::fits`].
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub translations: BTreeMap<String, SummaryText>,
}

impl Summary {
    /// The summary's own texts, in the shape of a translation.
    pub fn text(&self) -> SummaryText {
        SummaryText {
            one_line: self.one_line.clone(),
            body: self.body.clone(),
            decisions: self.decisions.clone(),
            actions: self.actions.iter().map(|a| a.text.clone()).collect(),
            chapters: self.chapters.iter().map(|c| c.title.clone()).collect(),
        }
    }
}

/// A summary's texts in another language. `decisions`, `actions` (the
/// items' text) and `chapters` (the titles) line up item for item with the
/// summary's own lists, so done marks, owners, dues and times are kept once,
/// on the summary.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct SummaryText {
    pub one_line: String,
    pub body: String,
    pub decisions: Vec<String>,
    pub actions: Vec<String>,
    pub chapters: Vec<String>,
}

impl SummaryText {
    /// Why this text does not line up with `summary`, if it does not: a list
    /// whose length differs.
    pub fn fits(&self, summary: &Summary) -> Result<(), String> {
        for (name, ours, theirs) in [
            ("decisions", self.decisions.len(), summary.decisions.len()),
            ("actions", self.actions.len(), summary.actions.len()),
            ("chapters", self.chapters.len(), summary.chapters.len()),
        ] {
            if ours != theirs {
                return Err(format!("{ours} {name} where the summary has {theirs}"));
            }
        }
        Ok(())
    }
}

/// The calendar event a meeting was matched to by start time. Attendee
/// emails stay in `meeting.json`: `meeting.md`, which the vault sync
/// publishes, carries only names.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CalendarEvent {
    /// The calendar's event id.
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub attendees: Vec<Attendee>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Attendee {
    /// Display name; empty when the calendar has none.
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub email: String,
    /// As the calendar gives it: `accepted`, `declined`, `tentative` or
    /// `needsAction`.
    #[serde(default)]
    pub response: String,
    /// The calendar's owner, i.e. the user.
    #[serde(default, rename = "self", skip_serializing_if = "std::ops::Not::not")]
    pub is_self: bool,
}

impl CalendarEvent {
    /// Names of the attendees other than the user who have not declined, in
    /// calendar order; attendees without a name are left out.
    pub fn attendee_names(&self) -> Vec<String> {
        self.attendees
            .iter()
            .filter(|a| !a.is_self && a.response != "declined")
            .map(|a| a.name.trim())
            .filter(|n| !n.is_empty())
            .map(str::to_owned)
            .collect()
    }
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
    /// Names from the Meet roster, offered as choices when renaming a speaker.
    #[serde(default)]
    pub participants_hint: Vec<String>,
    /// Milliseconds from the audio origin to the first screen frame, when a
    /// screen recording exists.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub screen_offset_ms: Option<i64>,
    /// Why the screen recording failed, when it did.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub screen_error: Option<String>,
    /// The user asked for this meeting to be published to the shared vault.
    /// Off unless turned on, because recall records every call, private
    /// ones included.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub sync: bool,
    /// The user's calendar event at the meeting's start, when the calendar
    /// lookup is on and found one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calendar: Option<CalendarEvent>,
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
            id: MeetingId("2026-10-27 14:30 Weekly sync".into()),
            title: "Weekly sync".into(),
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
                named_by: Some(NamedBy::Voiceprint),
            }],
            summary: Some(Summary {
                one_line: "Weekly sync.".into(),
                body: "Talked about the release.".into(),
                decisions: vec!["Ship Friday".into()],
                actions: vec![ActionItem {
                    text: "Write release notes".into(),
                    owner: Some("Leon".into()),
                    due: Some("2026-10-30".into()),
                    priority: Some(Priority::High),
                    source_ms: Some(2_500),
                    done: true,
                    done_at: Some(at),
                }],
                chapters: vec![Chapter {
                    start_ms: 0,
                    title: "Intro".into(),
                }],
                lang: Some("en".into()),
                translations: BTreeMap::from([(
                    "zh-TW".to_string(),
                    SummaryText {
                        one_line: "週會。".into(),
                        body: "討論了發版。".into(),
                        decisions: vec!["週五發版".into()],
                        actions: vec!["寫發版說明".into()],
                        chapters: vec!["開場".into()],
                    },
                )]),
            }),
            models,
            participants_hint: vec!["Tom Lin".into(), "Amy Chen".into()],
            screen_offset_ms: Some(1_250),
            screen_error: None,
            sync: false,
            calendar: Some(CalendarEvent {
                id: "ev1".into(),
                title: "Weekly sync".into(),
                attendees: vec![
                    attendee("Tom Lin", "tom@example.com", "accepted", false),
                    attendee("", "leon@example.com", "accepted", true),
                ],
            }),
        }
    }

    fn attendee(name: &str, email: &str, response: &str, is_self: bool) -> Attendee {
        Attendee {
            name: name.into(),
            email: email.into(),
            response: response.into(),
            is_self,
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
    fn old_meeting_json_loads_with_new_fields_defaulted() {
        let dir = tempfile::tempdir().unwrap();
        let mut value = serde_json::to_value(sample()).unwrap();
        let object = value.as_object_mut().unwrap();
        for key in [
            "participants_hint",
            "screen_offset_ms",
            "screen_error",
            "calendar",
        ] {
            object.remove(key);
        }
        value["clusters"][0]
            .as_object_mut()
            .unwrap()
            .remove("named_by");
        let summary = value["summary"].as_object_mut().unwrap();
        summary.remove("lang");
        summary.remove("translations");
        fs::write(dir.path().join(FILE), serde_json::to_vec(&value).unwrap()).unwrap();

        let loaded = Meeting::load(dir.path()).unwrap();
        assert!(loaded.participants_hint.is_empty());
        assert_eq!(loaded.screen_offset_ms, None);
        assert_eq!(loaded.screen_error, None);
        assert_eq!(loaded.calendar, None);
        assert_eq!(loaded.clusters[0].named_by, None);
        let summary = loaded.summary.as_ref().unwrap();
        assert_eq!(summary.lang, None);
        assert!(summary.translations.is_empty());

        loaded.save(dir.path()).unwrap();
        let saved: serde_json::Value =
            serde_json::from_slice(&fs::read(dir.path().join(FILE)).unwrap()).unwrap();
        assert!(saved.get("screen_offset_ms").is_none());
        assert!(saved.get("screen_error").is_none());
        assert!(saved.get("calendar").is_none());
        assert!(saved["clusters"][0].get("named_by").is_none());
        assert!(saved["summary"].get("lang").is_none());
        assert!(saved["summary"].get("translations").is_none());
    }

    #[test]
    fn summary_text_fits_only_lists_of_the_same_lengths() {
        let summary = sample().summary.unwrap();
        let text = &summary.translations["zh-TW"];
        assert_eq!(text.fits(&summary), Ok(()));
        assert_eq!(summary.text().fits(&summary), Ok(()));
        assert_eq!(summary.text().actions, ["Write release notes"]);

        let mut short = text.clone();
        short.actions.clear();
        assert_eq!(
            short.fits(&summary),
            Err("0 actions where the summary has 1".into())
        );
        let mut long = text.clone();
        long.chapters.push("多".into());
        assert!(long.fits(&summary).is_err());
    }

    #[test]
    fn old_action_items_load_open_without_priority() {
        let json = r#"{"text":"Write notes","owner":"Leon","due":"Friday"}"#;
        let item: ActionItem = serde_json::from_str(json).unwrap();
        assert_eq!(
            item,
            ActionItem {
                text: "Write notes".into(),
                owner: Some("Leon".into()),
                due: Some("Friday".into()),
                ..ActionItem::default()
            }
        );
    }

    #[test]
    fn attendee_names_skip_the_user_the_nameless_and_those_who_declined() {
        let event = CalendarEvent {
            id: "e".into(),
            title: "t".into(),
            attendees: vec![
                attendee("Tom Lin", "tom@example.com", "accepted", false),
                attendee("Me", "me@example.com", "accepted", true),
                attendee(" ", "anon@example.com", "needsAction", false),
                attendee("Bob", "bob@example.com", "declined", false),
                attendee("Amy Chen", "amy@example.com", "tentative", false),
            ],
        };
        assert_eq!(event.attendee_names(), ["Tom Lin", "Amy Chen"]);
        let json = serde_json::to_value(&event.attendees[1]).unwrap();
        assert_eq!(json["self"], true);
        assert!(
            serde_json::to_value(&event.attendees[0])
                .unwrap()
                .get("self")
                .is_none()
        );
    }

    #[test]
    fn action_item_json_has_new_fields() {
        let value = serde_json::to_value(sample()).unwrap();
        let action = &value["summary"]["actions"][0];
        assert_eq!(action["priority"], "high");
        assert_eq!(action["source_ms"], 2_500);
        assert_eq!(action["done"], true);
        assert_eq!(action["done_at"], "2026-10-27T14:30:00Z");
    }

    #[test]
    fn priority_parses_leniently() {
        assert_eq!(Priority::parse(" HIGH "), Some(Priority::High));
        assert_eq!(Priority::parse("medium"), Some(Priority::Medium));
        assert_eq!(Priority::parse("Low"), Some(Priority::Low));
        assert_eq!(Priority::parse("urgent"), None);
        assert_eq!(Priority::parse(""), None);
        assert!(Priority::High < Priority::Low);
    }

    #[test]
    fn named_by_is_snake_case() {
        assert_eq!(
            serde_json::to_value(NamedBy::Voiceprint).unwrap(),
            "voiceprint"
        );
        assert_eq!(serde_json::to_value(NamedBy::Meet).unwrap(), "meet");
        assert_eq!(serde_json::to_value(NamedBy::User).unwrap(), "user");
    }

    #[test]
    fn missing_meeting_json_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        assert!(Meeting::load(dir.path()).is_err());
    }

    #[test]
    fn json_uses_snake_case_and_string_step_keys() {
        let value = serde_json::to_value(sample()).unwrap();
        assert_eq!(value["id"], "2026-10-27 14:30 Weekly sync");
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
