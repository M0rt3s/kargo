//! MCP tests: the stdio server through a real rmcp client, and the hosted
//! Streamable HTTP server run in-process against the mock game server.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::sync::atomic::Ordering;
use std::time::Duration;

use common::{GOOD_TOKEN, Mock, STALE_TOKEN};
use rmcp::model::{CallToolRequestParams, CallToolResult, ClientConfig, ProtocolVersion};
use rmcp::service::RunningService;
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use rmcp::transport::{StreamableHttpClientTransport, TokioChildProcess};
use rmcp::{ClientLifecycleMode, ClientServiceExt, RoleClient, ServiceExt};
use serde_json::{Map, Value, json};
use tokio::net::TcpListener;
use tokio::process::Command;
use tokio::sync::oneshot;

type Client = RunningService<RoleClient, ()>;
type ConfiguredClient = RunningService<RoleClient, ClientConfig>;

fn args(value: &Value) -> Map<String, Value> {
    value.as_object().cloned().unwrap_or_default()
}

fn call(name: &'static str, arguments: &Value) -> CallToolRequestParams {
    CallToolRequestParams::new(name).with_arguments(args(arguments))
}

fn first_text(result: &CallToolResult) -> String {
    serde_json::to_value(&result.content[0]).unwrap()["text"].as_str().unwrap().to_owned()
}

async fn stdio_client(mock: &Mock, token: Option<&str>) -> Client {
    let dir = tempfile::tempdir().unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_kargo"));
    command
        .arg("mcp")
        .env("KARGO_URL", &mock.url)
        .env("KARGO_CONFIG_DIR", dir.path().join("config"))
        .env("KARGO_CACHE_DIR", dir.path().join("cache"))
        .env_remove("KARGO_TOKEN");
    if let Some(token) = token {
        command.env("KARGO_TOKEN", token);
    }
    let transport = TokioChildProcess::new(command).unwrap();
    ().serve(transport).await.unwrap()
}

#[tokio::test]
async fn stdio_lists_tools_generated_from_the_catalogue() {
    let mock = Mock::start();
    let client = stdio_client(&mock, Some(GOOD_TOKEN)).await;
    let tools = client.list_all_tools().await.unwrap();
    let names: Vec<&str> = tools.iter().map(|t| t.name.as_ref()).collect();
    assert_eq!(
        names,
        ["view", "help", "ping", "rename", "toggle", "shift", "tangle", "xref_http", "xref_file", "peek"]
    );

    let ping = tools.iter().find(|t| t.name == "ping").unwrap();
    assert_eq!(ping.title.as_deref(), Some("Ping a target"));
    assert_eq!(ping.description.as_deref(), Some("Send a ping to a target and get a pong back."));
    assert_eq!(ping.input_schema["required"], json!(["target"]));
    let hints = ping.annotations.as_ref().unwrap();
    assert_eq!(hints.read_only_hint, Some(false));
    assert_eq!(hints.idempotent_hint, Some(true));

    let view = tools.iter().find(|t| t.name == "view").unwrap();
    assert_eq!(view.input_schema["properties"]["section"]["enum"], json!(["alpha", "beta"]));
    assert_eq!(view.annotations.as_ref().unwrap().read_only_hint, Some(true));
    for tool in &tools {
        let hints = tool.annotations.as_ref().unwrap();
        assert_eq!(hints.open_world_hint, None, "{}: kargo must not claim to know this", tool.name);
        assert!(!tool.description.as_deref().unwrap_or_default().contains("Free to call"));
    }
    assert_eq!(
        tools.iter().find(|t| t.name == "toggle").unwrap().annotations.as_ref().unwrap().destructive_hint,
        Some(true)
    );
    client.cancel().await.unwrap();
}

