//! An in-process mock game server with made-up fixture commands.
//!
//! It implements only the HTTP contract (catalogue, help, view, act). The
//! commands (`ping`, `rename`, `toggle`, `peek`, ...) are invented for these tests.

#![allow(
    dead_code,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::result_large_err,
    clippy::assigning_clones
)]

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{Value, json};
use tokio::sync::oneshot;

pub const GOOD_TOKEN: &str = "good-token-0123456789";
pub const NO_COMPANY_TOKEN: &str = "nocompany-token-0123456789";
pub const DOWN_TOKEN: &str = "down-token-0123456789";
/// Passes `whoami` but is refused by every other endpoint, like a token revoked a moment ago.
pub const STALE_TOKEN: &str = "stale-token-0123456789";

/// Shared, inspectable server state.
#[derive(Default)]
pub struct MockState {
    pub schema_version: Mutex<Option<u64>>,
    pub catalogue_hits: AtomicUsize,
    pub act_hits: AtomicUsize,
    /// How many upcoming act calls answer 429.
    pub rate_limited_acts: AtomicUsize,
    pub retry_after: Mutex<String>,
    pub last_authorization: Mutex<Option<String>>,
    pub last_act_body: Mutex<Option<Value>>,
    pub in_flight: AtomicUsize,
    pub max_in_flight: AtomicUsize,
    pub act_delay_ms: AtomicUsize,
    pub help_hits: AtomicUsize,
    pub whoami_hits: AtomicUsize,
    /// Serve text containing terminal escape sequences in titles, help and error messages.
    pub hostile: std::sync::atomic::AtomicBool,
    /// Add repeated command names and names that are unsafe to turn into flags.
    pub junk_names: std::sync::atomic::AtomicBool,
    /// Serve the catalogue pretty-printed with odd trailing whitespace.
    pub pretty_catalogue: std::sync::atomic::AtomicBool,
    /// Answer `view` with a body far beyond any sane size.
    pub huge_view: std::sync::atomic::AtomicBool,
}

/// A running mock server.
pub struct Mock {
    pub url: String,
    pub state: Arc<MockState>,
    shutdown: Option<oneshot::Sender<()>>,
}

impl Mock {
    pub fn start() -> Self {
        let state = Arc::new(MockState { retry_after: Mutex::new("1".into()), ..MockState::default() });
        let app = Router::new()
            .route("/api/agent/tools", get(tools))
            .route("/api/agent/help", get(help))
            .route("/api/agent/view", get(view))
            .route("/api/agent/act", post(act))
            .route("/api/agent/whoami", get(whoami))
            .with_state(Arc::clone(&state));
        let (addr_tx, addr_rx) = std::sync::mpsc::channel::<SocketAddr>();
        let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
        std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
            runtime.block_on(async move {
                let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                addr_tx.send(listener.local_addr().unwrap()).unwrap();
                axum::serve(listener, app)
                    .with_graceful_shutdown(async {
                        let _ = shutdown_rx.await;
                    })
                    .await
                    .unwrap();
            });
        });
        let addr = addr_rx.recv().unwrap();
        Mock { url: format!("http://{addr}"), state, shutdown: Some(shutdown_tx) }
    }

    pub fn catalogue_hits(&self) -> usize {
        self.state.catalogue_hits.load(Ordering::SeqCst)
    }
}

impl Drop for Mock {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
    }
}

