//! Codex driver tests against fixtures recorded from Codex CLI 0.163.0.
//!
//! The recording harness used request ids 1 (initialize), 2 (thread start or
//! resume), 3 (turn/start) and 4 (turn/interrupt), which is also the order the
//! driver uses, so recorded responses line up with the driver's requests.

mod common;

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use common::{Record, collect, load, names, replay_script};
use serde_json::{Value, json};
use wardex_core::config::CodexConfig;
use wardex_core::driver::{DriverCommand, SpawnSpec};
use wardex_core::manager::{AnswerInput, Manager};
use wardex_core::process::Launcher;
use wardex_core::store::Store;
use wardex_core::{
    Answer, Config, DriverKind, EventKind, RunSpec, SessionId, SessionState, ToolStatus,
};
use wardex_drivers::CodexDriver;
use wardex_drivers::codex::CodexProtocol;
use wardex_drivers::runner::Protocol;

fn spec(readonly: bool, resume: Option<&str>) -> SpawnSpec {
    SpawnSpec {
        session_id: SessionId("abcd1234".into()),
        cwd: PathBuf::from("/work"),
        model: Some("gpt-6-luna".into()),
        reasoning: Some("low".into()),
        readonly,
        resume: resume.map(String::from),
        slice: None,
        unit: "wardex-abcd1234-1".into(),
    }
}

/// Start the protocol, queue a prompt, then feed every `out` record.
fn drive(p: &mut CodexProtocol, records: &[Record]) -> (Vec<EventKind>, Vec<Value>) {
    let mut events = Vec::new();
    let mut writes = Vec::new();
    collect(p.on_start(), &mut events, &mut writes);
    collect(
        p.on_command(DriverCommand::Send("hi".into())),
        &mut events,
        &mut writes,
    );
    for r in records.iter().filter(|r| r.dir == "out") {
        collect(p.on_line(&r.msg.to_string()), &mut events, &mut writes);
    }
    (events, writes)
}

fn methods(writes: &[Value]) -> Vec<String> {
    writes
        .iter()
        .map(|w| {
            w["method"]
                .as_str()
                .map(String::from)
                .unwrap_or_else(|| format!("response:{}", w["id"]))
        })
        .collect()
}

#[test]
fn handshake_tool_call_and_done() {
    let mut p = CodexProtocol::new(&spec(false, None));
    let (events, writes) = drive(&mut p, &load("codex", "tool"));
    assert_eq!(
        methods(&writes),
        ["initialize", "initialized", "thread/start", "turn/start"]
    );
    assert_eq!(writes[2]["params"]["approvalPolicy"], "never");
    assert_eq!(writes[2]["params"]["sandbox"], "danger-full-access");
    assert_eq!(writes[2]["params"]["model"], "gpt-6-luna");
    assert_eq!(writes[3]["params"]["effort"], "low");
    assert_eq!(writes[3]["params"]["input"][0]["text"], "hi");

    assert_eq!(
        names(&events),
        ["started", "tool_call", "tool_call", "message", "done"]
    );
    match &events[0] {
        EventKind::Started {
            driver_session_id,
            model,
            reasoning,
        } => {
            assert_eq!(
                driver_session_id.as_deref(),
                Some("01a15013-9936-7f63-9424-379ccce357ea")
            );
            // The resolved model and effort from `thread/start`.
            assert!(model.is_some());
            assert!(reasoning.is_some());
        }
        e => panic!("{e:?}"),
    }
    let statuses: Vec<ToolStatus> = events
        .iter()
        .filter_map(|e| match e {
            EventKind::ToolCall {
                status,
                name,
                input,
                output,
                ..
            } => {
                assert_eq!(name, "shell");
                match status {
                    ToolStatus::Started => assert!(input.is_some() && output.is_none()),
                    _ => assert!(input.is_none() && output.is_some()),
                }
                Some(*status)
            }
            _ => None,
        })
        .collect();
    assert_eq!(statuses, [ToolStatus::Started, ToolStatus::Completed]);
    match events.last().unwrap() {
        EventKind::Done { result, .. } => assert_eq!(result.as_deref(), Some("1 file.")),
        e => panic!("{e:?}"),
    }
}

