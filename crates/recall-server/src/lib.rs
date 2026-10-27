//! Local HTTP server for the web UI: the JSON API under `/api`, server-sent
//! events, meeting audio, and the embedded single-page app for every other
//! path.

mod ask;
mod audio;
mod control;
mod error;
mod events;
mod meetings;
mod people;
mod static_files;

use std::net::{Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, RwLock};

use anyhow::Context;
use axum::Router;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use recall_llm::Llm;
use recall_store::{Config, Event, Index, MeetingId, Speakers, Step};
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;

/// What the recorder is doing right now, as `GET /api/status` reports it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct StatusSnapshot {
    pub phase: String,
    /// Unix milliseconds when `phase` began.
    pub since_ms: u64,
    pub source_app: Option<String>,
    pub mic_peak: f32,
    pub sys_peak: f32,
}

/// A request from the UI to the recorder.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ControlCommand {
    Start,
    Stop,
    Discard,
}

/// Peak levels per 100 ms window of a short test recording.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SelfTestResult {
    pub mic_peaks: Vec<f32>,
    pub sys_peaks: Vec<f32>,
}

pub type ControlFn = dyn Fn(ControlCommand) -> anyhow::Result<()> + Send + Sync;
/// Blocks while it records; handlers call it on a blocking thread.
pub type SelfTestFn = dyn Fn() -> anyhow::Result<SelfTestResult> + Send + Sync;
/// Queues reprocessing of a meeting from a step and returns the job id.
pub type RerunFn = dyn Fn(&MeetingId, Step) -> anyhow::Result<i64> + Send + Sync;

/// Everything the handlers share. The binary wires the recorder and the job
/// queue in through the closures, so this crate does not depend on them.
///
/// Lock order: `speakers` before `index`. Neither lock is held across an
/// `.await`.
#[derive(Clone)]
pub struct AppState {
    pub config: Arc<Config>,
    /// Where `PUT /api/config` reads and writes; the binary sets it from
    /// [`Config::path`].
    pub config_path: PathBuf,
    pub index: Arc<Mutex<Index>>,
    pub speakers: Arc<Mutex<Speakers>>,
    pub events: broadcast::Sender<Event>,
    pub llm: Arc<dyn Llm>,
    pub status: Arc<RwLock<StatusSnapshot>>,
    pub control: Arc<ControlFn>,
    pub selftest: Arc<SelfTestFn>,
    pub rerun: Arc<RerunFn>,
}

pub fn build_router(state: AppState) -> Router {
    let api = Router::new()
        .route("/meetings", get(meetings::list))
        .route(
            "/meetings/{id}",
            get(meetings::get)
                .patch(meetings::patch)
                .delete(meetings::delete),
        )
        .route("/meetings/{id}/rerun/{step}", post(meetings::rerun))
        .route("/meetings/{id}/audio", get(audio::serve))
        .route("/meetings/{id}/ask", post(ask::ask))
        .route("/people", get(people::list))
        .route("/people/{id}", axum::routing::patch(people::patch))
        .route("/status", get(control::status))
        .route("/record/{action}", post(control::record))
        .route("/selftest", post(control::selftest))
        .route("/config", get(control::get_config).put(control::put_config))
        .route("/events", get(events::stream))
        .fallback(error::not_found);
    Router::new()
        .nest("/api", api)
        .fallback(static_files::serve)
        .layer(middleware::from_fn_with_state(
            state.clone(),
            same_origin_only,
        ))
        .with_state(state)
}

/// Rejects with 403 any request whose `Host` is not `127.0.0.1:<port>` or
/// `localhost:<port>`, or that carries an `Origin` other than `http://` plus
/// one of those. Binding to loopback alone is not enough: a page on another
/// site can still send simple POSTs (e.g. `/api/record/discard`) without a
/// preflight, and DNS rebinding lets it read responses under its own host
/// name.
///
/// `<port>` is always `config.port`, which must therefore be the port the
/// listener is bound to; tests bind first and put the bound port in the
/// config.
async fn same_origin_only(State(state): State<AppState>, request: Request, next: Next) -> Response {
    if !is_same_origin(request.headers(), state.config.port) {
        return (
            StatusCode::FORBIDDEN,
            "cross-origin requests are not allowed",
        )
            .into_response();
    }
    next.run(request).await
}

fn is_same_origin(headers: &HeaderMap, port: u16) -> bool {
    let hosts = [format!("127.0.0.1:{port}"), format!("localhost:{port}")];
    let get = |name| headers.get(name).map(|v| v.to_str().unwrap_or_default());
    let host_ok =
        get(header::HOST).is_some_and(|h| hosts.iter().any(|a| a.eq_ignore_ascii_case(h)));
    let origin_ok = get(header::ORIGIN).is_none_or(|o| {
        hosts
            .iter()
            .any(|a| o.eq_ignore_ascii_case(&format!("http://{a}")))
    });
    host_ok && origin_ok
}

/// Serves the app on `127.0.0.1:<config.port>` until the process exits. The
/// server never binds other interfaces: the API has no authentication.
pub async fn serve(state: AppState) -> anyhow::Result<()> {
    let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, state.config.port));
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("binding {addr}"))?;
    tracing::info!("web UI at http://{addr}");
    axum::serve(listener, build_router(state))
        .await
        .context("serving HTTP")
}
