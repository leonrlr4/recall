//! System prompts, transcript formatting and summary parsing.

use anyhow::{Context, Result, anyhow};
use recall_store::markdown::clock;
use recall_store::{ActionItem, Chapter, Summary};
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
         - \"actions\": array of {{\"text\": string, \"owner\": string or null, \"due\": string or null}}, \
         the follow-up tasks; use null when the transcript names no owner or due date.\n\
         - \"chapters\": array of {{\"start_ms\": integer, \"title\": string}}, topic changes in order, \
         start_ms being the milliseconds from the start of the meeting where the topic begins.\n\
         \n\
         Write all prose in {language}.\n\
         Keep English technical terms, product names and people's names exactly as spoken.\n\
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
    actions: Vec<ActionItem>,
    #[serde(default)]
    chapters: Vec<Chapter>,
}

/// Reads the summary JSON out of model output, tolerating a code fence and
/// prose around the object. Errors quote the start of `raw`.
pub fn parse_summary(raw: &str) -> Result<Summary> {
    let quoted = || raw.chars().take(RAW_IN_ERROR_CHARS).collect::<String>();
    let text = strip_fence(raw);
    let json = match (text.find('{'), text.rfind('}')) {
        (Some(start), Some(end)) if start < end => &text[start..=end],
        _ => return Err(anyhow!("no JSON object in model output: {}", quoted())),
    };
    let dto: SummaryDto = serde_json::from_str(json)
        .with_context(|| format!("model output is not a summary: {}", quoted()))?;
    Ok(Summary {
        one_line: dto.one_line,
        body: dto.body,
        decisions: dto.decisions,
        actions: dto.actions,
        chapters: dto.chapters,
    })
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
    fn summary_prompt_names_keys_and_language() {
        let zh = summary_prompt("zh-TW");
        for key in [
            "one_line",
            "body",
            "decisions",
            "actions",
            "chapters",
            "start_ms",
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
    fn ask_prompt_requires_citations_in_language() {
        let zh = ask_prompt("zh-TW");
        assert!(zh.contains("[mm:ss]"));
        assert!(zh.contains("Traditional Chinese (Taiwan)"));
        assert!(ask_prompt("en").contains("English"));
    }
}