#[tokio::test]
async fn stdio_call_returns_structured_content_and_a_text_copy() {
    let mock = Mock::start();
    let client = stdio_client(&mock, Some(GOOD_TOKEN)).await;
    let result = client.call_tool(call("ping", &json!({"target": "alpha", "count": 2}))).await.unwrap();
    assert_ne!(result.is_error, Some(true));
    let expected = json!({"ok": true, "result": {"pong": "alpha", "count": 2}});
    assert_eq!(result.structured_content, Some(expected.clone()));
    assert_eq!(serde_json::from_str::<Value>(&first_text(&result)).unwrap(), expected);
    client.cancel().await.unwrap();
}

#[tokio::test]
async fn stdio_rule_refusal_is_a_tool_error_with_the_servers_message() {
    let mock = Mock::start();
    let client = stdio_client(&mock, Some(GOOD_TOKEN)).await;
    let result = client.call_tool(call("rename", &json!({"name": "taken"}))).await.unwrap();
    assert_eq!(result.is_error, Some(true));
    assert_eq!(first_text(&result), "that name is taken");
    client.cancel().await.unwrap();
}

#[tokio::test]
async fn stdio_invalid_arguments_are_tool_errors_and_never_reach_the_game() {
    let mock = Mock::start();
    let client = stdio_client(&mock, Some(GOOD_TOKEN)).await;
    let result = client.call_tool(call("ping", &json!({"count": 0}))).await.unwrap();
    assert_eq!(result.is_error, Some(true));
    assert!(first_text(&result).contains("target"));
    assert_eq!(mock.state.act_hits.load(Ordering::SeqCst), 0);
    client.cancel().await.unwrap();
}

#[tokio::test]
async fn stdio_server_outages_are_tool_errors_of_kind_unavailable() {
    let mock = Mock::start();
    let client = stdio_client(&mock, Some(common::DOWN_TOKEN)).await;
    let result = client.call_tool(call("peek", &json!({}))).await.unwrap();
    assert_eq!(result.is_error, Some(true));
    assert_eq!(result.structured_content.unwrap()["kind"], "unavailable");
    client.cancel().await.unwrap();
}

#[tokio::test]
async fn stdio_schemas_with_external_references_are_tool_errors() {
    let mock = Mock::start();
    let client = stdio_client(&mock, Some(GOOD_TOKEN)).await;
    for name in ["xref_http", "xref_file"] {
        let result = client.call_tool(call(name, &json!({"a": 1}))).await.unwrap();
        assert_eq!(result.is_error, Some(true), "{name}");
        assert_eq!(result.structured_content.unwrap()["kind"], "schema");
    }
    client.cancel().await.unwrap();
}

#[tokio::test]
async fn stdio_text_from_the_server_is_stripped_of_escape_sequences() {
    let mock = Mock::start();
    mock.state.hostile.store(true, Ordering::SeqCst);
    let client = stdio_client(&mock, Some(GOOD_TOKEN)).await;
    let help = client.call_tool(call("help", &json!({}))).await.unwrap();
    assert!(!first_text(&help).contains(['\u{1b}', '\u{7}', '\u{9b}']));
    let refusal = client.call_tool(call("rename", &json!({"name": "hostile"}))).await.unwrap();
    assert_eq!(refusal.is_error, Some(true));
    assert_eq!(first_text(&refusal), "bad]0;owned name");
    let tools = client.list_all_tools().await.unwrap();
    assert_eq!(tools.iter().find(|t| t.name == "peek").unwrap().title.as_deref(), Some("Pe[31mek"));
    client.cancel().await.unwrap();
}

#[tokio::test]
async fn stdio_unknown_tool_is_a_protocol_error() {
    let mock = Mock::start();
    let client = stdio_client(&mock, Some(GOOD_TOKEN)).await;
    assert!(client.call_tool(call("teleport", &json!({}))).await.is_err());
    client.cancel().await.unwrap();
}

