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
        let dsid = (!spec.cwd.join("omit-resume-id").exists()).then_some(dsid);
        let release_eof = spec.cwd.join("release-eof");
        tokio::spawn(async move {
            let send = |k: EventKind| {
                let _ = ev_tx.send(DriverEvent::Event(k));
            };
            send(EventKind::Started {
                driver_session_id: dsid,
                model: Some(format!("fake-model-{n}")),
                reasoning: Some("fake-effort".into()),
            });
            let mut pending_tool = false;
            let mut ignore_eof = false;
            loop {
                let cmd = tokio::select! {
                    command = cmd_rx.recv() => match command { Some(command) => command, None => return },
                    _ = async {
                        if !ignore_eof { std::future::pending::<()>().await; }
                        while !release_eof.exists() { tokio::time::sleep(Duration::from_millis(5)).await; }
                    } => {
                        let _ = ev_tx.send(DriverEvent::Exited { code: Some(0), stderr_tail: Vec::new() });
                        return;
                    }
                };
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
                            "idle-ignore-eof" => {
                                ignore_eof = true;
                                send(done(Some("idle".into()), false));
                            }
                            "tool-hang" => {
                                pending_tool = true;
                                send(EventKind::ToolCall {
                                    call_id: "t1".into(),
                                    name: "Bash".into(),
                                    input_summary: "cargo build".into(),
                                    status: ToolStatus::Started,
                                    result_summary: None,
                                    input: None,
                                    output: None,
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
                    DriverCommand::Shutdown | DriverCommand::Drain => {
                        if ignore_eof {
                            continue;
                        }
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
    // The CLI-reported effort is recorded when none was given.
    assert_eq!(status.session.reasoning.as_deref(), Some("fake-effort"));
    // The duration stops at the end of the last turn, not at `summary` time.
    let turn_end = summary.last_turn_ended_ms.unwrap();
    assert_eq!(summary.duration_ms, turn_end - summary.created_ms);
    assert!(summary.last_turn_ms.unwrap() >= 0);
    assert!(
        summary.ended_ms.is_none(),
        "idle: the process is still alive"
    );
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(
        h.manager.summary(&id).await.unwrap().duration_ms,
        summary.duration_ms
    );

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
async fn delayed_stop_does_not_kill_a_resumed_process() {
    let mut config = Config::default();
    config.sessions.stop_grace_secs = 1;
    let h = harness_with(config).await;
    let id = h.manager.run(spec(&h, "hello")).await.unwrap().session.id;
    wait_state(&h.manager, &id, SessionState::Idle).await;
    h.manager.stop(&id).await.unwrap();
    wait_state(&h.manager, &id, SessionState::Stopped).await;
    h.manager.resume(&id, None).await.unwrap();
    wait_state(&h.manager, &id, SessionState::Idle).await;
    tokio::time::sleep(Duration::from_millis(1200)).await;
    let status = h.manager.status(&id).await.unwrap();
    assert!(status.live, "the old stop timer killed the new process");
    assert_eq!(status.session.spawn_count, 2);
    h.manager.stop(&id).await.unwrap();
}

#[tokio::test]
async fn simultaneous_resumes_spawn_only_one_process() {
    let h = harness().await;
    let id = h.manager.run(spec(&h, "hello")).await.unwrap().session.id;
    wait_state(&h.manager, &id, SessionState::Idle).await;
    h.manager.stop(&id).await.unwrap();
    wait_state(&h.manager, &id, SessionState::Stopped).await;
    let (first, second) = tokio::join!(h.manager.resume(&id, None), h.manager.resume(&id, None),);
    assert_eq!(usize::from(first.is_ok()) + usize::from(second.is_ok()), 1);
    let error = first.err().or_else(|| second.err()).unwrap();
    assert!(matches!(error, ManagerError::Conflict(_)));
    assert_eq!(h.spawns.load(Ordering::SeqCst), 2);
    h.manager.stop(&id).await.unwrap();
}

#[tokio::test]
async fn persistence_failure_reaches_history_clients() {
    let h = harness().await;
    let id = h.manager.run(spec(&h, "hello")).await.unwrap().session.id;
    wait_state(&h.manager, &id, SessionState::Idle).await;
    h.manager.events(&id, 0, 100).await.unwrap();
    let pool =
        sqlx::SqlitePool::connect(&format!("sqlite://{}", h.manager.store().path().display()))
            .await
            .unwrap();
    sqlx::query("CREATE TRIGGER reject_events BEFORE INSERT ON events BEGIN SELECT RAISE(FAIL, 'injected write failure'); END")
        .execute(&pool).await.unwrap();
    let send_result = h.manager.send(&id, "another turn".into()).await;
    let result = h.manager.events(&id, 0, 100).await;
    sqlx::query("DROP TRIGGER reject_events")
        .execute(&pool)
        .await
        .unwrap();
    pool.close().await;
    assert!(
        send_result.is_ok(),
        "an applied send must not be reported as unapplied"
    );
    assert!(!h.manager.persistence().persisted);
    assert!(
        result.is_err(),
        "a failed event write was reported as successful history"
    );
}

#[tokio::test]
async fn writer_fault_does_not_block_control_or_live_snapshots() {
    let h = harness().await;
    let busy = h.manager.run(spec(&h, "hang")).await.unwrap().session.id;
    let idle = h.manager.run(spec(&h, "hello")).await.unwrap().session.id;
    let question = h.manager.run(spec(&h, "ask")).await.unwrap().session.id;
    wait_state(&h.manager, &idle, SessionState::Idle).await;
    wait_state(&h.manager, &question, SessionState::WaitingInput).await;
    let pool =
        sqlx::SqlitePool::connect(&format!("sqlite://{}", h.manager.store().path().display()))
            .await
            .unwrap();
    sqlx::query("CREATE TRIGGER reject_writes BEFORE INSERT ON events BEGIN SELECT RAISE(FAIL, 'injected outage'); END").execute(&pool).await.unwrap();
    let _ = h.manager.send(&idle, "trigger outage".into()).await;
    let list = h.manager.list(true).await;
    let status = h.manager.status(&busy).await;
    let interrupt = h.manager.interrupt(&busy).await;
    let answer = h
        .manager
        .answer(&question, None, AnswerInput::Text("blue".into()))
        .await;
    let stop = h.manager.stop(&idle).await;
    let shutdown = h.manager.shutdown(Duration::from_millis(50)).await;
    sqlx::query("DROP TRIGGER reject_writes")
        .execute(&pool)
        .await
        .unwrap();
    pool.close().await;
    assert!(list.is_ok(), "writer fault blocked list: {list:?}");
    assert!(status.is_ok(), "writer fault blocked status: {status:?}");
    assert!(
        interrupt.is_ok(),
        "writer fault blocked interrupt: {interrupt:?}"
    );
    assert!(answer.is_ok(), "writer fault blocked answer: {answer:?}");
    assert!(stop.is_ok(), "writer fault blocked stop: {stop:?}");
    assert!(
        shutdown.is_ok(),
        "writer fault prevented shutdown: {shutdown:?}"
    );
    assert!(h.manager.live_ids().is_empty());
}

#[tokio::test]
async fn closed_database_keeps_cached_resolution_status_and_control_available() {
    let h = harness().await;
    let busy = h.manager.run(spec(&h, "hang")).await.unwrap().session.id;
    let idle = h.manager.run(spec(&h, "hello")).await.unwrap().session.id;
    wait_state(&h.manager, &idle, SessionState::Idle).await;
    h.manager.store().close().await.unwrap();
    assert_eq!(h.manager.resolve(&busy.as_str()[..4]).await.unwrap(), busy);
    assert_eq!(h.manager.list(true).await.unwrap().len(), 2);
    assert!(h.manager.status(&busy).await.unwrap().live);
    assert!(!h.manager.persistence().persisted);
    assert!(!h.manager.persistence().warnings.is_empty());
    h.manager.interrupt(&busy).await.unwrap();
    h.manager.stop(&idle).await.unwrap();
    wait_state(&h.manager, &idle, SessionState::Stopped).await;
    let shutdown = h.manager.shutdown(Duration::from_millis(50)).await.unwrap();
    assert!(!shutdown.persisted);
    assert!(!shutdown.warnings.is_empty());
    assert!(h.manager.live_ids().is_empty());
}

#[tokio::test]
async fn partial_drain_reports_closed_sessions_and_resets_remaining_flags() {
    let mut config = Config::default();
    config.sessions.stop_grace_secs = 0;
    let h = harness_with(config).await;
    let idle = h.manager.run(spec(&h, "hello")).await.unwrap().session.id;
    let stubborn = h
        .manager
        .run(spec(&h, "idle-ignore-eof"))
        .await
        .unwrap()
        .session
        .id;
    wait_state(&h.manager, &idle, SessionState::Idle).await;
    wait_state(&h.manager, &stubborn, SessionState::Idle).await;
    let error = h
        .manager
        .drain(Duration::ZERO, Duration::from_millis(100))
        .await
        .unwrap_err();
    let ManagerError::DrainBlocked {
        sessions,
        closed_sessions,
        ..
    } = &error
    else {
        panic!("{error}")
    };
    assert_eq!(sessions, std::slice::from_ref(&stubborn));
    assert_eq!(closed_sessions, std::slice::from_ref(&idle));
    let text = error.to_string();
    for step in ["answer <id>", "stop <id>", "restart --drain N", "--force"] {
        assert!(text.contains(step), "{text}");
    }
    let received = h.received.lock().unwrap().clone();
    assert!(
        h.manager
            .send(&stubborn, "cannot write after EOF".into())
            .await
            .is_err()
    );
    assert_eq!(*h.received.lock().unwrap(), received);
    // A late, natural exit must become Done, proving that the failed drain
    // no longer leaves drain_exit set and cannot orphan-mark this process.
    std::fs::write(h.dir.path().join("release-eof"), "").unwrap();
    wait_state(&h.manager, &stubborn, SessionState::Done).await;
    h.manager
        .resume(&stubborn, Some("again".into()))
        .await
        .unwrap();
    wait_state(&h.manager, &stubborn, SessionState::Idle).await;
    h.manager.shutdown(Duration::from_secs(1)).await.unwrap();
}

#[tokio::test]
async fn full_writer_queue_rejects_new_work_before_agent_commands() {
    let h = harness().await;
    let id = h.manager.run(spec(&h, "hello")).await.unwrap().session.id;
    wait_state(&h.manager, &id, SessionState::Idle).await;
    h.manager.store().flush().await.unwrap();
    let before = h.received.lock().unwrap().clone();
    let mut accepted = 0;
    for seq in 1..=10000 {
        let result = h.manager.store().append_event(wardex_core::Event {
            session_id: SessionId("backlog".into()),
            seq,
            ts_ms: 1,
            agent: None,
            task: None,
            driver: DriverKind::Claude,
            kind: EventKind::Prompt {
                text: "queued".into(),
            },
        });
        if result.is_err() {
            break;
        }
        accepted += 1;
    }
    assert!(accepted <= 4096);
    let send = h.manager.send(&id, "must not run".into()).await;
    let run = h.manager.run(spec(&h, "must not spawn")).await;
    assert!(matches!(
        send,
        Err(ManagerError::Store(
            wardex_core::store::StoreError::QueueFull(_)
        ))
    ));
    assert!(matches!(
        run,
        Err(ManagerError::Store(
            wardex_core::store::StoreError::QueueFull(_)
        ))
    ));
    assert_eq!(*h.received.lock().unwrap(), before);
    assert_eq!(h.spawns.load(Ordering::SeqCst), 1);
    // Management remains available even when the persistence budget is full.
    h.manager.stop(&id).await.unwrap();
    wait_state(&h.manager, &id, SessionState::Stopped).await;
    h.manager.store().flush().await.unwrap();
}

#[tokio::test]
async fn drain_without_resume_id_requires_explicit_stop() {
    let h = harness().await;
    std::fs::write(h.dir.path().join("omit-resume-id"), "").unwrap();
    let id = h.manager.run(spec(&h, "hello")).await.unwrap().session.id;
    wait_state(&h.manager, &id, SessionState::Idle).await;
    let error = h
        .manager
        .drain(Duration::ZERO, Duration::from_millis(100))
        .await
        .unwrap_err();
    assert!(error.to_string().contains("never reports a resume id"));
    assert!(error.to_string().contains("wardex stop <id>"));
    assert!(h.manager.status(&id).await.unwrap().live);
    h.manager.stop(&id).await.unwrap();
    wait_state(&h.manager, &id, SessionState::Stopped).await;
}

struct ScopedFakeDriver {
    inner: FakeDriver,
    unit: Option<String>,
    must_be_reaped: Option<std::path::PathBuf>,
}

impl Driver for ScopedFakeDriver {
    fn kind(&self) -> DriverKind {
        self.inner.kind()
    }
    fn spawn(&self, spec: SpawnSpec, launcher: &Launcher) -> Result<DriverHandle, DriverError> {
        if let Some(pid_path) = &self.must_be_reaped {
            let pid = std::fs::read_to_string(pid_path).unwrap();
            assert!(
                !process_alive(pid.trim()),
                "new generation started beside old descendants"
            );
        }
        let mut handle = self.inner.spawn(spec, launcher)?;
        handle.unit = self.unit.clone();
        Ok(handle)
    }
}

fn process_alive(pid: &str) -> bool {
    std::fs::read_to_string(format!("/proc/{pid}/status")).is_ok_and(|status| {
        !status
            .lines()
            .any(|line| line.starts_with("State:") && line.contains('Z'))
    })
}

async fn mock_descendant(dir: &std::path::Path) -> (tokio::process::Child, std::path::PathBuf) {
    use std::os::unix::fs::PermissionsExt;
    let mut child = tokio::process::Command::new("sh")
        .args([
            "-c",
            "trap '' TERM; echo $$ > descendant.pid; touch ready; exec sleep 60",
        ])
        .current_dir(dir)
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        while !dir.join("ready").exists() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    let controller = dir.join("scope-controller.sh");
    // This controller only signals the test-owned PID, never a host service.
    std::fs::write(
        &controller,
        format!(
            r#"#!/bin/sh
cd '{}' || exit 1
pid=$(cat descendant.pid)
if [ "$2" = kill ]; then
  printf '%s\n' "$3" >> signals
  if [ "$3" = --signal=SIGTERM ]; then kill -TERM "$pid" 2>/dev/null || :; fi
  if [ "$3" = --signal=SIGKILL ]; then kill -KILL "$pid" 2>/dev/null || :; fi
else
  state=$(awk '/^State:/ {{ print $2 }}' "/proc/$pid/status" 2>/dev/null)
  if [ -z "$state" ] || [ "$state" = Z ]; then
    printf 'LoadState=loaded\nActiveState=inactive\n'
  else printf 'LoadState=loaded\nActiveState=active\n'; fi
fi
"#,
            dir.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&controller, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert!(child.try_wait().unwrap().is_none());
    (child, controller)
}

#[tokio::test]
async fn drain_reaps_descendants_with_term_then_kill() {
    let dir = tempfile::tempdir().unwrap();
    let (mut child, controller) = mock_descendant(dir.path()).await;
    let store = Store::open(&dir.path().join("w.db")).await.unwrap();
    let spawns = Arc::new(AtomicU32::new(0));
    let driver = ScopedFakeDriver {
        inner: FakeDriver {
            spawns,
            received: Arc::default(),
        },
        unit: Some("mock.scope".into()),
        must_be_reaped: None,
    };
    let mut config = Config::default();
    config.sessions.stop_grace_secs = 0;
    let manager = Manager::new(
        config,
        store,
        vec![Arc::new(driver)],
        Launcher::with_scope_command(controller),
    );
    let id = manager
        .run(RunSpec {
            driver: Some(DriverKind::Claude),
            prompt: "hello".into(),
            cwd: dir.path().into(),
            ..Default::default()
        })
        .await
        .unwrap()
        .session
        .id;
    wait_state(&manager, &id, SessionState::Idle).await;
    let result = manager.drain(Duration::ZERO, Duration::from_secs(3)).await;
    let alive = child.try_wait().unwrap().is_none();
    if alive {
        child.kill().await.unwrap();
    }
    child.wait().await.unwrap();
    let report = result.unwrap();
    assert_eq!(report.closed_sessions, vec![id.clone()]);
    assert!(
        !alive,
        "drain returned while descendants were still running"
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("signals")).unwrap(),
        "--signal=SIGTERM\n--signal=SIGKILL\n"
    );
    assert!(manager.status(&id).await.unwrap().session.unit.is_none());
}

async fn resume_reaps_pending_scope(recover_first: bool) {
    let h = harness().await;
    let id = h.manager.run(spec(&h, "hello")).await.unwrap().session.id;
    wait_state(&h.manager, &id, SessionState::Idle).await;
    h.manager.shutdown(Duration::from_secs(1)).await.unwrap();
    let (mut child, controller) = mock_descendant(h.dir.path()).await;
    let mut record = h.manager.store().get_session(&id).await.unwrap().unwrap();
    record.unit = Some("old-mock.scope".into());
    record.resume_pending = true;
    h.manager.store().update_session(&record).await.unwrap();
    h.manager.store().close().await.unwrap();
    let driver = ScopedFakeDriver {
        inner: FakeDriver {
            spawns: h.spawns.clone(),
            received: h.received.clone(),
        },
        unit: None,
        must_be_reaped: Some(h.dir.path().join("descendant.pid")),
    };
    let mut config = Config::default();
    config.sessions.stop_grace_secs = 0;
    config.sessions.auto_resume_orphaned = true;
    let manager = Manager::new(
        config,
        Store::open(h.manager.store().path()).await.unwrap(),
        vec![Arc::new(driver)],
        Launcher::with_scope_command(controller),
    );
    let result = if recover_first {
        assert!(manager.recover().await.unwrap().is_empty());
        manager.auto_resume().await
    } else {
        manager.resume(&id, None).await.map(|_| vec![id.clone()])
    };
    if child.try_wait().unwrap().is_none() {
        child.kill().await.unwrap();
    }
    child.wait().await.unwrap();
    assert_eq!(result.unwrap(), vec![id.clone()]);
    wait_state(&manager, &id, SessionState::Idle).await;
    assert_eq!(h.spawns.load(Ordering::SeqCst), 2);
    manager.shutdown(Duration::from_secs(1)).await.unwrap();
}

#[tokio::test]
async fn pending_resume_reaps_old_scope_before_new_spawn() {
    resume_reaps_pending_scope(true).await;
}

#[tokio::test]
async fn manual_resume_reaps_pending_scope_before_new_spawn() {
    resume_reaps_pending_scope(false).await;
}

#[tokio::test]
async fn unverifiable_old_scope_prevents_a_new_spawn() {
    use std::os::unix::fs::PermissionsExt;
    let h = harness().await;
    let id = h.manager.run(spec(&h, "hello")).await.unwrap().session.id;
    wait_state(&h.manager, &id, SessionState::Idle).await;
    h.manager.shutdown(Duration::from_secs(1)).await.unwrap();
    let mut record = h.manager.store().get_session(&id).await.unwrap().unwrap();
    record.unit = Some("unverifiable.scope".into());
    record.state = SessionState::Stopped;
    record.resume_pending = false;
    h.manager.store().update_session(&record).await.unwrap();
    h.manager.store().close().await.unwrap();
    let controller = h.dir.path().join("unavailable-controller.sh");
    std::fs::write(&controller, "#!/bin/sh\nexit 1\n").unwrap();
    std::fs::set_permissions(&controller, std::fs::Permissions::from_mode(0o755)).unwrap();
    let manager = Manager::new(
        Config::default(),
        Store::open(h.manager.store().path()).await.unwrap(),
        vec![Arc::new(FakeDriver {
            spawns: h.spawns.clone(),
            received: h.received.clone(),
        })],
        Launcher::with_scope_command(controller),
    );
    let error = manager.resume(&id, None).await.unwrap_err();
    assert!(error.to_string().contains("cleanup_failed"));
    assert!(error.to_string().contains("wardex resume <id>"));
    assert_eq!(h.spawns.load(Ordering::SeqCst), 1);
    assert!(manager.live_ids().is_empty());
    assert!(manager.auto_resume().await.unwrap().is_empty());
    let record = manager.store().get_session(&id).await.unwrap().unwrap();
    assert_eq!(record.state, SessionState::Orphaned);
    assert!(record.resume_pending);
    assert!(record.cleanup_failed);
    assert_eq!(record.unit.as_deref(), Some("unverifiable.scope"));
}

async fn recover_with_unverifiable_scope(was_live: bool) {
    use std::os::unix::fs::PermissionsExt;

    let h = harness().await;
    let blocked = h.manager.run(spec(&h, "hello")).await.unwrap().session.id;
    let healthy = h.manager.run(spec(&h, "hello")).await.unwrap().session.id;
    for id in [&blocked, &healthy] {
        wait_state(&h.manager, id, SessionState::Idle).await;
    }
    h.manager.shutdown(Duration::from_secs(1)).await.unwrap();
    for (id, unit) in [(&blocked, "unverifiable.scope"), (&healthy, "empty.scope")] {
        let mut record = h.manager.store().get_session(id).await.unwrap().unwrap();
        record.unit = Some(unit.into());
        record.resume_pending = true;
        if was_live {
            record.state = SessionState::Idle;
        }
        h.manager.store().update_session(&record).await.unwrap();
    }
    h.manager.store().close().await.unwrap();
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
    config.sessions.stop_grace_secs = 0;
    config.sessions.auto_resume_orphaned = true;
    let manager = Manager::new(
        config,
        Store::open(h.manager.store().path()).await.unwrap(),
        vec![Arc::new(FakeDriver {
            spawns: h.spawns.clone(),
            received: h.received.clone(),
        })],
        Launcher::with_scope_command(controller),
    );
    let recovery = manager.recover().await;
    assert!(
        recovery.is_ok(),
        "one failed scope prevented startup: {recovery:?}"
    );
    let recovered = recovery.unwrap();
    assert_eq!(recovered.len(), if was_live { 2 } else { 0 });
    let stored = manager
        .store()
        .get_session(&blocked)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stored.state, SessionState::Orphaned);
    assert_eq!(stored.unit.as_deref(), Some("unverifiable.scope"));
    assert_eq!(
        serde_json::to_value(&stored).unwrap()["cleanup_failed"],
        true
    );
    assert!(
        stored
            .last_error
            .as_ref()
            .unwrap()
            .contains("cleanup_failed")
    );
    assert_eq!(manager.auto_resume().await.unwrap(), vec![healthy.clone()]);
    wait_state(&manager, &healthy, SessionState::Idle).await;
    assert_eq!(h.spawns.load(Ordering::SeqCst), 3);
    let status = manager.status(&blocked).await.unwrap();
    assert!(!status.live);
    assert_eq!(
        serde_json::to_value(status).unwrap()["cleanup_failed"],
        true
    );
    let listed = manager
        .list(true)
        .await
        .unwrap()
        .into_iter()
        .find(|view| view.session.id == blocked)
        .unwrap();
    assert_eq!(
        serde_json::to_value(listed).unwrap()["cleanup_failed"],
        true
    );
    let ended = manager
        .overview(0)
        .await
        .unwrap()
        .recently_ended
        .into_iter()
        .find(|entry| entry.id == blocked)
        .unwrap();
    assert_eq!(serde_json::to_value(ended).unwrap()["cleanup_failed"], true);
    let error = manager
        .resume(&blocked, None)
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("cleanup_failed"), "{error}");
    assert!(error.contains("wardex resume"), "{error}");
    assert_eq!(h.spawns.load(Ordering::SeqCst), 3);
    assert!(
        manager
            .store()
            .get_session(&blocked)
            .await
            .unwrap()
            .unwrap()
            .resume_pending
    );

    // Only an explicit retry after the scope becomes verifiable may spawn.
    std::fs::write(h.dir.path().join("repaired"), "").unwrap();
    let error = manager
        .send(&blocked, "implicit resume must stay blocked".into())
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("wardex resume <id>"), "{error}");
    assert_eq!(h.spawns.load(Ordering::SeqCst), 3);
    manager
        .resume(&blocked, Some("hello".into()))
        .await
        .unwrap();
    wait_state(&manager, &blocked, SessionState::Idle).await;
    manager.store().flush().await.unwrap();
    let repaired = manager
        .store()
        .get_session(&blocked)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        serde_json::to_value(&repaired).unwrap()["cleanup_failed"],
        false
    );
    assert!(repaired.last_error.is_none());
    assert!(repaired.unit.is_none());
    assert_eq!(h.spawns.load(Ordering::SeqCst), 4);
    manager.shutdown(Duration::from_secs(1)).await.unwrap();
}

#[tokio::test]
async fn recover_isolates_live_scope_failure_and_allows_manual_cleanup_retry() {
    recover_with_unverifiable_scope(true).await;
}

#[tokio::test]
async fn recover_isolates_pending_scope_failure_and_allows_manual_cleanup_retry() {
    recover_with_unverifiable_scope(false).await;
}

struct ScopedProcessDriver;

impl Driver for ScopedProcessDriver {
    fn kind(&self) -> DriverKind {
        DriverKind::Claude
    }
    fn spawn(&self, spec: SpawnSpec, launcher: &Launcher) -> Result<DriverHandle, DriverError> {
        let mut spawned = launcher.spawn(&wardex_core::process::LaunchRequest {
            unit: &spec.unit, slice: None, program: "sh", cwd: &spec.cwd,
            args: &["-c".into(), "sh -c 'trap \"\" TERM; exec sleep 60' </dev/null >/dev/null 2>&1 & echo $! > descendant.pid; exec cat".into()],
        }).map_err(|source| DriverError::Spawn { program: "sh".into(), source })?;
        let (commands, mut rx) = mpsc::unbounded_channel();
        let (events, event_rx) = mpsc::unbounded_channel();
        let mut stdin = spawned.child.stdin.take();
        tokio::spawn(async move {
            let _ = events.send(DriverEvent::Event(EventKind::Started {
                driver_session_id: Some("mock-session".into()),
                model: None,
                reasoning: None,
            }));
            loop {
                tokio::select! {
                    status = spawned.child.wait() => {
                        let _ = events.send(DriverEvent::Exited { code: status.ok().and_then(|s| s.code()), stderr_tail: Vec::new() });
                        return;
                    }
                    command = rx.recv() => match command {
                        Some(DriverCommand::Send(_)) => { let _ = events.send(DriverEvent::Event(done(None, false))); }
                        Some(DriverCommand::Shutdown | DriverCommand::Drain) | None => { stdin.take(); }
                        Some(DriverCommand::Kill) => { let _ = spawned.child.start_kill(); }
                        _ => {}
                    }
                }
            }
        });
        Ok(DriverHandle {
            pid: spawned.pid,
            unit: spawned.unit,
            commands,
            events: event_rx,
        })
    }
}

/// Run explicitly only in an isolated environment with user systemd available.
#[tokio::test]
#[ignore = "requires user systemd; default suite uses a mock scope controller"]
async fn drain_reaps_a_real_systemd_scope_before_returning() {
    let dir = tempfile::tempdir().unwrap();
    let launcher = Launcher::new(true);
    assert!(launcher.uses_scope());
    let mut config = Config::default();
    config.sessions.stop_grace_secs = 0;
    let manager = Manager::new(
        config,
        Store::open(&dir.path().join("w.db")).await.unwrap(),
        vec![Arc::new(ScopedProcessDriver)],
        launcher,
    );
    let id = manager
        .run(RunSpec {
            driver: Some(DriverKind::Claude),
            prompt: "go".into(),
            cwd: dir.path().into(),
            ..Default::default()
        })
        .await
        .unwrap()
        .session
        .id;
    wait_state(&manager, &id, SessionState::Idle).await;
    tokio::time::timeout(Duration::from_secs(3), async {
        while !dir.path().join("descendant.pid").exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let pid = std::fs::read_to_string(dir.path().join("descendant.pid")).unwrap();
    let unit = manager.status(&id).await.unwrap().session.unit.unwrap();
    let result = manager.drain(Duration::ZERO, Duration::from_secs(5)).await;
    let descendant_survived = process_alive(pid.trim());
    // Test-owned scope cleanup also runs if the regression assertion will fail.
    Launcher::kill_unit(&unit).await;
    assert!(result.is_ok(), "{result:?}");
    assert!(!descendant_survived, "drain leaked a real scope descendant");
}

#[tokio::test]
async fn drain_timeout_preserves_active_idle_questions_and_approvals() {
    let h = harness().await;
    let busy = h.manager.run(spec(&h, "hang")).await.unwrap().session.id;
    let idle = h.manager.run(spec(&h, "hello")).await.unwrap().session.id;
    let question = h.manager.run(spec(&h, "ask")).await.unwrap().session.id;
    let approval = h.manager.run(spec(&h, "approve")).await.unwrap().session.id;
    wait_state(&h.manager, &idle, SessionState::Idle).await;
    wait_state(&h.manager, &question, SessionState::WaitingInput).await;
    wait_state(&h.manager, &approval, SessionState::WaitingInput).await;
    let pending = h.manager.pending_questions().await.unwrap();
    let error = h
        .manager
        .drain(Duration::ZERO, Duration::from_millis(20))
        .await
        .unwrap_err();
    let ManagerError::DrainBlocked { mut sessions, .. } = error else {
        panic!("{error}")
    };
    sessions.sort();
    let mut expected = vec![busy.clone(), question.clone(), approval.clone()];
    expected.sort();
    assert_eq!(sessions, expected);
    for id in [&busy, &idle, &question, &approval] {
        assert!(h.manager.status(id).await.unwrap().live);
        assert!(!kinds(&h.manager, id).await.contains(&"ended".into()));
    }
    assert_eq!(h.manager.pending_questions().await.unwrap(), pending);
    // The original protocol request ids still work after a refused restart.
    h.manager
        .answer(&question, Some("q1"), AnswerInput::Text("blue".into()))
        .await
        .unwrap();
    h.manager
        .answer(&approval, Some("r1"), AnswerInput::Text("deny".into()))
        .await
        .unwrap();
    h.manager.interrupt(&busy).await.unwrap();
    wait_state(&h.manager, &busy, SessionState::Idle).await;
    wait_state(&h.manager, &question, SessionState::Idle).await;
    wait_state(&h.manager, &approval, SessionState::Idle).await;
    let report = h
        .manager
        .drain(Duration::ZERO, Duration::from_secs(1))
        .await
        .unwrap();
    assert_eq!(report.closed_sessions.len(), 4);
    assert!(report.interrupted_sessions.is_empty());
    for id in &report.closed_sessions {
        let record = h.manager.status(id).await.unwrap().session;
        assert_eq!(record.state, SessionState::Orphaned);
        assert!(record.resume_pending);
        assert!(!record.resume_with_prompt);
    }
    let path = h.manager.store().path().to_path_buf();
    h.manager.store().close().await.unwrap();
    let mut config = Config::default();
    config.sessions.auto_resume_orphaned = true;
    let next = Manager::new(
        config,
        Store::open(&path).await.unwrap(),
        vec![Arc::new(FakeDriver {
            spawns: h.spawns.clone(),
            received: h.received.clone(),
        })],
        Launcher::direct(),
    );
    assert!(next.recover().await.unwrap().is_empty());
    assert_eq!(next.auto_resume().await.unwrap().len(), 4);
    for id in &report.closed_sessions {
        wait_state(&next, id, SessionState::Idle).await;
        assert_eq!(next.status(id).await.unwrap().session.spawn_count, 2);
    }
    next.shutdown(Duration::from_secs(1)).await.unwrap();
}

#[tokio::test]
async fn drain_accepts_answers_but_reserves_against_new_work() {
    let h = harness().await;
    let question = h.manager.run(spec(&h, "ask")).await.unwrap().session.id;
    let idle = h.manager.run(spec(&h, "hello")).await.unwrap().session.id;
    wait_state(&h.manager, &question, SessionState::WaitingInput).await;
    wait_state(&h.manager, &idle, SessionState::Idle).await;
    let manager = h.manager.clone();
    let drain = tokio::spawn(async move {
        manager
            .drain(Duration::from_secs(2), Duration::from_secs(1))
            .await
    });
    tokio::task::yield_now().await;
    assert!(matches!(
        h.manager.run(spec(&h, "new")).await,
        Err(ManagerError::Conflict(_))
    ));
    assert!(matches!(
        h.manager.send(&idle, "new".into()).await,
        Err(ManagerError::Conflict(_))
    ));
    assert!(matches!(
        h.manager.resume(&SessionId("missing".into()), None).await,
        Err(ManagerError::Conflict(_))
    ));
    h.manager
        .answer(&question, None, AnswerInput::Text("blue".into()))
        .await
        .unwrap();
    let report = drain.await.unwrap().unwrap();
    assert_eq!(report.closed_sessions.len(), 2);
    assert!(report.interrupted_sessions.is_empty());
    assert_eq!(h.spawns.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn drain_never_escalates_idle_eof_and_explicit_stop_still_works() {
    let mut config = Config::default();
    config.sessions.stop_grace_secs = 0;
    let h = harness_with(config).await;
    let id = h
        .manager
        .run(spec(&h, "idle-ignore-eof"))
        .await
        .unwrap()
        .session
        .id;
    wait_state(&h.manager, &id, SessionState::Idle).await;
    assert!(matches!(
        h.manager
            .drain(Duration::ZERO, Duration::from_millis(20))
            .await,
        Err(ManagerError::DrainBlocked { .. })
    ));
    assert!(h.manager.status(&id).await.unwrap().live);
    assert!(matches!(
        h.manager.send(&id, "new".into()).await,
        Err(ManagerError::Conflict(_))
    ));
    // Explicit stop retains its force-kill fallback after a refused drain.
    h.manager.stop(&id).await.unwrap();
    wait_state(&h.manager, &id, SessionState::Stopped).await;
    h.manager.resume(&id, Some("again".into())).await.unwrap();
    wait_state(&h.manager, &id, SessionState::Idle).await;
    h.manager.shutdown(Duration::from_secs(1)).await.unwrap();
}

#[tokio::test]
async fn idle_shutdown_ends_as_done() {
    let mut config = Config::default();
    config.sessions.idle_shutdown_secs = 1;
    let h = harness_with(config).await;
    let id = h.manager.run(spec(&h, "hello")).await.unwrap().session.id;
    wait_state(&h.manager, &id, SessionState::Idle).await;
    let o = h.manager.overview(0).await.unwrap();
    let s = &o.live[0];
    assert!(s.idle_since_ms.is_some());
    let left = s.shutdown_in_ms.unwrap();
    assert!((0..=1_000).contains(&left), "{left}");
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
        store.close().await.unwrap();
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

#[tokio::test]
async fn worktree_session_is_created_and_cleaned_up() {
    let h = harness().await;
    let repo = h.dir.path().join("repo");
    std::fs::create_dir(&repo).unwrap();
    for args in [
        vec!["init", "-q", "-b", "main"],
        vec![
            "-c",
            "user.email=t@example.com",
            "-c",
            "user.name=T",
            "commit",
            "-q",
            "--allow-empty",
            "-m",
            "init",
        ],
    ] {
        assert!(
            std::process::Command::new("git")
                .arg("-C")
                .arg(&repo)
                .args(&args)
                .status()
                .unwrap()
                .success()
        );
    }
    let mut s = spec(&h, "hello");
    s.cwd = repo.clone();
    s.worktree = Some("w1".into());
    s.branch = Some("feature/w1".into());
    s.remove_worktree = true;
    let out = h.manager.run(s).await.unwrap();
    let id = out.session.id.clone();
    let wt = repo.join(".worktree/w1");
    assert_eq!(out.session.cwd, wt);
    assert_eq!(out.session.origin_cwd.as_deref(), Some(repo.as_path()));
    assert!(wt.join(".git").exists());
    wait_state(&h.manager, &id, SessionState::Idle).await;
    let summary = h.manager.summary(&id).await.unwrap();
    assert_eq!(summary.git.unwrap().branch.as_deref(), Some("feature/w1"));

    h.manager.stop(&id).await.unwrap();
    wait_state(&h.manager, &id, SessionState::Stopped).await;
    for _ in 0..100 {
        if !wt.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        !wt.exists(),
        "clean worktree is removed when the session ends"
    );

    // A name that is already taken is refused.
    std::fs::create_dir_all(repo.join(".worktree/taken")).unwrap();
    let mut s = spec(&h, "hello");
    s.cwd = repo.clone();
    s.worktree = Some("taken".into());
    assert!(matches!(
        h.manager.run(s).await,
        Err(ManagerError::Invalid(_))
    ));
}

#[tokio::test]
async fn shutdown_orphans_and_auto_resume_continues_busy_sessions() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("w.db");
    let received = Arc::new(std::sync::Mutex::new(Vec::new()));
    let make = |config: Config, store: Store| {
        let driver = Arc::new(FakeDriver {
            spawns: Arc::new(AtomicU32::new(0)),
            received: received.clone(),
        });
        Manager::new(config, store, vec![driver], Launcher::direct())
    };
    let run = |prompt: &str| RunSpec {
        driver: Some(DriverKind::Claude),
        prompt: prompt.into(),
        cwd: dir.path().to_path_buf(),
        ..Default::default()
    };
    let (busy, idle);
    {
        let store = Store::open(&db).await.unwrap();
        let m = make(Config::default(), store.clone());
        busy = m.run(run("hang")).await.unwrap().session.id;
        idle = m.run(run("hello")).await.unwrap().session.id;
        wait_state(&m, &busy, SessionState::Running).await;
        wait_state(&m, &idle, SessionState::Idle).await;
        // Drain gives up on the hanging turn but not on the idle session.
        assert!(!m.wait_for_turns(Duration::from_millis(200)).await);
        m.shutdown(Duration::from_secs(2)).await.unwrap();
        for id in [&busy, &idle] {
            let st = m.status(id).await.unwrap().session;
            assert_eq!(st.state, SessionState::Orphaned);
            assert!(st.resume_pending);
        }
        assert!(m.status(&busy).await.unwrap().session.resume_with_prompt);
        assert!(!m.status(&idle).await.unwrap().session.resume_with_prompt);
        store.close().await.unwrap();
    }
    let mut config = Config::default();
    config.sessions.auto_resume_orphaned = true;
    let m = make(config.clone(), Store::open(&db).await.unwrap());
    assert!(m.recover().await.unwrap().is_empty());
    let mut resumed = m.auto_resume().await.unwrap();
    resumed.sort();
    let mut expected = vec![busy.clone(), idle.clone()];
    expected.sort();
    assert_eq!(resumed, expected);
    wait_state(&m, &idle, SessionState::Idle).await;
    // Only the busy session is told to continue.
    let prompts: Vec<String> = received
        .lock()
        .unwrap()
        .iter()
        .filter(|t| **t == config.sessions.resume_prompt)
        .cloned()
        .collect();
    assert_eq!(prompts.len(), 1);
    assert!(!m.status(&busy).await.unwrap().session.resume_pending);
    // A second start resumes nothing.
    assert!(m.auto_resume().await.unwrap().is_empty());
}

#[tokio::test]
async fn orphans_are_not_resumed_unless_enabled() {
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
        id = m
            .run(RunSpec {
                driver: Some(DriverKind::Claude),
                prompt: "hello".into(),
                cwd: dir.path().to_path_buf(),
                ..Default::default()
            })
            .await
            .unwrap()
            .session
            .id;
        wait_state(&m, &id, SessionState::Idle).await;
        m.shutdown(Duration::from_secs(2)).await.unwrap();
        store.close().await.unwrap();
    }
    let spawns = Arc::new(AtomicU32::new(0));
    let driver = Arc::new(FakeDriver {
        spawns: spawns.clone(),
        received: Arc::new(std::sync::Mutex::new(Vec::new())),
    });
    let m = Manager::new(
        Config::default(),
        Store::open(&db).await.unwrap(),
        vec![driver],
        Launcher::direct(),
    );
    assert!(m.auto_resume().await.unwrap().is_empty());
    assert_eq!(spawns.load(Ordering::SeqCst), 0);
    let st = m.status(&id).await.unwrap().session;
    assert_eq!(st.state, SessionState::Orphaned);
    assert!(!st.resume_pending);
}

#[tokio::test]
async fn explicit_reasoning_is_kept() {
    let h = harness().await;
    let mut s = spec(&h, "hello");
    s.reasoning = Some("low".into());
    let id = h.manager.run(s).await.unwrap().session.id;
    wait_state(&h.manager, &id, SessionState::Idle).await;
    let status = h.manager.status(&id).await.unwrap();
    assert_eq!(status.session.reasoning.as_deref(), Some("low"));
    assert_eq!(status.session.model.as_deref(), Some("fake-model-1"));
}
