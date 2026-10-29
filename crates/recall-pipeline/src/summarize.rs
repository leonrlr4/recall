//! Meeting summaries through an [`Llm`].

use anyhow::{Context, Result};
use recall_llm::{Llm, meeting_transcript, parse_summary, summary_prompt};
use recall_store::{Segment, Speakers, Summary};

use crate::glossary;

/// System prompt for merging the summaries of consecutive parts of one long
/// meeting: the summary prompt, which fixes the output keys and language,
/// followed by what the input is this time.
pub fn merge_prompt(lang: &str) -> String {
    format!(
        "{}\n\
         This time the user message holds no transcript. It holds a JSON array of summaries, \
         each with the keys above, of consecutive parts of one long meeting, in meeting order. \
         Their start_ms values already count from the start of the whole meeting.\n\
         Merge them into one summary of the whole meeting with the same keys: one one_line, \
         one body, every distinct decision and action once, and the chapters in order.\n",
        summary_prompt(lang)
    )
}

/// Summarizes the transcript with `llm`, in `lang`. `glossary` holds the
/// right spellings of names and terms, which the model is asked to use for
/// misheard ones; see [`glossary::summary_note`].
///
/// A transcript of at most `max_chars` characters goes to the model in one
/// call. A longer one is cut at line boundaries into chunks of at most
/// `max_chars` (a single longer line is cut mid-line), each chunk is
/// summarized on its own, and one more call merges those summaries. Chapter
/// starts past the end of the last segment are moved back to it.
///
/// An empty transcript (nobody spoke, or voice detection found nothing)
/// gives an empty summary without calling `llm`, so a silent recording still
/// finishes.
pub async fn summarize(
    llm: &dyn Llm,
    segments: &[Segment],
    speakers: &Speakers,
    lang: &str,
    glossary: &[String],
    max_chars: usize,
) -> Result<Summary> {
    if segments.is_empty() {
        return Ok(Summary::default());
    }
    let text = meeting_transcript(segments, speakers);
    let system = summary_prompt(lang) + &glossary::summary_note(glossary);
    let mut summary = if text.chars().count() <= max_chars {
        parse_summary(&llm.complete(&system, &text).await?)?
    } else {
        let mut parts = Vec::new();
        for (n, chunk) in chunks(&text, max_chars).iter().enumerate() {
            let raw = llm.complete(&system, chunk).await?;
            parts.push(parse_summary(&raw).with_context(|| format!("summary of part {}", n + 1))?);
        }
        let user = serde_json::to_string(&parts)?;
        parse_summary(&llm.complete(&merge_prompt(lang), &user).await?)?
    };
    let end_ms = segments.iter().map(|s| s.end_ms).max().unwrap_or(0);
    for chapter in &mut summary.chapters {
        chapter.start_ms = chapter.start_ms.min(end_ms);
    }
    Ok(summary)
}