#[test]
fn question_answer_matches_recorded_response() {
    let records = load("codex", "question");
    let ask = records
        .iter()
        .position(|r| r.dir == "out" && r.msg["method"] == "item/tool/requestUserInput")
        .unwrap();
    let mut p = CodexProtocol::new(&spec(false, None));
    let (events, _) = drive(&mut p, &records[..=ask]);
    let (qid, items) = match events.last().unwrap() {
        EventKind::Question { question_id, items } => (question_id.clone(), items.clone()),
        e => panic!("{e:?}"),
    };
    assert_eq!(qid, "0");
    assert_eq!(items[0].id, "color_preference");
    assert!(items[0].allow_other);

    let mut answers = BTreeMap::new();
    answers.insert("color_preference".to_string(), vec!["blue".to_string()]);
    let mut ev = Vec::new();
    let mut writes = Vec::new();
    collect(
        p.on_command(DriverCommand::Answer {
            question_id: qid,
            answer: Answer::Question { items: answers },
        }),
        &mut ev,
        &mut writes,
    );
    let recorded = records
        .iter()
        .find(|r| r.dir == "in" && r.msg.get("result").is_some())
        .unwrap();
    assert_eq!(writes, std::slice::from_ref(&recorded.msg));

    let mut events = Vec::new();
    for r in records[ask + 1..].iter().filter(|r| r.dir == "out") {
        collect(p.on_line(&r.msg.to_string()), &mut events, &mut writes);
    }
    match events.iter().rev().find(|e| e.name() == "done").unwrap() {
        EventKind::Done { result, .. } => assert_eq!(result.as_deref(), Some("blue")),
        _ => unreachable!(),
    }
}

#[test]
fn approval_and_decline() {
    let records = load("codex", "approval");
    let ask = records
        .iter()
        .position(|r| r.dir == "out" && r.msg["method"] == "item/commandExecution/requestApproval")
        .unwrap();
    let mut p = CodexProtocol::new(&spec(false, None));
    let (events, _) = drive(&mut p, &records[..=ask]);
    let (rid, kind, summary) = match events.last().unwrap() {
        EventKind::Approval {
            request_id,
            kind,
            summary,
        } => (request_id.clone(), kind.clone(), summary.clone()),
        e => panic!("{e:?}"),
    };
    assert_eq!(kind, "command");
    assert!(summary.contains("rm -f nonexistent_file.txt"));
    let mut ev = Vec::new();
    let mut writes = Vec::new();
    collect(
        p.on_command(DriverCommand::Answer {
            question_id: rid,
            answer: Answer::Approval {
                allow: false,
                message: None,
            },
        }),
        &mut ev,
        &mut writes,
    );
    assert_eq!(
        writes,
        [json!({"id": 0, "result": {"decision": "decline"}})]
    );

    // A read-only session declines without asking.
    let mut p = CodexProtocol::new(&spec(true, None));
    let (events, writes) = drive(&mut p, &records[..=ask]);
    assert!(!events.iter().any(|e| e.name() == "approval"));
    assert_eq!(writes.last().unwrap()["result"]["decision"], "decline");
}

#[test]
fn readonly_uses_the_read_only_sandbox() {
    let mut p = CodexProtocol::new(&spec(true, None));
    let (events, writes) = drive(&mut p, &load("codex", "readonly"));
    assert_eq!(writes[2]["params"]["sandbox"], "read-only");
    assert_eq!(names(&events).last(), Some(&"done"));
}

#[test]
fn interrupt_sends_turn_interrupt() {
    let records = load("codex", "interrupt");
    let started = records
        .iter()
        .position(|r| {
            r.dir == "out"
                && r.msg["method"] == "item/started"
                && r.msg["params"]["item"]["type"] == "commandExecution"
        })
        .unwrap();
    let mut p = CodexProtocol::new(&spec(false, None));
    let (_, _) = drive(&mut p, &records[..=started]);
    let mut events = Vec::new();
    let mut writes = Vec::new();
    collect(
        p.on_command(DriverCommand::Interrupt),
        &mut events,
        &mut writes,
    );
    assert_eq!(writes[0]["method"], "turn/interrupt");
    assert_eq!(writes[0]["id"], 4);
    assert_eq!(
        writes[0]["params"]["turnId"],
        "01a15013-998d-7ba3-b364-17e1ef0652c1"
    );
    for r in records[started + 1..].iter().filter(|r| r.dir == "out") {
        collect(p.on_line(&r.msg.to_string()), &mut events, &mut writes);
    }
    assert!(matches!(
        events.last(),
        Some(EventKind::Done {
            interrupted: true,
            ..
        })
    ));
}

