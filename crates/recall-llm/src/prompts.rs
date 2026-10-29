//! System prompts, transcript formatting and summary parsing.

use anyhow::{Context, Result, anyhow};
use recall_store::markdown::clock;
use recall_store::{ActionItem, Chapter, Priority, Segment, Side, Speakers, Summary, SummaryText};
use serde::Deserialize;

/// How much of the raw model output a parse error quotes.
const RAW_IN_ERROR_CHARS: usize = 200;

/// The language name the prompts ask the model to write in.
fn language(lang: &str) -> String {
    match lang {
        "zh-TW" => "Traditional Chinese (Taiwan)".into(),
        "en" => "English".into(),
        other => format!("the language with tag {other}"),
    }
}

/// Extra writing rules for `lang` beyond the shared ones.
fn language_rules(lang: &str) -> &'static str {
    match lang {
        "zh-TW" => "Never use Simplified Chinese characters.\n",
        _ => "",
    }
}

/// System prompt asking for the meeting summary as one JSON object, in the
/// shape [`parse_summary`] reads.
pub fn summary_prompt(lang: &str) -> String {
    let language = language(lang);
    let rules = language_rules(lang);
    format!(
        "You summarize meeting transcripts. Each transcript line is \"[mm:ss] Speaker: text\".\n\
         \n\
         Answer only with a single JSON object, no prose and no code fence, with these keys:\n\
         - \"one_line\": string, the meeting in one sentence.\n\
         - \"body\": string, a summary of the discussion in a few short paragraphs.\n\
         - \"decisions\": array of strings, the decisions made.\n\
         - \"actions\": array of {{\"text\": string, \"owner\": string or null, \"due\": string or null, \
         \"priority\": \"high\", \"medium\", \"low\" or null, \"source_ms\": integer or null}}, \
         the follow-up tasks; use null when the transcript names no owner or due date. \
         priority is how urgent the meeting made the task sound, null when it gave no sign. \
         source_ms is the milliseconds from the start of the meeting where the task was raised.\n\
         - \"chapters\": array of {{\"start_ms\": integer, \"title\": string}}, topic changes in order, \
         start_ms being the milliseconds from the start of the meeting where the topic begins.\n\
         \n\
         Write all prose in {language}.\n\
         Keep English technical terms, product names and people's names exactly as spoken.\n\
         {rules}"
    )
}

/// System prompt asking for a summary's texts, sent as a [`SummaryText`]
/// JSON object written in `from`, translated into `to`, in the shape
/// [`parse_summary_text`] reads.
pub fn translate_prompt(from: &str, to: &str) -> String {
    let (from, to_name) = (language(from), language(to));
    let rules = language_rules(to);
    format!(
        "You translate meeting summaries. The user message is one JSON object written in {from}, \
         with the keys \"one_line\" and \"body\" (strings) and \"decisions\", \"actions\" and \
         \"chapters\" (arrays of strings).\n\
         \n\
         Answer only with a single JSON object, no prose and no code fence, with the same keys, \
         every string translated into {to_name}. Each array must have exactly as many items as \
         the one sent, in the same order: item n of your array translates item n of the sent \
         one. Never add, drop, merge, split or reorder items, and add no explanation.\n\
         Keep people's names, ticket ids (such as BB-412), product and project names, code, \
         file names and technical terms exactly as written.\n\
         {rules}"
    )
}

/// System prompt for answering a question about one meeting's transcript.
pub fn ask_prompt(lang: &str) -> String {
    let language = language(lang);
    let rules = language_rules(lang);
    format!(
        "You answer questions about a meeting. The user message contains the transcript, \
         one line per utterance as \"[mm:ss] Speaker: text\", followed by the question.\n\
         \n\
         Answer the question using only the transcript. If the transcript does not contain the answer, say so.\n\
         Answer in {language}, concisely.\n\
         Cite the [mm:ss] timestamp of each line you rely on.\n\
         Keep English technical terms, product names and people's names exactly as spoken.\n\
         {rules}"
    )
}

