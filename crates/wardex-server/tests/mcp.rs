//! The MCP endpoint end to end: an rmcp client starts a session, is woken by a
//! webhook, answers the question and reads the result. The agent is a recorded
//! Claude session replayed by a shell script.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::post;
use rmcp::ServiceExt;
use rmcp::model::{CallToolRequestParams, CallToolResult};
use rmcp::service::{RoleClient, RunningService};
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use wardex_core::manager::{ExternalSetup, Manager};
use wardex_core::process::Launcher;
use wardex_core::store::Store;
use wardex_core::{Config, SessionState};
use wardex_drivers::{ClaudeDriver, ClaudeStore};
use wardex_server::http;
use wardex_server::webhook::{Target, spawn_target};

const TOKEN: &str = "test-token-0123456789";

fn replay_script(dir: &Path, fixture: &str) -> std::path::PathBuf {
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../wardex-drivers/tests/fixtures/claude")
        .join(fixture);
    let mut script = String::from("#!/bin/sh\n");
    for line in std::fs::read_to_string(fixture).unwrap().lines() {
        let rec: Value = serde_json::from_str(line).unwrap();
        match rec["dir"].as_str().unwrap() {
            "out" => {
                let l = rec["msg"].to_string().replace('\'', "'\\''");
                script.push_str(&format!("printf '%s\\n' '{l}'\n"));
            }
            "in" => script.push_str("IFS= read -r _ || exit 0\n"),
            _ => {}
        }
    }
    let path = dir.join("fake.sh");
    std::fs::write(&path, script).unwrap();
    path
}

struct Daemon {
    manager: Manager,
    url: String,
    cancel: CancellationToken,
    _dir: tempfile::TempDir,
}

async fn daemon(config: Config) -> Daemon {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(&dir.path().join("w.db")).await.unwrap();
    let script = replay_script(dir.path(), "question.jsonl");
    let driver = Arc::new(ClaudeDriver::with_command(
        "sh",
        vec![script.to_string_lossy().into_owned()],
    ));
    // The synthetic transcript fixtures as the external Claude store.
    let fixtures =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../wardex-drivers/tests/fixtures/claude/store");
    let external = ExternalSetup {
        sources: vec![Arc::new(ClaudeStore::new(fixtures, 16 * 1024 * 1024))],
        ..Default::default()
    };
    let manager = Manager::with_external(config, store, vec![driver], Launcher::direct(), external);
    let cancel = CancellationToken::new();
    let app = http::router(manager.clone(), TOKEN, cancel.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    Daemon {
        manager,
        url: format!("http://{addr}"),
        cancel,
        _dir: dir,
    }
}

async fn connect(url: &str) -> RunningService<RoleClient, ()> {
    let transport = StreamableHttpClientTransport::from_config(
        StreamableHttpClientTransportConfig::with_uri(format!("{url}/mcp")).auth_header(TOKEN),
    );
    ().serve(transport).await.expect("mcp handshake")
}

async fn call(client: &RunningService<RoleClient, ()>, tool: &str, args: Value) -> CallToolResult {
    let Value::Object(args) = args else {
        panic!("arguments must be an object")
    };
    client
        .call_tool(CallToolRequestParams::new(tool.to_string()).with_arguments(args))
        .await
        .unwrap_or_else(|e| panic!("{tool}: {e}"))
}

/// The JSON a successful tool call returned.
fn data(result: &CallToolResult) -> Value {
    assert_ne!(result.is_error, Some(true), "tool error: {result:?}");
    let text = result.content[0].as_text().expect("text content");
    serde_json::from_str(&text.text).unwrap()
}

fn error_text(result: &CallToolResult) -> String {
    assert_eq!(result.is_error, Some(true), "expected an error: {result:?}");
    result.content[0].as_text().unwrap().text.clone()
}

async fn hook_receiver() -> (String, mpsc::UnboundedReceiver<Value>) {
    async fn receive(State(tx): State<mpsc::UnboundedSender<Value>>, body: Bytes) -> StatusCode {
        let _ = tx.send(serde_json::from_slice(&body).unwrap());
        StatusCode::OK
    }
    let (tx, rx) = mpsc::unbounded_channel();
    let app = Router::new().route("/hook", post(receive)).with_state(tx);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}/hook"), rx)
}

