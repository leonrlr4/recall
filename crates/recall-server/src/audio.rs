//! `GET /api/meetings/{id}/audio` and `GET /api/meetings/{id}/screen`: the
//! Opus recording and the screen video, with Range support so the player can
//! seek.

use std::path::PathBuf;

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
/// `screen.mp4` is the remuxed screen recording. A failed remux leaves
/// `screen.mkv` instead, which is not served.
const SCREEN_MIME: &str = "video/mp4";

pub async fn serve(
    State(state): State<AppState>,
    Path(id): Path<String>,
    request: Request,
) -> ApiResult<Response> {
    let dir = open_dir(&state.config, &id)?;
    serve_file(
        dir.audio(),
        AUDIO_MIME,
        "the meeting has no audio yet",
        request,
    )
    .await
}

pub async fn serve_screen(
    State(state): State<AppState>,
    Path(id): Path<String>,
    request: Request,
) -> ApiResult<Response> {
    let dir = open_dir(&state.config, &id)?;
    serve_file(
        dir.screen(),
        SCREEN_MIME,
        "the meeting has no screen video",
        request,
    )
    .await
}

async fn serve_file(
    path: PathBuf,
    mime: &str,
    missing: &str,
    request: Request,
) -> ApiResult<Response> {
    if !path.is_file() {
        return Err(ApiError::not_found(missing));
    }
    let mime: Mime = mime.parse().expect("valid mime literal");
    let response = ServeFile::new_with_mime(&path, &mime)
        .oneshot(request)
        .await
        .map_err(|e| anyhow::anyhow!("serving {}: {e}", path.display()))?;
    Ok(response.map(Body::new))
}