/// Name shown for a remote line no speaker was found for.
pub const UNKNOWN_REMOTE: &str = "Remote";

/// A meeting's transcript as `"[mm:ss] Name: text"` lines, with speaker ids
/// resolved to names. A line without a speaker is shown as me on the
/// microphone side and as [`UNKNOWN_REMOTE`] on the other. Summaries and
/// questions both send this text.
pub fn meeting_transcript(segments: &[Segment], speakers: &Speakers) -> String {
    let lines: Vec<(String, u32, String)> = segments
        .iter()
        .map(|s| {
            let name = match (&s.speaker, s.side) {
                (Some(id), _) => speakers.name_of(id),
                (None, Side::Me) => speakers.me().name.clone(),
                (None, Side::Them) => UNKNOWN_REMOTE.into(),
            };
            (name, s.start_ms, s.text.clone())
        })
        .collect();
    transcript_text(&lines)
}

/// Formats `(speaker, start_ms, text)` triples as `"[01:32] Tom: text"`
/// lines, each ending in a newline.
pub fn transcript_text(segments: &[(String, u32, String)]) -> String {
    segments
        .iter()
        .map(|(speaker, start_ms, text)| format!("[{}] {speaker}: {text}\n", clock(*start_ms)))
        .collect()
}

/// The summary as the model writes it: absent arrays mean none.
#[derive(Deserialize)]
struct SummaryDto {
    one_line: String,
    body: String,
    #[serde(default)]
    decisions: Vec<String>,
    #[serde(default)]
    actions: Vec<ActionDto>,
    #[serde(default)]
    chapters: Vec<Chapter>,
}

/// An action item as the model writes it. `priority` and `source_ms` are
/// read loosely: a word outside high/medium/low or a non-integer time
/// drops that field, not the whole summary.
#[derive(Deserialize)]
struct ActionDto {
    text: String,
    #[serde(default)]
    owner: Option<String>,
    #[serde(default)]
    due: Option<String>,
    #[serde(default)]
    priority: Option<serde_json::Value>,
    #[serde(default)]
    source_ms: Option<serde_json::Value>,
}

impl From<ActionDto> for ActionItem {
    fn from(dto: ActionDto) -> Self {
        ActionItem {
            text: dto.text,
            owner: dto.owner,
            due: dto.due,
            priority: dto
                .priority
                .as_ref()
                .and_then(serde_json::Value::as_str)
                .and_then(Priority::parse),
            source_ms: dto
                .source_ms
                .as_ref()
                .and_then(serde_json::Value::as_f64)
                .filter(|ms| *ms >= 0.0 && *ms <= f64::from(u32::MAX))
                .map(|ms| ms as u32),
            ..ActionItem::default()
        }
    }
}

/// Reads the summary JSON out of model output, tolerating a code fence and
/// prose around the object. Errors quote the start of `raw`.
pub fn parse_summary(raw: &str) -> Result<Summary> {
    let dto: SummaryDto = serde_json::from_str(json_object(raw)?)
        .with_context(|| format!("model output is not a summary: {}", quoted(raw)))?;
    Ok(Summary {
        one_line: dto.one_line,
        body: dto.body,
        decisions: dto.decisions,
        actions: dto.actions.into_iter().map(ActionItem::from).collect(),
        chapters: dto.chapters,
        ..Summary::default()
    })
}

