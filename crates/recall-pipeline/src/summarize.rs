//! Meeting summaries through an [`Llm`].

use std::collections::HashMap;

use anyhow::{Context, Result};
use chrono::{Datelike, Days, NaiveDate, Weekday};
use recall_llm::{Llm, meeting_transcript, parse_summary, summary_prompt};
use recall_store::{ActionItem, Segment, Speakers, Summary};

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

/// The part of the system prompt that gives the model the meeting's date,
/// so it can write due dates such as "next Wednesday" as calendar dates.
pub fn date_note(meeting_date: NaiveDate) -> String {
    format!(
        "\nThe meeting took place on {}. Write each action's due as YYYY-MM-DD when the \
         transcript gives a date that can be worked out from this one (\"next Wednesday\", \
         \"by the end of the month\"); otherwise write the deadline as stated, in the language of the summary, or null.\n",
        meeting_date.format("%A %Y-%m-%d")
    )
}

/// Summarizes the transcript with `llm`, in `lang`. `glossary` holds the
/// right spellings of names and terms, which the model is asked to use for
/// misheard ones; see [`glossary::summary_note`]. `meeting_date` is the
/// local date the meeting started on: the model gets it (see
/// [`date_note`]) and relative due dates the model left as words are
/// resolved against it with [`normalize_due`].
///
/// A transcript of at most `max_chars` characters goes to the model in one
/// call. A longer one is cut at line boundaries into chunks of at most
/// `max_chars` (a single longer line is cut mid-line), each chunk is
/// summarized on its own, and one more call merges those summaries. Chapter
/// starts and action sources past the end of the last segment are moved
/// back to it.
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
    meeting_date: NaiveDate,
    max_chars: usize,
) -> Result<Summary> {
    if segments.is_empty() {
        return Ok(Summary::default());
    }
    let text = meeting_transcript(segments, speakers);
    let system =
        summary_prompt(lang) + &glossary::summary_note(glossary) + &date_note(meeting_date);
    let mut summary = if text.chars().count() <= max_chars {
        parse_summary(&llm.complete(&system, &text).await?)?
    } else {
        let mut parts = Vec::new();
        for (n, chunk) in chunks(&text, max_chars).iter().enumerate() {
            let raw = llm.complete(&system, chunk).await?;
            let mut part =
                parse_summary(&raw).with_context(|| format!("summary of part {}", n + 1))?;
            // Resolved before merging, so the merge sees dates, not words
            // relative to a date it was not given.
            normalize_dues(&mut part, meeting_date);
            parts.push(part);
        }
        let user = serde_json::to_string(&parts)?;
        parse_summary(&llm.complete(&merge_prompt(lang), &user).await?)?
    };
    let end_ms = segments.iter().map(|s| s.end_ms).max().unwrap_or(0);
    for chapter in &mut summary.chapters {
        chapter.start_ms = chapter.start_ms.min(end_ms);
    }
    for action in &mut summary.actions {
        action.source_ms = action.source_ms.map(|ms| ms.min(end_ms));
    }
    normalize_dues(&mut summary, meeting_date);
    Ok(summary)
}

fn normalize_dues(summary: &mut Summary, meeting_date: NaiveDate) {
    for action in &mut summary.actions {
        action.due = action
            .due
            .take()
            .map(|due| normalize_due(&due, meeting_date))
            .filter(|due| !due.is_empty());
    }
}

