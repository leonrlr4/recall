//! `GET /api/meetings/{id}/audio`: the Opus recording, with Range support so
//! the player can seek.

use axum::body::Body;
use axum::extract::{Path, Request, State};
use axum::response::Response;
use mime_guess::mime::Mime;
use tower::ServiceExt;
use tower_http::services::ServeFile;

use crate::AppState;
use crate::error::{ApiError, ApiResult};
use crate::meetings::open_dir;

/// `audio.opus` is Opus in an Ogg container.
const AUDIO_MIME: &str = "audio/ogg";

pub async fn serve(
    State(state): State<AppState>,
    Path(id): Path<String>,
    request: Request,
) -> ApiResult<Response> {
    let dir = open_dir(&state.config, &id)?;
    let path = dir.audio();
    if !path.is_file() {
        return Err(ApiError::not_found("the meeting has no audio yet"));
    }
    let mime: Mime = AUDIO_MIME.parse().expect("valid mime literal");
    let response = ServeFile::new_with_mime(path, &mime)
        .oneshot(request)
        .await
        .map_err(|e| anyhow::anyhow!("serving audio: {e}"))?;
    Ok(response.map(Body::new))
}
