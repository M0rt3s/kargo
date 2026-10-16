//! The rmcp server handler.

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use http::request::Parts;
use rmcp::ServerHandler;
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, ErrorData as McpError,
    Implementation, ListToolsResult, PaginatedRequestParams, ServerCapabilities, ServerConfig,
};
use rmcp::service::{RequestContext, RoleServer};
use serde_json::{Map, Value};
use tokio::time::Instant;
use tokio_util::task::TaskTracker;

use crate::act::{self, Outcome};
use crate::api::Client;
use crate::cache::SwrCache;
use crate::catalogue::{CACHE_TTL, Catalogue, MemoryCache};
use crate::errors::{Error, Surface, describe_duration};
use crate::output::{sanitize, sanitize_json};
use crate::verifier::TokenVerifier;

use super::gate::{ActGate, DEFAULT_QUEUE};
use super::tools::{self, HELP_TOOL, VIEW_TOOL};

const INSTRUCTIONS: &str = "Tools mirror the game server's own catalogue. Call `help` first to read how the \
game works, `view` to look at the current state, then use the command tools to act. Commands are paced by \
the server; if one is refused, the message says why.";

/// Where the game API token comes from.
#[derive(Debug, Clone)]
pub enum Credentials {
    /// One token for the whole process (stdio).
    Fixed(Option<String>),
    /// Each request brings its own `Authorization: Bearer` header (HTTP).
    Forwarded,
}

/// How long the help text is kept before a background refresh.
const HELP_TTL: Duration = Duration::from_secs(300);
/// Longest a call waits before its command is sent (queue, rate-limit back-off), or for a read.
pub(crate) const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(60);

/// MCP server whose tools are generated from the game's catalogue.
#[derive(Clone)]
pub struct KargoMcp {
    client: Client,
    catalogue: Arc<MemoryCache>,
    help: Arc<SwrCache<String>>,
    credentials: Credentials,
    gate: Arc<ActGate>,
    request_timeout: Duration,
    verifier: Option<Arc<TokenVerifier>>,
    sent: TaskTracker,
}

/// Why a tool call did not produce a result.
enum Failure {
    /// No tool of that name: a protocol fault (JSON-RPC `invalid_params`).
    UnknownTool(String),
    /// Anything else, reported to the model or the transport depending on its kind.
    Fault(Error),
}

impl From<Error> for Failure {
    fn from(err: Error) -> Self {
        Failure::Fault(err)
    }
}

impl KargoMcp {
    /// Create a handler talking to the game server behind `client`.
    pub fn new(client: Client, credentials: Credentials) -> Self {
        let public = client.with_token(None);
        let catalogue = Arc::new(MemoryCache::new(public.clone()));
        let help = Arc::new(SwrCache::new(HELP_TTL, {
            let catalogue = Arc::clone(&catalogue);
            move || {
                let (catalogue, public) = (Arc::clone(&catalogue), public.clone());
                async move {
                    let catalogue = catalogue.get().await?;
                    Ok(public.get(&catalogue.endpoints.help, &[]).await?.text)
                }
            }
        }));
        Self {
            client,
            catalogue,
            help,
            credentials,
            gate: Arc::new(ActGate::default()),
            request_timeout: DEFAULT_REQUEST_TIMEOUT,
            verifier: None,
            sent: TaskTracker::new(),
        }
    }

    /// Bound how long a call waits before its command is sent (the queue behind the same token
    /// and any rate-limit back-off), and how long a read may take; `queue_wait` caps the queue
    /// part alone. Once a command is sent its answer is awaited, bounded only by the HTTP timeout.
    #[must_use]
    pub fn with_timeouts(mut self, request_timeout: Duration, queue_wait: Duration) -> Self {
        self.request_timeout = request_timeout;
        self.gate = Arc::new(ActGate::new(DEFAULT_QUEUE, queue_wait));
        self
    }

    /// Forget a token's cached verdict when the game API refuses it while a tool runs.
    #[must_use]
    pub(crate) fn with_verifier(mut self, verifier: Arc<TokenVerifier>) -> Self {
        self.verifier = Some(verifier);
        self
    }

    /// Commands already sent, so a shutdown can wait for them.
    pub(crate) fn sent_commands(&self) -> TaskTracker {
        self.sent.clone()
    }