/// `due` as `YYYY-MM-DD` when it is a date, or a day relative to
/// `meeting_date` this recognizes; otherwise `due` trimmed, unchanged.
///
/// The model is asked for ISO dates already, but small local models often
/// copy the wording or get the arithmetic wrong, so the common cases are
/// worked out here too: full dates in `-`, `/`, `.` or 年月日 form; a month
/// and day, in the meeting's year unless that is more than a month before
/// the meeting; today, tomorrow and 後天; and weekdays. `下週三`, `next
/// Wednesday` mean that day in the week (Monday to Sunday) after the
/// meeting's; `這週三`, `this Wednesday` that day in the meeting's week; a
/// bare `週三`, `Wednesday` the first one after the meeting day. Leading
/// "by", "before" and trailing 前, 之前 are ignored.
pub fn normalize_due(due: &str, meeting_date: NaiveDate) -> String {
    let original = due.trim();
    let mut text = original.to_lowercase();
    for prefix in ["by ", "before ", "due ", "on ", "在", "於"] {
        if let Some(rest) = text.strip_prefix(prefix) {
            text = rest.trim_start().to_string();
        }
    }
    for suffix in ["之前", "以前", "前"] {
        if let Some(rest) = text.strip_suffix(suffix) {
            text = rest.trim_end().to_string();
        }
    }
    let resolved = numeric_date(&text, meeting_date)
        .or_else(|| relative_day(&text, meeting_date))
        .or_else(|| weekday_date(&text, meeting_date));
    match resolved {
        Some(date) => date.format("%Y-%m-%d").to_string(),
        None => original.to_string(),
    }
}

/// `2026-11-03`, `2026/11/3`, `2026年11月3日`, or `11/3`, `11月3日`.
fn numeric_date(text: &str, meeting_date: NaiveDate) -> Option<NaiveDate> {
    if !text
        .chars()
        .all(|c| c.is_ascii_digit() || "-/.年月日號 ".contains(c))
    {
        return None;
    }
    let numbers: Vec<&str> = text
        .split(|c: char| !c.is_ascii_digit())
        .filter(|n| !n.is_empty())
        .collect();
    match numbers[..] {
        [y, m, d] if y.len() == 4 => {
            NaiveDate::from_ymd_opt(y.parse().ok()?, m.parse().ok()?, d.parse().ok()?)
        }
        [m, d] => {
            let (m, d) = (m.parse().ok()?, d.parse().ok()?);
            let date = NaiveDate::from_ymd_opt(meeting_date.year(), m, d)?;
            if date < meeting_date - Days::new(31) {
                NaiveDate::from_ymd_opt(meeting_date.year() + 1, m, d)
            } else {
                Some(date)
            }
        }
        _ => None,
    }
}

fn relative_day(text: &str, meeting_date: NaiveDate) -> Option<NaiveDate> {
    let days = match text {
        "today" | "今天" | "今日" => 0,
        "tomorrow" | "明天" | "明日" => 1,
        "後天" => 2,
        _ => return None,
    };
    meeting_date.checked_add_days(Days::new(days))
}

fn weekday_date(text: &str, meeting_date: NaiveDate) -> Option<NaiveDate> {
    enum Week {
        This,
        Next,
        Coming,
    }
    let (week, rest) = if let Some(rest) = ["下個", "下"]
        .iter()
        .find_map(|p| text.strip_prefix(p))
        .or_else(|| text.strip_prefix("next "))
    {
        (Week::Next, rest)
    } else if let Some(rest) = ["這個", "這", "本"]
        .iter()
        .find_map(|p| text.strip_prefix(p))
        .or_else(|| text.strip_prefix("this "))
    {
        (Week::This, rest)
    } else {
        (Week::Coming, text)
    };
    let weekday = weekday(rest.trim())?;
    let monday = meeting_date.week(Weekday::Mon).first_day();
    let offset = u64::from(weekday.num_days_from_monday());
    match week {
        Week::This => monday.checked_add_days(Days::new(offset)),
        Week::Next => monday.checked_add_days(Days::new(7 + offset)),
        Week::Coming => {
            let ahead = (weekday.num_days_from_monday() + 7
                - meeting_date.weekday().num_days_from_monday()
                - 1)
                % 7
                + 1;
            meeting_date.checked_add_days(Days::new(u64::from(ahead)))
        }
    }
}