/// Reads a translated [`SummaryText`] out of model output, as
/// [`parse_summary`] does a summary. Absent arrays read as empty, which
/// [`SummaryText::fits`] then rejects unless the summary has none either.
pub fn parse_summary_text(raw: &str) -> Result<SummaryText> {
    #[derive(Deserialize)]
    struct Dto {
        one_line: String,
        body: String,
        #[serde(default)]
        decisions: Vec<String>,
        #[serde(default)]
        actions: Vec<String>,
        #[serde(default)]
        chapters: Vec<String>,
    }
    let dto: Dto = serde_json::from_str(json_object(raw)?)
        .with_context(|| format!("model output is not a translation: {}", quoted(raw)))?;
    Ok(SummaryText {
        one_line: dto.one_line,
        body: dto.body,
        decisions: dto.decisions,
        actions: dto.actions,
        chapters: dto.chapters,
    })
}

/// The outermost `{...}` in model output, after any code fence.
fn json_object(raw: &str) -> Result<&str> {
    let text = strip_fence(raw);
    match (text.find('{'), text.rfind('}')) {
        (Some(start), Some(end)) if start < end => Ok(&text[start..=end]),
        _ => Err(anyhow!("no JSON object in model output: {}", quoted(raw))),
    }
}

/// The start of `raw`, for an error message.
fn quoted(raw: &str) -> String {
    raw.chars().take(RAW_IN_ERROR_CHARS).collect()
}

/// Removes a leading ```` ```json ```` or ```` ``` ```` line and a trailing
/// ```` ``` ````.
fn strip_fence(raw: &str) -> &str {
    let mut text = raw.trim();
    if let Some(rest) = text.strip_prefix("```") {
        text = rest.strip_prefix("json").unwrap_or(rest);
    }
    text.strip_suffix("```").unwrap_or(text).trim()
}

#[cfg(test)]
mod tests {
    use super::*;

    const JSON: &str = r#"{"one_line":"Weekly sync.","body":"Talked about the release.","decisions":["Ship Friday"],"actions":[{"text":"Write notes","owner":"Leon","due":null}],"chapters":[{"start_ms":0,"title":"Intro"}]}"#;

    #[test]
    fn parse_summary_strips_code_fence() {
        let raw = format!("```json\n{JSON}\n```\n");
        let summary = parse_summary(&raw).unwrap();
        assert_eq!(summary.one_line, "Weekly sync.");
        assert_eq!(summary.decisions, ["Ship Friday"]);
        assert_eq!(summary.actions[0].owner.as_deref(), Some("Leon"));
        assert_eq!(summary.actions[0].due, None);
        assert_eq!(summary.chapters[0].title, "Intro");

        let bare = parse_summary(&format!("```\n{JSON}\n```")).unwrap();
        assert_eq!(bare, summary);
    }

