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
    pub glossary: GlossaryConfig,
    pub claude_code: ClaudeCodeConfig,
    pub vault: VaultConfig,
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

/// Claude Code in print mode (`claude -p`) as the summarizer of meetings
/// marked for sync. It runs in `cwd`, so it reads the CLAUDE.md files and
/// Claude Code memory there; other meetings keep the `llm` backend.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ClaudeCodeConfig {
    pub enabled: bool,
    /// The `claude` executable, looked up on `PATH` unless it is a path.
    pub command: String,
    /// Working directory of the run. A leading `~` is the home directory;
    /// see [`ClaudeCodeConfig::work_dir`].
    pub cwd: PathBuf,
    /// Passed as `--model`; unset, Claude Code picks its own default.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// The whole run, including any files it reads, is killed after this.
    pub timeout_secs: u64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SttConfig {
    /// `"breeze"`.
    pub engine: String,
    pub threads: usize,
}

/// Names and terms that transcription and summaries should spell right.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct GlossaryConfig {
    /// Terms typed by the user, always included, before any other.
    pub terms: Vec<String>,
    /// Markdown files, or directories searched for them, to take more terms
    /// from. A leading `~` is the home directory; see
    /// [`GlossaryConfig::source_paths`].
    pub sources: Vec<PathBuf>,
    /// Most terms in the glossary; 0 turns the glossary off. Recognition
    /// prompts help most with a short list of the terms that matter (10 to
    /// 30 in published tests), so the default is 30.
    pub max_terms: usize,
}

/// Where chosen meetings are published: a git repository holding an Obsidian
/// vault that teammates read.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct VaultConfig {
    /// The vault's git remote, e.g. `https://github.com/org/vault`. Empty
    /// turns publishing off and hides the switch in the UI.
    pub remote: String,
    /// recall's own clone of the vault. Empty means `<data_dir>/vault`; a
    /// leading `~` is the home directory. See [`Config::vault_dir`].
    pub dir: PathBuf,
    /// Folder inside the vault that holds the published notes. recall
    /// writes nothing outside it.
    pub folder: String,
    /// Branch to commit to and push.
    pub branch: String,
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
            glossary: GlossaryConfig::default(),
            claude_code: ClaudeCodeConfig::default(),
            vault: VaultConfig::default(),
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

impl Default for ClaudeCodeConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            command: "claude".into(),
            cwd: home(),
            model: None,
            timeout_secs: 600,
        }
    }
}