/// `週三`, `星期三`, `禮拜三`, `wednesday`, `wed`.
fn weekday(text: &str) -> Option<Weekday> {
    let english = [
        ("monday", "mon", Weekday::Mon),
        ("tuesday", "tue", Weekday::Tue),
        ("wednesday", "wed", Weekday::Wed),
        ("thursday", "thu", Weekday::Thu),
        ("friday", "fri", Weekday::Fri),
        ("saturday", "sat", Weekday::Sat),
        ("sunday", "sun", Weekday::Sun),
    ];
    if let Some((_, _, day)) = english.iter().find(|(l, s, _)| text == *l || text == *s) {
        return Some(*day);
    }
    let rest = ["星期", "禮拜", "礼拜", "週", "周"]
        .iter()
        .find_map(|p| text.strip_prefix(p))?;
    match rest {
        "一" => Some(Weekday::Mon),
        "二" => Some(Weekday::Tue),
        "三" => Some(Weekday::Wed),
        "四" => Some(Weekday::Thu),
        "五" => Some(Weekday::Fri),
        "六" => Some(Weekday::Sat),
        "日" | "天" => Some(Weekday::Sun),
        _ => None,
    }
}

/// Lowest similarity at which a new action item counts as a rewording of an
/// old one. A reworded Chinese or English task scores about 0.8 (`寄 TRL6
/// 報告給 Amelia` against `把 TRL6 報告寄給 Amelia` is 0.81); different
/// tasks from one meeting rarely pass 0.4.
const SAME_TASK: f64 = 0.6;

/// Carries the user's done marks from `previous`, the action items before
/// a rerun of the summary, onto the new `summary`.
///
/// The model rewords tasks from run to run and may reorder, merge or drop
/// them, so neither position nor exact text identifies an item. Each done
/// item is matched to the most similar new item (character-bigram Dice
/// coefficient on the text, which works for Chinese and English alike, see
/// [`similarity`]), best pairs first and each new item at most once, when
/// the score reaches [`SAME_TASK`]. A done item with no match is appended
/// as it was: the user said it happened, and losing that record is worse
/// than a done duplicate of a task the model now words very differently.
/// Open items need nothing carried, since open is the default.
pub fn carry_done(previous: &[ActionItem], summary: &mut Summary) {
    let done: Vec<&ActionItem> = previous.iter().filter(|a| a.done).collect();
    let mut pairs = Vec::new();
    for (i, old) in done.iter().enumerate() {
        for (j, new) in summary.actions.iter().enumerate() {
            let score = similarity(&old.text, &new.text);
            if score >= SAME_TASK {
                pairs.push((score, i, j));
            }
        }
    }
    pairs.sort_by(|a, b| b.0.total_cmp(&a.0));
    let mut old_used = vec![false; done.len()];
    let mut new_used = vec![false; summary.actions.len()];
    for (_, i, j) in pairs {
        if old_used[i] || new_used[j] {
            continue;
        }
        old_used[i] = true;
        new_used[j] = true;
        summary.actions[j].done = true;
        summary.actions[j].done_at = done[i].done_at;
    }
    for (old, used) in done.into_iter().zip(old_used) {
        if !used {
            summary.actions.push(old.clone());
        }
    }
}

