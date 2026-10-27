//! The built web app (`web/dist`), embedded in the binary. Paths that are
//! not a file in it get `index.html`, so the app's client-side routes work
//! on reload.

use axum::body::Body;
use axum::http::{HeaderValue, Method, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use rust_embed::RustEmbed;

use crate::error::ApiError;

/// `build.rs` fails the build when `index.html` is missing here.
#[derive(RustEmbed)]
#[folder = "../../web/dist"]
struct Assets;

const INDEX: &str = "index.html";
/// Vite puts content-hashed files here; they never change under one name.
const HASHED_PREFIX: &str = "assets/";

pub async fn serve(method: Method, uri: Uri) -> Response {
    if method != Method::GET && method != Method::HEAD {
        return StatusCode::METHOD_NOT_ALLOWED.into_response();
    }
    let path = uri.path().trim_start_matches('/');
    if path == "api" || path.starts_with("api/") {
        return ApiError::not_found("no such API route").into_response();
    }
    if !path.is_empty()
        && let Some(file) = Assets::get(path)
    {
        let mime = mime_guess::from_path(path).first_or_octet_stream();
        let cache = if path.starts_with(HASHED_PREFIX) {
            "public, max-age=31536000, immutable"
        } else {
            "no-cache"
        };
        return file_response(file.data.into_owned(), mime.as_ref(), cache);
    }
    match Assets::get(INDEX) {
        Some(file) => file_response(
            file.data.into_owned(),
            "text/html; charset=utf-8",
            "no-cache",
        ),
        None => (StatusCode::NOT_FOUND, "web app not built").into_response(),
    }
}

fn file_response(data: Vec<u8>, mime: &str, cache: &'static str) -> Response {
    let mut response = Response::new(Body::from(data));
    let headers = response.headers_mut();
    if let Ok(value) = HeaderValue::from_str(mime) {
        headers.insert(header::CONTENT_TYPE, value);
    }
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static(cache));
    response
}
