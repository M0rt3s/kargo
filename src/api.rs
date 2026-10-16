//! HTTP client for the game server's agent API.
//!
//! The client knows nothing about individual game commands: callers hand it a
//! path (taken from the catalogue) and a JSON body, and it deals with
//! authentication, rate limiting and mapping HTTP failures to [`Error`]s.

use std::fmt::Write as _;
use std::time::Duration;

use reqwest::{Method, StatusCode, header};
use serde_json::Value;
use tokio::time::Instant;

use crate::errors::{Error, Result};
use crate::output::sanitize;

/// Shortest pause before retrying a 429, and the pause when `Retry-After` is missing.
const DEFAULT_RETRY_AFTER: Duration = Duration::from_secs(1);
/// Upper bound applied to a server-supplied `Retry-After`, in seconds.
const MAX_RETRY_AFTER_SECS: f64 = 86_400.0;
/// Longest piece of a non-JSON body quoted in an error message.
const MAX_QUOTED_BODY: usize = 300;
/// Longest a single HTTP exchange may take.
pub(crate) const HTTP_TIMEOUT: Duration = Duration::from_secs(30);
/// Largest response body accepted, in bytes.
pub(crate) const MAX_BODY_BYTES: usize = 8 * 1024 * 1024;

/// A successful HTTP exchange: the status and the body exactly as received.
#[derive(Debug, Clone)]
pub struct Reply {
    /// HTTP status code.
    pub status: u16,
    /// Response body, untouched.
    pub text: String,
}

impl Reply {
    /// Parse the body as JSON.
    pub fn json(&self) -> Result<Value> {
        serde_json::from_str(&self.text)
            .map_err(|err| Error::Unexpected(format!("the server sent a non-JSON body: {err}")))
    }

    /// If the body says `"ok": false`, the server's explanation.
    pub fn refusal(&self) -> Option<String> {
        let value: Value = serde_json::from_str(&self.text).ok()?;
        (value.get("ok") == Some(&Value::Bool(false))).then(|| message_of(&self.text))
    }
}

/// A client for one game server, optionally bound to a token.
#[derive(Debug, Clone)]
pub struct Client {
    http: reqwest::Client,
    base: String,
    token: Option<String>,
    max_wait: Duration,
    http_timeout: Duration,
}

/// What a send may promise the caller about a failure.
#[derive(Clone, Copy)]
struct Delivery {
    /// A command: once it is on the wire a failure leaves its outcome unknown.
    command: bool,
    /// Past this instant a command waiting to be sent is given up (nothing has been sent).
    deadline: Option<Instant>,
}

impl Client {
    /// Create a client for `base_url`. `max_wait` bounds the total time spent
    /// sleeping on `429 Too Many Requests` per call.
    pub fn new(base_url: &str, token: Option<String>, max_wait: Duration) -> Result<Self> {
        let http = reqwest::Client::builder()
            .user_agent(concat!("kargo/", env!("CARGO_PKG_VERSION")))
            .connect_timeout(Duration::from_secs(10))
            .build()
            .map_err(|err| Error::Unexpected(format!("cannot build the HTTP client: {err}")))?;
        Ok(Self {
            http,
            base: base_url.trim_end_matches('/').to_owned(),
            token,
            max_wait,
            http_timeout: HTTP_TIMEOUT,
        })
    }

    /// The same client with a different token (the connection pool is shared).
    #[must_use]
    pub fn with_token(&self, token: Option<String>) -> Self {
        Self { token, ..self.clone() }
    }

    /// The same client with a different rate-limit patience.
    #[must_use]
    pub fn with_max_wait(&self, max_wait: Duration) -> Self {
        Self { max_wait, ..self.clone() }
    }

    /// The same client with a different limit on one HTTP exchange.
    #[must_use]
    pub fn with_http_timeout(&self, http_timeout: Duration) -> Self {
        Self { http_timeout, ..self.clone() }
    }

    /// The token this client sends, if any.
    pub fn token(&self) -> Option<&str> {
        self.token.as_deref()
    }

    /// The server's base URL, without a trailing slash.
    pub fn base_url(&self) -> &str {
        &self.base
    }

    /// `GET path`, sending the token if there is one.
    pub async fn get(&self, path: &str, query: &[(&str, &str)]) -> Result<Reply> {
        let request = self.http.request(Method::GET, self.url(path)).query(query);
        self.send(request, Delivery { command: false, deadline: None }).await
    }

    /// `POST path` with a JSON body, sending the token if there is one.
    ///
    /// A POST is a command. If it fails after it was sent, the error is [`Error::Unknown`].
    pub async fn post(&self, path: &str, body: &Value) -> Result<Reply> {
        self.post_before(path, body, None).await
    }

