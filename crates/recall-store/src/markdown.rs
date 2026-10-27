//! Renders `meeting.md`: YAML frontmatter, the summary sections and the
//! transcript. Obsidian reads the file directly.

use std::fmt::Write;

use chrono::SecondsFormat;

use crate::meeting::{ActionItem, Meeting, Segment, Side};
use crate::speakers::Speakers;

struct Labels {
    summary: &'static str,
    decisions: &'static str,
    actions: &'static str,
    chapters: &'static str,
    transcript: &'static str,
    /// Name for a remote segment no speaker has been assigned to.
    them: &'static str,
}

const ZH_TW: Labels = Labels {
    summary: "摘要",
    decisions: "決策",
    actions: "待辦",
    chapters: "章節",
    transcript: "逐字稿",
    them: "對方",
};

const EN: Labels = Labels {
    summary: "Summary",
    decisions: "Decisions",
    actions: "Action items",
    chapters: "Chapters",
    transcript: "Transcript",
    them: "Them",
};

/// Renders the meeting as Markdown. `lang` is `"zh-TW"` or `"en"`; anything
/// else gets English headings.
pub fn render(meeting: &Meeting, speakers: &Speakers, lang: &str) -> String {
    let labels = if lang == "zh-TW" { &ZH_TW } else { &EN };
    let mut out = String::new();
    write_frontmatter(&mut out, meeting, speakers);

    if let Some(summary) = &meeting.summary {
        let _ = write!(out, "\n## {}\n\n", labels.summary);
        let paragraphs = [summary.one_line.trim(), summary.body.trim()];
        let text: Vec<&str> = paragraphs.into_iter().filter(|p| !p.is_empty()).collect();
        if !text.is_empty() {
            let _ = writeln!(out, "{}", text.join("\n\n"));
        }
        if !summary.decisions.is_empty() {
            let _ = write!(out, "\n## {}\n\n", labels.decisions);
            for decision in &summary.decisions {
                let _ = writeln!(out, "- {decision}");
            }
        }
        if !summary.actions.is_empty() {
            let _ = write!(out, "\n## {}\n\n", labels.actions);
            for action in &summary.actions {
                let _ = writeln!(out, "{}", action_line(action));
            }
        }
        if !summary.chapters.is_empty() {
            let _ = write!(out, "\n## {}\n\n", labels.chapters);
            for chapter in &summary.chapters {
                let _ = writeln!(out, "- [{}] {}", clock(chapter.start_ms), chapter.title);
            }
        }
    }

    let _ = write!(out, "\n## {}\n\n", labels.transcript);
    for segment in &meeting.segments {
        let name = segment_speaker(segment, speakers, labels.them);
        let _ = writeln!(
            out,
            "[{}] **{}** {}",
            clock(segment.start_ms),
            name,
            segment.text.trim()
        );
    }
    out
}

fn write_frontmatter(out: &mut String, meeting: &Meeting, speakers: &Speakers) {
    let participants: Vec<String> = meeting
        .participants()
        .iter()
        .map(|id| yaml_scalar(&speakers.name_of(id)))
        .collect();
    let source_app = meeting
        .source_app
        .as_deref()
        .map_or_else(|| "null".to_string(), yaml_scalar);
    let _ = write!(
        out,
        "---\ntitle: {}\ndate: {}\nduration: {}\nparticipants: [{}]\nsource_app: {}\ntags: []\n---\n",
        yaml_scalar(&meeting.title),
        meeting.started.to_rfc3339_opts(SecondsFormat::Secs, true),
        clock(meeting.duration_ms),
        participants.join(", "),
        source_app,
    );
}

/// `- [ ] text (owner, due)`, keeping only the parts that are present.
fn action_line(action: &ActionItem) -> String {
    let extras: Vec<&str> = [action.owner.as_deref(), action.due.as_deref()]
        .into_iter()
        .flatten()
        .collect();
    if extras.is_empty() {
        format!("- [ ] {}", action.text)
    } else {
        format!("- [ ] {} ({})", action.text, extras.join(", "))
    }
}

/// The display name for a segment: its assigned speaker, else `me` for the
/// microphone side and `them` for the remote side.
pub(crate) fn segment_speaker(segment: &Segment, speakers: &Speakers, them: &str) -> String {
    match (&segment.speaker, segment.side) {
        (Some(id), _) => speakers.name_of(id),
        (None, Side::Me) => speakers.me().name.clone(),
        (None, Side::Them) => them.to_string(),
    }
}

/// `mm:ss`, or `h:mm:ss` from one hour on.
pub fn clock(ms: u32) -> String {
    let secs = ms / 1000;
    let (h, m, s) = (secs / 3600, secs / 60 % 60, secs % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m:02}:{s:02}")
    }
}