pub fn catalogue_json(schema_version: u64) -> Value {
    json!({
        "schema_version": schema_version,
        "game_version": "9.9.9-fixture",
        "endpoints": {"view": "/api/agent/view", "act": "/api/agent/act", "help": "/api/agent/help"},
        "limits": {"act_gap_ms": 1234, "read_gap_ms": 567},
        "view_sections": ["alpha", "beta"],
        "commands": [
            {
                "name": "ping",
                "title": "Ping a target",
                "description": "Send a ping to a target and get a pong back.",
                "input_schema": {
                    "$schema": "https://json-schema.org/draft/2020-12/schema",
                    "type": "object",
                    "required": ["target"],
                    "properties": {
                        "target": {"type": "string", "description": "Who to ping"},
                        "count": {"type": ["integer", "null"], "minimum": 1, "description": "How many times"}
                    }
                },
                "annotations": {"read_only_hint": false, "destructive_hint": false, "idempotent_hint": true},
                "example": {"target": "alpha", "count": 2}
            },
            {
                "name": "rename",
                "title": "Rename yourself",
                "description": "Change your display name.",
                "input_schema": {
                    "type": "object",
                    "required": ["name"],
                    "properties": {
                        "name": {"type": "string"},
                        "style": {"$ref": "#/$defs/Style"}
                    },
                    "$defs": {"Style": {"type": "string", "enum": ["plain", "fancy"]}}
                },
                "annotations": {"read_only_hint": false, "destructive_hint": false, "idempotent_hint": true},
                "example": {"name": "Nova", "style": "fancy"}
            },
            {
                "name": "toggle",
                "title": "Toggle the lamp",
                "description": "Turn the lamp on or off.",
                "input_schema": {
                    "type": "object",
                    "required": ["on"],
                    "properties": {"on": {"type": "boolean"}, "tags": {"type": "array", "items": {"type": "string"}}}
                },
                "annotations": {"read_only_hint": false, "destructive_hint": true, "idempotent_hint": true},
                "example": {"on": true}
            },
            {
                "name": "shift",
                "title": "Shift the dial",
                "description": "Move the dial by an offset.",
                "input_schema": {
                    "type": "object",
                    "required": ["dx"],
                    "properties": {"dx": {"type": "integer"}, "scale": {"type": "number"}}
                },
                "annotations": {"read_only_hint": false, "destructive_hint": false, "idempotent_hint": false}
            },
            {
                "name": "tangle",
                "title": "Tangled names",
                "description": "Fields whose flags would clash.",
                "input_schema": {
                    "type": "object",
                    "properties": {"my_key": {"type": "string"}, "max_wait": {"type": "string"}, "note": {"type": "string"}, "ok_field": {"type": "string"}}
                },
                "annotations": {"read_only_hint": false, "destructive_hint": false, "idempotent_hint": true}
            },
            {
                "name": "xref_http",
                "title": "Remote reference",
                "description": "Schema that points at a remote document.",
                "input_schema": {"type": "object", "properties": {"a": {"$ref": "http://127.0.0.1:1/other.json"}}},
                "annotations": {"read_only_hint": false, "destructive_hint": false, "idempotent_hint": true}
            },
            {
                "name": "xref_file",
                "title": "File reference",
                "description": "Schema that points at a local file.",
                "input_schema": {"type": "object", "properties": {"a": {"$ref": "file:///etc/hosts"}}},
                "annotations": {"read_only_hint": false, "destructive_hint": false, "idempotent_hint": true}
            },
            {
                "name": "peek",
                "title": "Peek",
                "description": "Look around. Takes no input.",
                "input_schema": {"type": "object", "properties": {}},
                "annotations": {"read_only_hint": true, "destructive_hint": false, "idempotent_hint": true}
            }
        ]
    })
}

async fn tools(State(state): State<Arc<MockState>>) -> Response {
    state.catalogue_hits.fetch_add(1, Ordering::SeqCst);
    let version = state.schema_version.lock().unwrap().unwrap_or(1);
    let mut catalogue = catalogue_json(version);
    if state.hostile.load(Ordering::SeqCst) {
        hostile_catalogue(&mut catalogue);
    }
    if state.junk_names.load(Ordering::SeqCst) {
        junk_catalogue(&mut catalogue);
    }
    let cache = [(header::CACHE_CONTROL, "public, max-age=60")];
    if state.pretty_catalogue.load(Ordering::SeqCst) {
        let body = format!("{}\n\n", serde_json::to_string_pretty(&catalogue).unwrap());
        return (cache, [(header::CONTENT_TYPE, "application/json")], body).into_response();
    }
    (cache, Json(catalogue)).into_response()
}