    /// Run a read, giving up with an `unavailable` error after the request timeout.
    async fn within<T>(&self, work: impl Future<Output = Result<T, Error>>) -> Result<T, Error> {
        tokio::time::timeout(self.request_timeout, work).await.unwrap_or_else(|_| {
            Err(Error::Unavailable(format!("no answer within {}", describe_duration(self.request_timeout))))
        })
    }

    fn token(&self, context: &RequestContext<RoleServer>) -> Option<String> {
        match &self.credentials {
            Credentials::Fixed(token) => token.clone(),
            Credentials::Forwarded => {
                context.extensions.get::<Parts>().and_then(|parts| bearer(&parts.headers))
            }
        }
    }

    async fn dispatch(
        &self,
        request: CallToolRequestParams,
        token: Option<String>,
    ) -> Result<CallToolResult, Failure> {
        let deadline = Instant::now() + self.request_timeout;
        let catalogue = self.within(self.catalogue.get()).await?;
        let client = self.client.with_token(token.clone());
        let arguments = request.arguments.unwrap_or_default();
        match request.name.as_ref() {
            VIEW_TOOL => Ok(self.within(view(&client, &catalogue, &arguments)).await?),
            HELP_TOOL => {
                let text = self.within(self.help.get()).await?;
                Ok(CallToolResult::success(vec![ContentBlock::text(sanitize(&text))]))
            }
            name => {
                if catalogue.command(name).is_none() {
                    return Err(Failure::UnknownTool(name.to_owned()));
                }
                let turn = self.gate.enter(token.as_deref(), deadline).await?;
                let name = name.to_owned();
                // The send runs on a tracked task that keeps the turn until the server has
                // answered: a caller that goes away, or a shutdown, must not cancel a command
                // that is already on the wire. This call waits for the answer, bounded only by
                // the HTTP timeout; the deadline applies to the wait before sending.
                let sent = self.sent.spawn(async move {
                    let _turn = turn;
                    act::submit(&client, &catalogue, &name, arguments, None, Some(deadline)).await
                });
                let outcome = sent
                    .await
                    .map_err(|err| Error::Unexpected(format!("the command task failed: {err}")))??;
                match outcome {
                    Outcome::Done(reply) => Ok(structured(&reply.json()?)),
                    Outcome::Refused { message, reply } => Ok(refusal(&message, &reply.json()?)),
                }
            }
        }
    }

    /// The game API refused this token at use: the verdict cached for it is stale.
    fn forget(&self, token: Option<&str>) {
        if let (Some(verifier), Some(token)) = (&self.verifier, token) {
            verifier.evict(token);
        }
    }
}

impl ServerHandler for KargoMcp {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("kargo", env!("CARGO_PKG_VERSION")))
            .with_instructions(INSTRUCTIONS)
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        let catalogue = self.catalogue.get().await.map_err(|e| protocol_fault(&e))?;
        Ok(ListToolsResult::with_all_items(tools::build(&catalogue))
            .with_ttl_ms(u64::try_from(CACHE_TTL.as_millis()).unwrap_or(u64::MAX)))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, McpError> {
        let token = self.token(&context);
        match self.dispatch(request, token.clone()).await {
            Ok(result) => Ok(result.into()),
            Err(Failure::UnknownTool(name)) => {
                Err(McpError::invalid_params(format!("unknown tool `{}`", sanitize(&name)), None))
            }
            Err(Failure::Fault(err)) if is_tool_level(&err) => {
                if matches!(err, Error::Auth(_)) {
                    self.forget(token.as_deref());
                }
                Ok(failure(&err).into())
            }
            Err(Failure::Fault(err)) => Err(protocol_fault(&err)),
        }
    }
}

async fn view(
    client: &Client,
    catalogue: &Catalogue,
    arguments: &Map<String, Value>,
) -> Result<CallToolResult, Error> {
    let section = match arguments.get("section") {
        None => None,
        Some(Value::String(section)) => Some(section.as_str()),
        Some(_) => return Err(Error::Schema("`section` must be a string".into())),
    };
    if let Some(section) = section
        && !catalogue.view_sections.iter().any(|s| s == section)
    {
        return Err(Error::Schema(format!(
            "unknown section `{section}`; available: {}",
            catalogue.view_sections.join(", ")
        )));
    }
    let query: Vec<(&str, &str)> = section.map(|s| ("section", s)).into_iter().collect();
    let reply = client.get(&catalogue.endpoints.view, &query).await?;
    Ok(structured(&reply.json()?))
}