#[tokio::test]
async fn stdio_without_a_token_reports_an_auth_tool_error() {
    let mock = Mock::start();
    let client = stdio_client(&mock, None).await;
    let result = client.call_tool(call("peek", &json!({}))).await.unwrap();
    assert_eq!(result.is_error, Some(true));
    let text = first_text(&result);
    assert!(text.contains("the agent token was refused"), "{text}");
    for cli_only in ["auth login", "KARGO", "kargo", "--"] {
        assert!(!text.contains(cli_only), "{text}");
    }
    let structured = result.structured_content.unwrap();
    assert_eq!(structured["hint"], "the agent token was refused");
    client.cancel().await.unwrap();
}

#[tokio::test]
async fn stdio_schema_errors_point_at_the_input_schema_not_the_command_line() {
    let mock = Mock::start();
    let client = stdio_client(&mock, Some(GOOD_TOKEN)).await;
    let result = client.call_tool(call("ping", &json!({"count": 0}))).await.unwrap();
    let hint = result.structured_content.unwrap()["hint"].as_str().unwrap().to_owned();
    assert_eq!(hint, "see the tool's input schema");
    client.cancel().await.unwrap();
}

#[tokio::test]
async fn stdio_a_command_key_in_the_arguments_is_a_schema_error_and_never_sent() {
    let mock = Mock::start();
    let client = stdio_client(&mock, Some(GOOD_TOKEN)).await;
    for tool in ["ping", "peek"] {
        let arguments = json!({"target": "x", "command": "destroy_thing"});
        let result = client.call_tool(call(tool, &arguments)).await.unwrap();
        assert_eq!(result.is_error, Some(true), "{tool}");
        assert_eq!(result.structured_content.unwrap()["kind"], "schema", "{tool}");
    }
    assert_eq!(mock.state.act_hits.load(Ordering::SeqCst), 0, "the server never sees it");
    client.cancel().await.unwrap();
}

#[tokio::test]
async fn stdio_unexpected_server_replies_are_json_rpc_errors_not_tool_results() {
    let mock = Mock::start();
    let client = stdio_client(&mock, Some(GOOD_TOKEN)).await;
    let outcome = client.call_tool(call("ping", &json!({"target": "reject404"}))).await;
    assert!(outcome.is_err(), "an unexpected status is a protocol fault: {outcome:?}");
    let ok = client.call_tool(call("ping", &json!({"target": "fine"}))).await.unwrap();
    assert_ne!(ok.is_error, Some(true));
    client.cancel().await.unwrap();
}

#[tokio::test]
async fn stdio_a_reply_beyond_the_size_cap_is_a_json_rpc_error() {
    let mock = Mock::start();
    mock.state.huge_view.store(true, Ordering::SeqCst);
    let client = stdio_client(&mock, Some(GOOD_TOKEN)).await;
    assert!(client.call_tool(call("view", &json!({}))).await.is_err());
    client.cancel().await.unwrap();
}

#[tokio::test]
async fn stdio_a_section_that_is_not_a_string_is_a_schema_error() {
    let mock = Mock::start();
    let client = stdio_client(&mock, Some(GOOD_TOKEN)).await;
    for section in [json!(5), json!(["alpha"]), json!(null), json!({"a": 1})] {
        let result = client.call_tool(call("view", &json!({ "section": section }))).await.unwrap();
        assert_eq!(result.is_error, Some(true), "{section}");
        assert_eq!(result.structured_content.unwrap()["kind"], "schema", "{section}");
    }
    let fine = client.call_tool(call("view", &json!({"section": "alpha"}))).await.unwrap();
    assert_ne!(fine.is_error, Some(true));
    client.cancel().await.unwrap();
}

#[tokio::test]
async fn stdio_repeated_and_unusable_catalogue_names_are_not_offered() {
    let mock = Mock::start();
    mock.state.junk_names.store(true, Ordering::SeqCst);
    let client = stdio_client(&mock, Some(GOOD_TOKEN)).await;
    let tools = client.list_all_tools().await.unwrap();
    let names: Vec<&str> = tools.iter().map(|t| t.name.as_ref()).collect();
    assert_eq!(names.iter().filter(|n| **n == "ping").count(), 1);
    for junk in ["-x", "a b", "k=v", "--help"] {
        assert!(!names.contains(&junk), "{junk}");
    }
    let ping = tools.iter().find(|t| t.name == "ping").unwrap();
    assert_eq!(ping.description.as_deref(), Some("Send a ping to a target and get a pong back."));
    client.cancel().await.unwrap();
}

