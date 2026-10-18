//! Session manager behavior against an in-process fake driver.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use tokio::sync::mpsc;
use wardex_core::driver::{
    Driver, DriverCommand, DriverError, DriverEvent, DriverHandle, READONLY_PREFIX, SpawnSpec,
};
use wardex_core::manager::{AnswerInput, Manager, ManagerError};
use wardex_core::process::Launcher;
use wardex_core::store::Store;
use wardex_core::time::now_ms;
use wardex_core::{
    Answer, Config, DriverKind, EventKind, QuestionItem, RunSpec, SessionId, SessionState,
    StuckReason, ToolStatus,
};

/// Scripted agent. Behavior depends on the message text:
/// `ask`, `ask2`, `approve`, `hang`, `tool-hang`, `crash`, anything else echoes.
struct FakeDriver {
    spawns: Arc<AtomicU32>,
    received: Arc<std::sync::Mutex<Vec<String>>>,
}

impl Driver for FakeDriver {
    fn kind(&self) -> DriverKind {
        DriverKind::Claude
    }

    fn spawn(&self, spec: SpawnSpec, _launcher: &Launcher) -> Result<DriverHandle, DriverError> {
        let n = self.spawns.fetch_add(1, Ordering::SeqCst) + 1;
        let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel();
        let (ev_tx, ev_rx) = mpsc::unbounded_channel();
        let received = self.received.clone();
        let dsid = spec
            .resume
            .clone()
            .unwrap_or_else(|| format!("fake-{}", spec.session_id));
        tokio::spawn(async move {
            let send = |k: EventKind| {
                let _ = ev_tx.send(DriverEvent::Event(k));
            };
            send(EventKind::Started {
                driver_session_id: Some(dsid),
                model: Some(format!("fake-model-{n}")),
            });
            let mut pending_tool = false;
            while let Some(cmd) = cmd_rx.recv().await {
                match cmd {
                    DriverCommand::Send(text) => {
                        received.lock().unwrap().push(text.clone());
                        let word = text.rsplit("\n\n").next().unwrap_or(&text).to_string();
                        match word.as_str() {
                            "ask" | "ask2" => {
                                let mut items = vec![item("color")];
                                if word == "ask2" {
                                    items.push(item("size"));
                                }
                                send(EventKind::Question {
                                    question_id: "q1".into(),
                                    items,
                                });
                            }
                            "approve" => send(EventKind::Approval {
                                request_id: "r1".into(),
                                kind: "command".into(),
                                summary: "rm -rf build".into(),
                            }),
                            "hang" => {}
                            "tool-hang" => {
                                pending_tool = true;
                                send(EventKind::ToolCall {
                                    call_id: "t1".into(),
                                    name: "Bash".into(),
                                    input_summary: "cargo build".into(),
                                    status: ToolStatus::Started,
                                    result_summary: None,
                                });
                            }
                            "crash" => {
                                let _ = ev_tx.send(DriverEvent::Exited {
                                    code: Some(3),
                                    stderr_tail: vec!["boom".into()],
                                });
                                return;
                            }
                            other => {
                                send(EventKind::Message {
                                    text: format!("echo {other}"),
                                    delta: true,
                                });
                                send(EventKind::Message {
                                    text: format!("echo {other}"),
                                    delta: false,
                                });
                                send(done(Some(format!("echo {other}")), false));
                            }
                        }
                    }
                    DriverCommand::Answer { answer, .. } => {
                        let text = match answer {
                            Answer::Question { items } => items
                                .values()
                                .map(|v| v.join(","))
                                .collect::<Vec<_>>()
                                .join(";"),
                            Answer::Approval { allow, .. } => format!("allow={allow}"),
                        };
                        send(done(Some(format!("answered {text}")), false));
                    }
                    DriverCommand::Interrupt => {
                        if pending_tool {
                            pending_tool = false;
                        }
                        send(done(None, true));
                    }
                    DriverCommand::Shutdown => {
                        let _ = ev_tx.send(DriverEvent::Exited {
                            code: Some(0),
                            stderr_tail: vec![],
                        });
                        return;
                    }
                    DriverCommand::Kill => {
                        let _ = ev_tx.send(DriverEvent::Exited {
                            code: None,
                            stderr_tail: vec![],
                        });
                        return;
                    }
                }
            }
        });
        Ok(DriverHandle {
            pid: None,
            unit: None,
            commands: cmd_tx,
            events: ev_rx,
        })
    }
}

