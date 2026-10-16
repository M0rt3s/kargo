//! `kargo serve`: the hosted MCP endpoint (Streamable HTTP, stateless).
//!
//! The server holds no credentials. Every request must bring its own
//! `Authorization: Bearer` token, which is forwarded to the game API unchanged
//! and never stored. It is meant to sit behind a reverse proxy.

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::{Request, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use rmcp::transport::streamable_http_server::session::never::NeverSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use serde_json::json;
use tokio::net::TcpListener;
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

use crate::api::Client;
use crate::errors::{Error, Result};
use crate::mcp::{Credentials, DEFAULT_QUEUE_WAIT, DEFAULT_REQUEST_TIMEOUT, DRAIN_LIMIT, KargoMcp, drain};
use crate::verifier::TokenVerifier;

/// Well-known path of the RFC 9728 protected-resource metadata document.
const METADATA_PATH: &str = "/.well-known/oauth-protected-resource";
/// Requests handled at once before the rest are turned away.
const DEFAULT_MAX_IN_FLIGHT: usize = 256;
const LOOPBACK_HOSTS: [&str; 3] = ["localhost", "127.0.0.1", "::1"];

/// Settings of the hosted endpoint.
#[derive(Debug, Clone)]
pub struct ServeOptions {
    game_url: String,
    bind: String,
    public_url: String,
    authorization_server: Option<String>,
    mount_path: String,
    allowed_origins: Vec<String>,
    max_wait: Duration,
    max_in_flight: usize,
    request_timeout: Duration,
    queue_wait: Duration,
}

impl ServeOptions {
    /// Validate and assemble the settings.
    ///
    /// `public_url` defaults to `http://<bind><mount_path>`. `mount_path` is the
    /// path the endpoint answers on, e.g. `/mcp`, or `/` when the reverse proxy
    /// strips the prefix.
    pub fn new(
        game_url: &str,
        bind: &str,
        public_url: Option<String>,
        authorization_server: Option<String>,
        mount_path: &str,
        extra_origins: &[String],
        max_wait: Duration,
    ) -> Result<Self> {
        let mount_path = normalise_mount(mount_path)?;
        let public_url = public_url.map_or_else(
            || format!("http://{bind}{}", mount_path.trim_end_matches('/')),
            |url| url.trim_end_matches('/').to_owned(),
        );
        let parts = UrlParts::parse(&public_url).ok_or_else(|| {
            Error::Usage(format!("--public-url `{public_url}` is not an absolute http(s) URL"))
        })?;
        let mut allowed_origins = vec![with_default_port(&parts.origin)];
        allowed_origins.extend(extra_origins.iter().map(|o| with_default_port(o.trim_end_matches('/'))));
        Ok(Self {
            game_url: game_url.to_owned(),
            bind: bind.to_owned(),
            public_url,
            authorization_server,
            mount_path,
            allowed_origins,
            max_wait,
            max_in_flight: DEFAULT_MAX_IN_FLIGHT,
            request_timeout: DEFAULT_REQUEST_TIMEOUT,
            queue_wait: DEFAULT_QUEUE_WAIT,
        })
    }

    /// Cap concurrent requests (extra ones get 503) and bound how long a call may take once
    /// it has its turn. Time queued behind another call of the same token is not counted.
    #[must_use]
    pub fn with_limits(mut self, max_in_flight: usize, request_timeout: Duration) -> Self {
        self.max_in_flight = max_in_flight.max(1);
        self.request_timeout = request_timeout;
        self
    }

    /// Bound how long a call may wait for the previous call of the same token; past it the
    /// call is answered `busy` and nothing is sent to the game.
    #[must_use]
    pub fn with_queue_wait(mut self, queue_wait: Duration) -> Self {
        self.queue_wait = queue_wait;
        self
    }

    fn allowed_hosts(&self) -> Vec<String> {
        let mut hosts: Vec<String> = LOOPBACK_HOSTS.iter().map(|h| (*h).to_owned()).collect();
        if let Some(parts) = UrlParts::parse(&self.public_url) {
            hosts.push(parts.authority);
        }
        if let Some((host, _)) = self.bind.rsplit_once(':')
            && host != "0.0.0.0"
            && host != "[::]"
        {
            hosts.push(host.trim_matches(['[', ']']).to_owned());
        }
        hosts
    }

    /// URL clients should fetch to discover how to authorise.
    fn metadata_url(&self) -> String {
        match UrlParts::parse(&self.public_url) {
            Some(parts) if parts.path != "/" => format!("{}{METADATA_PATH}{}", parts.origin, parts.path),
            Some(parts) => format!("{}{METADATA_PATH}", parts.origin),
            None => format!("{}{METADATA_PATH}", self.public_url),
        }
    }
}

fn normalise_mount(path: &str) -> Result<String> {
    let trimmed = path.trim_end_matches('/');
    if trimmed.is_empty() {
        return Ok("/".to_owned());
    }
    if !trimmed.starts_with('/') || trimmed.contains(['?', '#', '{', '}', '*']) {
        return Err(Error::Usage(format!(
            "--mount-path `{path}` must be a plain absolute path such as /mcp"
        )));
    }
    Ok(trimmed.to_owned())
}

/// The parts of an absolute URL that matter here.
struct UrlParts {
    origin: String,
    authority: String,
    path: String,
}

impl UrlParts {
    fn parse(url: &str) -> Option<Self> {
        let (scheme, rest) = url.split_once("://")?;
        if !matches!(scheme, "http" | "https") || rest.is_empty() {
            return None;
        }
        let (authority, path) = rest.find('/').map_or((rest, "/"), |i| (&rest[..i], &rest[i..]));
        (!authority.is_empty()).then(|| Self {
            origin: format!("{scheme}://{authority}"),
            authority: authority.to_owned(),
            path: path.to_owned(),
        })
    }
}

/// Spell out the scheme's default port, which rmcp's origin matching wants.
fn with_default_port(origin: &str) -> String {
    let Some((scheme, authority)) = origin.split_once("://") else { return origin.to_owned() };
    let has_port =
        authority.rsplit_once(':').is_some_and(|(_, port)| port.chars().all(|c| c.is_ascii_digit()));
    match (has_port, scheme) {
        (false, "https") => format!("{origin}:443"),
        (false, "http") => format!("{origin}:80"),
        _ => origin.to_owned(),
    }
}

/// Bind `options.bind`, serve until SIGINT/SIGTERM, then shut down gracefully.
pub async fn run_until_signal(options: ServeOptions) -> Result<()> {
    let listener = TcpListener::bind(&options.bind)
        .await
        .map_err(|err| Error::Unexpected(format!("cannot listen on {}: {err}", options.bind)))?;
    let local = listener.local_addr()?;
    eprintln!(
        "kargo serve: listening on http://{local}{} (public {})",
        options.mount_path, options.public_url
    );
    run(listener, options, shutdown_signal()).await
}

/// Serve on `listener` until `shutdown` completes.
pub async fn run(
    listener: TcpListener,
    options: ServeOptions,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> Result<()> {
    let ct = CancellationToken::new();
    let (app, sent) = build(&options, &ct)?;
    let (began, began_rx) = tokio::sync::oneshot::channel::<()>();
    let serving = axum::serve(listener, app).with_graceful_shutdown(async move {
        shutdown.await;
        let _ = began.send(());
    });
    // Stop accepting, let requests in flight finish (bounded), then end what is left and wait
    // for commands already sent: exiting must never cancel one that is on the wire.
    let result = tokio::select! {
        served = serving => served.map_err(|err| Error::Unexpected(format!("server error: {err}"))),
        () = async {
            let _ = began_rx.await;
            tokio::time::sleep(DRAIN_LIMIT).await;
        } => Ok(()),
    };
    ct.cancel();
    drain(&sent, DRAIN_LIMIT).await;
    result
}

/// Build the axum application.
pub fn router(options: &ServeOptions, ct: &CancellationToken) -> Result<Router> {
    build(options, ct).map(|(app, _)| app)
}

fn build(options: &ServeOptions, ct: &CancellationToken) -> Result<(Router, TaskTracker)> {
    let client = Client::new(&options.game_url, None, options.max_wait)?;
    let verifier = Arc::new(TokenVerifier::new(Client::new(&options.game_url, None, options.max_wait)?));
    let handler = KargoMcp::new(client, Credentials::Forwarded)
        .with_timeouts(options.request_timeout, options.queue_wait)
        .with_verifier(Arc::clone(&verifier));
    let sent = handler.sent_commands();
    let config = StreamableHttpServerConfig::default()
        .with_legacy_session_mode(false)
        .with_allowed_hosts(options.allowed_hosts())
        .with_allowed_origins(options.allowed_origins.clone())
        .with_cancellation_token(ct.child_token());
    let service = StreamableHttpService::new(
        move || Ok(handler.clone()),
        Arc::new(NeverSessionManager::default()),
        config,
    );

    let guard = Arc::new(AuthGuard { metadata_url: options.metadata_url(), verifier });
    let limits = Arc::new(Limits { permits: Semaphore::new(options.max_in_flight) });
    let mcp = Router::new()
        .fallback_service(service)
        .layer(middleware::from_fn_with_state(guard, require_valid_token))
        .layer(middleware::from_fn_with_state(limits, limit_load));

    let metadata = Arc::new(protected_resource_metadata(options));
    let mut app = Router::new().route("/healthz", get(|| async { "ok\n" }));
    for path in metadata_paths(options) {
        let metadata = Arc::clone(&metadata);
        app = app.route(
            &path,
            get(move || {
                let metadata = Arc::clone(&metadata);
                async move { Json((*metadata).clone()) }
            }),
        );
    }
    let app = if options.mount_path == "/" { app.merge(mcp) } else { app.nest(&options.mount_path, mcp) };
    Ok((app, sent))
}

fn metadata_paths(options: &ServeOptions) -> Vec<String> {
    let mut paths = vec![METADATA_PATH.to_owned()];
    if let Some(parts) = UrlParts::parse(&options.public_url)
        && parts.path != "/"
    {
        paths.push(format!("{METADATA_PATH}{}", parts.path));
    }
    paths
}

fn protected_resource_metadata(options: &ServeOptions) -> serde_json::Value {
    let mut doc = json!({
        "resource": options.public_url,
        "bearer_methods_supported": ["header"],
        "resource_name": "kargo",
    });
    if let (Some(server), Some(map)) = (&options.authorization_server, doc.as_object_mut()) {
        map.insert("authorization_servers".into(), json!([server]));
    }
    doc
}

struct AuthGuard {
    metadata_url: String,
    verifier: Arc<TokenVerifier>,
}

/// Answer 401 (with the resource-metadata hint) unless the request carries a bearer token the
/// game API accepts. This runs on every MCP request, `tools/list` included.
async fn require_valid_token(State(guard): State<Arc<AuthGuard>>, request: Request, next: Next) -> Response {
    let Some(token) = crate::mcp::bearer(request.headers()) else {
        return unauthorized(&guard.metadata_url, "a bearer token is required", None);
    };
    match guard.verifier.check(&token).await {
        Ok(true) => next.run(request).await,
        Ok(false) => {
            unauthorized(&guard.metadata_url, "the token is invalid or expired", Some("invalid_token"))
        }
        Err(_) => {
            let mut response =
                (StatusCode::SERVICE_UNAVAILABLE, "the game API could not check the token\n").into_response();
            response.headers_mut().insert(header::RETRY_AFTER, HeaderValue::from_static("5"));
            response
        }
    }
}

fn unauthorized(metadata_url: &str, message: &str, error: Option<&str>) -> Response {
    let challenge = match error {
        Some(code) => format!("Bearer error=\"{code}\", resource_metadata=\"{metadata_url}\""),
        None => format!("Bearer resource_metadata=\"{metadata_url}\""),
    };
    let mut response = (StatusCode::UNAUTHORIZED, format!("{message}\n")).into_response();
    if let Ok(value) = HeaderValue::from_str(&challenge) {
        response.headers_mut().insert(header::WWW_AUTHENTICATE, value);
    }
    response
}

struct Limits {
    permits: Semaphore,
}

/// Turn away load beyond the in-flight cap.
async fn limit_load(State(limits): State<Arc<Limits>>, request: Request, next: Next) -> Response {
    let Ok(_permit) = limits.permits.try_acquire() else {
        let mut response =
            (StatusCode::SERVICE_UNAVAILABLE, "busy: too many requests in flight\n").into_response();
        response.headers_mut().insert(header::RETRY_AFTER, HeaderValue::from_static("1"));
        return response;
    };
    next.run(request).await
}

async fn shutdown_signal() {
    let interrupt = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        match signal(SignalKind::terminate()) {
            Ok(mut terminate) => tokio::select! { () = interrupt => {}, _ = terminate.recv() => {} },
            Err(_) => interrupt.await,
        }
    }
    #[cfg(not(unix))]
    interrupt.await;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn options(public: Option<&str>, mount: &str) -> ServeOptions {
        ServeOptions::new(
            "http://game",
            "0.0.0.0:8787",
            public.map(str::to_owned),
            Some("https://auth.example".into()),
            mount,
            &[],
            Duration::from_secs(1),
        )
        .unwrap()
    }

    #[test]
    fn metadata_url_inserts_the_resource_path() {
        let o = options(Some("https://example.com/mcp"), "/mcp");
        assert_eq!(o.metadata_url(), "https://example.com/.well-known/oauth-protected-resource/mcp");
        assert_eq!(metadata_paths(&o).len(), 2);
        let root = options(Some("https://example.com"), "/");
        assert_eq!(root.metadata_url(), "https://example.com/.well-known/oauth-protected-resource");
    }

    #[test]
    fn origins_get_explicit_default_ports_and_hosts_include_the_public_one() {
        let o = options(Some("https://example.com/mcp"), "/mcp");
        assert_eq!(o.allowed_origins, ["https://example.com:443"]);
        assert!(o.allowed_hosts().contains(&"example.com".to_owned()));
    }

    #[test]
    fn bad_mount_paths_and_urls_are_usage_errors() {
        assert!(normalise_mount("mcp").is_err());
        assert_eq!(normalise_mount("/mcp/").unwrap(), "/mcp");
        assert_eq!(normalise_mount("").unwrap(), "/");
        assert!(
            ServeOptions::new("g", "a:1", Some("ftp://x".into()), None, "/", &[], Duration::ZERO).is_err()
        );
    }

    #[test]
    fn metadata_document_names_the_authorization_server() {
        let doc = protected_resource_metadata(&options(Some("https://example.com/mcp"), "/mcp"));
        assert_eq!(doc["authorization_servers"][0], "https://auth.example");
        assert_eq!(doc["resource"], "https://example.com/mcp");
    }
}
