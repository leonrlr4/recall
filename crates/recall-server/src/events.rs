//! `GET /api/events`: the daemon's status events as server-sent events, one
//! JSON `Event` per message.

use std::convert::Infallible;

use axum::extract::State;
use axum::response::sse::{self, KeepAlive, Sse};
use futures_util::Stream;
use tokio::sync::broadcast::error::RecvError;

use crate::AppState;

pub async fn stream(
    State(state): State<AppState>,
) -> Sse<impl Stream<Item = Result<sse::Event, Infallible>>> {
    // Subscribe before returning, so events sent once the client has the
    // response headers are not missed.
    let mut rx = state.events.subscribe();
    let stream = async_stream::stream! {
        loop {
            match rx.recv().await {
                Ok(event) => match sse::Event::default().json_data(&event) {
                    Ok(e) => yield Ok(e),
                    Err(e) => tracing::warn!("cannot encode event: {e}"),
                },
                // A slow client misses events; the UI refetches on the next one.
                Err(RecvError::Lagged(n)) => tracing::debug!("events client lagged by {n}"),
                Err(RecvError::Closed) => break,
            }
        }
    };
    Sse::new(stream).keep_alive(KeepAlive::default())
}