    /// Like [`Client::post`], but give up with [`Error::Busy`] rather than wait out a
    /// rate limit past `deadline`. Nothing has been applied when that happens. Once a
    /// request is on the wire the deadline no longer applies.
    pub async fn post_before(&self, path: &str, body: &Value, deadline: Option<Instant>) -> Result<Reply> {
        let request = self.http.request(Method::POST, self.url(path)).json(body);
        self.send(request, Delivery { command: true, deadline }).await
    }

    fn url(&self, path: &str) -> String {
        format!("{}/{}", self.base, path.trim_start_matches('/'))
    }

    async fn send(&self, mut request: reqwest::RequestBuilder, delivery: Delivery) -> Result<Reply> {
        if let Some(token) = &self.token {
            request = request.bearer_auth(token);
        }
        request = request.timeout(self.http_timeout);
        let mut waited = Duration::ZERO;
        loop {
            let attempt =
                request.try_clone().ok_or_else(|| Error::Unexpected("request cannot be retried".into()))?;
            let response = attempt.send().await.map_err(|e| failure_of(&e, delivery))?;
            let status = response.status();
            let retry_after = parse_retry_after(response.headers());
            let text = read_capped(response, delivery).await?;
            if status == StatusCode::TOO_MANY_REQUESTS {
                let wait = retry_after.unwrap_or(DEFAULT_RETRY_AFTER).max(DEFAULT_RETRY_AFTER);
                if delivery.deadline.is_some_and(|deadline| Instant::now() + wait >= deadline) {
                    return Err(Error::Throttled(
                        "the game's rate limit outlasted the timeout; nothing was applied".into(),
                    ));
                }
                if waited + wait > self.max_wait {
                    return Err(Error::RateLimited { retry_after: wait });
                }
                tokio::time::sleep(wait).await;
                waited += wait;
                continue;
            }
            return classify(status, text, delivery);
        }
    }
}

/// Read a body chunk by chunk, refusing to buffer more than [`MAX_BODY_BYTES`].
async fn read_capped(mut response: reqwest::Response, delivery: Delivery) -> Result<String> {
    let too_large = || {
        if delivery.command {
            return Error::Unknown;
        }
        Error::Protocol(format!("the server's reply is larger than {} MiB", MAX_BODY_BYTES / (1024 * 1024)))
    };
    if response.content_length().is_some_and(|len| len > MAX_BODY_BYTES as u64) {
        return Err(too_large());
    }
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|e| failure_of(&e, delivery))? {
        if body.len() + chunk.len() > MAX_BODY_BYTES {
            return Err(too_large());
        }
        body.extend_from_slice(&chunk);
    }
    Ok(String::from_utf8_lossy(&body).into_owned())
}

/// Map a final HTTP answer. After a command is sent, only the game's own verdict is believed:
/// a gateway error (any 5xx but 503) or a success without usable JSON leaves the outcome unknown.
fn classify(status: StatusCode, text: String, delivery: Delivery) -> Result<Reply> {
    if status.is_success() {
        if delivery.command && serde_json::from_str::<Value>(&text).is_err() {
            return Err(Error::Unknown);
        }
        return Ok(Reply { status: status.as_u16(), text });
    }
    if delivery.command && status.is_server_error() && status != StatusCode::SERVICE_UNAVAILABLE {
        return Err(Error::Unknown);
    }
    let message = message_of(&text);
    Err(match status {
        StatusCode::UNAUTHORIZED => Error::Auth(message),
        StatusCode::CONFLICT => Error::NoCompany(message),
        StatusCode::BAD_REQUEST => Error::Usage(message),
        StatusCode::UNPROCESSABLE_ENTITY => Error::Refused(message),
        s if s.is_server_error() => Error::Unavailable(format!("server error {}: {message}", s.as_u16())),
        s => Error::Unexpected(format!("unexpected HTTP status {}: {message}", s.as_u16())),
    })
}

/// A failed exchange: for a command, anything but a failure to connect leaves it unknown.
fn failure_of(err: &reqwest::Error, delivery: Delivery) -> Error {
    if delivery.command && !err.is_connect() { Error::Unknown } else { network_error(err) }
}

fn network_error(err: &reqwest::Error) -> Error {
    let mut message = format!("cannot reach the server: {err}");
    let mut source = std::error::Error::source(err);
    while let Some(cause) = source {
        let _ = write!(message, ": {cause}");
        source = cause.source();
    }
    Error::Unavailable(message)
}