    #[test]
    fn parse_summary_reads_priority_and_source_loosely() {
        let raw = r#"{"one_line":"a","body":"b","actions":[
            {"text":"Send the report","owner":null,"due":"2026-11-03","priority":"High","source_ms":92000},
            {"text":"Book a room","priority":"urgent","source_ms":"soon"},
            {"text":"Tidy up","priority":null,"source_ms":1500.7}
        ]}"#;
        let actions = parse_summary(raw).unwrap().actions;
        assert_eq!(actions[0].priority, Some(Priority::High));
        assert_eq!(actions[0].source_ms, Some(92_000));
        assert_eq!(actions[0].due.as_deref(), Some("2026-11-03"));
        assert!(!actions[0].done);
        assert_eq!(actions[1].priority, None);
        assert_eq!(actions[1].source_ms, None);
        assert_eq!(actions[1].owner, None);
        assert_eq!(actions[2].priority, None);
        assert_eq!(actions[2].source_ms, Some(1_500));
    }

    #[test]
    fn parse_summary_skips_leading_prose() {
        let raw = format!("Here is the summary:\n{JSON}\nHope this helps.");
        assert_eq!(
            parse_summary(&raw).unwrap().body,
            "Talked about the release."
        );
    }

    #[test]
    fn parse_summary_defaults_missing_arrays() {
        let summary = parse_summary(r#"{"one_line":"a","body":"b"}"#).unwrap();
        assert!(summary.decisions.is_empty());
        assert!(summary.actions.is_empty());
        assert!(summary.chapters.is_empty());
    }

    #[test]
    fn parse_summary_rejects_garbage_with_raw_in_error() {
        let raw = format!("I cannot summarize this. {}", "x".repeat(300));
        let err = parse_summary(&raw).unwrap_err().to_string();
        assert!(err.contains("I cannot summarize this."), "{err}");
        assert!(
            !err.contains(&"x".repeat(250)),
            "raw text is cut at 200 chars"
        );

        let err = parse_summary("{not json}").unwrap_err().to_string();
        assert!(err.contains("{not json}"), "{err}");
    }

    #[test]
    fn transcript_text_formats_lines() {
        let segments = [
            ("Tom".to_string(), 92_000, "Ship it Friday.".to_string()),
            ("Leon".to_string(), 3_723_000, "OK.".to_string()),
        ];
        assert_eq!(
            transcript_text(&segments),
            "[01:32] Tom: Ship it Friday.\n[1:02:03] Leon: OK.\n"
        );
        assert_eq!(transcript_text(&[]), "");
    }

    #[test]
    fn meeting_transcript_names_unknown_lines_by_side() {
        let mut speakers = Speakers::default();
        let tom = speakers.add("Tom", None);
        let line =
            |start_ms, side, speaker: Option<&recall_store::SpeakerId>, text: &str| Segment {
                start_ms,
                end_ms: start_ms + 500,
                side,
                speaker: speaker.cloned(),
                text: text.into(),
            };
        let segments = [
            line(0, Side::Me, None, "Morning."),
            line(2_000, Side::Them, Some(&tom), "Ship it."),
            line(5_000, Side::Them, None, "Agreed."),
        ];
        assert_eq!(
            meeting_transcript(&segments, &speakers),
            format!(
                "[00:00] {}: Morning.\n[00:02] Tom: Ship it.\n[00:05] Remote: Agreed.\n",
                speakers.me().name
            )
        );
    }

    #[test]
    fn summary_prompt_names_keys_and_language() {
        let zh = summary_prompt("zh-TW");
        for key in [
            "one_line",
            "body",
            "decisions",
            "actions",
            "chapters",
            "start_ms",
            "priority",
            "source_ms",
        ] {
            assert!(zh.contains(key), "missing {key}");
        }
        assert!(zh.contains("Traditional Chinese (Taiwan)"));
        assert!(zh.contains("Simplified"));
        assert!(zh.contains("JSON"));

        let en = summary_prompt("en");
        assert!(en.contains("English"));
        assert!(!en.contains("Traditional Chinese"));
    }

    #[test]
    fn translate_prompt_names_both_languages_and_keeps_terms() {
        let zh = translate_prompt("en", "zh-TW");
        assert!(zh.contains("written in English"));
        assert!(zh.contains("into Traditional Chinese (Taiwan)"));
        assert!(zh.contains("Simplified"));
        assert!(zh.contains("ticket ids"));
        assert!(zh.contains("exactly as many items"));
    }

    #[test]
    fn parse_summary_text_reads_fenced_json() {
        let raw =
            "```json\n{\"one_line\":\"週會。\",\"body\":\"b\",\"actions\":[\"寫 BB-412\"]}\n```";
        let text = parse_summary_text(raw).unwrap();
        assert_eq!(text.one_line, "週會。");
        assert_eq!(text.actions, ["寫 BB-412"]);
        assert!(text.decisions.is_empty());
        let err = parse_summary_text("{\"one_line\":1}").unwrap_err();
        assert!(format!("{err:#}").contains("not a translation"), "{err:#}");
    }

    #[test]
    fn ask_prompt_requires_citations_in_language() {
        let zh = ask_prompt("zh-TW");
        assert!(zh.contains("[mm:ss]"));
        assert!(zh.contains("Traditional Chinese (Taiwan)"));
        assert!(ask_prompt("en").contains("English"));
    }
}
