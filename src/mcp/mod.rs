//! The MCP server: tools generated from the game's catalogue.
//!
//! The same [`KargoMcp`] handler backs both transports: `kargo mcp` (stdio,
//! token from the environment or config) and `kargo serve` (Streamable HTTP,
//! the caller's bearer token is forwarded to the game API unchanged).

mod gate;
mod handler;
mod tools;

use rmcp::{ServiceExt, transport::stdio};

use std::time::Duration;

use tokio_util::task::TaskTracker;

use crate::api::{Client, HTTP_TIMEOUT};
use crate::errors::{Error, Result};

pub(crate) use gate::DEFAULT_QUEUE_WAIT;
pub(crate) use handler::{Credentials, DEFAULT_REQUEST_TIMEOUT, KargoMcp, bearer};

/// Longest a shutdown waits for commands that are already sent: one HTTP exchange, and a margin.
pub(crate) const DRAIN_LIMIT: Duration = Duration::from_secs(HTTP_TIMEOUT.as_secs() + 5);

/// Wait (bounded) for every command already sent to be answered.
pub(crate) async fn drain(sent: &TaskTracker, limit: Duration) {
    sent.close();
    let _ = tokio::time::timeout(limit, sent.wait()).await;
}

/// Serve MCP over stdin/stdout until the client disconnects.
///
/// The token (if any) is taken from `client`; stdio MCP does not use OAuth.
pub async fn run_stdio(client: Client) -> Result<()> {
    let token = client.token().map(str::to_owned);
    let server = KargoMcp::new(client, Credentials::Fixed(token));
    let sent = server.sent_commands();
    let running = server
        .serve(stdio())
        .await
        .map_err(|err| Error::Unexpected(format!("cannot start the MCP server: {err}")))?;
    let stopped = running
        .waiting()
        .await
        .map_err(|err| Error::Unexpected(format!("the MCP server stopped abnormally: {err}")));
    // The client is gone (stdin closed); a command it already sent still gets its answer read.
    drain(&sent, DRAIN_LIMIT).await;
    stopped.map(drop)
}
