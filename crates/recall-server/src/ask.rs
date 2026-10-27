//! `POST /api/meetings/{id}/ask`: a question about one meeting, answered by
//! the LLM and streamed back as server-sent events.

use std::convert::Infallible;

use axum::Json;
use axum::extract::{Path, State};
use axum::response::sse::{Event, Sse};
use futures_util::Stream;
use recall_store::Side;
use serde::Deserialize;
use tokio::sync::mpsc;

use crate::AppState;
use crate::error::{ApiError, ApiResult};
use crate::meetings::{load, open_dir};

/// Tokens buffered between the LLM task and the response.
const TOKEN_BUFFER: usize = 64;
/// Name for a remote segment without a speaker; matches the index.
const THEM: &str = "Them";

#[derive(Deserialize)]
pub struct AskBody {
    question: String,
}

/// Streams the answer as `data: <token>` events. The stream ends with
/// `event: done` (data `end`), preceded by `event: error` (data: the message)
/// if the model failed.
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
    let lines: Vec<(String, u32, String)> = {
        let speakers = state.speakers.lock().expect("speakers lock poisoned");
        meeting
            .segments
            .iter()
            .map(|s| {
                let name = match (&s.speaker, s.side) {
                    (Some(id), _) => speakers.name_of(id),
                    (None, Side::Me) => speakers.me().name.clone(),
                    (None, Side::Them) => THEM.to_string(),
                };
                (name, s.start_ms, s.text.clone())
            })
            .collect()
    };
    let system = recall_llm::ask_prompt(&state.config.summary_lang);
    let user = format!(
        "Transcript:\n{}\nQuestion: {question}",
        recall_llm::transcript_text(&lines)
    );

    let (tx, mut rx) = mpsc::channel::<String>(TOKEN_BUFFER);
    let llm = state.llm.clone();
    let task = tokio::spawn(async move { llm.stream(&system, &user, tx).await });

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