fn item(id: &str) -> QuestionItem {
    QuestionItem {
        id: id.into(),
        header: None,
        text: format!("Which {id}?"),
        options: vec![],
        multi_select: false,
        allow_other: true,
    }
}

fn done(result: Option<String>, interrupted: bool) -> EventKind {
    EventKind::Done {
        result,
        interrupted,
        note: None,
        git: None,
    }
}

struct Harness {
    manager: Manager,
    received: Arc<std::sync::Mutex<Vec<String>>>,
    spawns: Arc<AtomicU32>,
    dir: tempfile::TempDir,
}

async fn harness_with(config: Config) -> Harness {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(&dir.path().join("w.db")).await.unwrap();
    let received = Arc::new(std::sync::Mutex::new(Vec::new()));
    let spawns = Arc::new(AtomicU32::new(0));
    let driver = Arc::new(FakeDriver {
        spawns: spawns.clone(),
        received: received.clone(),
    });
    let manager = Manager::new(config, store, vec![driver], Launcher::direct());
    Harness {
        manager,
        received,
        spawns,
        dir,
    }
}

async fn harness() -> Harness {
    harness_with(Config::default()).await
}

fn spec(h: &Harness, prompt: &str) -> RunSpec {
    RunSpec {
        driver: Some(DriverKind::Claude),
        prompt: prompt.into(),
        cwd: h.dir.path().to_path_buf(),
        agent: Some("agent-a".into()),
        task: Some("task-1".into()),
        ..Default::default()
    }
}

