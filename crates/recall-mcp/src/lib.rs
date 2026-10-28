//! `recall mcp`: a stdio MCP server whose tools call the daemon's local HTTP
//! API. It holds no state, so the daemon stays the only writer and its 409
//! for meetings under processing still applies.

mod client;
mod tools;

use anyhow::Context;
use rmcp::ServiceExt;

pub use reqwest::Url;
pub use tools::{PAGE_TOKENS, Recall};

/// Serves the tools on stdin/stdout until the client disconnects. `base` is
/// the daemon's address, `http://127.0.0.1:{config.port}`.
pub async fn serve_stdio(base: Url) -> anyhow::Result<()> {
    let service = Recall::new(base)?
        .serve(rmcp::transport::stdio())
        .await
        .context("starting the MCP session")?;
    service.waiting().await.context("serving MCP")?;
    Ok(())
}
