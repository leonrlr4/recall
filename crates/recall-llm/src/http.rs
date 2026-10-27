//! HTTP helpers shared by the backends.

use std::pin::Pin;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use futures_util::{Stream, StreamExt};
use reqwest::Response;

/// How long the backends wait on the network.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Timeouts {
    /// Opening the TCP (and TLS) connection.
    pub connect: Duration,
    /// A whole non-streamed reply, from sending the request to the last
    /// byte. A long summary on a slow local model takes minutes.
    pub total: Duration,
    /// A streamed reply: the response headers, and then each piece of the
    /// body, must arrive within this long of the previous one.
    pub idle: Duration,
}

impl Default for Timeouts {
    fn default() -> Self {
        Self {
            connect: Duration::from_secs(10),
            total: Duration::from_secs(15 * 60),
            idle: Duration::from_secs(120),
        }
    }
}

impl Timeouts {
    /// A client that applies the connect timeout. The other two are applied
    /// per request.
    pub(crate) fn client(&self) -> reqwest::Client {
        reqwest::Client::builder()
            .connect_timeout(self.connect)
            .build()
            .expect("building the HTTP client")
    }
}

/// Sends `request` and waits at most `timeout` for the response headers.
pub(crate) async fn send_within(
    request: reqwest::RequestBuilder,
    timeout: Duration,
    url: &str,
) -> Result<Response> {
    match tokio::time::timeout(timeout, request.send()).await {
        Ok(resp) => resp.with_context(|| format!("POST {url}")),
        Err(_) => bail!("POST {url}: no response within {}s", timeout.as_secs()),
    }
}

/// Returns `resp` if its status is a success, else an error carrying the
/// status and the response body.
pub(crate) async fn check(resp: Response, what: &str) -> Result<Response> {
    let status = resp.status();
    if status.is_success() {
        return Ok(resp);
    }
    let body = resp.text().await.unwrap_or_default();
    bail!("{what} returned {status}: {}", body.trim())
}

/// Reads a streamed response body line by line.
pub(crate) struct Lines<S> {
    body: Pin<Box<S>>,
    buf: Vec<u8>,
    ended: bool,
    idle: Duration,
}

/// Opens `resp`'s body as a [`Lines`] reader that fails when no data
/// arrives for `idle`.
pub(crate) fn lines(
    resp: Response,
    idle: Duration,
) -> Lines<impl Stream<Item = reqwest::Result<impl AsRef<[u8]>>>> {
    Lines::new(resp.bytes_stream(), idle)
}

impl<S, B> Lines<S>
where
    S: Stream<Item = reqwest::Result<B>>,
    B: AsRef<[u8]>,
{
    fn new(body: S, idle: Duration) -> Self {
        Self {
            body: Box::pin(body),
            buf: Vec::new(),
            ended: false,
            idle,
        }
    }

    /// The next non-empty line without its line ending, or `None` once the
    /// body is exhausted. A final line without a newline is still returned.
    /// Fails when the body sends nothing for the idle timeout.
    pub(crate) async fn next(&mut self) -> Result<Option<String>> {
        loop {
            if let Some(pos) = self.buf.iter().position(|&b| b == b'\n') {
                let line: Vec<u8> = self.buf.drain(..=pos).collect();
                let line = String::from_utf8_lossy(&line);
                let line = line.trim_end_matches(['\n', '\r']);
                if !line.is_empty() {
                    return Ok(Some(line.to_string()));
                }
                continue;
            }
            if self.ended {
                let rest = String::from_utf8_lossy(&std::mem::take(&mut self.buf)).into_owned();
                let rest = rest.trim_end_matches('\r');
                return Ok((!rest.is_empty()).then(|| rest.to_string()));
            }
            let next = tokio::time::timeout(self.idle, self.body.next())
                .await
                .map_err(|_| {
                    anyhow::anyhow!("the stream sent nothing for {}s", self.idle.as_secs())
                })?;
            match next {
                Some(chunk) => self.buf.extend_from_slice(chunk?.as_ref()),
                None => self.ended = true,
            }
        }
    }
}
