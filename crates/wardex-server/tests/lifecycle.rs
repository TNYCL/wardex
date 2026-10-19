//! Lifecycle API regressions use a temporary SQLite database and fake agents.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::mpsc;
use wardex_core::driver::{
    Driver, DriverCommand, DriverError, DriverEvent, DriverHandle, SpawnSpec,
};
use wardex_core::manager::{AnswerInput, Manager};
use wardex_core::process::Launcher;
use wardex_core::store::Store;
use wardex_core::{Config, DriverKind, EventKind, QuestionItem, RunSpec, SessionId, SessionState};
use wardex_server::api::{self, Request, Response};
use wardex_server::socket::{self, Client, Control};

struct Agent(Arc<Mutex<Vec<String>>>);

impl Driver for Agent {
    fn kind(&self) -> DriverKind {
        DriverKind::Claude
    }
    fn spawn(&self, spec: SpawnSpec, _launcher: &Launcher) -> Result<DriverHandle, DriverError> {
        let (commands, mut rx) = mpsc::unbounded_channel();
        let (tx, events) = mpsc::unbounded_channel();
        let received = self.0.clone();
        tokio::spawn(async move {
            let _ = tx.send(DriverEvent::Event(EventKind::Started {
                driver_session_id: Some(format!("fake-{}", spec.session_id)),
                model: None,
                reasoning: None,
            }));
            let mut stubborn = false;
            while let Some(command) = rx.recv().await {
                match command {
                    DriverCommand::Send(text) => {
                        received.lock().unwrap().push(text.clone());
                        match text.as_str() {
                            "hang" => continue,
                            "ask" => {
                                let _ = tx.send(DriverEvent::Event(EventKind::Question {
                                    question_id: "q1".into(),
                                    items: vec![QuestionItem {
                                        id: "color".into(),
                                        header: None,
                                        text: "Color?".into(),
                                        options: Vec::new(),
                                        multi_select: false,
                                        allow_other: true,
                                    }],
                                }));
                                continue;
                            }
                            "stubborn" => stubborn = true,
                            _ => {}
                        }
                    }
                    DriverCommand::Shutdown | DriverCommand::Drain if stubborn => continue,
                    DriverCommand::Shutdown | DriverCommand::Drain | DriverCommand::Kill => {
                        let _ = tx.send(DriverEvent::Exited {
                            code: Some(0),
                            stderr_tail: Vec::new(),
                        });
                        return;
                    }
                    DriverCommand::Answer { .. } | DriverCommand::Interrupt => {}
                }
                let _ = tx.send(DriverEvent::Event(EventKind::Done {
                    result: Some("done".into()),
                    interrupted: false,
                    note: None,
                    git: None,
                }));
            }
        });
        Ok(DriverHandle {
            pid: None,
            unit: None,
            commands,
            events,
        })
    }
}

struct Harness {
    manager: Manager,
    received: Arc<Mutex<Vec<String>>>,
    dir: tempfile::TempDir,
    pool: sqlx::SqlitePool,
}

