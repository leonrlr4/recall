//! Local HTTP server for the web UI: the JSON API under `/api`, server-sent
//! events, meeting audio and screen video, and the embedded single-page app
//! for every other path.

mod actions;
mod ask;
mod audio;
mod control;
mod error;
mod events;
mod meet;
mod meetings;
mod people;
mod qa;
mod static_files;

use std::net::{Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, RwLock};

use anyhow::Context;
use axum::Router;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use recall_llm::Llm;
use recall_store::{Config, Event, Index, MeetBatch, MeetingId, Speakers, Step};
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;

pub use qa::QaState;

/// What the recorder is doing right now, as `GET /api/status` reports it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct StatusSnapshot {
    pub phase: String,
    /// Unix milliseconds when `phase` began.
    pub since_ms: u64,
    pub source_app: Option<String>,
    pub mic_peak: f32,
    pub sys_peak: f32,
    /// The latest `status` the Meet extension sent during this recording
    /// (e.g. `"signal_lost"`); `None` outside a recording.
    #[serde(default)]
    pub meet: Option<String>,
    /// The screen recording's state: `"recording"`, `"off"` or `"error"`
    /// during a recording; `"picker"` outside one when screen recording is
    /// on and the next one will show the portal's picker.
    #[serde(default)]
    pub screen: Option<String>,
}

/// A request from the UI to the recorder.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ControlCommand {
    Start,
    Stop,
    Discard,
    /// Stop the live recording's screen recorder and delete its video.
    ScreenOff,
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
/// Stores a batch of Meet events on the live recording's timeline and
/// returns how many it stored.
pub type MeetSinkFn = dyn Fn(MeetBatch) -> anyhow::Result<usize> + Send + Sync;

/// How soon the vault sync should look at a meeting after a change.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SyncWhen {
    /// The switch was flipped or the meeting deleted: right away.
    Now,
    /// An edit: after a quiet spell, so a burst of edits is one commit.
    Debounced,
}

/// Where a meeting stands with the shared vault, as `GET
/// /api/meetings/{id}` reports it under `vault`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct VaultStatus {
    /// The switch value asked for while a job was processing the meeting;
    /// it is written to `meeting.json` when the job ends.
    pub requested: Option<bool>,
    /// A change is waiting to be pushed or is being pushed.
    pub queued: bool,
    /// The note's path in the vault as last pushed; `None` when it is not
    /// in the vault.
    pub path: Option<String>,
    pub synced_at: Option<chrono::DateTime<chrono::Utc>>,
    /// Why the last attempt failed; cleared by the next success. A failed
    /// change is retried with backoff.
    pub error: Option<String>,
}

/// The daemon's vault sync, as the handlers see it. Every method returns
/// at once; the git work runs on the sync's own thread.
pub trait Vault: Send + Sync {
    /// Schedules a look at the meeting: publish, update, move or remove its
    /// note to match `meeting.json`.
    fn touch(&self, id: &MeetingId, when: SyncWhen);
    /// The meeting's directory was renamed from `from` to `to`. Call it
    /// while still holding the `speakers` lock taken for the rename, so the
    /// sync never sees the new directory under the old state.
    fn renamed(&self, from: &MeetingId, to: &MeetingId);
    /// Records the switch value for a meeting a job is processing: the job
    /// saves its own copy of `meeting.json` and would undo a direct write.
    /// `None` clears it.
    fn request(&self, id: &MeetingId, sync: Option<bool>);
    fn status(&self, id: &MeetingId) -> VaultStatus;
}

/// The Meet extension's origin. Its id is pinned by the `key` in its
/// manifest, so it is the same on every machine.
pub const EXTENSION_ORIGIN: &str = "chrome-extension://oiejafpeoacoblcpomkoloaibfmdfnhd";
/// The one route [`EXTENSION_ORIGIN`] may call.
const MEET_EVENTS_PATH: &str = "/api/meet/events";

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
    pub meet: Arc<MeetSinkFn>,
    /// `None` when `vault.remote` is empty: publishing is off.
    pub vault: Option<Arc<dyn Vault>>,
    /// Questions across meetings (`POST /api/ask`).
    pub qa: QaState,
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
        .route("/meetings/{id}/screen", get(audio::serve_screen))
        .route("/meetings/{id}/ask", post(ask::ask))
        .route("/ask", post(qa::ask))
        .route("/people", get(people::list))
        .route("/people/{id}", axum::routing::patch(people::patch))
        .route("/status", get(control::status))
        .route("/record/{action}", post(control::record))
        .route("/selftest", post(control::selftest))
        .route("/meet/events", post(meet::events))
        .route("/action-items", get(actions::list))
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
/// The one exception is [`EXTENSION_ORIGIN`] on `POST /api/meet/events`
/// (Host still loopback). Its response carries
/// `Access-Control-Allow-Origin`, and its CORS preflight, which Brave sends
/// when the user has withheld the extension's site access, is answered
/// here with 204.
///
/// `<port>` is always `config.port`, which must therefore be the port the
/// listener is bound to; tests bind first and put the bound port in the
/// config.
async fn same_origin_only(State(state): State<AppState>, request: Request, next: Next) -> Response {
    let port = state.config.port;
    if is_extension_request(&request, port) {
        let origin = HeaderValue::from_static(EXTENSION_ORIGIN);
        if request.method() == Method::OPTIONS {
            return (
                StatusCode::NO_CONTENT,
                [
                    (header::ACCESS_CONTROL_ALLOW_ORIGIN, origin),
                    (
                        header::ACCESS_CONTROL_ALLOW_METHODS,
                        HeaderValue::from_static("POST"),
                    ),
                    (
                        header::ACCESS_CONTROL_ALLOW_HEADERS,
                        HeaderValue::from_static("Content-Type"),
                    ),
                ],
            )
                .into_response();
        }
        let mut response = next.run(request).await;
        response
            .headers_mut()
            .insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, origin);
        return response;
    }
    if !is_same_origin(request.headers(), port) {
        return (
            StatusCode::FORBIDDEN,
            "cross-origin requests are not allowed",
        )
            .into_response();
    }
    next.run(request).await
}

fn loopback_hosts(port: u16) -> [String; 2] {
    [format!("127.0.0.1:{port}"), format!("localhost:{port}")]
}

fn header_str(headers: &HeaderMap, name: header::HeaderName) -> Option<&str> {
    headers.get(name).map(|v| v.to_str().unwrap_or_default())
}

fn is_loopback_host(headers: &HeaderMap, hosts: &[String]) -> bool {
    header_str(headers, header::HOST)
        .is_some_and(|h| hosts.iter().any(|a| a.eq_ignore_ascii_case(h)))
}

fn is_same_origin(headers: &HeaderMap, port: u16) -> bool {
    let hosts = loopback_hosts(port);
    let origin_ok = header_str(headers, header::ORIGIN).is_none_or(|o| {
        hosts
            .iter()
            .any(|a| o.eq_ignore_ascii_case(&format!("http://{a}")))
    });
    is_loopback_host(headers, &hosts) && origin_ok
}

/// `POST` (or its preflight `OPTIONS`) to `/api/meet/events` from exactly
/// [`EXTENSION_ORIGIN`], addressed to a loopback Host.
fn is_extension_request(request: &Request, port: u16) -> bool {
    let headers = request.headers();
    (request.method() == Method::POST || request.method() == Method::OPTIONS)
        && request.uri().path() == MEET_EVENTS_PATH
        && header_str(headers, header::ORIGIN) == Some(EXTENSION_ORIGIN)
        && is_loopback_host(headers, &loopback_hosts(port))
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
