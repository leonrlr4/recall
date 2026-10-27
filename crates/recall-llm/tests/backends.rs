//! Backend tests against a local mock HTTP server.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Json;
use axum::Router;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::routing::post;
use recall_llm::{Claude, Llm, Ollama, Timeouts, from_config};
use recall_store::LlmConfig;
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tokio::sync::mpsc;

#[derive(Clone, Default)]
struct Seen {
    body: Arc<Mutex<Option<Value>>>,
    headers: Arc<Mutex<Option<HeaderMap>>>,
}

/// Serves `router` on an ephemeral port and returns its base URL.
async fn serve(router: Router) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    format!("http://{addr}")
}

async fn record(seen: &Seen, headers: HeaderMap, body: Value) {
    *seen.body.lock().unwrap() = Some(body);
    *seen.headers.lock().unwrap() = Some(headers);
}

#[tokio::test]
async fn ollama_complete_posts_chat_and_returns_content() {
    let seen = Seen::default();
    let router = Router::new()
        .route(
            "/api/chat",
            post(
                |State(seen): State<Seen>, headers: HeaderMap, Json(body): Json<Value>| async move {
                    record(&seen, headers, body).await;
                    Json(json!({"message": {"role": "assistant", "content": "hi"}, "done": true}))
                },
            ),
        )
        .with_state(seen.clone());
    let base = serve(router).await;

    let llm = Ollama::new(&base, "qwen3.6:35b-a3b", 32_768);
    assert_eq!(llm.complete("sys", "usr").await.unwrap(), "hi");
    assert_eq!(llm.name(), "ollama/qwen3.6:35b-a3b");

    let body = seen.body.lock().unwrap().clone().unwrap();
    assert_eq!(body["model"], "qwen3.6:35b-a3b");
    assert_eq!(body["stream"], false);
    assert_eq!(body["think"], false);
    assert_eq!(body["keep_alive"], "30m");
    assert_eq!(body["format"], "json");
    assert_eq!(body["options"]["num_ctx"], 32_768);
    assert_eq!(
        body["messages"][0],
        json!({"role": "system", "content": "sys"})
    );
    assert_eq!(
        body["messages"][1],
        json!({"role": "user", "content": "usr"})
    );
}

#[tokio::test]
async fn ollama_stream_sends_each_chunk_until_done() {
    let router = Router::new().route(
        "/api/chat",
        post(|Json(body): Json<Value>| async move {
            assert_eq!(body["stream"], true);
            assert!(body.get("format").is_none());
            assert_eq!(body["options"]["num_ctx"], 4096);
            [
                r#"{"message":{"content":"Hel"},"done":false}"#,
                r#"{"message":{"content":"lo"},"done":false}"#,
                r#"{"message":{"content":""},"done":true}"#,
            ]
            .join("\n")
        }),
    );
    let base = serve(router).await;

    let (tx, mut rx) = mpsc::channel(8);
    Ollama::new(&base, "m", 4096)
        .stream("s", "u", tx)
        .await
        .unwrap();
    let mut out = Vec::new();
    while let Some(chunk) = rx.recv().await {
        out.push(chunk);
    }
    assert_eq!(out, ["Hel", "lo"]);
}

#[tokio::test]
async fn claude_complete_sends_api_key_header() {
    let seen = Seen::default();
    let router = Router::new()
        .route(
            "/v1/messages",
            post(
                |State(seen): State<Seen>, headers: HeaderMap, Json(body): Json<Value>| async move {
                    record(&seen, headers, body).await;
                    Json(json!({"content": [{"type": "text", "text": "hi"}]}))
                },
            ),
        )
        .with_state(seen.clone());
    let base = serve(router).await;

    let llm = Claude::new(Some("sk-test".into()), "claude-sonnet-5-5")
        .unwrap()
        .with_base_url(&base);
    assert_eq!(llm.complete("sys", "usr").await.unwrap(), "hi");
    assert_eq!(llm.name(), "claude/claude-sonnet-5-5");

    let headers = seen.headers.lock().unwrap().clone().unwrap();
    assert_eq!(headers["x-api-key"], "sk-test");
    assert_eq!(headers["anthropic-version"], "2023-06-01");
    let body = seen.body.lock().unwrap().clone().unwrap();
    assert_eq!(body["model"], "claude-sonnet-5-5");
    assert_eq!(body["max_tokens"], 16_000);
    assert_eq!(body["system"], "sys");
    assert_eq!(
        body["messages"][0],
        json!({"role": "user", "content": "usr"})
    );
}

