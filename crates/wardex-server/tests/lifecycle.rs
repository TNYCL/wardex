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
        Request::send(idle.to_string(), "next"),
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
    let _ = api::handle(&h.manager, Request::send(idle.to_string(), "outage")).await;
    let (client, control, server) = h.socket().await;
    for request in [
        Request::list(true),
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

#[tokio::test]
async fn cleanup_failed_drain_recommends_resume_or_force_in_error_and_data() {
    use std::os::unix::fs::PermissionsExt;

    let h = Harness::new().await;
    let blocked = h.run("hello", SessionState::Idle).await;
    h.manager.shutdown(Duration::from_secs(1)).await.unwrap();
    let mut record = h
        .manager
        .store()
        .get_session(&blocked)
        .await
        .unwrap()
        .unwrap();
    record.unit = Some("unverifiable.scope".into());
    h.manager.store().update_session(&record).await.unwrap();
    let controller = h.dir.path().join("scope-controller.sh");
    std::fs::write(&controller, "#!/bin/sh\nexit 1\n").unwrap();
    std::fs::set_permissions(&controller, std::fs::Permissions::from_mode(0o755)).unwrap();
    let manager = Manager::new(
        Config::default(),
        h.manager.store().clone(),
        vec![],
        Launcher::with_scope_command(controller),
    );
    assert!(manager.resume(&blocked, None).await.is_err());
    let error = manager
        .drain(Duration::ZERO, Duration::ZERO)
        .await
        .unwrap_err();
    let response = Response::from(error);
    let text = response.error.unwrap();
    assert!(text.contains("resume <id>"), "{text}");
    assert!(text.contains("--force"), "{text}");
    assert!(!text.contains("stop <id>"), "{text}");
    let data = response.data.unwrap();
    assert_eq!(data["blocked_sessions"], serde_json::json!([blocked]));
    assert_eq!(data["cleanup_failed"], serde_json::json!([blocked]));
    let steps = data["next_steps"].to_string();
    assert!(steps.contains("resume <id>"), "{steps}");
    assert!(steps.contains("--force"), "{steps}");
    assert!(!steps.contains("stop <id>"), "{steps}");
}

#[tokio::test]
async fn daemon_starts_with_failed_scope_and_keeps_quarantine_across_restarts() {
    use std::collections::BTreeMap;
    use std::os::unix::fs::PermissionsExt;
    use wardex_core::config::Env;
    use wardex_server::daemon::{self, Exit, Source};

    let h = Harness::new().await;
    let blocked = h.run("hello", SessionState::Idle).await;
    let healthy = h.run("hello", SessionState::Idle).await;
    h.manager.shutdown(Duration::from_secs(1)).await.unwrap();
    for (id, unit) in [(&blocked, "unverifiable.scope"), (&healthy, "empty.scope")] {
        let mut record = h.manager.store().get_session(id).await.unwrap().unwrap();
        record.state = SessionState::Idle;
        record.unit = Some(unit.into());
        h.manager.store().update_session(&record).await.unwrap();
    }
    h.manager.store().close().await.unwrap();
    h.pool.close().await;
    let controller = h.dir.path().join("scope-controller.sh");
    std::fs::write(
        &controller,
        r##"#!/bin/sh
cd "$(dirname "$0")"
if [ "$2" = show ]; then
    if [ "$4" = unverifiable.scope ] && [ ! -e repaired ]; then exit 1; fi
    printf 'LoadState=loaded\nActiveState=inactive\nTasksCurrent=0\n'
fi
"##,
    )
    .unwrap();
    std::fs::set_permissions(&controller, std::fs::Permissions::from_mode(0o755)).unwrap();
    let mut config = Config::default();
    config.daemon.database = h.manager.store().path().to_string_lossy().into_owned();
    config.daemon.socket = h
        .dir
        .path()
        .join("daemon.sock")
        .to_string_lossy()
        .into_owned();
    config.daemon.env_file = h
        .dir
        .path()
        .join("unused.env")
        .to_string_lossy()
        .into_owned();
    config.sessions.auto_resume_orphaned = true;
    // The healthy session was idle; bring it back at start.
    config.sessions.auto_resume_idle = true;
    config.sessions.stop_grace_secs = 0;
    config.auth.token_env = "WARDEX_LIFECYCLE_TEST_TOKEN".into();
    for attempt in 0..2 {
        if attempt == 1 {
            // Repair alone must not enable automatic resume on a new daemon.
            std::fs::write(h.dir.path().join("repaired"), "").unwrap();
        }
        let received = h.received.clone();
        let daemon = tokio::spawn(daemon::run_with(
            Source {
                config_path: Some(h.dir.path().join("unused.toml")),
                socket: None,
            },
            config.clone(),
            Env::with(BTreeMap::from([(
                config.auth.token_env.clone(),
                String::new(),
            )])),
            move |_| vec![Arc::new(Agent(received.clone())) as Arc<dyn Driver>],
            Launcher::with_scope_command(controller.clone()),
        ));
        let client = Client::new(config.socket_path());
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                assert!(!daemon.is_finished(), "failed scope stopped daemon startup");
                if let Ok(response) = client.request(&Request::Ping).await {
                    assert!(response.ok, "{response:?}");
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        let status = client
            .request(&Request::Status {
                id: blocked.to_string(),
            })
            .await
            .unwrap();
        assert!(status.ok, "{status:?}");
        let status = status.data.unwrap();
        assert_eq!(status["cleanup_failed"], true);
        assert_eq!(status["unit"], "unverifiable.scope");
        assert_eq!(status["live"], false);
        assert!(
            status["last_error"]
                .as_str()
                .unwrap()
                .contains("wardex resume")
        );
        let status = client
            .request(&Request::Status {
                id: healthy.to_string(),
            })
            .await
            .unwrap();
        assert_eq!(status.data.unwrap()["live"], true);
        let received = h.received.lock().unwrap().clone();
        let send = client
            .request(&Request::send(
                blocked.to_string(),
                "implicit resume must stay blocked",
            ))
            .await
            .unwrap();
        assert!(!send.ok, "send bypassed the cleanup quarantine");
        assert!(send.error.unwrap().contains("wardex resume <id>"));
        assert_eq!(*h.received.lock().unwrap(), received);
        let resume = client
            .request(&Request::Resume {
                id: blocked.to_string(),
                message: None,
            })
            .await
            .unwrap();
        if attempt == 0 {
            assert!(!resume.ok, "an unverifiable scope allowed a new spawn");
            assert!(resume.error.unwrap().contains("cleanup_failed"));
        } else {
            assert!(resume.ok, "{resume:?}");
            assert_eq!(resume.data.unwrap()["cleanup_failed"], false);
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
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(3), daemon)
                .await
                .unwrap()
                .unwrap()
                .unwrap(),
            Exit::Stopped
        );
    }
}
