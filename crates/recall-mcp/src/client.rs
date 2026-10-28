//! The daemon's HTTP API, with its failures turned into the text a tool
//! returns as its error.

use std::time::Duration;

use reqwest::{Method, RequestBuilder, StatusCode, Url};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;

/// The tool error when nothing listens on the daemon's port.
pub const DAEMON_DOWN: &str =
    "recall daemon is not running; start it with: systemctl --user start recall";
/// The tool error for the daemon's 409: the meeting has a pending or running
/// job, which would overwrite the edit.
pub const BUSY: &str = "meeting is being processed; try again when it is done";

/// Generous for a loopback call; the slowest handler rewrites every meeting
/// a speaker appears in.
const TIMEOUT: Duration = Duration::from_secs(60);

pub type ToolResult<T> = Result<T, String>;

#[derive(Clone)]
pub struct Daemon {
    http: reqwest::Client,
    base: Url,
}

impl Daemon {
    pub fn new(base: Url) -> anyhow::Result<Self> {
        if base.cannot_be_a_base() {
            anyhow::bail!("{base} cannot be a base URL");
        }
        let http = reqwest::Client::builder()
            // A proxy from the environment must not see loopback calls.
            .no_proxy()
            .timeout(TIMEOUT)
            .build()?;
        Ok(Self { http, base })
    }

    /// `base` plus `segments`, each percent-encoded, so meeting ids with
    /// spaces and colons stay one segment.
    fn url(&self, segments: &[&str]) -> Url {
        let mut url = self.base.clone();
        url.path_segments_mut()
            .expect("checked in new")
            .pop_if_empty()
            .extend(segments);
        url
    }

    fn request(&self, method: Method, segments: &[&str]) -> RequestBuilder {
        self.http.request(method, self.url(segments))
    }

    pub async fn get<T: DeserializeOwned>(
        &self,
        segments: &[&str],
        query: &[(&str, String)],
    ) -> ToolResult<T> {
        json(send(self.request(Method::GET, segments).query(query)).await?).await
    }

    /// POSTs without a body and returns the response body, `Value::Null`
    /// when it is empty.
    pub async fn post(&self, segments: &[&str]) -> ToolResult<Value> {
        let response = send(self.request(Method::POST, segments)).await?;
        if response.status() == StatusCode::NO_CONTENT {
            return Ok(Value::Null);
        }
        json(response).await
    }

    pub async fn patch<T: DeserializeOwned>(
        &self,
        segments: &[&str],
        body: &impl Serialize,
    ) -> ToolResult<T> {
        json(send(self.request(Method::PATCH, segments).json(body)).await?).await
    }
}

/// Sends the request; a refused connection, a 409 and any other non-2xx
/// status become the tool's error text.
async fn send(request: RequestBuilder) -> ToolResult<reqwest::Response> {
    let response = request.send().await.map_err(|e| {
        if e.is_connect() {
            DAEMON_DOWN.to_string()
        } else {
            format!("calling the recall daemon: {e}")
        }
    })?;
    let status = response.status();
    if status == StatusCode::CONFLICT {
        return Err(BUSY.to_string());
    }
    if !status.is_success() {
        // Handlers send `{ "error": msg }`; anything else is shown as is.
        let body = response.text().await.unwrap_or_default();
        let message = serde_json::from_str::<Value>(&body)
            .ok()
            .and_then(|v| v.get("error")?.as_str().map(str::to_string))
            .unwrap_or(body);
        return Err(format!("recall daemon returned {status}: {message}"));
    }
    Ok(response)
}

async fn json<T: DeserializeOwned>(response: reqwest::Response) -> ToolResult<T> {
    response
        .json()
        .await
        .map_err(|e| format!("reading the recall daemon's reply: {e}"))
}