async fn wait_state(m: &Manager, id: &SessionId, state: SessionState) {
    for _ in 0..200 {
        if m.status(id).await.unwrap().session.state == state {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!(
        "session {id} never reached {state:?}; is {:?}",
        m.status(id).await.unwrap().session.state
    );
}

async fn kinds(m: &Manager, id: &SessionId) -> Vec<String> {
    m.events(id, 0, 1000)
        .await
        .unwrap()
        .into_iter()
        .map(|e| e.kind.name().to_string())
        .collect()
}

#[tokio::test]
async fn run_to_idle_and_persist() {
    let h = harness().await;
    let mut rx = h.manager.subscribe();
    let out = h.manager.run(spec(&h, "hello")).await.unwrap();
    let id = out.session.id.clone();
    assert!(out.warnings.is_empty());
    wait_state(&h.manager, &id, SessionState::Idle).await;

    let status = h.manager.status(&id).await.unwrap();
    assert!(status.live);
    assert_eq!(
        status.session.driver_session_id.as_deref(),
        Some(&*format!("fake-{id}"))
    );
    assert_eq!(status.session.last_result.as_deref(), Some("echo hello"));
    assert_eq!(status.session.agent.as_deref(), Some("agent-a"));

    // Deltas are broadcast but not persisted.
    let mut saw_delta = false;
    while let Ok(ev) = rx.try_recv() {
        saw_delta |= ev.kind.is_delta();
    }
    assert!(saw_delta);
    assert_eq!(
        kinds(&h.manager, &id).await,
        ["prompt", "started", "message", "done"]
    );
    let events = h.manager.events(&id, 0, 100).await.unwrap();
    let seqs: Vec<u64> = events.iter().map(|e| e.seq).collect();
    assert_eq!(seqs, [1, 2, 3, 4]);

    let summary = h.manager.summary(&id).await.unwrap();
    assert_eq!(summary.prompts, 1);
    assert_eq!(summary.turns, 1);
    assert_eq!(summary.last_result.as_deref(), Some("echo hello"));

    // Prefix resolution.
    assert_eq!(h.manager.resolve(&id.as_str()[..4]).await.unwrap(), id);
    assert!(matches!(
        h.manager.resolve("zzzzzzzzz").await,
        Err(ManagerError::NotFound(_))
    ));
}

#[tokio::test]
async fn question_and_answer() {
    let h = harness().await;
    let id = h.manager.run(spec(&h, "ask")).await.unwrap().session.id;
    wait_state(&h.manager, &id, SessionState::WaitingInput).await;
    let status = h.manager.status(&id).await.unwrap();
    assert_eq!(status.pending.len(), 1);
    assert_eq!(status.pending[0].kind, "question");
    assert_eq!(h.manager.pending_questions().await.unwrap().len(), 1);

    let qid = h
        .manager
        .answer(&id, None, AnswerInput::Text("blue".into()))
        .await
        .unwrap();
    assert_eq!(qid, "q1");
    wait_state(&h.manager, &id, SessionState::Idle).await;
    assert_eq!(
        h.manager
            .status(&id)
            .await
            .unwrap()
            .session
            .last_result
            .as_deref(),
        Some("answered blue")
    );
    assert!(h.manager.pending_questions().await.unwrap().is_empty());
    assert!(
        kinds(&h.manager, &id)
            .await
            .contains(&"answered".to_string())
    );

    // No pending question left.
    assert!(matches!(
        h.manager
            .answer(&id, None, AnswerInput::Text("x".into()))
            .await,
        Err(ManagerError::Conflict(_))
    ));
}

#[tokio::test]
async fn multi_question_needs_items() {
    let h = harness().await;
    let id = h.manager.run(spec(&h, "ask2")).await.unwrap().session.id;
    wait_state(&h.manager, &id, SessionState::WaitingInput).await;
    assert!(matches!(
        h.manager
            .answer(&id, None, AnswerInput::Text("blue".into()))
            .await,
        Err(ManagerError::Invalid(_))
    ));
    let mut items = BTreeMap::new();
    items.insert("color".to_string(), "blue".to_string());
    items.insert("size".to_string(), "L".to_string());
    h.manager
        .answer(&id, Some("q1"), AnswerInput::Items(items))
        .await
        .unwrap();
    wait_state(&h.manager, &id, SessionState::Idle).await;
    assert_eq!(
        h.manager
            .status(&id)
            .await
            .unwrap()
            .session
            .last_result
            .as_deref(),
        Some("answered blue;L")
    );
}

#[tokio::test]
async fn approval_decisions() {
    let h = harness().await;
    let id = h.manager.run(spec(&h, "approve")).await.unwrap().session.id;
    wait_state(&h.manager, &id, SessionState::WaitingInput).await;
    assert!(matches!(
        h.manager
            .answer(&id, None, AnswerInput::Text("maybe".into()))
            .await,
        Err(ManagerError::Invalid(_))
    ));
    h.manager
        .answer(&id, None, AnswerInput::Text("deny".into()))
        .await
        .unwrap();
    wait_state(&h.manager, &id, SessionState::Idle).await;
    assert_eq!(
        h.manager
            .status(&id)
            .await
            .unwrap()
            .session
            .last_result
            .as_deref(),
        Some("answered allow=false")
    );
}

fn fast_config() -> Config {
    let mut c = Config::default();
    c.timeouts.question_reminder_secs = 1;
    c.timeouts.question_stuck_secs = 2;
    c.timeouts.idle_stuck_secs = 2;
    c
}

async fn last_of(m: &Manager, id: &SessionId, name: &str) -> Option<EventKind> {
    m.events(id, 0, 1000)
        .await
        .unwrap()
        .into_iter()
        .rev()
        .find(|e| e.kind.name() == name)
        .map(|e| e.kind)
}

#[tokio::test]
async fn reminder_then_stuck_while_waiting() {
    let h = harness_with(fast_config()).await;
    let id = h.manager.run(spec(&h, "ask")).await.unwrap().session.id;
    wait_state(&h.manager, &id, SessionState::WaitingInput).await;
    h.manager.tick(now_ms() + 1_100);
    assert!(last_of(&h.manager, &id, "reminder").await.is_some());
    assert!(last_of(&h.manager, &id, "stuck").await.is_none());
    h.manager.tick(now_ms() + 2_100);
    assert!(matches!(
        last_of(&h.manager, &id, "stuck").await,
        Some(EventKind::Stuck {
            reason: StuckReason::WaitingInput,
            ..
        })
    ));
    // Fired once only.
    h.manager.tick(now_ms() + 5_000);
    let n = kinds(&h.manager, &id)
        .await
        .iter()
        .filter(|k| *k == "reminder" || *k == "stuck")
        .count();
    assert_eq!(n, 2);
}

#[tokio::test]
async fn stuck_without_output_but_not_while_a_tool_runs() {
    let h = harness_with(fast_config()).await;
    let hang = h.manager.run(spec(&h, "hang")).await.unwrap().session.id;
    let tool = h
        .manager
        .run(spec(&h, "tool-hang"))
        .await
        .unwrap()
        .session
        .id;
    wait_state(&h.manager, &hang, SessionState::Running).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    h.manager.tick(now_ms() + 3_000);
    assert!(matches!(
        last_of(&h.manager, &hang, "stuck").await,
        Some(EventKind::Stuck {
            reason: StuckReason::NoOutput,
            ..
        })
    ));
    assert!(last_of(&h.manager, &tool, "stuck").await.is_none());
    assert_eq!(
        h.manager.status(&tool).await.unwrap().running_tools,
        ["Bash"]
    );

    // Interrupt ends the hanging turn as an interrupted done.
    h.manager.interrupt(&hang).await.unwrap();
    wait_state(&h.manager, &hang, SessionState::Idle).await;
    assert!(matches!(
        last_of(&h.manager, &hang, "done").await,
        Some(EventKind::Done {
            interrupted: true,
            ..
        })
    ));
}

#[tokio::test]
async fn stop_then_send_resumes() {
    let h = harness().await;
    let id = h.manager.run(spec(&h, "hello")).await.unwrap().session.id;
    wait_state(&h.manager, &id, SessionState::Idle).await;
    h.manager.stop(&id).await.unwrap();
    wait_state(&h.manager, &id, SessionState::Stopped).await;
    assert!(!h.manager.status(&id).await.unwrap().live);

    let resumed = h.manager.send(&id, "again".into()).await.unwrap();
    assert!(resumed);
    wait_state(&h.manager, &id, SessionState::Idle).await;
    let st = h.manager.status(&id).await.unwrap();
    assert_eq!(st.session.spawn_count, 2);
    assert_eq!(st.session.last_result.as_deref(), Some("echo again"));
    // The driver id survives the resume.
    assert_eq!(
        st.session.driver_session_id.as_deref(),
        Some(&*format!("fake-{id}"))
    );
    assert_eq!(h.spawns.load(Ordering::SeqCst), 2);
    // Sequence numbers keep increasing across processes.
    let seqs: Vec<u64> = h
        .manager
        .events(&id, 0, 100)
        .await
        .unwrap()
        .iter()
        .map(|e| e.seq)
        .collect();
    assert!(seqs.windows(2).all(|w| w[0] < w[1]));
}

#[tokio::test]
async fn idle_shutdown_ends_as_done() {
    let mut config = Config::default();
    config.sessions.idle_shutdown_secs = 1;
    let h = harness_with(config).await;
    let id = h.manager.run(spec(&h, "hello")).await.unwrap().session.id;
    wait_state(&h.manager, &id, SessionState::Idle).await;
    h.manager.tick(now_ms() + 1_500);
    wait_state(&h.manager, &id, SessionState::Done).await;
    assert!(matches!(
        last_of(&h.manager, &id, "ended").await,
        Some(EventKind::Ended {
            state: SessionState::Done,
            ..
        })
    ));
}

#[tokio::test]
async fn crash_is_an_error_with_stderr() {
    let h = harness().await;
    let id = h.manager.run(spec(&h, "crash")).await.unwrap().session.id;
    wait_state(&h.manager, &id, SessionState::Error).await;
    let st = h.manager.status(&id).await.unwrap();
    let err = st.session.last_error.unwrap();
    assert!(err.contains("code 3") && err.contains("boom"), "{err}");
}

#[tokio::test]
async fn readonly_prefix_and_same_cwd_warning() {
    let h = harness().await;
    let first = h.manager.run(spec(&h, "hello")).await.unwrap();
    let mut ro = spec(&h, "look");
    ro.readonly = true;
    let second = h.manager.run(ro).await.unwrap();
    assert_eq!(second.warnings.len(), 1);
    assert!(second.warnings[0].contains(first.session.id.as_str()));
    wait_state(&h.manager, &second.session.id, SessionState::Idle).await;
    let received = h.received.lock().unwrap().clone();
    assert!(received.contains(&format!("{READONLY_PREFIX}look")));
    assert!(received.contains(&"hello".to_string()));
}

#[tokio::test]
async fn invalid_specs_are_rejected() {
    let h = harness().await;
    let mut s = spec(&h, "x");
    s.driver = Some(DriverKind::Codex);
    assert!(matches!(
        h.manager.run(s).await,
        Err(ManagerError::Invalid(_))
    ));
    let mut s = spec(&h, "x");
    s.slice = Some("nope.slice".into());
    assert!(matches!(
        h.manager.run(s).await,
        Err(ManagerError::Invalid(_))
    ));
    let mut s = spec(&h, "x");
    s.cwd = "relative".into();
    assert!(matches!(
        h.manager.run(s).await,
        Err(ManagerError::Invalid(_))
    ));
    let s = spec(&h, "  ");
    assert!(matches!(
        h.manager.run(s).await,
        Err(ManagerError::Invalid(_))
    ));
}

#[tokio::test]
async fn recover_marks_live_sessions_orphaned() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("w.db");
    let id;
    {
        let store = Store::open(&db).await.unwrap();
        let driver = Arc::new(FakeDriver {
            spawns: Arc::new(AtomicU32::new(0)),
            received: Arc::new(std::sync::Mutex::new(Vec::new())),
        });
        let m = Manager::new(
            Config::default(),
            store.clone(),
            vec![driver],
            Launcher::direct(),
        );
        let mut s = RunSpec {
            driver: Some(DriverKind::Claude),
            prompt: "hang".into(),
            cwd: dir.path().to_path_buf(),
            ..Default::default()
        };
        s.agent = Some("a".into());
        id = m.run(s).await.unwrap().session.id;
        wait_state(&m, &id, SessionState::Running).await;
        // Simulate a crash of the daemon: the store is closed, the session
        // row still says running.
        store.close().await;
    }
    let store = Store::open(&db).await.unwrap();
    let driver = Arc::new(FakeDriver {
        spawns: Arc::new(AtomicU32::new(0)),
        received: Arc::new(std::sync::Mutex::new(Vec::new())),
    });
    let m = Manager::new(Config::default(), store, vec![driver], Launcher::direct());
    assert_eq!(m.recover().await.unwrap(), vec![id.clone()]);
    let st = m.status(&id).await.unwrap();
    assert_eq!(st.session.state, SessionState::Orphaned);
    let list = m.list(true).await.unwrap();
    assert_eq!(list.len(), 1);
    // History is intact and resumable.
    assert_eq!(
        kinds(&m, &id).await,
        ["prompt", "started", "error", "ended"]
    );
    m.resume(&id, Some("hello".into())).await.unwrap();
    wait_state(&m, &id, SessionState::Idle).await;
}