impl Harness {
    async fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("w.db")).await.unwrap();
        let pool = sqlx::SqlitePool::connect(&format!("sqlite://{}", store.path().display()))
            .await
            .unwrap();
        let received = Arc::new(Mutex::new(Vec::new()));
        let mut config = Config::default();
        config.sessions.stop_grace_secs = 0;
        let manager = Manager::new(
            config,
            store,
            vec![Arc::new(Agent(received.clone()))],
            Launcher::direct(),
        );
        Self {
            manager,
            received,
            dir,
            pool,
        }
    }

    fn spec(&self, prompt: &str) -> RunSpec {
        RunSpec {
            driver: Some(DriverKind::Claude),
            prompt: prompt.into(),
            cwd: self.dir.path().into(),
            ..Default::default()
        }
    }

    async fn run(&self, prompt: &str, state: SessionState) -> SessionId {
        let id = self
            .manager
            .run(self.spec(prompt))
            .await
            .unwrap()
            .session
            .id;
        tokio::time::timeout(Duration::from_secs(3), async {
            while self.manager.status(&id).await.unwrap().session.state != state {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        self.manager.store().flush().await.unwrap();
        id
    }

    async fn fail_writes(&self) {
        sqlx::query("CREATE TRIGGER reject_writes BEFORE INSERT ON events BEGIN SELECT RAISE(FAIL, 'injected write outage'); END").execute(&self.pool).await.unwrap();
    }

    async fn repair(&self) {
        sqlx::query("DROP TRIGGER reject_writes")
            .execute(&self.pool)
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(3), async {
            while self.manager.store().flush().await.is_err() {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
    }

    async fn socket(&self) -> (Client, Control, tokio::task::JoinHandle<()>) {
        let path = self.dir.path().join("api.sock");
        let listener = socket::bind(&path).await.unwrap();
        let (control, _reload) = Control::new();
        let server = tokio::spawn(socket::serve(
            listener,
            self.manager.clone(),
            control.clone(),
        ));
        (Client::new(path), control, server)
    }
}

#[tokio::test]
async fn applied_run_send_and_answer_report_uncommitted_state_without_resending() {
    let h = Harness::new().await;
    let question = h.run("ask", SessionState::WaitingInput).await;
    let idle = h.run("hello", SessionState::Idle).await;
    h.fail_writes().await;
    for request in [
        Request::Answer {
            id: question.to_string(),
            question_id: None,
            input: AnswerInput::Text("blue".into()),
        },
        Request::Send {
            id: idle.to_string(),
            message: "next".into(),
        },
        Request::Run(h.spec("new")),
    ] {
        let response = api::handle(&h.manager, request).await;
        assert!(response.ok, "{response:?}");
        assert_eq!(response.applied, Some(true));
        assert_eq!(response.persisted, Some(false));
        assert!(!response.warnings.is_empty());
        assert_eq!(response.client_data()["applied"], true);
        assert_eq!(response.client_data()["persisted"], false);
    }
    tokio::time::timeout(Duration::from_secs(2), async {
        while h.received.lock().unwrap().len() < 4 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(h.received.lock().unwrap().len(), 4);
    h.repair().await;
    assert!(h.manager.persistence().persisted);
    h.manager.shutdown(Duration::from_secs(1)).await.unwrap();
}

#[tokio::test]
async fn shutdown_signals_daemon_exit_even_when_final_flush_fails() {
    let h = Harness::new().await;
    let busy = h.run("hang", SessionState::Running).await;
    let idle = h.run("hello", SessionState::Idle).await;
    h.fail_writes().await;
    let _ = api::handle(
        &h.manager,
        Request::Send {
            id: idle.to_string(),
            message: "outage".into(),
        },
    )
    .await;
    let (client, control, server) = h.socket().await;
    for request in [
        Request::List { all: true },
        Request::Status {
            id: busy.to_string(),
        },
        Request::Interrupt {
            id: busy.to_string(),
        },
        Request::Stop {
            id: idle.to_string(),
        },
    ] {
        let response = client.request(&request).await.unwrap();
        assert!(response.ok, "{response:?}");
        assert_eq!(response.persisted, Some(false));
        assert!(!response.warnings.is_empty());
    }
    let response = client
        .request(&Request::Shutdown {
            drain_secs: None,
            restart: false,
            force: false,
        })
        .await
        .unwrap();
    assert!(response.ok, "{response:?}");
    assert_eq!(response.persisted, Some(false));
    assert_eq!(response.data.as_ref().unwrap()["shutting_down"], true);
    assert!(*control.shutdown.borrow());
    server.await.unwrap();
    assert!(h.manager.live_ids().is_empty());
    h.repair().await;
}

#[tokio::test]
async fn force_drain_reports_interrupted_sessions_and_persists_their_notes() {
    let h = Harness::new().await;
    let busy = h.run("hang", SessionState::Running).await;
    let question = h.run("ask", SessionState::WaitingInput).await;
    let idle = h.run("hello", SessionState::Idle).await;
    let (client, control, server) = h.socket().await;
    let refused = client
        .request(&Request::Shutdown {
            drain_secs: Some(0),
            restart: true,
            force: false,
        })
        .await
        .unwrap();
    assert!(!refused.ok);
    let data = refused.data.unwrap();
    assert_eq!(data["closed_sessions"], serde_json::json!([]));
    assert_eq!(data["next_steps"].as_array().unwrap().len(), 3);
    assert!(!*control.shutdown.borrow());
    let forced = client
        .request(&Request::Shutdown {
            drain_secs: Some(0),
            restart: true,
            force: true,
        })
        .await
        .unwrap();
    assert!(forced.ok, "{forced:?}");
    let report = forced.data.unwrap();
    let mut expected = vec![busy.clone(), question.clone()];
    expected.sort();
    assert_eq!(report["interrupted_sessions"], serde_json::json!(expected));
    assert_eq!(report["closed_sessions"].as_array().unwrap().len(), 3);
    assert_eq!(report["forced"], true);
    assert!(*control.shutdown.borrow());
    assert!(control.restart_requested());
    server.await.unwrap();
    for id in [busy, question] {
        let events = h.manager.events(&id, 0, 100).await.unwrap();
        assert!(events.iter().any(|event| matches!(&event.kind, EventKind::Done { interrupted: true, note: Some(note), .. } if note.contains("--force"))));
    }
    assert!(
        !h.manager
            .events(&idle, 0, 100)
            .await
            .unwrap()
            .iter()
            .any(|event| matches!(
                event.kind,
                EventKind::Done {
                    interrupted: true,
                    ..
                }
            ))
    );
}

#[tokio::test]
async fn partial_drain_error_includes_closed_sessions_and_operator_steps() {
    let h = Harness::new().await;
    let closed = h.run("hello", SessionState::Idle).await;
    let blocked = h.run("stubborn", SessionState::Idle).await;
    let error = h
        .manager
        .drain(Duration::ZERO, Duration::from_millis(100))
        .await
        .unwrap_err();
    let response = Response::from(error);
    let data = response.data.unwrap();
    assert_eq!(data["closed_sessions"], serde_json::json!([closed]));
    assert_eq!(data["blocked_sessions"], serde_json::json!([blocked]));
    let steps = data["next_steps"].to_string();
    for text in ["answer", "stop <id>", "--drain", "--force"] {
        assert!(steps.contains(text));
    }
    h.manager.shutdown(Duration::ZERO).await.unwrap();
}
