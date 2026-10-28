//! `POST /api/meet/events`: batches of Google Meet events from the browser
//! extension. The only route another origin may call; see
//! [`crate::same_origin_only`].

use axum::body::Bytes;
use axum::extract::State;
use axum::http::StatusCode;
use recall_store::MeetBatch;

use crate::AppState;
use crate::error::{ApiError, ApiResult};

/// Hands the batch to the recorder's sink. The body is parsed by hand, so
/// any body that is not a [`MeetBatch`] is a 400 whatever its content type.
pub async fn events(State(state): State<AppState>, body: Bytes) -> ApiResult<StatusCode> {
    let batch: MeetBatch = serde_json::from_slice(&body)
        .map_err(|e| ApiError::bad_request(format!("not a Meet event batch: {e}")))?;
    let received = batch.events.len();
    let stored = (state.meet)(batch)?;
    tracing::debug!(received, stored, "Meet events");
    Ok(StatusCode::NO_CONTENT)
}
