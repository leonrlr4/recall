//! Status events broadcast by the daemon and streamed to the web UI.

use serde::{Deserialize, Serialize};

use crate::meeting::{MeetingId, Step};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Event {
    Recording {
        app: String,
    },
    Stopped,
    Progress {
        meeting: MeetingId,
        step: Step,
        pct: f32,
    },
    Done {
        meeting: MeetingId,
    },
    Failed {
        meeting: MeetingId,
        step: Step,
        error: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn events_serialize_with_kind_tag() {
        let ev = Event::Progress {
            meeting: MeetingId("m".into()),
            step: Step::Transcribe,
            pct: 0.5,
        };
        assert_eq!(
            serde_json::to_value(&ev).unwrap(),
            serde_json::json!({"kind": "progress", "meeting": "m", "step": "transcribe", "pct": 0.5})
        );
        assert_eq!(
            serde_json::to_value(Event::Stopped).unwrap(),
            serde_json::json!({"kind": "stopped"})
        );
        let back: Event = serde_json::from_str(
            r#"{"kind":"failed","meeting":"m","step":"write","error":"disk full"}"#,
        )
        .unwrap();
        assert!(matches!(
            back,
            Event::Failed {
                step: Step::Write,
                ..
            }
        ));
    }
}