#[tokio::test]
async fn stdio_view_and_help_tools_work() {
    let mock = Mock::start();
    let client = stdio_client(&mock, Some(GOOD_TOKEN)).await;
    let view = client.call_tool(call("view", &json!({"section": "beta"}))).await.unwrap();
    assert_eq!(view.structured_content.unwrap()["section"], "beta");
    let bad = client.call_tool(call("view", &json!({"section": "zzz"}))).await.unwrap();
    assert_eq!(bad.is_error, Some(true));
    let help = client.call_tool(call("help", &json!({}))).await.unwrap();
    assert!(first_text(&help).starts_with("Fixture help"));
    client.cancel().await.unwrap();
}

/// A running in-process `kargo serve`.
struct Hosted {
    addr: std::net::SocketAddr,
    stop: Option<oneshot::Sender<()>>,
    task: tokio::task::JoinHandle<kargo::errors::Result<()>>,
}

impl Hosted {
    async fn start(mock: &Mock) -> Self {
        Self::start_with(mock, |options| options).await
    }

    async fn start_with(
        mock: &Mock,
        tune: impl FnOnce(kargo::serve::ServeOptions) -> kargo::serve::ServeOptions,
    ) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let options = kargo::serve::ServeOptions::new(
            &mock.url,
            &addr.to_string(),
            Some("https://games.example/mcp".to_owned()),
            Some("https://auth.example".into()),
            "/mcp",
            &[],
            Duration::from_secs(5),
        )
        .unwrap();
        let options = tune(options);
        let (stop, stopped) = oneshot::channel::<()>();
        let task = tokio::spawn(kargo::serve::run(listener, options, async move {
            let _ = stopped.await;
        }));
        Self { addr, stop: Some(stop), task }
    }

    fn url(&self, path: &str) -> String {
        format!("http://{}{path}", self.addr)
    }

    fn transport(&self, token: &str) -> StreamableHttpClientTransport<reqwest::Client> {
        StreamableHttpClientTransport::from_config(
            StreamableHttpClientTransportConfig::with_uri(self.url("/mcp")).auth_header(token),
        )
    }

    async fn legacy_client(&self, token: &str) -> Client {
        ().serve(self.transport(token)).await.unwrap()
    }

    async fn stateless_client(&self, token: &str) -> ConfiguredClient {
        ClientConfig::default()
            .serve_with_lifecycle(
                self.transport(token),
                ClientLifecycleMode::Discover { preferred_versions: vec![ProtocolVersion::V_2026_07_28] },
            )
            .await
            .unwrap()
    }

    async fn shutdown(mut self) {
        let _ = self.stop.take().map(|tx| tx.send(()));
        tokio::time::timeout(Duration::from_secs(5), self.task)
            .await
            .expect("graceful shutdown in time")
            .unwrap()
            .unwrap();
    }
}

