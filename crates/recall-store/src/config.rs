//! User configuration from `config.toml`.
//!
//! Every field has a default, so a partial file or no file at all is valid.

use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub meetings_dir: PathBuf,
    pub data_dir: PathBuf,
    pub record_screen: bool,
    pub allowlist: Vec<String>,
    pub debounce_secs: u64,
    pub end_grace_secs: u64,
    pub llm: LlmConfig,
    pub summary_lang: String,
    /// UI language, if set.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ui_lang: Option<String>,
    pub stt: SttConfig,
    pub port: u16,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct LlmConfig {
    /// `"ollama"` or `"claude"`.
    pub backend: String,
    pub model: String,
    pub endpoint: String,
    /// The model's context window in tokens. Ollama gets it as `num_ctx`
    /// (its own default is far smaller and silently drops the start of a
    /// long prompt), and it sizes the transcript sent in one call; see
    /// [`LlmConfig::transcript_budget_chars`].
    pub context_tokens: u32,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SttConfig {
    /// `"breeze"`.
    pub engine: String,
    pub threads: usize,
}

/// Environment variable naming the config file, overriding [`Config::path`].
const CONFIG_ENV: &str = "RECALL_CONFIG";
/// Environment variable giving the default `data_dir`.
const HOME_ENV: &str = "RECALL_HOME";

impl Default for Config {
    fn default() -> Self {
        Self {
            meetings_dir: home().join("Meetings"),
            data_dir: env_path(HOME_ENV).unwrap_or_else(|| home().join(".local/share/recall")),
            record_screen: false,
            allowlist: [
                "brave", "chromium", "chrome", "firefox", "zoom", "slack", "discord", "teams",
            ]
            .map(String::from)
            .to_vec(),
            debounce_secs: 8,
            end_grace_secs: 30,
            llm: LlmConfig::default(),
            summary_lang: "zh-TW".into(),
            ui_lang: None,
            stt: SttConfig::default(),
            port: 7474,
        }
    }
}

impl Default for LlmConfig {
    fn default() -> Self {
        Self {
            backend: "ollama".into(),
            model: "qwen3.6:35b-a3b".into(),
            endpoint: "http://127.0.0.1:11434".into(),
            context_tokens: 32_768,
        }
    }
}

impl LlmConfig {
    /// The most transcript characters sent to the model in one call:
    /// `context_tokens * 6 / 10`. Mixed Chinese and English transcripts run
    /// at roughly one token per character or less, and the rest of the
    /// window holds the prompt and the reply.
    pub fn transcript_budget_chars(&self) -> usize {
        self.context_tokens as usize * 6 / 10
    }
}

impl Default for SttConfig {
    fn default() -> Self {
        Self {
            engine: "breeze".into(),
            threads: 8,
        }
    }
}

impl Config {
    /// Loads the config from [`Config::path`].
    pub fn load() -> Result<Self> {
        Self::load_from(&Self::path())
    }

    /// Parses the TOML file at `path`. A missing file gives the defaults, and
    /// keys absent from the file keep their default values.
    pub fn load_from(path: &Path) -> Result<Self> {
        let text = match fs::read_to_string(path) {
            Ok(text) => text,
            Err(e) if e.kind() == ErrorKind::NotFound => return Ok(Self::default()),
            Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
        };
        toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))
    }

    /// `$RECALL_CONFIG` if set, else `~/.config/recall/config.toml`.
    pub fn path() -> PathBuf {
        env_path(CONFIG_ENV).unwrap_or_else(|| home().join(".config/recall/config.toml"))
    }

    pub fn speakers_path(&self) -> PathBuf {
        self.data_dir.join("speakers.json")
    }

    pub fn index_path(&self) -> PathBuf {
        self.data_dir.join("index.sqlite")
    }

    pub fn models_dir(&self) -> PathBuf {
        self.data_dir.join("models")
    }
}

/// The home directory, or an empty path (so paths resolve against the
/// working directory) when it cannot be determined.
fn home() -> PathBuf {
    dirs::home_dir().unwrap_or_default()
}

/// The value of `var` as a path, treating unset and empty alike.
fn env_path(var: &str) -> Option<PathBuf> {
    std::env::var_os(var)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_defaults_when_file_missing() {
        let dir = tempfile::tempdir().unwrap();
        let config = Config::load_from(&dir.path().join("absent.toml")).unwrap();
        assert_eq!(config, Config::default());
        assert!(config.meetings_dir.ends_with("Meetings"));
        assert!(!config.record_screen);
        assert_eq!(
            config.allowlist,
            [
                "brave", "chromium", "chrome", "firefox", "zoom", "slack", "discord", "teams"
            ]
        );
        assert_eq!((config.debounce_secs, config.end_grace_secs), (8, 30));
        assert_eq!(config.llm.backend, "ollama");
        assert_eq!(config.llm.model, "qwen3.6:35b-a3b");
        assert_eq!(config.llm.endpoint, "http://127.0.0.1:11434");
        assert_eq!(config.llm.context_tokens, 32_768);
        assert_eq!(config.llm.transcript_budget_chars(), 19_660);
        assert_eq!(config.summary_lang, "zh-TW");
        assert_eq!(config.ui_lang, None);
        assert_eq!(config.stt.engine, "breeze");
        assert_eq!(config.stt.threads, 8);
        assert_eq!(config.port, 7474);
    }

    #[test]
    fn config_parses_partial_toml() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "[llm]\nbackend = \"claude\"\n").unwrap();
        let config = Config::load_from(&path).unwrap();
        let mut expected = Config::default();
        expected.llm.backend = "claude".into();
        assert_eq!(config, expected);
    }

    #[test]
    fn config_rejects_invalid_toml() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "port = \"not a number\"\n").unwrap();
        assert!(Config::load_from(&path).is_err());
    }

    #[test]
    fn config_round_trips_through_toml() {
        let config = Config {
            ui_lang: Some("en".into()),
            data_dir: PathBuf::from("/data"),
            ..Config::default()
        };
        let text = toml::to_string(&config).unwrap();
        assert_eq!(toml::from_str::<Config>(&text).unwrap(), config);
    }

    #[test]
    fn data_paths_live_under_data_dir() {
        let config = Config {
            data_dir: PathBuf::from("/data"),
            ..Config::default()
        };
        assert_eq!(config.speakers_path(), PathBuf::from("/data/speakers.json"));
        assert_eq!(config.index_path(), PathBuf::from("/data/index.sqlite"));
        assert_eq!(config.models_dir(), PathBuf::from("/data/models"));
    }
}
