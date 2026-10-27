//! Anthropic's Messages API.

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::mpsc;

use crate::Llm;
use crate::http::{check, lines};

const BASE_URL: &str = "https://api.anthropic.com";
const API_VERSION: &str = "2023-06-01";
const MAX_TOKENS: u32 = 4096;

pub struct Claude {
    api_key: String,
    model: String,
    base_url: String,
    client: reqwest::Client,
}

#[derive(Deserialize)]
struct MessageReply {
    content: Vec<ContentBlock>,
}

#[derive(Deserialize)]
struct ContentBlock {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    text: String,
}

#[derive(Deserialize)]
struct StreamEvent {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    delta: Option<Delta>,
    #[serde(default)]
    error: Option<Value>,
}

#[derive(Deserialize)]
struct Delta {
    #[serde(default)]
    text: Option<String>,
}

impl Claude {
    /// Fails when `api_key` is missing or empty.
    pub fn new(api_key: Option<String>, model: &str) -> Result<Self> {
        let api_key = api_key
            .filter(|k| !k.is_empty())
            .context("the claude backend needs ANTHROPIC_API_KEY to be set")?;
        Ok(Self {
            api_key,
            model: model.to_string(),
            base_url: BASE_URL.to_string(),
            client: reqwest::Client::new(),
        })
    }

    /// Sends requests to `base_url` instead of `https://api.anthropic.com`.
    pub fn with_base_url(mut self, base_url: &str) -> Self {
        self.base_url = base_url.trim_end_matches('/').to_string();
        self
    }

    async fn post(&self, system: &str, user: &str, stream: bool) -> Result<reqwest::Response> {
        let url = format!("{}/v1/messages", self.base_url);
        let body = json!({
            "model": self.model,
            "max_tokens": MAX_TOKENS,
            "system": system,
            "messages": [{"role": "user", "content": user}],
            "stream": stream,
        });
        let resp = self
            .client
            .post(&url)
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", API_VERSION)
            .json(&body)
            .send()
            .await
            .with_context(|| format!("POST {url}"))?;
        check(resp, "Claude API").await
    }
}

#[async_trait]
impl Llm for Claude {
    async fn complete(&self, system: &str, user: &str) -> Result<String> {
        let resp = self.post(system, user, false).await?;
        let reply: MessageReply = resp.json().await.context("reading Claude reply")?;
        Ok(reply
            .content
            .into_iter()
            .filter(|block| block.kind == "text")
            .map(|block| block.text)
            .collect())
    }

    async fn stream(&self, system: &str, user: &str, tx: mpsc::Sender<String>) -> Result<()> {
        let resp = self.post(system, user, true).await?;
        let mut lines = lines(resp);
        while let Some(line) = lines.next().await? {
            let Some(data) = line.strip_prefix("data:") else {
                continue;
            };
            let event: StreamEvent = serde_json::from_str(data.trim())
                .with_context(|| format!("parsing Claude stream event {data:?}"))?;
            match event.kind.as_str() {
                "content_block_delta" => {
                    if let Some(text) = event.delta.and_then(|d| d.text)
                        && tx.send(text).await.is_err()
                    {
                        break;
                    }
                }
                "message_stop" => break,
                "error" => bail!(
                    "Claude API stream error: {}",
                    event.error.unwrap_or_default()
                ),
                _ => {}
            }
        }
        Ok(())
    }

    fn name(&self) -> String {
        format!("claude/{}", self.model)
    }
}
