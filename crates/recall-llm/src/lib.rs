//! Language model clients behind one trait, plus the prompts recall sends
//! them.

mod claude;
mod claude_code;
mod http;
mod ollama;
mod prompts;

use std::sync::Arc;

use anyhow::{Result, bail};
use async_trait::async_trait;
use recall_store::LlmConfig;
use tokio::sync::mpsc;

pub use claude::Claude;
pub use claude_code::{AgentEvent, CLAUDE_CODE_BUDGET_CHARS, ClaudeCode, McpAccess};
pub use http::Timeouts;
pub use ollama::Ollama;
pub use prompts::{
    UNKNOWN_REMOTE, ask_prompt, meeting_transcript, parse_summary, summary_prompt, transcript_text,
};

/// Model used for the Claude backend when the config names none of its own.
pub const DEFAULT_CLAUDE_MODEL: &str = "claude-sonnet-5-5";

#[async_trait]
pub trait Llm: Send + Sync {
    /// Sends one system and one user message and returns the whole reply.
    async fn complete(&self, system: &str, user: &str) -> Result<String>;

    /// Like [`Llm::complete`], but sends the reply to `tx` piece by piece as
    /// it arrives. Stops early without error when the receiver is dropped.
    async fn stream(&self, system: &str, user: &str, tx: mpsc::Sender<String>) -> Result<()>;

    /// `"backend/model"`, e.g. `"ollama/qwen3.6:35b-a3b"`.
    fn name(&self) -> String;
}

/// Builds the backend `cfg.backend` names. The Claude backend reads its key
/// from `ANTHROPIC_API_KEY` and fails when it is unset.
pub fn from_config(cfg: &LlmConfig) -> Result<Arc<dyn Llm>> {
    match cfg.backend.as_str() {
        "ollama" => Ok(Arc::new(Ollama::new(
            &cfg.endpoint,
            &cfg.model,
            cfg.context_tokens,
        ))),
        "claude" => {
            let key = std::env::var("ANTHROPIC_API_KEY").ok();
            Ok(Arc::new(Claude::new(key, &claude_model(cfg))?))
        }
        other => bail!("unknown LLM backend {other:?}; expected \"ollama\" or \"claude\""),
    }
}

/// Like [`from_config`], but never fails: a backend that cannot be built
/// (the Claude backend without `ANTHROPIC_API_KEY`, an unknown backend) is
/// replaced by one whose every call fails with the reason. Recording and
/// transcription do not need the model, so a bad LLM config must only fail
/// the steps that call it.
pub fn from_config_or_unavailable(cfg: &LlmConfig) -> Arc<dyn Llm> {
    match from_config(cfg) {
        Ok(llm) => llm,
        Err(err) => Arc::new(Unavailable {
            reason: format!("{err:#}"),
            backend: cfg.backend.clone(),
        }),
    }
}

/// An LLM backend that could not be built.
struct Unavailable {
    reason: String,
    backend: String,
}

#[async_trait]
impl Llm for Unavailable {
    async fn complete(&self, _: &str, _: &str) -> Result<String> {
        bail!("the LLM is unavailable: {}", self.reason)
    }

    async fn stream(&self, _: &str, _: &str, _: mpsc::Sender<String>) -> Result<()> {
        bail!("the LLM is unavailable: {}", self.reason)
    }

    fn name(&self) -> String {
        format!("{}/unavailable", self.backend)
    }
}

/// The config's model, unless it is empty or an Ollama model left over from
/// the default config (they start with `qwen`).
fn claude_model(cfg: &LlmConfig) -> String {
    if cfg.model.is_empty() || cfg.model.starts_with("qwen") {
        DEFAULT_CLAUDE_MODEL.into()
    } else {
        cfg.model.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn unbuildable_backend_fails_each_call_with_the_reason() {
        let cfg = LlmConfig {
            backend: "nonsense".into(),
            ..LlmConfig::default()
        };
        let llm = from_config_or_unavailable(&cfg);
        let err = llm.complete("s", "u").await.unwrap_err();
        assert!(
            format!("{err:#}").contains("unknown LLM backend"),
            "{err:#}"
        );
        let (tx, _rx) = mpsc::channel(1);
        assert!(llm.stream("s", "u", tx).await.is_err());
        assert_eq!(llm.name(), "nonsense/unavailable");

        let ok = from_config_or_unavailable(&LlmConfig::default());
        assert_eq!(ok.name(), "ollama/qwen3.6:35b-a3b");
    }

    #[test]
    fn claude_model_replaces_empty_and_ollama_default() {
        let mut cfg = LlmConfig {
            backend: "claude".into(),
            ..LlmConfig::default()
        };
        assert_eq!(claude_model(&cfg), DEFAULT_CLAUDE_MODEL);
        cfg.model = String::new();
        assert_eq!(claude_model(&cfg), DEFAULT_CLAUDE_MODEL);
        cfg.model = "claude-opus-5-5".into();
        assert_eq!(claude_model(&cfg), "claude-opus-5-5");
    }
}
