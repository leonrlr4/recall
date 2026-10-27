//! `POST /api/meetings/{id}/ask`: a question about one meeting, answered by
//! the LLM and streamed back as server-sent events.

use std::convert::Infallible;

use axum::Json;
use axum::extract::{Path, State};
use axum::response::sse::{Event, Sse};
use futures_util::Stream;
use serde::Deserialize;
use tokio::sync::mpsc;

use crate::AppState;
use crate::error::{ApiError, ApiResult};
use crate::meetings::{load, open_dir};

/// Tokens buffered between the LLM task and the response.
const TOKEN_BUFFER: usize = 64;
/// The `error` event sent instead of asking the model when the transcript
/// and question do not fit the configured context. The web UI matches this
/// text to show it translated.
pub const TOO_LONG: &str =
    "this meeting is too long for the current context; raise llm.context_tokens";

#[derive(Deserialize)]
pub struct AskBody {
    question: String,
}

/// Streams the answer as `data: <token>` events. The stream ends with
/// `event: done` (data `end`), preceded by `event: error` (data: the message)
/// if the model failed. A transcript plus question longer than
/// [`recall_store::LlmConfig::transcript_budget_chars`] gets the [`TOO_LONG`]
/// error without calling the model, since the model would see only part of
/// it.
pub async fn ask(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<AskBody>,
) -> ApiResult<Sse<impl Stream<Item = Result<Event, Infallible>>>> {
    let question = body.question.trim().to_string();
    if question.is_empty() {
        return Err(ApiError::bad_request("empty question"));
    }
    let dir = open_dir(&state.config, &id)?;
    let meeting = load(&dir)?;
    let transcript = {
        let speakers = state.speakers.lock().expect("speakers lock poisoned");
        recall_llm::meeting_transcript(&meeting.segments, &speakers)
    };
    let fits = transcript.chars().count() + question.chars().count()
        <= state.config.llm.transcript_budget_chars();
    let system = recall_llm::ask_prompt(&state.config.summary_lang);
    let user = format!("Transcript:\n{transcript}\nQuestion: {question}");

    let (tx, mut rx) = mpsc::channel::<String>(TOKEN_BUFFER);
    let llm = state.llm.clone();
    let task = tokio::spawn(async move {
        if !fits {
            anyhow::bail!(TOO_LONG);
        }
        llm.stream(&system, &user, tx).await
    });

    let stream = async_stream::stream! {
        while let Some(token) = rx.recv().await {
            if !token.is_empty() {
                yield Ok(Event::default().data(token));
            }
        }
        let failure = match task.await {
            Ok(Ok(())) => None,
            Ok(Err(e)) => Some(format!("{e:#}")),
            Err(e) => Some(format!("the model task stopped: {e}")),
        };
        if let Some(message) = failure {
            tracing::warn!("ask failed: {message}");
            yield Ok(Event::default().event("error").data(message));
        }
        yield Ok(Event::default().event("done").data("end"));
    };
    Ok(Sse::new(stream))
}
