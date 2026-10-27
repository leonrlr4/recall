//! Recorder status and control, the audio self-test, and the config file.

use std::fs;

use anyhow::Context;
use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use recall_store::Config;
use serde_json::{Map, Value, json};

use crate::error::{ApiError, ApiResult};
use crate::{AppState, ControlCommand, SelfTestResult, StatusSnapshot};

pub async fn status(State(state): State<AppState>) -> Json<StatusSnapshot> {
    Json(state.status.read().expect("status lock poisoned").clone())
}

/// `POST /api/record/{start|stop|discard}`.
pub async fn record(
    State(state): State<AppState>,
    Path(action): Path<String>,
) -> ApiResult<StatusCode> {
    let command: ControlCommand = serde_json::from_value(Value::String(action.clone()))
        .map_err(|_| ApiError::bad_request(format!("unknown record action {action:?}")))?;
    (state.control)(command)?;
    Ok(StatusCode::NO_CONTENT)
}

/// Records a few seconds from both sources and returns their peak levels.
pub async fn selftest(State(state): State<AppState>) -> ApiResult<Json<SelfTestResult>> {
    let run = state.selftest.clone();
    let result = tokio::task::spawn_blocking(move || run())
        .await
        .context("self-test thread")??;
    Ok(Json(result))
}

/// The config the server was started with, not what is on disk now.
pub async fn get_config(State(state): State<AppState>) -> Json<Config> {
    Json((*state.config).clone())
}

/// Top-level key that `Config` omits when unset, so it is absent from the
/// serialized defaults that define the known keys.
const OPTIONAL_KEYS: [&str; 1] = ["ui_lang"];

/// Merges the body, which may hold any subset of the config's keys (nested
/// `llm`/`stt` too), onto the config file on disk and writes the result back
/// as TOML. Unknown keys, a zero port and relative directories are 400s. The
/// running server keeps its config until restarted.
pub async fn put_config(
    State(state): State<AppState>,
    Json(body): Json<Value>,
) -> ApiResult<Json<Value>> {
    let Value::Object(patch) = body else {
        return Err(ApiError::bad_request("the config must be a JSON object"));
    };
    let path = &state.config_path;
    let current = Config::load_from(path)?;
    let Value::Object(mut merged) = serde_json::to_value(&current).context("encoding config")?
    else {
        unreachable!("Config serializes as an object");
    };
    let Value::Object(known) =
        serde_json::to_value(Config::default()).context("encoding config")?
    else {
        unreachable!("Config serializes as an object");
    };
    merge(&mut merged, patch, &known, "")?;
    let config: Config = serde_json::from_value(Value::Object(merged))
        .map_err(|e| ApiError::bad_request(format!("invalid config: {e}")))?;
    if config.port == 0 {
        return Err(ApiError::bad_request("port must not be 0"));
    }
    for (key, dir) in [
        ("meetings_dir", &config.meetings_dir),
        ("data_dir", &config.data_dir),
    ] {
        if !dir.is_absolute() {
            return Err(ApiError::bad_request(format!(
                "{key} must be an absolute path, got {}",
                dir.display()
            )));
        }
    }
    let text = toml::to_string_pretty(&config).context("encoding config as TOML")?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    }
    fs::write(path, text).with_context(|| format!("writing {}", path.display()))?;
    Ok(Json(json!({ "restart_required": true })))
}

/// Copies `patch` into `target`, recursing into sections (keys whose known
/// value is an object) so a partial section keeps its other keys. Every key
/// must appear in `known`; `prefix` names the section in errors.
fn merge(
    target: &mut Map<String, Value>,
    patch: Map<String, Value>,
    known: &Map<String, Value>,
    prefix: &str,
) -> ApiResult<()> {
    for (key, value) in patch {
        let name = if prefix.is_empty() {
            key.clone()
        } else {
            format!("{prefix}.{key}")
        };
        let known_value = known.get(&key);
        if known_value.is_none() && !(prefix.is_empty() && OPTIONAL_KEYS.contains(&key.as_str())) {
            return Err(ApiError::bad_request(format!(
                "unknown config key {name:?}"
            )));
        }
        match (known_value, value) {
            (Some(Value::Object(section)), Value::Object(inner)) => {
                let slot = target
                    .entry(key)
                    .or_insert_with(|| Value::Object(Map::new()));
                let Value::Object(slot) = slot else {
                    return Err(ApiError::bad_request(format!("{name} must be an object")));
                };
                merge(slot, inner, section, &name)?;
            }
            (_, value) => {
                target.insert(key, value);
            }
        }
    }
    Ok(())
}