/// The server JSON as `structuredContent` plus the same JSON as a text block.
fn structured(value: &Value) -> CallToolResult {
    CallToolResult::structured(as_object(value))
}

fn refusal(message: &str, value: &Value) -> CallToolResult {
    let mut result = CallToolResult::error(vec![ContentBlock::text(sanitize(message))]);
    result.structured_content = Some(as_object(value));
    result
}

/// `structuredContent` must be an object; wrap any other JSON value.
fn as_object(value: &Value) -> Value {
    if value.is_object() { value.clone() } else { serde_json::json!({ "result": value }) }
}

/// Anything that went wrong while serving a well-formed call is a tool result the model can see;
/// only protocol faults (bad catalogue or reply, unexpected local failure) are JSON-RPC errors.
fn is_tool_level(err: &Error) -> bool {
    !matches!(
        err,
        Error::UpgradeRequired { .. } | Error::Protocol(_) | Error::Unexpected(_) | Error::BrokenPipe
    )
}

/// A tool error carrying the message and hint, with `{"error", "kind", "hint"?}` as structured content.
fn failure(err: &Error) -> CallToolResult {
    let text = match err.hint(Surface::Mcp) {
        Some(hint) => format!("{err} ({hint})"),
        None => err.to_string(),
    };
    let mut result = CallToolResult::error(vec![ContentBlock::text(sanitize(&text))]);
    result.structured_content = Some(sanitize_json(&err.to_json(Surface::Mcp)));
    result
}

fn protocol_fault(err: &Error) -> McpError {
    McpError::internal_error(sanitize(&err.to_string()), Some(serde_json::json!({ "kind": err.kind() })))
}

/// Extract the token from an `Authorization: Bearer <token>` header.
pub fn bearer(headers: &http::HeaderMap) -> Option<String> {
    let value = headers.get(http::header::AUTHORIZATION)?.to_str().ok()?;
    let (scheme, token) = value.split_once(' ')?;
    let token = token.trim();
    (scheme.eq_ignore_ascii_case("bearer") && !token.is_empty()).then(|| token.to_owned())
}

#[cfg(test)]
mod tests {
    use http::HeaderMap;

    use super::*;

    #[test]
    fn bearer_is_extracted_case_insensitively() {
        let mut headers = HeaderMap::new();
        headers.insert("authorization", "bearer abc".parse().unwrap());
        assert_eq!(bearer(&headers).as_deref(), Some("abc"));
        headers.insert("authorization", "Basic abc".parse().unwrap());
        assert_eq!(bearer(&headers), None);
        headers.insert("authorization", "Bearer ".parse().unwrap());
        assert_eq!(bearer(&headers), None);
    }

    #[test]
    fn only_protocol_faults_are_json_rpc_errors_and_everything_else_reaches_the_model() {
        let tool_level = [
            Error::Usage(String::new()),
            Error::Schema(String::new()),
            Error::Auth(String::new()),
            Error::RateLimited { retry_after: Duration::ZERO },
            Error::Refused(String::new()),
            Error::Unavailable(String::new()),
            Error::NoCompany(String::new()),
            Error::Busy(String::new()),
            Error::Throttled(String::new()),
            Error::Unknown,
        ];
        for error in tool_level {
            assert!(is_tool_level(&error), "{error:?} must be a tool result");
        }
        let protocol_faults = [
            Error::Unexpected(String::new()),
            Error::UpgradeRequired { found: 2, supported: 1 },
            Error::Protocol(String::new()),
            Error::BrokenPipe,
        ];
        for error in protocol_faults {
            assert!(!is_tool_level(&error), "{error:?} must be a JSON-RPC error");
        }
    }

    #[test]
    fn non_object_json_is_wrapped_for_structured_content() {
        assert_eq!(as_object(&serde_json::json!([1])), serde_json::json!({"result": [1]}));
        assert_eq!(as_object(&serde_json::json!({"a": 1})), serde_json::json!({"a": 1}));
    }
}
