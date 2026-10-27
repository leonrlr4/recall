//! Recorder status and control, the audio self-test, and the config file.

use std::fs;

use anyhow::Context;
use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use recall_store::Config;
use serde_json::{Value, json};

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

/// Writes the config to `Config::path()`. The running server keeps its
/// config until restarted.
pub async fn put_config(Json(config): Json<Config>) -> ApiResult<Json<Value>> {
    let path = Config::path();
    let text = toml::to_string_pretty(&config).context("encoding config as TOML")?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    }
    fs::write(&path, text).with_context(|| format!("writing {}", path.display()))?;
    Ok(Json(json!({ "restart_required": true })))
}