fn parse_retry_after(headers: &header::HeaderMap) -> Option<Duration> {
    let seconds: f64 = headers.get(header::RETRY_AFTER)?.to_str().ok()?.trim().parse().ok()?;
    // Clamp before converting: `Duration::from_secs_f64` panics on huge values.
    (seconds.is_finite() && seconds >= 0.0)
        .then(|| Duration::from_secs_f64(seconds.ceil().min(MAX_RETRY_AFTER_SECS)))
}

/// Pull a human-readable message out of an error body.
fn message_of(text: &str) -> String {
    sanitize(&raw_message_of(text))
}

fn raw_message_of(text: &str) -> String {
    if let Ok(value) = serde_json::from_str::<Value>(text) {
        let candidates =
            [value.get("error"), value.get("message"), value.get("error").and_then(|e| e.get("message"))];
        if let Some(message) = candidates.into_iter().flatten().find_map(Value::as_str) {
            return message.to_owned();
        }
    }
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return "the server gave no explanation".to_owned();
    }
    trimmed.chars().take(MAX_QUOTED_BODY).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const READ: Delivery = Delivery { command: false, deadline: None };
    const COMMAND: Delivery = Delivery { command: true, deadline: None };

    #[test]
    fn messages_come_from_json_or_plain_text() {
        assert_eq!(message_of(r#"{"ok":false,"error":"nope"}"#), "nope");
        assert_eq!(message_of(r#"{"message":"bad"}"#), "bad");
        assert_eq!(message_of(r#"{"error":{"message":"deep"}}"#), "deep");
        assert_eq!(message_of("  plain  "), "plain");
        assert_eq!(message_of(""), "the server gave no explanation");
    }

    #[test]
    fn statuses_map_to_error_kinds() {
        let kind = |s: u16| classify(StatusCode::from_u16(s).unwrap(), "x".into(), READ).unwrap_err().kind();
        assert_eq!(kind(401), "auth");
        assert_eq!(kind(409), "no_company");
        assert_eq!(kind(400), "usage");
        assert_eq!(kind(422), "refused");
        assert_eq!(kind(503), "unavailable");
        assert_eq!(kind(404), "unexpected");
    }

    #[test]
    fn after_a_command_is_sent_only_the_games_own_verdict_is_believed() {
        let outcome = |status: u16, body: &str| {
            classify(StatusCode::from_u16(status).unwrap(), body.into(), COMMAND)
                .map_or_else(|e| e.kind(), |_| "ok")
        };
        assert_eq!(outcome(502, "bad gateway"), "unknown");
        assert_eq!(outcome(504, ""), "unknown");
        assert_eq!(outcome(500, "oops"), "unknown");
        assert_eq!(outcome(503, "down"), "unavailable", "a 503 means the game is down");
        assert_eq!(outcome(200, "<html>proxy page</html>"), "unknown");
        assert_eq!(outcome(200, ""), "unknown");
        assert_eq!(outcome(200, r#"{"ok":true}"#), "ok");
        assert_eq!(outcome(422, r#"{"error":"no"}"#), "refused");
        assert_eq!(outcome(401, "{}"), "auth");
        // Reads are unchanged.
        let read = |status: u16, body: &str| {
            classify(StatusCode::from_u16(status).unwrap(), body.into(), READ)
                .map_or_else(|e| e.kind(), |_| "ok")
        };
        assert_eq!(read(502, "x"), "unavailable");
        assert_eq!(read(200, "not json"), "ok");
    }

    #[tokio::test]
    async fn an_oversize_reply_to_a_command_is_unknown_but_to_a_read_a_protocol_error() {
        let client = serve_body(MAX_BODY_BYTES + 1).await;
        let app = axum::Router::new()
            .route("/big", axum::routing::post(|| async { "x".repeat(MAX_BODY_BYTES + 1) }));
        let poster = Client::new(&crate::testutil::serve(app).await, None, Duration::from_secs(1)).unwrap();
        let sent = poster.post("/big", &serde_json::json!({})).await.unwrap_err();
        assert_eq!(sent.kind(), "unknown", "{sent:?}");
        assert_eq!(client.get("/big", &[]).await.unwrap_err().kind(), "protocol");
    }

    #[test]
    fn retry_after_rounds_up_to_whole_seconds() {
        let mut headers = header::HeaderMap::new();
        headers.insert(header::RETRY_AFTER, "1.2".parse().unwrap());
        assert_eq!(parse_retry_after(&headers), Some(Duration::from_secs(2)));
        headers.insert(header::RETRY_AFTER, "1e20".parse().unwrap());
        assert_eq!(parse_retry_after(&headers), Some(Duration::from_secs(86_400)));
        headers.insert(header::RETRY_AFTER, "soon".parse().unwrap());
        assert_eq!(parse_retry_after(&headers), None);
    }

    #[test]
    fn server_messages_lose_terminal_escapes() {
        assert_eq!(message_of("{\"error\":\"a\\u001b[31mb\"}"), "a[31mb");
    }

    async fn serve_body(len: usize) -> Client {
        let app =
            axum::Router::new().route("/big", axum::routing::get(move || async move { "x".repeat(len) }));
        Client::new(&crate::testutil::serve(app).await, None, Duration::from_secs(1)).unwrap()
    }

    #[tokio::test]
    async fn bodies_over_the_cap_are_a_protocol_error() {
        let client = serve_body(MAX_BODY_BYTES + 1).await;
        let err = client.get("/big", &[]).await.unwrap_err();
        assert_eq!(err.kind(), "protocol", "{err:?}");
        assert_eq!(err.status().code(), 8);
    }

    #[tokio::test]
    async fn a_body_exactly_at_the_cap_is_accepted() {
        let reply = serve_body(MAX_BODY_BYTES).await.get("/big", &[]).await.unwrap();
        assert_eq!(reply.text.len(), MAX_BODY_BYTES);
    }

    #[tokio::test]
    async fn an_unannounced_oversize_stream_is_cut_off_too() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let _ = socket.read(&mut [0; 4096]).await;
            let _ = socket.write_all(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n").await;
            let chunk = format!("100000\r\n{}\r\n", "x".repeat(0x10_0000));
            for _ in 0..9 {
                if socket.write_all(chunk.as_bytes()).await.is_err() {
                    break;
                }
            }
        });
        let err = Client::new(&url, None, Duration::from_secs(1)).unwrap().get("/", &[]).await.unwrap_err();
        assert_eq!(err.kind(), "protocol", "{err:?}");
    }

    async fn slow_server() -> Client {
        let app = axum::Router::new().route(
            "/slow",
            axum::routing::any(|| async {
                tokio::time::sleep(Duration::from_millis(600)).await;
                "late"
            }),
        );
        Client::new(&crate::testutil::serve(app).await, None, Duration::from_secs(1))
            .unwrap()
            .with_http_timeout(Duration::from_millis(150))
    }

    #[tokio::test]
    async fn a_command_that_times_out_after_sending_has_an_unknown_outcome_but_a_read_is_unavailable() {
        let client = slow_server().await;
        let sent = client.post("/slow", &serde_json::json!({})).await.unwrap_err();
        assert!(matches!(sent, Error::Unknown), "{sent:?}");
        assert_eq!(sent.status().code(), 9);
        let read = client.get("/slow", &[]).await.unwrap_err();
        assert_eq!(read.kind(), "unavailable", "{read:?}");
    }

    #[tokio::test]
    async fn a_command_that_never_connected_is_unavailable_not_unknown() {
        let client = Client::new("http://127.0.0.1:1", None, Duration::from_secs(1)).unwrap();
        let err = client.post("/x", &serde_json::json!({})).await.unwrap_err();
        assert_eq!(err.kind(), "unavailable", "{err:?}");
    }

    #[tokio::test]
    async fn a_rate_limit_that_outlasts_the_deadline_is_busy_and_is_not_retried() {
        let hits = crate::testutil::counter();
        let seen = std::sync::Arc::clone(&hits);
        let app = axum::Router::new().route(
            "/act",
            axum::routing::post(move || {
                seen.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                async { (StatusCode::TOO_MANY_REQUESTS, [(header::RETRY_AFTER, "1")], "slow down") }
            }),
        );
        let client = Client::new(&crate::testutil::serve(app).await, None, Duration::from_secs(10)).unwrap();
        let deadline = Instant::now() + Duration::from_millis(300);
        let err = tokio::time::timeout(
            Duration::from_secs(5),
            client.post_before("/act", &serde_json::json!({}), Some(deadline)),
        )
        .await
        .expect("must give up, not wait")
        .unwrap_err();
        assert_eq!(err.kind(), "busy", "{err:?}");
        assert!(
            err.to_string().contains("nothing was applied") && !err.to_string().contains("nothing was sent")
        );
        assert_eq!(err.hint(crate::errors::Surface::Cli), Some("wait, then send it again"));
        assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[test]
    fn refusal_is_read_from_a_successful_body() {
        let refused = Reply { status: 200, text: r#"{"ok":false,"error":"no"}"#.into() };
        assert_eq!(refused.refusal().as_deref(), Some("no"));
        let fine = Reply { status: 200, text: r#"{"ok":true}"#.into() };
        assert!(fine.refusal().is_none());
    }
}
