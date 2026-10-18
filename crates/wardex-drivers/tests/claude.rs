//! Claude driver tests against fixtures recorded from Claude Code 2.1.302.

mod common;

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use common::{collect, feed_all, load, names, replay_script};
use serde_json::Value;
use wardex_core::driver::{DriverCommand, SpawnSpec};
use wardex_core::manager::{AnswerInput, Manager};
use wardex_core::process::Launcher;
use wardex_core::store::Store;
use wardex_core::{
    Answer, Config, DriverKind, EventKind, RunSpec, SessionId, SessionState, ToolStatus,
};
use wardex_drivers::ClaudeDriver;
use wardex_drivers::claude::ClaudeProtocol;
use wardex_drivers::runner::Protocol;

fn first_out(records: &[common::Record], pred: impl Fn(&Value) -> bool) -> usize {
    records
        .iter()
        .position(|r| r.dir == "out" && pred(&r.msg))
        .unwrap()
}

#[test]
fn partial_messages_and_done() {
    let mut p = ClaudeProtocol::new(false);
    let (events, writes) = feed_all(&mut p, &load("claude", "partial"));
    assert!(writes.is_empty());
    assert_eq!(names(&events), ["started", "message", "done"]);
    assert!(events.iter().any(|e| e.is_delta()));
    match &events[0] {
        EventKind::Started {
            driver_session_id,
            model,
        } => {
            assert!(driver_session_id.is_some());
            assert_eq!(model.as_deref(), Some("claude-haiku-5-5"));
        }
        e => panic!("{e:?}"),
    }
    match events.last().unwrap() {
        EventKind::Done {
            result,
            interrupted,
            note,
            ..
        } => {
            assert_eq!(result.as_deref(), Some("pong"));
            assert!(!interrupted);
            assert_eq!(note.as_deref(), Some("replied pong as requested"));
        }
        e => panic!("{e:?}"),
    }
}

#[test]
fn tool_call_start_and_finish() {
    let mut p = ClaudeProtocol::new(false);
    let (events, _) = feed_all(&mut p, &load("claude", "tool"));
    let tools: Vec<(&str, ToolStatus)> = events
        .iter()
        .filter_map(|e| match e {
            EventKind::ToolCall { name, status, .. } => Some((name.as_str(), *status)),
            _ => None,
        })
        .collect();
    assert_eq!(
        tools,
        [
            ("Bash", ToolStatus::Started),
            ("Bash", ToolStatus::Completed)
        ]
    );
    match &events.iter().find(|e| e.name() == "tool_call").unwrap() {
        EventKind::ToolCall { input_summary, .. } => assert_eq!(input_summary, "ls"),
        _ => unreachable!(),
    }
    assert_eq!(names(&events).last(), Some(&"done"));
}

#[test]
fn question_answer_matches_recorded_response() {
    let records = load("claude", "question");
    let mut p = ClaudeProtocol::new(false);
    let ask = first_out(&records, |m| m["type"] == "control_request");
    let (events, _) = feed_all(&mut p, &records[..=ask]);
    let (question_id, items) = match events.last().unwrap() {
        EventKind::Question { question_id, items } => (question_id.clone(), items.clone()),
        e => panic!("{e:?}"),
    };
    // AskUserQuestion is not reported as a tool call.
    assert!(!events.iter().any(|e| e.name() == "tool_call"));
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].options.len(), 2);
    assert_eq!(items[0].options[1].label, "Blue");

    let mut answers = BTreeMap::new();
    answers.insert(items[0].id.clone(), vec!["blue".to_string()]);
    let mut events = Vec::new();
    let mut writes = Vec::new();
    collect(
        p.on_command(DriverCommand::Answer {
            question_id: question_id.clone(),
            answer: Answer::Question { items: answers },
        }),
        &mut events,
        &mut writes,
    );
    // Same response the recording harness sent (and the CLI accepted).
    let recorded = records
        .iter()
        .find(|r| r.dir == "in" && r.msg["type"] == "control_response")
        .unwrap();
    assert_eq!(writes, std::slice::from_ref(&recorded.msg));

    let (events, _) = feed_all(&mut p, &records[ask + 1..]);
    match events.iter().rev().find(|e| e.name() == "done").unwrap() {
        EventKind::Done { result, .. } => assert_eq!(result.as_deref(), Some("blue")),
        _ => unreachable!(),
    }
}

