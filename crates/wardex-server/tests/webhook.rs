//! Webhook delivery against a local receiver, driven by a recorded Claude
//! session replayed by a shell script.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use axum::Router;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::routing::post;
use serde_json::Value;
use tokio::sync::mpsc;
use wardex_core::manager::{AnswerInput, Manager};
use wardex_core::process::Launcher;
use wardex_core::store::Store;
use wardex_core::time::now_ms;
use wardex_core::{Config, DriverKind, RunSpec, SessionState};
use wardex_drivers::ClaudeDriver;
use wardex_server::webhook::{Target, signature, spawn_target};

#[derive(Clone)]
struct Receiver {
    tx: mpsc::UnboundedSender<(HeaderMap, Bytes)>,
    hits: Arc<AtomicUsize>,
}

async fn receive(State(r): State<Receiver>, headers: HeaderMap, body: Bytes) -> StatusCode {
    let n = r.hits.fetch_add(1, Ordering::SeqCst);
    let _ = r.tx.send((headers, body));
    // Fail the very first delivery to exercise the retry.
    if n == 0 {
        StatusCode::INTERNAL_SERVER_ERROR
    } else {
        StatusCode::OK
    }
}

fn replay_script(dir: &Path) -> std::path::PathBuf {
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../wardex-drivers/tests/fixtures/claude/question.jsonl");
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

async fn next(rx: &mut mpsc::UnboundedReceiver<(HeaderMap, Bytes)>) -> (HeaderMap, Value) {
    let (h, b) = tokio::time::timeout(Duration::from_secs(10), rx.recv())
        .await
        .expect("webhook delivery")
        .unwrap();
    (h, serde_json::from_slice(&b).unwrap())
}

#[tokio::test]
async fn delivers_notable_events_with_retry_and_signature() {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let receiver = Receiver {
        tx,
        hits: Arc::new(AtomicUsize::new(0)),
    };
    let app = Router::new()
        .route("/hook", post(receive))
        .with_state(receiver.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let dir = tempfile::tempdir().unwrap();
    let mut config = Config::default();
    config.timeouts.question_reminder_secs = 1;
    let store = Store::open(&dir.path().join("w.db")).await.unwrap();
    let script = replay_script(dir.path());
    let driver = Arc::new(ClaudeDriver::with_command(
        "sh",
        vec![script.to_string_lossy().into_owned()],
    ));
    let manager = Manager::new(config, store, vec![driver], Launcher::direct());
    spawn_target(
        Target {
            index: 0,
            url: format!("http://{addr}/hook"),
            events: vec!["question".into(), "reminder".into(), "done".into()],
            secret: Some("s3cret".into()),
            headers: vec![("x-extra".into(), "yes".into())],
            timeout: Duration::from_secs(5),
            max_attempts: 3,
            backoff: Duration::from_millis(20),
        },
        manager.clone(),
    );

    let id = manager
        .run(RunSpec {
            driver: Some(DriverKind::Claude),
            prompt: "ask".into(),
            cwd: dir.path().to_path_buf(),
            agent: Some("bot".into()),
            ..Default::default()
        })
        .await
        .unwrap()
        .session
        .id;

    // First attempt (500) and its retry carry the same delivery id.
    let (h1, b1) = next(&mut rx).await;
    let (h2, b2) = next(&mut rx).await;
    assert_eq!(b1, b2);
    assert_eq!(h1["x-wardex-delivery"], h2["x-wardex-delivery"]);
    assert_eq!(h2["x-wardex-event"], "question");
    assert_eq!(h2["x-extra"], "yes");
    assert_eq!(b2["event"]["type"], "question");
    assert_eq!(b2["session"]["agent"], "bot");
    assert_eq!(b2["session"]["state"], "waiting_input");
    let body = serde_json::to_vec(&b2).unwrap();
    assert_eq!(
        h2["x-wardex-signature"],
        signature("s3cret", &body).as_str()
    );

    // Reminder from the supervision loop.
    manager.tick(now_ms() + 1_500);
    let (h, b) = next(&mut rx).await;
    assert_eq!(h["x-wardex-event"], "reminder");
    assert_eq!(
        b["event"]["payload"]["question_id"],
        b2["event"]["payload"]["question_id"]
    );

    manager
        .answer(&id, None, AnswerInput::Text("blue".into()))
        .await
        .unwrap();
    let (h, b) = next(&mut rx).await;
    assert_eq!(h["x-wardex-event"], "done");
    assert_eq!(b["event"]["payload"]["result"], "blue");

    // Only the configured event types were sent.
    for _ in 0..50 {
        if manager.status(&id).await.unwrap().session.state == SessionState::Done {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(rx.try_recv().is_err());
    assert_eq!(receiver.hits.load(Ordering::SeqCst), 4);
}