/// A YAML scalar that is safe both as a plain value and inside a flow list.
/// Strings that YAML would read as something else (syntax characters such as
/// `:` `#` `,` `[`, a leading indicator, surrounding spaces, or a null or
/// boolean word) are double-quoted.
fn yaml_scalar(s: &str) -> String {
    const SPECIAL: &[char] = &[
        ':', '#', ',', '[', ']', '{', '}', '"', '\'', '\\', '\n', '\r', '\t',
    ];
    const LEADING: &[char] = &['-', '?', '!', '&', '*', '|', '>', '%', '@', '`'];
    const WORDS: &[&str] = &["null", "~", "true", "false", "yes", "no", "on", "off"];
    let needs_quotes = s.is_empty()
        || s.contains(SPECIAL)
        || s.starts_with(LEADING)
        || s.trim() != s
        || WORDS.contains(&s.to_ascii_lowercase().as_str());
    if !needs_quotes {
        return s.to_string();
    }
    let mut quoted = String::with_capacity(s.len() + 2);
    quoted.push('"');
    for c in s.chars() {
        match c {
            '"' => quoted.push_str("\\\""),
            '\\' => quoted.push_str("\\\\"),
            '\n' => quoted.push_str("\\n"),
            '\r' => quoted.push_str("\\r"),
            '\t' => quoted.push_str("\\t"),
            c => quoted.push(c),
        }
    }
    quoted.push('"');
    quoted
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::meeting::{ActionItem, Chapter, MeetingId, Segment, Side, SpeakerId, Summary};
    use chrono::{TimeZone, Utc};
    use std::collections::BTreeMap;

    fn sample() -> (Meeting, Speakers) {
        let mut speakers = Speakers::default();
        let tom = speakers.add("Tom", None);
        let meeting = Meeting {
            id: MeetingId("2026-10-27 14:30 Weekly".into()),
            title: "Weekly: release #3".into(),
            started: Utc.with_ymd_and_hms(2026, 10, 27, 6, 30, 0).unwrap(),
            duration_ms: 1_832_000,
            source_app: Some("Google Meet".into()),
            steps: BTreeMap::new(),
            segments: vec![
                Segment {
                    start_ms: 0,
                    end_ms: 2_000,
                    side: Side::Me,
                    speaker: Some(SpeakerId("me".into())),
                    text: "早安".into(),
                },
                Segment {
                    start_ms: 92_000,
                    end_ms: 95_000,
                    side: Side::Them,
                    speaker: Some(tom),
                    text: "這段 export 會 timeout".into(),
                },
                Segment {
                    start_ms: 96_000,
                    end_ms: 97_000,
                    side: Side::Them,
                    speaker: None,
                    text: "嗯".into(),
                },
            ],
            clusters: vec![],
            summary: Some(Summary {
                one_line: "週會".into(),
                body: "討論 release。".into(),
                decisions: vec!["週五上線".into()],
                actions: vec![
                    ActionItem {
                        text: "寫 release notes".into(),
                        owner: Some("Leon".into()),
                        due: Some("2026-10-30".into()),
                    },
                    ActionItem {
                        text: "修 export".into(),
                        owner: Some("Tom".into()),
                        due: None,
                    },
                    ActionItem {
                        text: "看 log".into(),
                        owner: None,
                        due: Some("Friday".into()),
                    },
                    ActionItem {
                        text: "整理".into(),
                        owner: None,
                        due: None,
                    },
                ],
                chapters: vec![Chapter {
                    start_ms: 92_000,
                    title: "Export".into(),
                }],
            }),
            models: BTreeMap::new(),
        };
        (meeting, speakers)
    }

    #[test]
    fn markdown_renders_frontmatter_and_zh_headings() {
        let (meeting, speakers) = sample();
        let md = render(&meeting, &speakers, "zh-TW");
        let expected = "\
---
title: \"Weekly: release #3\"
date: 2026-10-27T06:30:00Z
duration: 30:32
participants: [Me, Tom]
source_app: Google Meet
tags: []
---

## 摘要

週會

討論 release。

## 決策

- 週五上線

## 待辦

- [ ] 寫 release notes (Leon, 2026-10-30)
- [ ] 修 export (Tom)
- [ ] 看 log (Friday)
- [ ] 整理

## 章節

- [01:32] Export

## 逐字稿

[00:00] **Me** 早安
[01:32] **Tom** 這段 export 會 timeout
[01:36] **對方** 嗯
";
        assert_eq!(md, expected);
    }

    #[test]
    fn markdown_en_headings() {
        let (meeting, speakers) = sample();
        let md = render(&meeting, &speakers, "en");
        for heading in [
            "## Summary",
            "## Decisions",
            "## Action items",
            "## Chapters",
            "## Transcript",
        ] {
            assert!(md.contains(&format!("\n{heading}\n")), "missing {heading}");
        }
        assert!(md.contains("[01:36] **Them** 嗯"));
        assert!(!md.contains("## 摘要"));
    }

    #[test]
    fn markdown_without_summary_has_only_transcript() {
        let (mut meeting, speakers) = sample();
        meeting.summary = None;
        meeting.source_app = None;
        let md = render(&meeting, &speakers, "en");
        assert!(md.contains("source_app: null\n"));
        assert!(!md.contains("## Summary"));
        assert!(md.contains("\n## Transcript\n"));
    }

    #[test]
    fn yaml_scalars_are_quoted_when_needed() {
        assert_eq!(yaml_scalar("Tom"), "Tom");
        assert_eq!(yaml_scalar("a: b"), "\"a: b\"");
        assert_eq!(yaml_scalar("#1"), "\"#1\"");
        assert_eq!(yaml_scalar("Lee, Ann"), "\"Lee, Ann\"");
        assert_eq!(yaml_scalar("say \"hi\""), "\"say \\\"hi\\\"\"");
        assert_eq!(yaml_scalar(""), "\"\"");
        assert_eq!(yaml_scalar("null"), "\"null\"");
    }

    #[test]
    fn clock_formats_hours() {
        assert_eq!(clock(0), "00:00");
        assert_eq!(clock(92_000), "01:32");
        assert_eq!(clock(3_599_999), "59:59");
        assert_eq!(clock(3_600_000), "1:00:00");
        assert_eq!(clock(3_723_000), "1:02:03");
    }
}