#[test]
fn interrupt_reports_interrupted_done() {
    let records = load("claude", "interrupt");
    let mut p = ClaudeProtocol::new(false);
    let mut writes = Vec::new();
    let mut ev = Vec::new();
    collect(p.on_command(DriverCommand::Interrupt), &mut ev, &mut writes);
    assert_eq!(writes[0]["request"]["subtype"], "interrupt");
    let (events, _) = feed_all(&mut p, &records);
    assert!(!events.iter().any(|e| e.name() == "error"));
    assert!(matches!(
        events.iter().rev().find(|e| e.name() == "done"),
        Some(EventKind::Done {
            interrupted: true,
            ..
        })
    ));
}

#[test]
fn multi_turn_reports_one_start() {
    let mut p = ClaudeProtocol::new(false);
    let (events, _) = feed_all(&mut p, &load("claude", "multi"));
    assert_eq!(
        names(&events),
        ["started", "message", "done", "message", "done"]
    );
}

#[test]
fn readonly_denies_permission_requests() {
    let records = load("claude", "plan_deny");
    let mut p = ClaudeProtocol::new(true);
    let (events, writes) = feed_all(&mut p, &records);
    assert!(!events.iter().any(|e| e.name() == "approval"));
    assert_eq!(writes.len(), 1);
    assert_eq!(writes[0]["response"]["response"]["behavior"], "deny");
    // Without read-only the same request is surfaced as an approval.
    let mut p = ClaudeProtocol::new(false);
    let (events, writes) = feed_all(&mut p, &records);
    assert!(writes.is_empty());
    assert!(events.iter().any(|e| matches!(
        e,
        EventKind::Approval { kind, .. } if kind == "exit_plan"
    )));
}

#[test]
fn args_for_new_resume_and_readonly() {
    let d = ClaudeDriver::with_command("claude", vec![]);
    let mut spec = SpawnSpec {
        session_id: SessionId("abcd1234".into()),
        cwd: "/".into(),
        model: Some("haiku".into()),
        reasoning: Some("low".into()),
        readonly: false,
        resume: None,
        slice: None,
        unit: "wardex-abcd1234-1".into(),
    };
    let a = d.args(&spec);
    let joined = a.join(" ");
    assert!(joined.contains("--permission-prompt-tool stdio"));
    assert!(joined.contains("--permission-mode bypassPermissions"));
    assert!(joined.contains("--model haiku --effort low"));
    let sid = &a[a.iter().position(|x| x == "--session-id").unwrap() + 1];
    assert!(uuid::Uuid::parse_str(sid).is_ok());
    spec.resume = Some("prev".into());
    spec.readonly = true;
    let joined = d.args(&spec).join(" ");
    assert!(joined.contains("--resume prev"));
    assert!(!joined.contains("--session-id"));
    assert!(joined.contains("--permission-mode plan"));
}

async fn manager_for(script: &std::path::Path, db: &std::path::Path) -> Manager {
    let store = Store::open(db).await.unwrap();
    let driver = Arc::new(ClaudeDriver::with_command(
        "sh",
        vec![script.to_string_lossy().into_owned()],
    ));
    Manager::new(Config::default(), store, vec![driver], Launcher::direct())
}

