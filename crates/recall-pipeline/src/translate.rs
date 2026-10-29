//! Translations of a meeting summary, for reading in the web UI.

use anyhow::{Error, Result, anyhow};
use recall_llm::{Llm, parse_summary_text, translate_prompt};
use recall_store::{Summary, SummaryText};

/// Translates `summary`'s texts from `from` into `to` with `llm`. The model
/// gets only those texts as JSON: not the transcript, and not the owners,
/// dues or times, which the translation shares with the summary. A reply
/// that does not parse, or whose lists do not line up with the summary's
/// (see [`SummaryText::fits`]), is an error.
pub async fn translate(
    llm: &dyn Llm,
    summary: &Summary,
    from: &str,
    to: &str,
) -> Result<SummaryText> {
    let user = serde_json::to_string(&summary.text())?;
    let text = parse_summary_text(&llm.complete(&translate_prompt(from, to), &user).await?)?;
    text.fits(summary)
        .map_err(|why| anyhow!("the translation has {why}"))?;
    Ok(text)
}

/// Replaces `summary.translations` with one translation into each of
/// `langs`, skipping `from` and repeats. A language whose translation fails
/// is left out and returned with the error, for the caller to log: a
/// translation is a convenience and never fails the summary. An empty
/// summary (a silent recording) gets none and calls no model.
pub async fn translate_all(
    llm: &dyn Llm,
    summary: &mut Summary,
    from: &str,
    langs: &[String],
) -> Vec<(String, Error)> {
    summary.translations.clear();
    let mut failed = Vec::new();
    if summary.text() == SummaryText::default() {
        return failed;
    }
    for (i, to) in langs.iter().enumerate() {
        if to == from || langs[..i].contains(to) {
            continue;
        }
        match translate(llm, summary, from, to).await {
            Ok(text) => {
                summary.translations.insert(to.clone(), text);
            }
            Err(err) => failed.push((to.clone(), err)),
        }
    }
    failed
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use async_trait::async_trait;
    use recall_store::{ActionItem, Chapter};
    use tokio::sync::mpsc;

    use super::*;

    /// Replies with each of its replies in turn and records the user
    /// messages.
    struct Scripted {
        replies: Mutex<Vec<Result<String, String>>>,
        sent: Mutex<Vec<String>>,
    }

    impl Scripted {
        fn new(replies: Vec<Result<&str, &str>>) -> Self {
            let replies = replies
                .into_iter()
                .rev()
                .map(|r| r.map(Into::into).map_err(Into::into))
                .collect();
            Self {
                replies: Mutex::new(replies),
                sent: Mutex::default(),
            }
        }
    }

    #[async_trait]
    impl Llm for Scripted {
        async fn complete(&self, _: &str, user: &str) -> Result<String> {
            self.sent.lock().unwrap().push(user.into());
            let reply = self.replies.lock().unwrap().pop().expect("a reply left");
            reply.map_err(|e| anyhow!(e))
        }

        async fn stream(&self, _: &str, _: &str, _: mpsc::Sender<String>) -> Result<()> {
            unreachable!()
        }

        fn name(&self) -> String {
            "scripted".into()
        }
    }

    fn summary() -> Summary {
        Summary {
            one_line: "Sync.".into(),
            body: "Talked.".into(),
            decisions: vec!["Ship BB-412".into()],
            actions: vec![ActionItem {
                text: "Write the notes".into(),
                owner: Some("Tom".into()),
                done: true,
                ..ActionItem::default()
            }],
            chapters: vec![Chapter {
                start_ms: 0,
                title: "Intro".into(),
            }],
            ..Summary::default()
        }
    }

    const ZH: &str = r#"{"one_line":"同步。","body":"討論。","decisions":["發布 BB-412"],"actions":["寫筆記"],"chapters":["開場"]}"#;
    const ZH_SHORT: &str = r#"{"one_line":"同步。","body":"討論。","decisions":["發布 BB-412"],"actions":[],"chapters":["開場"]}"#;

    #[tokio::test]
    async fn sends_only_the_texts_and_keeps_aligned_replies() {
        let llm = Scripted::new(vec![Ok(ZH)]);
        let mut s = summary();
        let failed = translate_all(&llm, &mut s, "en", &["zh-TW".into(), "en".into()]).await;
        assert!(failed.is_empty());
        assert_eq!(s.translations["zh-TW"].actions, ["寫筆記"]);
        // Done marks and owners stay on the summary's items.
        assert!(s.actions[0].done);
        assert_eq!(s.actions[0].text, "Write the notes");
        let sent = llm.sent.lock().unwrap();
        assert_eq!(sent.len(), 1);
        assert!(
            !sent[0].contains("Tom") && !sent[0].contains("done"),
            "{}",
            sent[0]
        );
    }

    #[tokio::test]
    async fn drops_a_translation_whose_lists_do_not_line_up_or_that_fails() {
        let llm = Scripted::new(vec![Ok(ZH_SHORT), Err("connection refused"), Ok(ZH)]);
        let mut s = summary();
        let langs = ["zh-TW".into(), "ja".into(), "ko".into()];
        let failed = translate_all(&llm, &mut s, "en", &langs).await;
        let reasons: Vec<String> = failed
            .iter()
            .map(|(lang, err)| format!("{lang}: {err:#}"))
            .collect();
        assert_eq!(
            reasons,
            [
                "zh-TW: the translation has 0 actions where the summary has 1",
                "ja: connection refused",
            ]
        );
        assert_eq!(s.translations.keys().collect::<Vec<_>>(), ["ko"]);
    }

    #[tokio::test]
    async fn empty_summary_is_not_translated() {
        let llm = Scripted::new(Vec::new());
        let mut s = Summary::default();
        s.translations
            .insert("zh-TW".into(), SummaryText::default());
        assert!(
            translate_all(&llm, &mut s, "en", &["zh-TW".into()])
                .await
                .is_empty()
        );
        assert!(s.translations.is_empty());
    }
}