/// Dice coefficient of the character bigrams of `a` and `b`, ignoring case,
/// spaces and punctuation: 1 for the same text, 0 for nothing in common.
fn similarity(a: &str, b: &str) -> f64 {
    fn grams(s: &str) -> HashMap<(char, char), usize> {
        let chars: Vec<char> = s
            .chars()
            .filter(|c| c.is_alphanumeric())
            .flat_map(char::to_lowercase)
            .collect();
        let mut out = HashMap::new();
        if chars.len() == 1 {
            out.insert((chars[0], chars[0]), 1);
        }
        for pair in chars.windows(2) {
            *out.entry((pair[0], pair[1])).or_insert(0) += 1;
        }
        out
    }
    let (a, b) = (grams(a), grams(b));
    let total: usize = a.values().sum::<usize>() + b.values().sum::<usize>();
    if total == 0 {
        return 0.0;
    }
    let common: usize = a
        .iter()
        .map(|(g, n)| (*n).min(b.get(g).copied().unwrap_or(0)))
        .sum();
    2.0 * common as f64 / total as f64
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

    /// Wednesday.
    fn date() -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 10, 28).unwrap()
    }

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

        let summary = summarize(&llm, &[], &Speakers::default(), "en", &[], date(), 60_000)
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

        let summary = summarize(&llm, &segments, &speakers, "en", &[], date(), 60_000)
            .await
            .unwrap();

        assert_eq!(summary.one_line, "Release sync.");
        assert_eq!(summary.decisions, ["Ship Friday"]);
        // A chapter past the end of the meeting is pulled back to it.
        assert_eq!(summary.chapters[1].start_ms, 61_000);
        let calls = llm.calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, summary_prompt("en") + &date_note(date()));
        assert_eq!(
            calls[0].1,
            "[00:00] Me: Morning.\n[00:02] Tom: Ship it Friday.\n[00:05] Remote: Agreed.\n"
        );
    }

    #[tokio::test]
    async fn summarize_gives_the_glossary_to_the_model() {
        let (segments, speakers) = meeting();
        let llm = FakeLlm::new(|_, _| SUMMARY.into());
        let glossary = ["Amelia".to_string(), "PROJ-220".to_string()];

        summarize(
            &llm,
            &segments,
            &speakers,
            "zh-TW",
            &glossary,
            date(),
            60_000,
        )
        .await
        .unwrap();

        let system = &llm.calls()[0].0;
        assert!(system.starts_with(&summary_prompt("zh-TW")));
        assert!(system.contains(&glossary::summary_note(&glossary)));
        assert!(system.contains("Amelia, PROJ-220."), "{system}");
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

        let summary = summarize(&llm, &segments, &speakers, "zh-TW", &[], date(), 100)
            .await
            .unwrap();

        assert_eq!(summary.one_line, "Whole meeting.");
        assert_eq!(summary.chapters[0].start_ms, 3_000);
        let calls = llm.calls();
        assert_eq!(calls.len(), 4, "3 chunks and 1 merge");
        let chunks: Vec<&str> = calls[..3].iter().map(|(_, user)| user.as_str()).collect();
        for (system, user) in &calls[..3] {
            assert_eq!(system, &(summary_prompt("zh-TW") + &date_note(date())));
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

        summarize(&llm, &segments, &speakers, "zh-TW", &[], date(), 100)
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

        let err = summarize(&llm, &segments, &speakers, "en", &[], date(), 60_000)
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

        let err = summarize(&llm, &segments, &speakers, "en", &[], date(), 100)
            .await
            .unwrap_err();

        let text = format!("{err:#}");
        assert!(text.starts_with("summary of part 2: "), "{text}");
        assert!(text.contains("not a summary"), "{text}");
    }

    #[tokio::test]
    async fn summarize_resolves_due_dates_and_clamps_sources() {
        let (segments, speakers) = meeting();
        let llm = FakeLlm::new(|_, _| {
            r#"{"one_line":"a","body":"b","actions":[
                {"text":"Send the report","due":"下週三前","priority":"high","source_ms":2000},
                {"text":"Plan Q1","due":"next sprint","source_ms":99999999},
                {"text":"Tidy","due":"  "}
            ]}"#
            .into()
        });

        let summary = summarize(&llm, &segments, &speakers, "zh-TW", &[], date(), 60_000)
            .await
            .unwrap();

        assert!(llm.calls()[0].0.contains("Wednesday 2026-10-28"));
        let actions = &summary.actions;
        assert_eq!(actions[0].due.as_deref(), Some("2026-11-04"));
        assert_eq!(actions[1].due.as_deref(), Some("next sprint"));
        assert_eq!(actions[1].source_ms, Some(61_000));
        assert_eq!(actions[2].due, None);
    }

    #[test]
    fn normalize_due_resolves_dates_relative_to_the_meeting() {
        // 2026-10-28 is a Wednesday; its week runs 10-26 to 11-01.
        let cases = [
            ("2026-11-03", "2026-11-03"),
            ("2026/11/3", "2026-11-03"),
            ("2026年11月3日", "2026-11-03"),
            ("11/3", "2026-11-03"),
            ("11月3日前", "2026-11-03"),
            ("1/5", "2027-01-05"),
            ("10/20", "2026-10-20"),
            ("明天", "2026-10-29"),
            ("today", "2026-10-28"),
            ("後天", "2026-10-30"),
            ("下週三", "2026-11-04"),
            ("下禮拜一", "2026-11-02"),
            ("下星期日", "2026-11-08"),
            ("next Wednesday", "2026-11-04"),
            ("這週五", "2026-10-30"),
            ("本週一", "2026-10-26"),
            ("this friday", "2026-10-30"),
            ("週五之前", "2026-10-30"),
            ("by Friday", "2026-10-30"),
            ("週三", "2026-11-04"),
            ("Mon", "2026-11-02"),
            ("  next sprint ", "next sprint"),
            ("下週", "下週"),
            ("月底", "月底"),
            ("2026-02-30", "2026-02-30"),
            ("13/40", "13/40"),
        ];
        for (due, want) in cases {
            assert_eq!(normalize_due(due, date()), want, "{due}");
        }
    }

    fn item(text: &str) -> ActionItem {
        ActionItem {
            text: text.into(),
            ..ActionItem::default()
        }
    }

    fn done(text: &str, day: u32) -> ActionItem {
        use chrono::TimeZone;
        ActionItem {
            done: true,
            done_at: Some(
                chrono::Utc
                    .with_ymd_and_hms(2026, 10, day, 9, 0, 0)
                    .unwrap(),
            ),
            ..item(text)
        }
    }

    #[test]
    fn carry_done_matches_reworded_items() {
        let previous = [
            done("寄 TRL6 報告給 Amelia", 29),
            item("Book the meeting room"),
            done("Update the pricing page", 30),
        ];
        let mut summary = Summary {
            actions: vec![
                item("Update pricing page copy"),
                item("把 TRL6 報告寄給 Amelia"),
                item("Book the meeting room"),
                item("Write the onboarding guide"),
            ],
            ..Summary::default()
        };

        carry_done(&previous, &mut summary);

        let marks: Vec<(bool, Option<u32>)> = summary
            .actions
            .iter()
            .map(|a| (a.done, a.done_at.map(|t| t.day())))
            .collect();
        assert_eq!(
            marks,
            [
                (true, Some(30)),
                (true, Some(29)),
                (false, None),
                (false, None)
            ]
        );
    }

    #[test]
    fn carry_done_keeps_unmatched_done_items_and_matches_each_once() {
        let previous = [done("Send invoice", 27), done("Send invoices", 28)];
        let mut summary = Summary {
            actions: vec![item("Send invoices"), item("Plan the offsite")],
            ..Summary::default()
        };

        carry_done(&previous, &mut summary);

        let texts: Vec<(&str, bool)> = summary
            .actions
            .iter()
            .map(|a| (a.text.as_str(), a.done))
            .collect();
        assert_eq!(
            texts,
            [
                ("Send invoices", true),
                ("Plan the offsite", false),
                ("Send invoice", true)
            ]
        );
        // The exact match wins the one new item.
        assert_eq!(summary.actions[0].done_at.unwrap().day(), 28);
    }

    #[test]
    fn similarity_ignores_case_and_punctuation() {
        assert_eq!(similarity("Ship it!", "ship it"), 1.0);
        assert!(similarity("寄 TRL6 報告給 Amelia", "把 TRL6 報告寄給 Amelia") > 0.8);
        assert!(similarity("Book the room", "Fix the export timeout") < SAME_TASK);
        assert_eq!(similarity("", "x"), 0.0);
    }

    #[test]
    fn merge_prompt_keeps_schema_and_language() {
        let prompt = merge_prompt("zh-TW");
        assert!(prompt.starts_with(&summary_prompt("zh-TW")));
        assert!(prompt.contains("Merge them into one summary"));
    }
}