#[tokio::test]
async fn claude_stream_sends_text_deltas() {
    let router = Router::new().route(
        "/v1/messages",
        post(|Json(body): Json<Value>| async move {
            assert_eq!(body["stream"], true);
            [
                "event: message_start",
                r#"data: {"type":"message_start","message":{}}"#,
                "",
                "event: content_block_delta",
                r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hel"}}"#,
                "",
                "event: content_block_delta",
                r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"lo"}}"#,
                "",
                "event: message_stop",
                r#"data: {"type":"message_stop"}"#,
                "",
            ]
            .join("\n")
        }),
    );
    let base = serve(router).await;

    let (tx, mut rx) = mpsc::channel(8);
    Claude::new(Some("k".into()), "m")
        .unwrap()
        .with_base_url(&base)
        .stream("s", "u", tx)
        .await
        .unwrap();
    let mut out = Vec::new();
    while let Some(chunk) = rx.recv().await {
        out.push(chunk);
    }
    assert_eq!(out, ["Hel", "lo"]);
}

#[tokio::test]
async fn http_errors_include_status_and_body() {
    let router = Router::new().route(
        "/api/chat",
        post(|| async { (axum::http::StatusCode::NOT_FOUND, "model not found") }),
    );
    let base = serve(router).await;
    let err = Ollama::new(&base, "m", 4096)
        .complete("s", "u")
        .await
        .unwrap_err();
    let msg = format!("{err:#}");
    assert!(msg.contains("404"), "{msg}");
    assert!(msg.contains("model not found"), "{msg}");
}

#[test]
fn claude_without_key_errors() {
    let err = Claude::new(None, "claude-sonnet-5-5").err().unwrap();
    assert!(err.to_string().contains("ANTHROPIC_API_KEY"), "{err}");
    assert!(Claude::new(Some(String::new()), "m").is_err());
}

#[test]
fn from_config_picks_backend_and_rejects_unknown() {
    let ollama = from_config(&LlmConfig::default()).unwrap();
    assert_eq!(ollama.name(), "ollama/qwen3.6:35b-a3b");

    let cfg = LlmConfig {
        backend: "gpt".into(),
        ..LlmConfig::default()
    };
    let err = from_config(&cfg).err().unwrap();
    assert!(err.to_string().contains("gpt"), "{err}");
}

/// Short timeouts so the tests below finish quickly.
fn short() -> Timeouts {
    Timeouts {
        connect: Duration::from_secs(1),
        total: Duration::from_millis(300),
        idle: Duration::from_millis(300),
    }
}

/// A server that accepts connections and never answers.
async fn silent_server() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let mut held = Vec::new();
        loop {
            let (socket, _) = listener.accept().await.unwrap();
            held.push(socket);
        }
    });
    format!("http://{addr}")
}

#[tokio::test]
async fn complete_times_out_when_the_server_never_answers() {
    let base = silent_server().await;
    let ollama = Ollama::new(&base, "m", 4096).with_timeouts(short());
    let err = format!("{:#}", ollama.complete("s", "u").await.unwrap_err());
    assert!(
        err.contains("timed out") || err.contains("no response"),
        "{err}"
    );

    let claude = Claude::new(Some("k".into()), "m")
        .unwrap()
        .with_base_url(&base)
        .with_timeouts(short());
    assert!(claude.complete("s", "u").await.is_err());
    let (tx, _rx) = mpsc::channel(8);
    let err = claude.stream("s", "u", tx).await.unwrap_err();
    assert!(format!("{err:#}").contains("no response"), "{err:#}");
}

