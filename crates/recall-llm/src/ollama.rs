//! Ollama's `/api/chat` endpoint.

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::mpsc;

use crate::Llm;
use crate::http::{check, lines};

/// How long Ollama keeps the model loaded after a request.
const KEEP_ALIVE: &str = "30m";

pub struct Ollama {
    endpoint: String,
    model: String,
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
    pub fn new(endpoint: &str, model: &str) -> Self {
        Self {
            endpoint: endpoint.trim_end_matches('/').to_string(),
            model: model.to_string(),
            client: reqwest::Client::new(),
        }
    }

    /// Chat request body. Thinking is off because recall only wants the
    /// answer, and `complete` asks for JSON since its callers parse it.
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
        });
        if !stream {
            body["format"] = json!("json");
        }
        body
    }

    async fn post(&self, body: &Value) -> Result<reqwest::Response> {
        let url = format!("{}/api/chat", self.endpoint);
        let resp = self
            .client
            .post(&url)
            .json(body)
            .send()
            .await
            .with_context(|| format!("POST {url}"))?;
        check(resp, "Ollama").await
    }
}

#[async_trait]
impl Llm for Ollama {
    async fn complete(&self, system: &str, user: &str) -> Result<String> {
        let resp = self.post(&self.body(system, user, false)).await?;
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
        let resp = self.post(&self.body(system, user, true)).await?;
        let mut lines = lines(resp);
        while let Some(line) = lines.next().await? {
            let chunk: ChatChunk = serde_json::from_str(&line)
                .with_context(|| format!("parsing Ollama stream line {line:?}"))?;
            if let Some(error) = chunk.error {
                bail!("Ollama: {error}");
            }
            if let Some(m) = chunk.message.filter(|m| !m.content.is_empty())
                && tx.send(m.content).await.is_err()
            {
                break;
            }
            if chunk.done {
                break;
            }
        }
        Ok(())
    }

    fn name(&self) -> String {
        format!("ollama/{}", self.model)
    }
}
