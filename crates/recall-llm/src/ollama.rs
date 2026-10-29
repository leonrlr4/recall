//! Ollama's `/api/chat` endpoint.

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::mpsc;

use crate::Llm;
use crate::http::{Timeouts, check, lines, send_within};

/// How long Ollama keeps the model loaded after a request: long enough to
/// span the calls of one meeting's summary, short enough that a 22 GB model
/// on a laptop's shared memory is freed soon after. 30m held it through
/// the next half hour of the user's work.
const KEEP_ALIVE: &str = "2m";

pub struct Ollama {
    endpoint: String,
    model: String,
    context_tokens: u32,
    timeouts: Timeouts,
    client: reqwest::Client,
}

#[derive(Deserialize)]
struct ChatChunk {
    #[serde(default)]
    message: Option<ChatMessage>,
    #[serde(default)]
    done: bool,
    #[serde(default)]
    error: Option<String>,
}

#[derive(Deserialize)]
struct ChatMessage {
    #[serde(default)]
    content: String,
}

impl Ollama {
    /// `endpoint` is the server's base URL, e.g. `http://127.0.0.1:11434`.
    /// `context_tokens` is sent as `num_ctx` with every request.
    pub fn new(endpoint: &str, model: &str, context_tokens: u32) -> Self {
        let timeouts = Timeouts::default();
        Self {
            endpoint: endpoint.trim_end_matches('/').to_string(),
            model: model.to_string(),
            context_tokens,
            timeouts,
            client: timeouts.client(),
        }
    }

    /// Replaces the default [`Timeouts`]; tests use short ones.
    pub fn with_timeouts(mut self, timeouts: Timeouts) -> Self {
        self.timeouts = timeouts;
        self.client = timeouts.client();
        self
    }

    /// Chat request body. Thinking is off because recall only wants the
    /// answer, and `complete` asks for JSON since its callers parse it.
    /// `num_ctx` is set because Ollama's default context (2048 to 8192
    /// tokens, depending on the version) silently drops the start of a
    /// longer prompt.
    fn body(&self, system: &str, user: &str, stream: bool) -> Value {
        let mut body = json!({
            "model": self.model,
            "messages": [
                {"role": "system", "content": system},
                {"role": "user", "content": user},
            ],
            "stream": stream,
            "think": false,
            "keep_alive": KEEP_ALIVE,
            "options": {"num_ctx": self.context_tokens},
        });
        if !stream {
            body["format"] = json!("json");
        }
        body
    }

    fn url(&self) -> String {
        format!("{}/api/chat", self.endpoint)
    }
}

#[async_trait]
impl Llm for Ollama {
    async fn complete(&self, system: &str, user: &str) -> Result<String> {
        let url = self.url();
        let request = self
            .client
            .post(&url)
            .timeout(self.timeouts.total)
            .json(&self.body(system, user, false));
        let resp = check(
            send_within(request, self.timeouts.total, &url).await?,
            "Ollama",
        )
        .await?;
        let chunk: ChatChunk = resp.json().await.context("reading Ollama reply")?;
        if let Some(error) = chunk.error {
            bail!("Ollama: {error}");
        }
        chunk
            .message
            .map(|m| m.content)
            .context("Ollama reply has no message")
    }

    async fn stream(&self, system: &str, user: &str, tx: mpsc::Sender<String>) -> Result<()> {
        let url = self.url();
        let request = self.client.post(&url).json(&self.body(system, user, true));
        let resp = check(
            send_within(request, self.timeouts.idle, &url).await?,
            "Ollama",
        )
        .await?;
        let mut lines = lines(resp, self.timeouts.idle);
        while let Some(line) = lines.next().await? {
            let chunk: ChatChunk = serde_json::from_str(&line)
                .with_context(|| format!("parsing Ollama stream line {line:?}"))?;
            if let Some(error) = chunk.error {
                bail!("Ollama: {error}");
            }
            if let Some(m) = chunk.message.filter(|m| !m.content.is_empty())
                && tx.send(m.content).await.is_err()
            {
                return Ok(());
            }
            if chunk.done {
                return Ok(());
            }
        }
        bail!("the Ollama stream ended before its last message (done: true)")
    }

    fn name(&self) -> String {
        format!("ollama/{}", self.model)
    }
}