#[tokio::test]
async fn stream_times_out_when_the_body_stalls() {
    let router = Router::new().route(
        "/api/chat",
        post(|| async {
            let first = futures_util::stream::once(async {
                Ok::<_, std::io::Error>(
                    r#"{"message":{"content":"Hel"},"done":false}"#.to_string() + "\n",
                )
            });
            let stall = futures_util::stream::pending();
            axum::body::Body::from_stream(futures_util::StreamExt::chain(first, stall))
        }),
    );
    let base = serve(router).await;
    let (tx, mut rx) = mpsc::channel(8);
    let err = Ollama::new(&base, "m", 4096)
        .with_timeouts(short())
        .stream("s", "u", tx)
        .await
        .unwrap_err();
    assert!(format!("{err:#}").contains("sent nothing"), "{err:#}");
    assert_eq!(rx.recv().await.as_deref(), Some("Hel"));
}

#[tokio::test]
async fn stream_cut_before_its_last_event_is_an_error() {
    let router = Router::new()
        .route(
            "/api/chat",
            post(|| async { r#"{"message":{"content":"Hel"},"done":false}"# }),
        )
        .route(
            "/v1/messages",
            post(|| async {
                [
                    "event: content_block_delta",
                    r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hel"}}"#,
                    "",
                ]
                .join("\n")
            }),
        );
    let base = serve(router).await;

    let (tx, _rx) = mpsc::channel(8);
    let err = Ollama::new(&base, "m", 4096)
        .stream("s", "u", tx)
        .await
        .unwrap_err();
    assert!(format!("{err:#}").contains("done: true"), "{err:#}");

    let (tx, _rx) = mpsc::channel(8);
    let err = Claude::new(Some("k".into()), "m")
        .unwrap()
        .with_base_url(&base)
        .stream("s", "u", tx)
        .await
        .unwrap_err();
    assert!(format!("{err:#}").contains("message_stop"), "{err:#}");
}

#[tokio::test]
async fn claude_complete_rejects_truncated_and_refused_replies() {
    let router = Router::new().route(
        "/v1/messages",
        post(|Json(body): Json<Value>| async move {
            let reason = body["system"].as_str().unwrap().to_string();
            Json(json!({
                "content": [{"type": "text", "text": "{\"one_line\": \"cut"}],
                "stop_reason": reason,
            }))
        }),
    );
    let base = serve(router).await;
    let llm = Claude::new(Some("k".into()), "m")
        .unwrap()
        .with_base_url(&base);

    let err = llm.complete("max_tokens", "u").await.unwrap_err();
    assert!(
        format!("{err:#}").contains("Claude stopped at max_tokens"),
        "{err:#}"
    );
    let err = llm.complete("refusal", "u").await.unwrap_err();
    assert!(format!("{err:#}").contains("Claude refused"), "{err:#}");
    assert!(llm.complete("end_turn", "u").await.is_ok());
}

#[tokio::test]
async fn claude_stream_rejects_max_tokens_stop() {
    let router = Router::new().route(
        "/v1/messages",
        post(|| async {
            [
                r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hel"}}"#,
                r#"data: {"type":"message_delta","delta":{"stop_reason":"max_tokens"}}"#,
                r#"data: {"type":"message_stop"}"#,
            ]
            .join("\n")
        }),
    );
    let base = serve(router).await;
    let (tx, _rx) = mpsc::channel(8);
    let err = Claude::new(Some("k".into()), "m")
        .unwrap()
        .with_base_url(&base)
        .stream("s", "u", tx)
        .await
        .unwrap_err();
    assert!(format!("{err:#}").contains("max_tokens"), "{err:#}");
}