/// `text` cut into pieces of at most `max_chars` characters, each ending at
/// a line end unless one line alone is longer than `max_chars`.
fn chunks(text: &str, max_chars: usize) -> Vec<String> {
    let max_chars = max_chars.max(1);
    let mut chunks = Vec::new();
    let mut current = String::new();
    let mut current_chars = 0;
    for line in text.split_inclusive('\n') {
        let mut line_chars = line.chars().count();
        if current_chars + line_chars > max_chars && !current.is_empty() {
            chunks.push(std::mem::take(&mut current));
            current_chars = 0;
        }
        let mut rest = line;
        while line_chars > max_chars {
            let cut = rest
                .char_indices()
                .nth(max_chars)
                .map_or(rest.len(), |(i, _)| i);
            chunks.push(rest[..cut].to_string());
            rest = &rest[cut..];
            line_chars -= max_chars;
        }
        current.push_str(rest);
        current_chars += line_chars;
    }
    if !current.is_empty() {
        chunks.push(current);
    }
    chunks
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use async_trait::async_trait;
    use recall_store::{Side, SpeakerId};
    use tokio::sync::mpsc;

    use super::*;

    /// Records every call and answers with `reply(system, user)`.
    struct FakeLlm {
        calls: Mutex<Vec<(String, String)>>,
        reply: fn(&str, &str) -> String,
    }

    impl FakeLlm {
        fn new(reply: fn(&str, &str) -> String) -> Self {
            Self {
                calls: Mutex::new(Vec::new()),
                reply,
            }
        }

        fn calls(&self) -> Vec<(String, String)> {
            self.calls.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl Llm for FakeLlm {
        async fn complete(&self, system: &str, user: &str) -> Result<String> {
            self.calls
                .lock()
                .unwrap()
                .push((system.into(), user.into()));
            Ok((self.reply)(system, user))
        }

        async fn stream(&self, system: &str, user: &str, tx: mpsc::Sender<String>) -> Result<()> {
            let reply = self.complete(system, user).await?;
            for piece in reply.split_inclusive(' ') {
                let _ = tx.send(piece.to_string()).await;
            }
            Ok(())
        }

        fn name(&self) -> String {
            "fake/model".into()
        }
    }

    fn line(start_ms: u32, end_ms: u32, side: Side, speaker: &str, text: &str) -> Segment {
        Segment {
            start_ms,
            end_ms,
            side,
            speaker: Some(SpeakerId(speaker.into())),
            text: text.into(),
        }
    }

    /// `Tom` and me, plus one remote line nobody was matched to.
    fn meeting() -> (Vec<Segment>, Speakers) {
        let mut speakers = Speakers::default();
        let tom = speakers.add("Tom", None);
        let segments = vec![
            line(0, 2_000, Side::Me, "me", "Morning."),
            line(2_000, 5_000, Side::Them, &tom.0, "Ship it Friday."),
            Segment {
                speaker: None,
                ..line(5_000, 61_000, Side::Them, "", "Agreed.")
            },
        ];
        (segments, speakers)
    }

    const SUMMARY: &str = r#"{"one_line":"Release sync.","body":"Agreed to ship.","decisions":["Ship Friday"],"actions":[],"chapters":[{"start_ms":0,"title":"Release"},{"start_ms":999999999,"title":"Wrap-up"}]}"#;

    #[tokio::test]
    async fn summarize_empty_transcript_is_empty_without_calling_llm() {
        let llm = FakeLlm::new(|_, _| SUMMARY.into());

        let summary = summarize(&llm, &[], &Speakers::default(), "en", &[], 60_000)
            .await
            .unwrap();

        assert!(llm.calls().is_empty());
        assert_eq!(summary.one_line, "");
        assert_eq!(summary.body, "");
        assert!(summary.decisions.is_empty());
        assert!(summary.actions.is_empty());
        assert!(summary.chapters.is_empty());
    }

    #[tokio::test]
    async fn summarize_single_chunk_parses() {
        let (segments, speakers) = meeting();
        let llm = FakeLlm::new(|_, _| format!("```json\n{SUMMARY}\n```"));

        let summary = summarize(&llm, &segments, &speakers, "en", &[], 60_000)
            .await
            .unwrap();

        assert_eq!(summary.one_line, "Release sync.");
        assert_eq!(summary.decisions, ["Ship Friday"]);
        // A chapter past the end of the meeting is pulled back to it.
        assert_eq!(summary.chapters[1].start_ms, 61_000);
        let calls = llm.calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, summary_prompt("en"));
        assert_eq!(
            calls[0].1,
            "[00:00] Me: Morning.\n[00:02] Tom: Ship it Friday.\n[00:05] Remote: Agreed.\n"
        );
    }

    #[tokio::test]
    async fn summarize_gives_the_glossary_to_the_model() {
        let (segments, speakers) = meeting();
        let llm = FakeLlm::new(|_, _| SUMMARY.into());
        let glossary = ["Arlene".to_string(), "BBINT-220".to_string()];

        summarize(&llm, &segments, &speakers, "zh-TW", &glossary, 60_000)
            .await
            .unwrap();

        let system = &llm.calls()[0].0;
        assert!(system.starts_with(&summary_prompt("zh-TW")));
        assert!(system.ends_with(&glossary::summary_note(&glossary)));
        assert!(system.contains("Arlene, BBINT-220."), "{system}");
    }

    #[tokio::test]
    async fn summarize_chunks_long_transcript_and_merges() {
        let speakers = Speakers::default();
        // "[00:0N] Me: " is 12 characters, so each line is 50 with its
        // newline and two lines fill a 100-character chunk.
        let segments: Vec<Segment> = (0..6)
            .map(|i| {
                let text = format!("{i}{}", "x".repeat(36));
                line(i * 1_000, i * 1_000 + 900, Side::Me, "me", &text)
            })
            .collect();
        let llm = FakeLlm::new(|system, user| {
            if system == merge_prompt("zh-TW") {
                r#"{"one_line":"Whole meeting.","body":"All parts.","chapters":[{"start_ms":3000,"title":"Second half"}]}"#.into()
            } else {
                let first = &user[12..13];
                format!(r#"{{"one_line":"Part {first}.","body":"b"}}"#)
            }
        });

        let summary = summarize(&llm, &segments, &speakers, "zh-TW", &[], 100)
            .await
            .unwrap();

        assert_eq!(summary.one_line, "Whole meeting.");
        assert_eq!(summary.chapters[0].start_ms, 3_000);
        let calls = llm.calls();
        assert_eq!(calls.len(), 4, "3 chunks and 1 merge");
        let chunks: Vec<&str> = calls[..3].iter().map(|(_, user)| user.as_str()).collect();
        for (system, user) in &calls[..3] {
            assert_eq!(system, &summary_prompt("zh-TW"));
            assert_eq!(user.chars().count(), 100, "{user:?}");
        }
        assert!(chunks[0].starts_with("[00:00] Me: 0"));
        assert!(chunks[2].ends_with(&format!("5{}\n", "x".repeat(36))));
        let (system, user) = &calls[3];
        assert_eq!(system, &merge_prompt("zh-TW"));
        let parts = ["Part 0.", "Part 2.", "Part 4."];
        let at: Vec<usize> = parts.iter().map(|p| user.find(p).unwrap()).collect();
        assert!(at.is_sorted(), "parts in meeting order: {user}");
        let merged: Vec<serde_json::Value> = serde_json::from_str(user).unwrap();
        assert_eq!(merged.len(), 3, "the user message is the JSON array alone");
    }

    #[tokio::test]
    async fn summarize_splits_a_line_longer_than_a_chunk() {
        let speakers = Speakers::default();
        let segments = [line(0, 1_000, Side::Me, "me", &"字".repeat(150))];
        let llm = FakeLlm::new(|_, _| SUMMARY.into());

        summarize(&llm, &segments, &speakers, "zh-TW", &[], 100)
            .await
            .unwrap();

        let calls = llm.calls();
        assert_eq!(calls.len(), 3, "2 chunks and 1 merge");
        assert!(calls[..2].iter().all(|(_, u)| u.chars().count() <= 100));
    }

    #[tokio::test]
    async fn summarize_reports_bad_json_as_error_with_raw() {
        let (segments, speakers) = meeting();
        let llm = FakeLlm::new(|_, _| "Sorry, I cannot summarize this meeting.".into());

        let err = summarize(&llm, &segments, &speakers, "en", &[], 60_000)
            .await
            .unwrap_err();

        assert!(
            format!("{err:#}").contains("Sorry, I cannot summarize this meeting."),
            "{err:#}"
        );
    }

    #[tokio::test]
    async fn summarize_names_the_part_whose_summary_is_bad() {
        let speakers = Speakers::default();
        let segments: Vec<Segment> = (0..3)
            .map(|i| line(i * 1_000, i * 1_000 + 900, Side::Me, "me", &"x".repeat(80)))
            .collect();
        let llm = FakeLlm::new(|_, user| {
            if user.starts_with("[00:01]") {
                "not a summary".into()
            } else {
                SUMMARY.into()
            }
        });

        let err = summarize(&llm, &segments, &speakers, "en", &[], 100)
            .await
            .unwrap_err();

        let text = format!("{err:#}");
        assert!(text.starts_with("summary of part 2: "), "{text}");
        assert!(text.contains("not a summary"), "{text}");
    }

    #[test]
    fn merge_prompt_keeps_schema_and_language() {
        let prompt = merge_prompt("zh-TW");
        assert!(prompt.starts_with(&summary_prompt("zh-TW")));
        assert!(prompt.contains("Merge them into one summary"));
    }
}
