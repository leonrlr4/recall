//! HTTP helpers shared by the backends.

use std::pin::Pin;

use anyhow::{Result, bail};
use futures_util::{Stream, StreamExt};
use reqwest::Response;

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
}

/// Opens `resp`'s body as a [`Lines`] reader.
pub(crate) fn lines(
    resp: Response,
) -> Lines<impl Stream<Item = reqwest::Result<impl AsRef<[u8]>>>> {
    Lines::new(resp.bytes_stream())
}

impl<S, B> Lines<S>
where
    S: Stream<Item = reqwest::Result<B>>,
    B: AsRef<[u8]>,
{
    fn new(body: S) -> Self {
        Self {
            body: Box::pin(body),
            buf: Vec::new(),
            ended: false,
        }
    }

    /// The next non-empty line without its line ending, or `None` once the
    /// body is exhausted. A final line without a newline is still returned.
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
            match self.body.next().await {
                Some(chunk) => self.buf.extend_from_slice(chunk?.as_ref()),
                None => self.ended = true,
            }
        }
    }
}