async fn next_hook(rx: &mut mpsc::UnboundedReceiver<Value>) -> Value {
    tokio::time::timeout(Duration::from_secs(10), rx.recv())
        .await
        .expect("webhook delivery")
        .unwrap()
}

#[tokio::test]
async fn bot_runs_answers_and_reads_result_over_mcp() {
    let d = daemon(Config::default()).await;
    let (hook_url, mut hooks) = hook_receiver().await;
    spawn_target(
        Target {
            index: 0,
            url: hook_url,
            events: vec!["question".into(), "done".into()],
            secret: None,
            headers: Vec::new(),
            timeout: Duration::from_secs(5),
            max_attempts: 3,
            backoff: Duration::from_millis(20),
            ..Target::new("", &[])
        },
        d.manager.clone(),
    );
    let client = connect(&d.url).await;

    let info = client.peer_info().expect("server info");
    assert_eq!(info.server_info.as_ref().unwrap().name, "wardex");
    assert!(
        info.instructions
            .as_deref()
            .unwrap()
            .contains("pending_questions")
    );
    let listed_tools = client.list_all_tools().await.unwrap();
    for t in &listed_tools {
        assert_eq!(t.input_schema["type"], "object", "{}", t.name);
        assert!(t.description.is_some(), "{}", t.name);
    }
    let run_tool = listed_tools.iter().find(|t| t.name == "run").unwrap();
    assert!(
        !serde_json::to_string(&run_tool.input_schema)
            .unwrap()
            .contains("$ref")
    );
    let required = run_tool.input_schema["required"].as_array().unwrap();
    for field in ["prompt", "cwd", "driver"] {
        assert!(required.contains(&json!(field)), "run requires {field}");
    }
    let mut tools: Vec<String> = listed_tools.iter().map(|t| t.name.to_string()).collect();
    tools.sort();
    assert_eq!(
        tools,
        [
            "answer",
            "interrupt",
            "list",
            "overview",
            "pending_questions",
            "resume",
            "run",
            "send",
            "status",
            "stop",
            "summary",
            "tail",
            "tool_output"
        ]
    );

    let cwd = d._dir.path().to_string_lossy().into_owned();
    let run = data(
        &call(
            &client,
            "run",
            json!({"prompt": "ask me", "cwd": cwd, "driver": "claude", "agent": "bot", "task": "t1"}),
        )
        .await,
    );
    let id = run["session"]["id"].as_str().unwrap().to_string();

    // The webhook wakes the bot; it pulls the details over MCP.
    let hook = next_hook(&mut hooks).await;
    assert_eq!(hook["event"]["type"], "question");
    assert_eq!(hook["session"]["id"], id.as_str());

    let pending = data(&call(&client, "pending_questions", json!({})).await);
    let pending = pending.as_array().unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0]["session_id"], id.as_str());
    let question_id = pending[0]["question_id"].as_str().unwrap().to_string();

    let overview = data(&call(&client, "overview", json!({})).await);
    assert_eq!(overview["counts"]["waiting_input"], 1);
    assert_eq!(overview["pending_questions"], 1);
    let live = &overview["live"][0];
    assert_eq!(live["id"], id.as_str());
    assert_eq!(live["agent"], "bot");
    assert_eq!(live["pending"][0]["question_id"], question_id.as_str());
    assert!(live["latest"].as_array().unwrap().len() <= 3);
    assert_eq!(
        live["latest"].as_array().unwrap().last().unwrap()["type"],
        "question"
    );

    // Prefix ids work; an answer needs exactly one of text / answers.
    let err = error_text(&call(&client, "answer", json!({"id": &id[..4]})).await);
    assert!(err.starts_with("invalid:"), "{err}");
    let seq = data(&call(&client, "status", json!({"id": &id[..4]})).await)["last_seq"]
        .as_u64()
        .unwrap();
    let answered = data(
        &call(
            &client,
            "answer",
            json!({"id": &id[..4], "question_id": question_id, "text": "blue"}),
        )
        .await,
    );
    assert_eq!(answered["question_id"], question_id.as_str());

    // Long-poll tail until the turn's result arrives.
    let mut since = seq;
    let mut result = None;
    for _ in 0..20 {
        let events = data(
            &call(
                &client,
                "tail",
                json!({"id": id, "since_seq": since, "wait_secs": 5}),
            )
            .await,
        );
        for e in events.as_array().unwrap() {
            since = e["seq"].as_u64().unwrap();
            if e["type"] == "done" {
                result = e["payload"]["result"].as_str().map(String::from);
            }
        }
        if result.is_some() {
            break;
        }
    }
    assert_eq!(result.as_deref(), Some("blue"));
    assert_eq!(next_hook(&mut hooks).await["event"]["type"], "done");

    for _ in 0..100 {
        if d.manager.status(&run_id(&id)).await.unwrap().session.state == SessionState::Done {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let summary = data(&call(&client, "summary", json!({"id": id})).await);
    assert_eq!(summary["last_result"], "blue");
    assert_eq!(summary["state"], "done");

    let overview = data(&call(&client, "overview", json!({"events": 0})).await);
    assert_eq!(overview["live"].as_array().unwrap().len(), 0);
    assert_eq!(overview["recently_ended"][0]["id"], id.as_str());
    assert_eq!(overview["recently_ended"][0]["last_result"], "blue");

    let listed = data(&call(&client, "list", json!({"all": true})).await);
    assert_eq!(listed.as_array().unwrap().len(), 1);
    // Find it again by label, among ended and resumable sessions.
    let found = data(&call(&client, "list", json!({"ended": true, "agent": "b*"})).await);
    assert_eq!(found[0]["id"], id.as_str());
    assert_eq!(found[0]["ended_reason"], "done");
    let found = data(&call(&client, "list", json!({"resumable": true, "task": "t?"})).await);
    assert_eq!(found.as_array().unwrap().len(), 1);
    let found = data(&call(&client, "list", json!({"all": true, "agent": "other"})).await);
    assert!(found.as_array().unwrap().is_empty());
    let found = data(&call(&client, "list", json!({"agent": "bot"})).await);
    assert!(
        found.as_array().unwrap().is_empty(),
        "ended sessions need all/ended"
    );

    let send_tool = listed_tools.iter().find(|t| t.name == "send").unwrap();
    assert_eq!(send_tool.input_schema["required"], json!(["message"]));
    let err = error_text(&call(&client, "send", json!({"message": "x"})).await);
    assert!(err.starts_with("invalid:"), "{err}");
    let err = error_text(
        &call(
            &client,
            "send",
            json!({"latest": true, "agent": "nobody", "message": "x"}),
        )
        .await,
    );
    assert!(err.starts_with("not_found:"), "{err}");
    for args in [
        json!({"id": id, "latest": true, "agent": "bot", "message": "x"}),
        json!({"id": id, "agent": "bot", "message": "x"}),
        json!({"latest": true, "message": "x"}),
    ] {
        let err = error_text(&call(&client, "send", args.clone()).await);
        assert!(err.starts_with("invalid:"), "{args}: {err}");
    }
    let sent = data(
        &call(
            &client,
            "send",
            json!({"latest": true, "agent": "bot", "task": "t1", "message": "again"}),
        )
        .await,
    );
    assert_eq!(sent["session_id"], id.as_str());
    assert_eq!(sent["resumed"], true);
    data(&call(&client, "stop", json!({"id": id})).await);
    let err = error_text(&call(&client, "status", json!({"id": "ffffffff"})).await);
    assert!(err.starts_with("not_found:"), "{err}");

    // `tail` without new events returns after the wait.
    let started = std::time::Instant::now();
    let events = data(
        &call(
            &client,
            "tail",
            json!({"id": id, "since_seq": 10_000, "wait_secs": 1}),
        )
        .await,
    );
    assert!(events.as_array().unwrap().is_empty());
    assert!(started.elapsed() >= Duration::from_millis(900));

    client.cancel().await.unwrap();
    d.cancel.cancel();
}

fn run_id(id: &str) -> wardex_core::SessionId {
    wardex_core::SessionId(id.to_string())
}

/// Clients on an older protocol version use plain JSON-RPC POSTs without a
/// session id.
async fn tool_names(client: &RunningService<RoleClient, ()>) -> Vec<String> {
    client
        .list_all_tools()
        .await
        .unwrap()
        .into_iter()
        .map(|t| t.name.to_string())
        .collect()
}

#[tokio::test]
async fn external_tools_need_both_switches() {
    let mut only_enabled = Config::default();
    only_enabled.external.enabled = true;
    for config in [Config::default(), only_enabled] {
        let d = daemon(config).await;
        let client = connect(&d.url).await;
        let names = tool_names(&client).await;
        assert!(names.contains(&"overview".to_string()));
        assert!(
            !names.iter().any(|n| n == "external_list" || n == "history"),
            "{names:?}"
        );
        let refused = client
            .call_tool(
                CallToolRequestParams::new("history")
                    .with_arguments(json!({"id": "11111111"}).as_object().unwrap().clone()),
            )
            .await;
        assert!(
            refused.is_err() || refused.unwrap().is_error == Some(true),
            "hidden tools are not callable"
        );
        client.cancel().await.unwrap();
        d.cancel.cancel();
    }
}

#[tokio::test]
async fn external_sessions_over_mcp() {
    let mut config = Config::default();
    config.external.enabled = true;
    config.external.mcp = true;
    // The fixtures may have been checked out moments ago.
    config.external.quiet_secs = 0;
    let d = daemon(config).await;
    let client = connect(&d.url).await;
    let names = tool_names(&client).await;
    assert!(names.contains(&"external_list".to_string()));
    assert!(names.contains(&"history".to_string()));

    let list = data(&call(&client, "external_list", json!({"cwd": "/work/app"})).await);
    let sessions = list["sessions"].as_array().unwrap();
    assert_eq!(sessions.len(), 1);
    assert_eq!(
        sessions[0]["native_id"],
        "11111111-1111-4111-8111-111111111111"
    );
    assert_eq!(sessions[0]["liveness"]["state"], "inactive");
    assert!(sessions[0].get("path").is_none());

    let page = data(
        &call(
            &client,
            "history",
            json!({"id": "claude:11111111", "turns": 1, "tools": "full"}),
        )
        .await,
    );
    let items = page["items"].as_array().unwrap();
    assert_eq!(items[0]["text"], "Run the tests");
    assert_eq!(page["next_before"], "7");
    assert!(page.to_string().contains("API_KEY=[REDACTED]"));
    let older = data(&call(&client, "history", json!({"id": "11111111", "before": "7"})).await);
    assert_eq!(older["items"][0]["text"], "Add a health check endpoint");
    assert!(error_text(&call(&client, "history", json!({"id": "1111"})).await).contains("invalid"));

    // Adopt over MCP has no `force`; the session record is created.
    let adopted = data(
        &call(
            &client,
            "adopt",
            json!({"id": "claude:33333333", "agent": "bot"}),
        )
        .await,
    );
    assert_eq!(adopted["adopted"], true);
    assert_eq!(adopted["session"]["origin"], "adopted");
    assert_eq!(adopted["session"]["state"], "stopped");
    let tools = client.list_all_tools().await.unwrap();
    let adopt = tools.iter().find(|t| t.name == "adopt").unwrap();
    let props = adopt.input_schema.get("properties");
    assert!(
        props.and_then(|p| p.get("force")).is_none(),
        "no force over MCP"
    );
    let listed = data(&call(&client, "list", json!({"all": true, "origin": "adopted"})).await);
    assert_eq!(listed.as_array().unwrap().len(), 1);
    client.cancel().await.unwrap();
    d.cancel.cancel();
}

#[tokio::test]
async fn stateless_json_rpc_with_older_protocol() {
    let d = daemon(Config::default()).await;
    let http = reqwest::Client::new();
    let post = |body: Value| {
        http.post(format!("{}/mcp", d.url))
            .bearer_auth(TOKEN)
            .header("accept", "application/json, text/event-stream")
            .header("mcp-protocol-version", "2025-06-18")
            .json(&body)
    };
    let init = post(json!({
        "jsonrpc": "2.0", "id": 1, "method": "initialize",
        "params": {"protocolVersion": "2025-06-18", "capabilities": {},
                   "clientInfo": {"name": "raw", "version": "1"}}
    }))
    .send()
    .await
    .unwrap();
    assert_eq!(init.status(), 200);
    assert!(init.headers().get("mcp-session-id").is_none());
    let init: Value = init.json().await.unwrap();
    assert_eq!(init["result"]["serverInfo"]["name"], "wardex");

    let resp: Value = post(json!({
        "jsonrpc": "2.0", "id": 2, "method": "tools/call",
        "params": {"name": "overview", "arguments": {}}
    }))
    .send()
    .await
    .unwrap()
    .json()
    .await
    .unwrap();
    let text = resp["result"]["content"][0]["text"].as_str().unwrap();
    let overview: Value = serde_json::from_str(text).unwrap();
    assert_eq!(overview["live"], json!([]));
    d.cancel.cancel();
}

#[tokio::test]
async fn requires_token_and_limits_failures() {
    let mut config = Config::default();
    config.http.auth_failures_per_minute = 3;
    let d = daemon(config).await;
    let http = reqwest::Client::new();
    let mcp = format!("{}/mcp", d.url);
    let body = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"});

    let health: Value = http
        .get(format!("{}/healthz", d.url))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(health["ok"], true);
    assert!(health.get("live_sessions").is_none());
    let health: Value = http
        .get(format!("{}/healthz", d.url))
        .bearer_auth(TOKEN)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(health["live_sessions"], 0);

    let resp = http.post(&mcp).json(&body).send().await.unwrap();
    assert_eq!(resp.status(), 401);
    assert_eq!(resp.headers()["www-authenticate"], "Bearer");
    for _ in 0..2 {
        let resp = http
            .post(&mcp)
            .bearer_auth("wrong")
            .json(&body)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 401);
    }
    let resp = http
        .post(&mcp)
        .bearer_auth("wrong")
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 429);

    // The right token is never limited.
    let client = connect(&d.url).await;
    assert_eq!(client.list_all_tools().await.unwrap().len(), 13);
    client.cancel().await.unwrap();
    d.cancel.cancel();
}

#[tokio::test]
async fn host_header_is_checked() {
    let host_status = |url: String, host: &'static str| async move {
        reqwest::Client::new()
            .post(format!("{url}/mcp"))
            .bearer_auth(TOKEN)
            .header("host", host)
            .header("accept", "application/json, text/event-stream")
            .json(&json!({
                "jsonrpc": "2.0", "id": 1, "method": "initialize",
                "params": {"protocolVersion": "2025-06-18", "capabilities": {},
                           "clientInfo": {"name": "raw", "version": "1"}}
            }))
            .send()
            .await
            .unwrap()
            .status()
    };

    let d = daemon(Config::default()).await;
    assert_eq!(host_status(d.url.clone(), "wardex.example.com").await, 403);
    assert_eq!(host_status(d.url.clone(), "localhost").await, 200);
    d.cancel.cancel();

    let mut config = Config::default();
    config.http.allowed_hosts = vec!["wardex.example.com".into()];
    let d = daemon(config).await;
    assert_eq!(host_status(d.url.clone(), "wardex.example.com").await, 200);
    assert_eq!(host_status(d.url.clone(), "other.example.com").await, 403);
    d.cancel.cancel();
}