/// Terminal escape sequences in a title, an example, an enum value and a command name.
fn hostile_catalogue(catalogue: &mut Value) {
    let commands = catalogue["commands"].as_array_mut().unwrap();
    for command in commands.iter_mut() {
        match command["name"].as_str() {
            Some("peek") => command["title"] = json!("Pe\u{1b}[31mek"),
            Some("rename") => {
                command["example"] = json!({"name": "No\u{1b}]0;PWNED\u{7}va", "style": "bo\u{1b}[2Jld"});
                command["input_schema"]["$defs"]["Style"]["enum"] = json!(["plain", "bo\u{1b}[2Jld"]);
            }
            _ => {}
        }
    }
    commands.push(json!({
        "name": "evil\u{1b}]0;PWNED\u{7}",
        "title": "Evil",
        "description": "Has an unusable name.",
        "input_schema": {"type": "object", "properties": {}}
    }));
}

/// A repeated name, names no argument parser should ever see, and unusable field names.
fn junk_catalogue(catalogue: &mut Value) {
    let commands = catalogue["commands"].as_array_mut().unwrap();
    for command in commands.iter_mut() {
        if command["name"] == "peek" {
            command["input_schema"]["properties"] = json!({
                "-x": {"type": "string"}, "a b": {"type": "string"}, "k=v": {"type": "string"},
                "ok_field": {"type": "string"}
            });
        }
    }
    commands.push(json!({
        "name": "ping", "description": "A second ping that must lose.",
        "input_schema": {"type": "object", "properties": {"other": {"type": "string"}}}
    }));
    for name in ["-x", "a b", "k=v", "--help"] {
        commands.push(json!({"name": name, "input_schema": {"type": "object", "properties": {}}}));
    }
}

async fn help(State(state): State<Arc<MockState>>) -> String {
    state.help_hits.fetch_add(1, Ordering::SeqCst);
    if state.hostile.load(Ordering::SeqCst) {
        return "Fixture help\u{1b}]0;owned\u{7} text\u{9b}2J\n".to_owned();
    }
    "Fixture help: ping things, rename yourself, toggle the lamp.\n".to_owned()
}

async fn whoami(State(state): State<Arc<MockState>>, headers: HeaderMap) -> Response {
    state.whoami_hits.fetch_add(1, Ordering::SeqCst);
    let token = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|a| a.strip_prefix("Bearer "));
    match token {
        Some(GOOD_TOKEN | DOWN_TOKEN | STALE_TOKEN) => {
            Json(json!({"username": "tester", "company": true})).into_response()
        }
        Some(NO_COMPANY_TOKEN) => Json(json!({"username": "tester", "company": false})).into_response(),
        _ => (StatusCode::UNAUTHORIZED, Json(json!({"error": "bad token"}))).into_response(),
    }
}

/// Check the bearer token the way a game server would.
fn authorize(state: &MockState, headers: &HeaderMap) -> Result<(), Response> {
    let auth = headers.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok()).map(str::to_owned);
    *state.last_authorization.lock().unwrap() = auth.clone();
    match auth.as_deref().and_then(|a| a.strip_prefix("Bearer ")) {
        Some(GOOD_TOKEN) => Ok(()),
        Some(NO_COMPANY_TOKEN) => {
            Err((StatusCode::CONFLICT, Json(json!({"error": "finish setup first"}))).into_response())
        }
        Some(DOWN_TOKEN) => Err((StatusCode::SERVICE_UNAVAILABLE, "maintenance").into_response()),
        _ => Err((StatusCode::UNAUTHORIZED, Json(json!({"error": "bad token"}))).into_response()),
    }
}