#[test]
fn resume_uses_thread_resume() {
    let mut p = CodexProtocol::new(&spec(false, Some("01a15013-9936-7f63-9424-379ccce357ea")));
    let (events, writes) = drive(&mut p, &load("codex", "resume"));
    assert_eq!(writes[2]["method"], "thread/resume");
    assert_eq!(
        writes[2]["params"]["threadId"],
        "01a15013-9936-7f63-9424-379ccce357ea"
    );
    assert_eq!(writes[2]["params"]["excludeTurns"], true);
    match events.last().unwrap() {
        EventKind::Done { result, .. } => assert_eq!(result.as_deref(), Some("file")),
        e => panic!("{e:?}"),
    }
}

#[test]
fn messages_during_a_turn_are_queued() {
    let mut p = CodexProtocol::new(&spec(false, None));
    let (_, writes) = drive(&mut p, &load("codex", "tool")[..12]);
    let turn_starts = writes
        .iter()
        .filter(|w| w["method"] == "turn/start")
        .count();
    assert_eq!(turn_starts, 1);
    let out = p.on_command(DriverCommand::Send("next".into()));
    assert!(out.writes.is_empty(), "queued while the turn runs");
}

#[test]
fn unsupported_server_requests_get_an_error() {
    let mut p = CodexProtocol::new(&spec(false, None));
    let out = p.on_line(r#"{"id":7,"method":"item/tool/call","params":{}}"#);
    let w: Value = serde_json::from_str(&out.writes[0]).unwrap();
    assert_eq!(w["id"], 7);
    assert_eq!(w["error"]["code"], -32601);
}

#[test]
fn args_enable_questions_when_configured() {
    assert_eq!(
        CodexDriver::new(CodexConfig::default()).args(),
        ["app-server"]
    );
    let cfg = CodexConfig {
        request_user_input: true,
        ..CodexConfig::default()
    };
    assert_eq!(
        CodexDriver::new(cfg).args(),
        ["app-server", "--enable", "default_mode_request_user_input"]
    );
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

async fn manager_with(fixture: &str, dir: &std::path::Path) -> Manager {
    let script = replay_script(&load("codex", fixture), dir);
    let store = Store::open(&dir.join("w.db")).await.unwrap();
    let driver = Arc::new(CodexDriver::with_command(
        "sh",
        vec![script.to_string_lossy().into_owned()],
    ));
    Manager::new(Config::default(), store, vec![driver], Launcher::direct())
}

fn run_spec(dir: &std::path::Path) -> RunSpec {
    RunSpec {
        driver: Some(DriverKind::Codex),
        prompt: "hi".into(),
        cwd: dir.to_path_buf(),
        ..Default::default()
    }
}

#[tokio::test]
async fn end_to_end_question_through_a_mock_process() {
    let dir = tempfile::tempdir().unwrap();
    let m = manager_with("question", dir.path()).await;
    let id = m.run(run_spec(dir.path())).await.unwrap().session.id;
    wait_state(&m, &id, SessionState::WaitingInput).await;
    m.answer(&id, None, AnswerInput::Text("blue".into()))
        .await
        .unwrap();
    wait_state(&m, &id, SessionState::Done).await;
    let st = m.status(&id).await.unwrap();
    assert_eq!(st.session.last_result.as_deref(), Some("blue"));
    assert_eq!(
        st.session.driver_session_id.as_deref(),
        Some("01a15013-9937-73a0-b79e-072d56e54afc")
    );
}

#[tokio::test]
async fn end_to_end_tool_turn() {
    let dir = tempfile::tempdir().unwrap();
    let m = manager_with("tool", dir.path()).await;
    let id = m.run(run_spec(dir.path())).await.unwrap().session.id;
    wait_state(&m, &id, SessionState::Done).await;
    let summary = m.summary(&id).await.unwrap();
    assert_eq!(summary.last_result.as_deref(), Some("1 file."));
    assert_eq!(summary.tool_counts.get("shell"), Some(&1));
}