#[tokio::test]
async fn hosted_requires_a_bearer_token_and_points_at_the_metadata() {
    let mock = Mock::start();
    let hosted = Hosted::start(&mock).await;
    let response = reqwest::Client::new()
        .post(hosted.url("/mcp"))
        .json(&json!({"jsonrpc": "2.0", "id": 1, "method": "ping"}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 401);
    let challenge = response.headers()["www-authenticate"].to_str().unwrap().to_owned();
    assert_eq!(
        challenge,
        r#"Bearer resource_metadata="https://games.example/.well-known/oauth-protected-resource/mcp""#
    );
    hosted.shutdown().await;
}

#[tokio::test]
async fn hosted_serves_protected_resource_metadata_and_health() {
    let mock = Mock::start();
    let hosted = Hosted::start(&mock).await;
    let http = reqwest::Client::new();
    for path in ["/.well-known/oauth-protected-resource", "/.well-known/oauth-protected-resource/mcp"] {
        let doc: Value = http.get(hosted.url(path)).send().await.unwrap().json().await.unwrap();
        assert_eq!(doc["resource"], "https://games.example/mcp");
        assert_eq!(doc["authorization_servers"], json!(["https://auth.example"]));
    }
    let health = http.get(hosted.url("/healthz")).send().await.unwrap();
    assert_eq!(health.status(), 200);
    hosted.shutdown().await;
}

#[tokio::test]
async fn hosted_rejects_foreign_origins() {
    let mock = Mock::start();
    let hosted = Hosted::start(&mock).await;
    let response = reqwest::Client::new()
        .post(hosted.url("/mcp"))
        .bearer_auth(GOOD_TOKEN)
        .header("Origin", "https://evil.example")
        .json(&json!({"jsonrpc": "2.0", "id": 1, "method": "ping"}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 403);
    hosted.shutdown().await;
}

#[tokio::test]
async fn hosted_serves_2026_stateless_clients_and_forwards_the_token_unchanged() {
    let mock = Mock::start();
    let hosted = Hosted::start(&mock).await;
    let client = hosted.stateless_client(GOOD_TOKEN).await;
    let tools = client.list_all_tools().await.unwrap();
    assert!(tools.iter().any(|t| t.name == "ping"));
    let result = client.call_tool(call("ping", &json!({"target": "beta"}))).await.unwrap();
    assert_eq!(result.structured_content.unwrap()["result"]["pong"], "beta");
    assert_eq!(
        mock.state.last_authorization.lock().unwrap().as_deref(),
        Some(format!("Bearer {GOOD_TOKEN}").as_str())
    );
    client.cancel().await.unwrap();
    hosted.shutdown().await;
}

#[tokio::test]
async fn hosted_stays_compatible_with_legacy_initialize_clients() {
    let mock = Mock::start();
    let hosted = Hosted::start(&mock).await;
    let client = hosted.legacy_client(GOOD_TOKEN).await;
    assert!(client.list_all_tools().await.unwrap().len() >= 5);
    let result = client.call_tool(call("peek", &json!({}))).await.unwrap();
    assert_ne!(result.is_error, Some(true));
    client.cancel().await.unwrap();
    hosted.shutdown().await;
}

#[tokio::test]
async fn hosted_allows_one_act_in_flight_per_token() {
    let mock = Mock::start();
    mock.state.act_delay_ms.store(300, Ordering::SeqCst);
    let hosted = Hosted::start(&mock).await;
    let (a, b) = (hosted.stateless_client(GOOD_TOKEN).await, hosted.stateless_client(GOOD_TOKEN).await);
    let (ra, rb) = tokio::join!(a.call_tool(call("peek", &json!({}))), b.call_tool(call("peek", &json!({}))));
    ra.unwrap();
    rb.unwrap();
    assert_eq!(mock.state.act_hits.load(Ordering::SeqCst), 2);
    assert_eq!(mock.state.max_in_flight.load(Ordering::SeqCst), 1, "same token must be serialised");
    hosted.shutdown().await;
}

#[tokio::test]
async fn hosted_rate_limits_are_waited_out_before_answering() {
    let mock = Mock::start();
    mock.state.rate_limited_acts.store(1, Ordering::SeqCst);
    let hosted = Hosted::start(&mock).await;
    let client = hosted.stateless_client(GOOD_TOKEN).await;
    let result = client.call_tool(call("peek", &json!({}))).await.unwrap();
    assert_ne!(result.is_error, Some(true));
    assert_eq!(mock.state.act_hits.load(Ordering::SeqCst), 2);
    hosted.shutdown().await;
}

fn rpc(method: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": 1, "method": method})
}

#[tokio::test]
async fn hosted_answers_401_for_unknown_tokens_on_every_request_including_tools_list() {
    let mock = Mock::start();
    let hosted = Hosted::start(&mock).await;
    for method in ["tools/list", "ping", "tools/call"] {
        let response = reqwest::Client::new()
            .post(hosted.url("/mcp"))
            .bearer_auth("not-a-real-token")
            .json(&rpc(method))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 401, "{method}");
        let challenge = response.headers()["www-authenticate"].to_str().unwrap().to_owned();
        assert!(challenge.contains(r#"error="invalid_token""#), "{challenge}");
        assert!(challenge.contains("resource_metadata="), "{challenge}");
    }
    assert_eq!(mock.state.act_hits.load(Ordering::SeqCst), 0, "nothing reaches the game");
    hosted.shutdown().await;
}

#[tokio::test]
async fn hosted_checks_each_token_once_per_ttl_not_once_per_request() {
    let mock = Mock::start();
    let hosted = Hosted::start(&mock).await;
    let client = hosted.stateless_client(GOOD_TOKEN).await;
    client.list_all_tools().await.unwrap();
    client.call_tool(call("peek", &json!({}))).await.unwrap();
    client.call_tool(call("peek", &json!({}))).await.unwrap();
    assert_eq!(mock.state.whoami_hits.load(Ordering::SeqCst), 1);
    let http = reqwest::Client::new();
    for _ in 0..3 {
        let status = http
            .post(hosted.url("/mcp"))
            .bearer_auth("bogus")
            .json(&rpc("ping"))
            .send()
            .await
            .unwrap()
            .status();
        assert_eq!(status, 401);
    }
    assert_eq!(mock.state.whoami_hits.load(Ordering::SeqCst), 2, "the rejection was remembered too");
    hosted.shutdown().await;
}

#[tokio::test]
async fn hosted_reports_503_when_the_game_api_cannot_vouch_for_a_token() {
    let mock = Mock::start();
    let hosted = Hosted::start(&mock).await;
    drop(mock);
    tokio::time::sleep(Duration::from_millis(200)).await;
    let response = reqwest::Client::new()
        .post(hosted.url("/mcp"))
        .bearer_auth(GOOD_TOKEN)
        .json(&rpc("ping"))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 503);
    hosted.shutdown().await;
}

#[tokio::test]
async fn hosted_shares_one_help_text_and_catalogue_across_concurrent_callers() {
    let mock = Mock::start();
    let hosted = Hosted::start(&mock).await;
    let client = hosted.stateless_client(GOOD_TOKEN).await;
    let tasks: Vec<_> = (0..10)
        .map(|_| {
            let peer = client.peer().clone();
            tokio::spawn(async move { peer.call_tool(call("help", &json!({}))).await })
        })
        .collect();
    for task in tasks {
        assert!(first_text(&task.await.unwrap().unwrap()).starts_with("Fixture help"));
    }
    assert_eq!(mock.state.help_hits.load(Ordering::SeqCst), 1);
    assert_eq!(mock.catalogue_hits(), 1);
    hosted.shutdown().await;
}

#[tokio::test]
async fn hosted_rejects_unknown_hosts_and_accepts_the_public_origin() {
    let mock = Mock::start();
    let hosted = Hosted::start(&mock).await;
    let http = reqwest::Client::new();
    let evil_host = http
        .post(hosted.url("/mcp"))
        .bearer_auth(GOOD_TOKEN)
        .header("Host", "evil.example")
        .json(&rpc("ping"))
        .send()
        .await
        .unwrap();
    assert_eq!(evil_host.status(), 403);
    let public = http
        .post(hosted.url("/mcp"))
        .bearer_auth(GOOD_TOKEN)
        .header("Origin", "https://games.example")
        .json(&rpc("ping"))
        .send()
        .await
        .unwrap();
    assert_ne!(public.status(), 403, "the public origin is allowed");
    assert_ne!(public.status(), 401);
    hosted.shutdown().await;
}

#[tokio::test]
async fn hosted_turns_away_load_beyond_its_in_flight_cap() {
    let mock = Mock::start();
    mock.state.act_delay_ms.store(700, Ordering::SeqCst);
    let hosted = Hosted::start_with(&mock, |o| o.with_limits(1, Duration::from_secs(30))).await;
    let client = hosted.stateless_client(GOOD_TOKEN).await;
    let slow = tokio::spawn(async move { client.call_tool(call("peek", &json!({}))).await });
    tokio::time::sleep(Duration::from_millis(300)).await;
    let busy = reqwest::Client::new()
        .post(hosted.url("/mcp"))
        .bearer_auth(GOOD_TOKEN)
        .json(&rpc("ping"))
        .send()
        .await
        .unwrap();
    assert_eq!(busy.status(), 503);
    assert!(busy.headers().contains_key("retry-after"));
    slow.await.unwrap().unwrap();
    hosted.shutdown().await;
}

/// Fail a test that would otherwise hang.
async fn bounded<T>(work: impl std::future::Future<Output = T>) -> T {
    tokio::time::timeout(Duration::from_secs(30), work).await.expect("the test hung")
}

#[tokio::test]
async fn hosted_waits_for_a_sent_command_however_long_it_takes_and_never_cancels_it() {
    let mock = Mock::start();
    mock.state.act_delay_ms.store(1500, Ordering::SeqCst);
    let hosted = Hosted::start_with(&mock, |o| o.with_limits(8, Duration::from_secs(1))).await;
    let client = hosted.stateless_client(GOOD_TOKEN).await;
    let result = bounded(client.call_tool(call("peek", &json!({})))).await.unwrap();
    assert_ne!(result.is_error, Some(true), "the timeout only covers the wait before sending");
    assert!(mock.state.last_act_body.lock().unwrap().is_some());
    assert_eq!(mock.state.act_hits.load(Ordering::SeqCst), 1);
    hosted.shutdown().await;
}

#[tokio::test]
async fn hosted_answers_busy_and_sends_nothing_when_the_timeout_runs_out_in_the_queue() {
    let mock = Mock::start();
    mock.state.act_delay_ms.store(2000, Ordering::SeqCst);
    // The queue allowance (10 s) is longer than the timeout (1 s): the timeout is what ends it.
    let hosted = Hosted::start_with(&mock, |o| {
        o.with_limits(8, Duration::from_secs(1)).with_queue_wait(Duration::from_secs(10))
    })
    .await;
    let (a, b) = (hosted.stateless_client(GOOD_TOKEN).await, hosted.stateless_client(GOOD_TOKEN).await);
    let first = tokio::spawn(async move { a.call_tool(call("peek", &json!({}))).await });
    tokio::time::sleep(Duration::from_millis(150)).await;
    let started = std::time::Instant::now();
    let second = bounded(b.call_tool(call("peek", &json!({})))).await.unwrap();
    assert!(started.elapsed() < Duration::from_millis(1800), "{:?}", started.elapsed());
    assert_eq!(second.is_error, Some(true));
    let structured = second.structured_content.unwrap();
    assert_eq!(structured["kind"], "busy");
    assert!(structured["error"].as_str().unwrap().contains("nothing was sent"));
    let first = bounded(first).await.unwrap().unwrap();
    assert_ne!(first.is_error, Some(true), "the command already sent is not cancelled");
    assert_eq!(mock.state.act_hits.load(Ordering::SeqCst), 1, "the busy call never reached the game");
    hosted.shutdown().await;
}

#[tokio::test]
async fn hosted_answers_busy_without_sending_when_the_queue_wait_runs_out() {
    let mock = Mock::start();
    mock.state.act_delay_ms.store(1000, Ordering::SeqCst);
    let hosted = Hosted::start_with(&mock, |o| o.with_queue_wait(Duration::from_millis(300))).await;
    let (a, b) = (hosted.stateless_client(GOOD_TOKEN).await, hosted.stateless_client(GOOD_TOKEN).await);
    let first = tokio::spawn(async move { a.call_tool(call("peek", &json!({}))).await });
    tokio::time::sleep(Duration::from_millis(150)).await;
    let second = bounded(b.call_tool(call("peek", &json!({})))).await.unwrap();
    assert_eq!(second.structured_content.unwrap()["kind"], "busy");
    assert_ne!(bounded(first).await.unwrap().unwrap().is_error, Some(true));
    assert_eq!(mock.state.act_hits.load(Ordering::SeqCst), 1);
    hosted.shutdown().await;
}

#[tokio::test]
async fn hosted_gives_up_on_a_rate_limit_that_outlasts_the_timeout_without_sending_again() {
    let mock = Mock::start();
    mock.state.rate_limited_acts.store(100, Ordering::SeqCst);
    let hosted = Hosted::start_with(&mock, |o| o.with_limits(8, Duration::from_secs(1))).await;
    let client = hosted.stateless_client(GOOD_TOKEN).await;
    let result = bounded(client.call_tool(call("peek", &json!({})))).await.unwrap();
    assert_eq!(result.is_error, Some(true));
    assert_eq!(result.structured_content.unwrap()["kind"], "busy");
    assert_eq!(mock.state.act_hits.load(Ordering::SeqCst), 1, "no second attempt after the deadline");
    hosted.shutdown().await;
}

#[tokio::test]
async fn hosted_shutdown_lets_a_command_already_sent_finish() {
    let mock = Mock::start();
    mock.state.act_delay_ms.store(1000, Ordering::SeqCst);
    let hosted = Hosted::start(&mock).await;
    let client = hosted.stateless_client(GOOD_TOKEN).await;
    let call = tokio::spawn(async move { client.call_tool(call("peek", &json!({}))).await });
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(mock.state.act_hits.load(Ordering::SeqCst), 1, "the command is on the wire");
    bounded(hosted.shutdown()).await;
    assert!(mock.state.last_act_body.lock().unwrap().is_some(), "shutdown waited for it");
    let answer = bounded(call).await.unwrap().expect("the caller still gets its answer");
    assert_ne!(answer.is_error, Some(true));
}

#[tokio::test]
async fn stdio_closing_the_client_lets_a_command_already_sent_finish() {
    let mock = Mock::start();
    mock.state.act_delay_ms.store(1000, Ordering::SeqCst);
    let client = stdio_client(&mock, Some(GOOD_TOKEN)).await;
    let peer = client.peer().clone();
    let pending = tokio::spawn(async move { peer.call_tool(call("peek", &json!({}))).await });
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!(mock.state.act_hits.load(Ordering::SeqCst), 1, "the command is on the wire");
    bounded(client.cancel()).await.unwrap();
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert!(mock.state.last_act_body.lock().unwrap().is_some(), "the exit waited for it");
    let _ = pending.await;
}

#[tokio::test]
async fn hosted_forgets_a_cached_verdict_when_the_game_refuses_the_token_at_use() {
    let mock = Mock::start();
    let hosted = Hosted::start(&mock).await;
    let client = hosted.stateless_client(STALE_TOKEN).await;
    let result = client.call_tool(call("peek", &json!({}))).await.unwrap();
    assert_eq!(result.is_error, Some(true));
    let kind = result.structured_content.clone().unwrap()["kind"].clone();
    assert_eq!(kind, "auth");
    let hint = result.structured_content.unwrap()["hint"].as_str().unwrap().to_owned();
    assert_eq!(hint, "the agent token was refused");
    assert_eq!(mock.state.whoami_hits.load(Ordering::SeqCst), 1);
    client.list_all_tools().await.unwrap();
    assert_eq!(mock.state.whoami_hits.load(Ordering::SeqCst), 2, "the positive verdict was evicted");
    hosted.shutdown().await;
}