async fn wait_state(m: &Manager, id: &SessionId, state: SessionState) {
    for _ in 0..500 {
        if m.status(id).await.unwrap().session.state == state {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!(
        "never reached {state:?}: {:?}",
        m.status(id).await.unwrap().session
    );
}

#[tokio::test]
async fn end_to_end_question_through_a_mock_process() {
    let dir = tempfile::tempdir().unwrap();
    let script = replay_script(&load("claude", "question"), dir.path());
    let m = manager_for(&script, &dir.path().join("w.db")).await;
    let id = m
        .run(RunSpec {
            driver: Some(DriverKind::Claude),
            prompt: "ask me".into(),
            cwd: dir.path().to_path_buf(),
            ..Default::default()
        })
        .await
        .unwrap()
        .session
        .id;
    wait_state(&m, &id, SessionState::WaitingInput).await;
    let status = m.status(&id).await.unwrap();
    assert!(status.session.pid.is_some());
    assert!(status.session.driver_session_id.is_some());
    m.answer(&id, None, AnswerInput::Text("blue".into()))
        .await
        .unwrap();
    // The recorded process exits after its result: the session is done.
    wait_state(&m, &id, SessionState::Done).await;
    let summary = m.summary(&id).await.unwrap();
    assert_eq!(summary.last_result.as_deref(), Some("blue"));
    let kinds: Vec<String> = m
        .events(&id, 0, 100)
        .await
        .unwrap()
        .iter()
        .map(|e| e.kind.name().to_string())
        .collect();
    assert_eq!(
        kinds,
        [
            "prompt", "started", "question", "answered", "message", "done", "ended"
        ]
    );
}

#[tokio::test]
async fn end_to_end_unexpected_exit_is_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let script = dir.path().join("crash.sh");
    std::fs::write(
        &script,
        "#!/bin/sh\nread -r _\necho 'fatal: no auth' >&2\nexit 7\n",
    )
    .unwrap();
    let m = manager_for(&script, &dir.path().join("w.db")).await;
    let id = m
        .run(RunSpec {
            driver: Some(DriverKind::Claude),
            prompt: "hi".into(),
            cwd: dir.path().to_path_buf(),
            ..Default::default()
        })
        .await
        .unwrap()
        .session
        .id;
    wait_state(&m, &id, SessionState::Error).await;
    let err = m.status(&id).await.unwrap().session.last_error.unwrap();
    assert!(
        err.contains("code 7") && err.contains("fatal: no auth"),
        "{err}"
    );
}

#[tokio::test]
async fn end_to_end_stop_kills_a_silent_process() {
    let dir = tempfile::tempdir().unwrap();
    let script = dir.path().join("silent.sh");
    // Ignores stdin EOF: only a kill ends it.
    std::fs::write(
        &script,
        "#!/bin/sh\ntrap '' TERM\nwhile :; do sleep 0.05; done\n",
    )
    .unwrap();
    let mut config = Config::default();
    config.sessions.stop_grace_secs = 0;
    let store = Store::open(&dir.path().join("w.db")).await.unwrap();
    let driver = Arc::new(ClaudeDriver::with_command(
        "sh",
        vec![script.to_string_lossy().into_owned()],
    ));
    let m = Manager::new(config, store, vec![driver], Launcher::direct());
    let id = m
        .run(RunSpec {
            driver: Some(DriverKind::Claude),
            prompt: "hi".into(),
            cwd: dir.path().to_path_buf(),
            ..Default::default()
        })
        .await
        .unwrap()
        .session
        .id;
    m.stop(&id).await.unwrap();
    wait_state(&m, &id, SessionState::Stopped).await;
}

#[tokio::test]
async fn fifteen_concurrent_sessions_with_real_processes() {
    let dir = tempfile::tempdir().unwrap();
    let script = replay_script(&load("claude", "question"), dir.path());
    let m = manager_for(&script, &dir.path().join("w.db")).await;
    let mut ids = Vec::new();
    for i in 0..15 {
        let out = m
            .run(RunSpec {
                driver: Some(DriverKind::Claude),
                prompt: format!("ask {i}"),
                cwd: dir.path().to_path_buf(),
                agent: Some(format!("agent-{}", i % 3)),
                ..Default::default()
            })
            .await
            .unwrap();
        ids.push(out.session.id);
    }
    for id in &ids {
        wait_state(&m, id, SessionState::WaitingInput).await;
    }
    assert_eq!(m.pending_questions().await.unwrap().len(), 15);
    assert_eq!(m.list(false).await.unwrap().len(), 15);
    for id in &ids {
        m.answer(id, None, AnswerInput::Text("blue".into()))
            .await
            .unwrap();
    }
    for id in &ids {
        wait_state(&m, id, SessionState::Done).await;
        assert_eq!(
            m.status(id).await.unwrap().session.last_result.as_deref(),
            Some("blue")
        );
    }
    assert!(m.live_ids().is_empty());
}