async fn view(
    State(state): State<Arc<MockState>>,
    headers: HeaderMap,
    Query(query): Query<std::collections::HashMap<String, String>>,
) -> Response {
    if let Err(response) = authorize(&state, &headers) {
        return response;
    }
    if state.huge_view.load(Ordering::SeqCst) {
        return "x".repeat(9 * 1024 * 1024).into_response();
    }
    let section = query.get("section").map_or("all", String::as_str);
    if state.hostile.load(Ordering::SeqCst) {
        let hostile_key = "k\u{1b}]0;PWNED\u{7}\u{1b}[2J";
        return Json(json!({
            "section": section,
            hostile_key: {hostile_key: 1},
            "items": [{"id": 1, hostile_key: "one"}, {"id": 2, hostile_key: "two"}]
        }))
        .into_response();
    }
    Json(json!({"section": section, "items": [{"id": 1, "name": "one"}, {"id": 2, "name": "two"}]}))
        .into_response()
}

async fn act(State(state): State<Arc<MockState>>, headers: HeaderMap, Json(body): Json<Value>) -> Response {
    state.act_hits.fetch_add(1, Ordering::SeqCst);
    let pending = state.rate_limited_acts.load(Ordering::SeqCst);
    if pending > 0 {
        state.rate_limited_acts.fetch_sub(1, Ordering::SeqCst);
        let retry_after = state.retry_after.lock().unwrap().clone();
        return (StatusCode::TOO_MANY_REQUESTS, [(header::RETRY_AFTER, retry_after)], "slow down")
            .into_response();
    }
    if let Err(response) = authorize(&state, &headers) {
        return response;
    }
    let now = state.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
    state.max_in_flight.fetch_max(now, Ordering::SeqCst);
    let delay = state.act_delay_ms.load(Ordering::SeqCst);
    if delay > 0 {
        tokio::time::sleep(Duration::from_millis(delay as u64)).await;
    }
    state.in_flight.fetch_sub(1, Ordering::SeqCst);
    *state.last_act_body.lock().unwrap() = Some(body.clone());

    let command = &body["command"];
    match command["command"].as_str() {
        Some("ping") if command["target"] == "reject400" => {
            (StatusCode::BAD_REQUEST, Json(json!({"error": "malformed request"}))).into_response()
        }
        Some("ping") if command["target"] == "gateway502" => {
            (StatusCode::BAD_GATEWAY, "<html>bad gateway</html>").into_response()
        }
        Some("ping") if command["target"] == "down503" => {
            (StatusCode::SERVICE_UNAVAILABLE, "maintenance").into_response()
        }
        Some("ping") if command["target"] == "page200" => "<html>proxy page</html>".into_response(),
        Some("ping") if command["target"] == "reject404" => {
            (StatusCode::NOT_FOUND, Json(json!({"error": "no such thing"}))).into_response()
        }
        Some("rename") if command["name"] == "hostile" => {
            Json(json!({"ok": false, "error": "bad\u{1b}]0;owned\u{7} name"})).into_response()
        }
        Some("shift") => {
            Json(json!({"ok": true, "result": {"dx": command["dx"], "scale": command["scale"]}}))
                .into_response()
        }
        Some("tangle") => Json(json!({"ok": true, "result": command})).into_response(),
        Some("ping") => {
            Json(json!({"ok": true, "result": {"pong": command["target"], "count": command["count"]}}))
                .into_response()
        }
        Some("rename") if command["name"] == "taken" => {
            Json(json!({"ok": false, "error": "that name is taken"})).into_response()
        }
        Some("rename") => {
            Json(json!({"ok": true, "result": {"name": command["name"], "style": command["style"]}}))
                .into_response()
        }
        Some("toggle") => Json(json!({"ok": true, "result": {"on": command["on"]}})).into_response(),
        Some("peek") => Json(json!({"ok": true, "result": {"seen": ["a", "b"]}})).into_response(),
        _ => (StatusCode::BAD_REQUEST, Json(json!({"error": "unknown command"}))).into_response(),
    }
}