impl ClaudeCodeConfig {
    /// `cwd` with a leading `~` replaced by the home directory.
    pub fn work_dir(&self) -> PathBuf {
        match self.cwd.strip_prefix("~") {
            Ok(rest) => home().join(rest),
            Err(_) => self.cwd.clone(),
        }
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

impl Default for VaultConfig {
    fn default() -> Self {
        Self {
            remote: String::new(),
            dir: PathBuf::new(),
            folder: "meetings".into(),
            branch: "main".into(),
        }
    }
}

impl VaultConfig {
    /// Publishing is on when a remote is set.
    pub fn enabled(&self) -> bool {
        !self.remote.trim().is_empty()
    }

    /// The folder without leading or trailing slashes.
    pub fn folder(&self) -> &str {
        self.folder.trim_matches('/')
    }

    /// Checks what the vault sync relies on: `folder` is a relative path
    /// with no empty, `.` or `..` parts (so writes stay inside it), `branch`
    /// names a branch, and `dir` is empty, absolute or starts with `~/`.
    /// The error names the key.
    pub fn validate(&self) -> std::result::Result<(), String> {
        let folder = self.folder();
        if folder.is_empty()
            || self.folder.starts_with('/')
            || folder.contains('\\')
            || folder
                .split('/')
                .any(|part| part.is_empty() || part == "." || part == "..")
        {
            return Err(format!(
                "vault.folder must be a relative folder such as \"meetings\", got {:?}",
                self.folder
            ));
        }
        if self.branch.trim().is_empty() || self.branch.starts_with('-') {
            return Err(format!(
                "vault.branch must name a branch, got {:?}",
                self.branch
            ));
        }
        if !self.dir.as_os_str().is_empty() && !self.dir.is_absolute() && !self.dir.starts_with("~")
        {
            return Err(format!(
                "vault.dir must be an absolute path or start with ~/, got {}",
                self.dir.display()
            ));
        }
        Ok(())
    }
}

impl Default for GlossaryConfig {
    fn default() -> Self {
        Self {
            terms: Vec::new(),
            sources: Vec::new(),
            max_terms: 30,
        }
    }
}

impl GlossaryConfig {
    /// `sources` with a leading `~` replaced by the home directory.
    pub fn source_paths(&self) -> Vec<PathBuf> {
        self.sources
            .iter()
            .map(|p| match p.strip_prefix("~") {
                Ok(rest) => home().join(rest),
                Err(_) => p.clone(),
            })
            .collect()
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

    /// The vault clone: `vault.dir` with `~` expanded, or `<data_dir>/vault`
    /// when it is empty.
    pub fn vault_dir(&self) -> PathBuf {
        let dir = &self.vault.dir;
        if dir.as_os_str().is_empty() {
            return self.data_dir.join("vault");
        }
        match dir.strip_prefix("~") {
            Ok(rest) => home().join(rest),
            Err(_) => dir.clone(),
        }
    }

    /// What the vault sync last published for each meeting.
    pub fn vault_state_path(&self) -> PathBuf {
        self.data_dir.join("vault.json")
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
        assert!(config.glossary.terms.is_empty());
        assert!(config.glossary.sources.is_empty());
        assert_eq!(config.glossary.max_terms, 30);
        assert!(!config.claude_code.enabled);
        assert_eq!(config.claude_code.command, "claude");
        assert_eq!(config.claude_code.model, None);
        assert_eq!(config.claude_code.timeout_secs, 600);
    }

    #[test]
    fn claude_code_section_parses_and_expands_tilde() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            "[claude_code]\nenabled = true\ncwd = \"~/Work/Biobuild\"\nmodel = \"opus\"\n",
        )
        .unwrap();
        let cc = Config::load_from(&path).unwrap().claude_code;
        assert!(cc.enabled);
        assert_eq!(cc.command, "claude", "unset keys keep their defaults");
        assert_eq!(cc.model.as_deref(), Some("opus"));
        assert_eq!(cc.work_dir(), home().join("Work/Biobuild"));

        let absolute = ClaudeCodeConfig {
            cwd: PathBuf::from("/srv/work"),
            ..ClaudeCodeConfig::default()
        };
        assert_eq!(absolute.work_dir(), PathBuf::from("/srv/work"));
        // Only a whole leading component is the home directory.
        let tilde_name = ClaudeCodeConfig {
            cwd: PathBuf::from("~bob/x"),
            ..ClaudeCodeConfig::default()
        };
        assert_eq!(tilde_name.work_dir(), PathBuf::from("~bob/x"));
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
        assert_eq!(config.vault_dir(), PathBuf::from("/data/vault"));
        assert_eq!(config.vault_state_path(), PathBuf::from("/data/vault.json"));
    }

    #[test]
    fn vault_is_off_by_default_and_dir_expands_tilde() {
        let mut config = Config::default();
        assert!(!config.vault.enabled());
        assert_eq!(config.vault.folder, "meetings");
        assert_eq!(config.vault.branch, "main");
        config.vault.dir = PathBuf::from("~/vault");
        assert_eq!(config.vault_dir(), home().join("vault"));
        config.vault.dir = PathBuf::from("/srv/vault");
        assert_eq!(config.vault_dir(), PathBuf::from("/srv/vault"));
    }

    #[test]
    fn vault_validate_keeps_writes_inside_the_folder() {
        let ok = |folder: &str| {
            VaultConfig {
                folder: folder.into(),
                ..VaultConfig::default()
            }
            .validate()
            .is_ok()
        };
        assert!(ok("meetings"));
        assert!(ok("notes/meetings/"));
        for bad in ["", "/", "/abs", "../up", "a/../b", "a//b", "./a", "a\\b"] {
            assert!(!ok(bad), "{bad:?}");
        }
        let relative_dir = VaultConfig {
            dir: PathBuf::from("vault"),
            ..VaultConfig::default()
        };
        assert!(relative_dir.validate().unwrap_err().contains("vault.dir"));
        let no_branch = VaultConfig {
            branch: " ".into(),
            ..VaultConfig::default()
        };
        assert!(no_branch.validate().unwrap_err().contains("vault.branch"));
    }

    #[test]
    fn glossary_parses_and_expands_home() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            "[glossary]\nterms = [\"Arlene\", \"TRL6\"]\n\
             sources = [\"~/notes\", \"/abs/people\", \"~other/x\"]\n",
        )
        .unwrap();
        let glossary = Config::load_from(&path).unwrap().glossary;
        assert_eq!(glossary.terms, ["Arlene", "TRL6"]);
        assert_eq!(glossary.max_terms, 30);
        assert_eq!(
            glossary.source_paths(),
            [
                home().join("notes"),
                PathBuf::from("/abs/people"),
                // Only `~` alone is the home directory, not `~user`.
                PathBuf::from("~other/x"),
            ]
        );
    }
}
