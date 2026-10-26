//! Antigravity CLI driver tests against fixtures recorded from `agy` 1.4.7
//! (`--input-format stream-json --output-format stream-json`).
//!
//! `interrupt.jsonl` was recorded by sending SIGINT while the tool ran; the
//! signal itself is not part of the fixture format.

mod common;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use common::{Record, collect, load, names, replay_script};
use serde_json::Value;
use wardex_core::config::{AgyApproval, AgyConfig};
use wardex_core::driver::{DriverCommand, SpawnSpec};
use wardex_core::manager::Manager;
use wardex_core::process::Launcher;
use wardex_core::store::Store;
use wardex_core::{
    Answer, Config, DriverKind, EventKind, RunSpec, SessionId, SessionState, ToolStatus,
};
use wardex_drivers::AgyDriver;
use wardex_drivers::agy::AgyProtocol;
use wardex_drivers::runner::Protocol;

const CONVERSATION: &str = "d2eb5f95-689d-4002-a041-b7ee78d78f2b";
const INTERRUPTED: &str = "49736ba7-89b0-4266-8944-99e5fe312959";

fn spec(resume: Option<&str>) -> SpawnSpec {
    SpawnSpec {
        session_id: SessionId("abcd1234".into()),
        cwd: PathBuf::from("/work"),
        model: None,
        reasoning: None,
        readonly: false,
        resume: resume.map(String::from),
        fork: false,
        slice: None,
        unit: "wardex-abcd1234-1".into(),
    }
}

fn prompts(records: &[Record]) -> Vec<String> {
    records
        .iter()
        .filter(|r| r.dir == "in")
        .map(|r| r.msg["message"]["content"].as_str().unwrap().to_string())
        .collect()
}

/// Send every recorded prompt up front, then feed every `out` record.
fn drive(p: &mut AgyProtocol, records: &[Record]) -> (Vec<EventKind>, Vec<Value>) {
    let mut events = Vec::new();
    let mut writes = Vec::new();
    collect(p.on_start(), &mut events, &mut writes);
    for text in prompts(records) {
        collect(
            p.on_command(DriverCommand::Send(text)),
            &mut events,
            &mut writes,
        );
    }
    for r in records.iter().filter(|r| r.dir == "out") {
        collect(p.on_line(&r.msg.to_string()), &mut events, &mut writes);
    }
    (events, writes)
}

fn errors(events: &[EventKind]) -> Vec<&str> {
    events
        .iter()
        .filter_map(|e| match e {
            EventKind::Error { message } => Some(message.as_str()),
            _ => None,
        })
        .collect()
}

#[test]
fn args_carry_model_effort_resume_and_the_operator_mode() {
    let mut s = spec(Some(CONVERSATION));
    s.model = Some("gemini-3.8-flash".into());
    s.reasoning = Some("low".into());
    let args = AgyDriver::new(AgyConfig::default()).args(&s);
    assert_eq!(
        args,
        [
            "--input-format",
            "stream-json",
            "--output-format",
            "stream-json",
            "--model",
            "gemini-3.8-flash",
            "--effort",
            "low",
            "--conversation",
            CONVERSATION,
        ]
    );
    let config = AgyConfig {
        approval: AgyApproval::AlwaysApprove,
        extra_args: vec!["--sandbox".into()],
        ..AgyConfig::default()
    };
    let args = AgyDriver::new(config).args(&spec(None));
    assert_eq!(
        args[4..],
        [
            "--dangerously-skip-permissions".to_string(),
            "--sandbox".into()
        ]
    );
}

#[test]
fn turns_run_one_at_a_time_and_report_the_model_agy_accepted() {
    let records = load("agy", "basic");
    let mut p = AgyProtocol::new(&spec(None));
    let (events, writes) = drive(&mut p, &records);
    // The second prompt waits for the first result.
    let recorded: Vec<Value> = records
        .iter()
        .filter(|r| r.dir == "in")
        .map(|r| r.msg.clone())
        .collect();
    assert_eq!(writes, recorded);
    assert_eq!(
        events[0],
        EventKind::Started {
            driver_session_id: Some(CONVERSATION.into()),
            model: Some("gemini-3.8-flash".into()),
            reasoning: None,
        }
    );
    let results: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            EventKind::Done {
                result,
                interrupted: false,
                ..
            } => result.clone(),
            _ => None,
        })
        .collect();
    assert_eq!(results, ["pong", "ping"]);
}

#[test]
fn queued_prompt_is_written_after_the_result() {
    let records = load("agy", "basic");
    let mut p = AgyProtocol::new(&spec(None));
    assert_eq!(
        p.on_command(DriverCommand::Send("a".into())).writes.len(),
        1
    );
    assert!(
        p.on_command(DriverCommand::Send("b".into()))
            .writes
            .is_empty()
    );
    let result = records.iter().find(|r| r.msg["event"] == "result").unwrap();
    let out = p.on_line(&result.msg.to_string());
    let w: Value = serde_json::from_str(&out.writes[0]).unwrap();
    assert_eq!(w["message"]["content"], "b");
}

#[test]
fn tool_steps_become_tool_calls() {
    let records = load("agy", "tool");
    let mut p = AgyProtocol::new(&spec(None));
    let (events, _) = drive(&mut p, &records);
    assert_eq!(
        names(&events),
        ["started", "tool_call", "tool_call", "message", "done"]
    );
    let tools: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            EventKind::ToolCall {
                call_id,
                name,
                status,
                input,
                output,
                ..
            } => Some((call_id, name, *status, input, output)),
            _ => None,
        })
        .collect();
    assert_eq!(tools[0].0, "step-2");
    assert_eq!(tools[0].1, "run_command");
    assert_eq!(tools[0].2, ToolStatus::Started);
    assert_eq!(tools[0].3.as_deref(), Some("echo hi > wardex_approval.txt"));
    assert_eq!(tools[1].0, "step-2");
    assert_eq!(tools[1].2, ToolStatus::Completed);
    assert!(
        tools[1]
            .4
            .as_deref()
            .unwrap()
            .contains("exited with code 0")
    );
}

#[test]
fn a_tool_error_fails_the_tool_call() {
    let mut p = AgyProtocol::new(&spec(None));
    p.on_command(DriverCommand::Send("x".into()));
    // Built from the recorded step shape plus the `error` field agy defines.
    let line = r#"{"event":"step_update","step_update":{"conversation_id":"c","step_index":4,"state":"DONE","step_type":"tool","tool_name":"view_file","tool_info":{"name":"view_file","parameters":{"AbsolutePath":"/work/x"},"error":"file not found"}}}"#;
    let out = p.on_line(line);
    match &out.events[0] {
        EventKind::ToolCall {
            status,
            result_summary,
            ..
        } => {
            assert_eq!(*status, ToolStatus::Failed);
            assert_eq!(result_summary.as_deref(), Some("file not found"));
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn denied_actions_are_not_reported_as_success() {
    // agy reports SUCCESS and exits 0 when headless mode auto-denies a tool.
    let records = load("agy", "denied");
    let mut p = AgyProtocol::new(&spec(None));
    let (events, _) = drive(&mut p, &records);
    assert!(!names(&events).contains(&"done"), "{events:?}");
    let errors = errors(&events);
    assert_eq!(errors.len(), 1);
    assert!(errors[0].contains("RunCommand"), "{}", errors[0]);
}

#[test]
fn interrupt_signals_the_process_and_ends_the_turn() {
    let records = load("agy", "interrupt");
    let mut p = AgyProtocol::new(&spec(None));
    let mut events = Vec::new();
    let mut writes = Vec::new();
    collect(
        p.on_command(DriverCommand::Send(prompts(&records).remove(0))),
        &mut events,
        &mut writes,
    );
    let (before, after) = records
        .iter()
        .filter(|r| r.dir == "out")
        .partition::<Vec<_>, _>(|r| r.msg["event"] != "result");
    for r in before {
        collect(p.on_line(&r.msg.to_string()), &mut events, &mut writes);
    }
    let out = p.on_command(DriverCommand::Interrupt);
    assert!(out.sigint);
    assert!(!p.on_command(DriverCommand::Interrupt).sigint);
    for r in after {
        collect(p.on_line(&r.msg.to_string()), &mut events, &mut writes);
    }
    assert!(matches!(
        events.last(),
        Some(EventKind::Done {
            interrupted: true,
            ..
        })
    ));
    assert!(errors(&events).is_empty());
}

#[test]
fn an_interrupt_result_without_our_interrupt_is_an_error() {
    let records = load("agy", "interrupt");
    let mut p = AgyProtocol::new(&spec(None));
    let (events, _) = drive(&mut p, &records);
    assert!(errors(&events)[0].contains("interrupted"));
}

#[test]
fn messages_queued_behind_an_interrupt_are_reported_lost() {
    let records = load("agy", "interrupt");
    let mut p = AgyProtocol::new(&spec(None));
    p.on_command(DriverCommand::Send("first".into()));
    p.on_command(DriverCommand::Send("second".into()));
    assert!(p.on_command(DriverCommand::Interrupt).sigint);
    let result = records.iter().find(|r| r.msg["event"] == "result").unwrap();
    let out = p.on_line(&result.msg.to_string());
    assert!(out.writes.is_empty());
    assert_eq!(names(&out.events), ["done", "error"]);
    assert!(errors(&out.events)[0].contains("1 queued message"));
}

#[test]
fn resume_continues_an_interrupted_conversation() {
    let records = load("agy", "resume");
    let s = spec(Some(INTERRUPTED));
    let args = AgyDriver::new(AgyConfig::default()).args(&s);
    assert!(args.ends_with(&["--conversation".into(), INTERRUPTED.into()]));
    let mut p = AgyProtocol::new(&s);
    let (events, _) = drive(&mut p, &records);
    assert!(matches!(
        &events[0],
        EventKind::Started { driver_session_id: Some(id), .. } if id == INTERRUPTED
    ));
    assert!(matches!(
        events.last(),
        Some(EventKind::Done { result: Some(r), .. }) if r == "sleep 60 && echo done"
    ));
}

#[test]
fn headless_agy_skips_questions() {
    let records = load("agy", "question");
    let mut p = AgyProtocol::new(&spec(None));
    let (events, _) = drive(&mut p, &records);
    assert!(!names(&events).contains(&"question"));
    assert_eq!(names(&events).last(), Some(&"done"));
    let out = p.on_command(DriverCommand::Answer {
        question_id: "q".into(),
        answer: Answer::Approval {
            allow: true,
            message: None,
        },
    });
    assert!(out.writes.is_empty() && out.events.is_empty());
}

#[test]
fn a_model_effort_conflict_fails_the_turn() {
    let records = load("agy", "bad_model");
    let mut p = AgyProtocol::new(&spec(None));
    let (events, _) = drive(&mut p, &records);
    assert!(!names(&events).contains(&"started"));
    assert!(errors(&events)[0].contains("conflicts with --effort=high"));
}

#[test]
fn fork_is_refused() {
    let mut s = spec(Some(CONVERSATION));
    s.fork = true;
    assert!(
        !AgyDriver::new(AgyConfig::default())
            .args(&s)
            .contains(&"--conversation".into())
    );
    let out = AgyProtocol::new(&s).on_start();
    assert!(out.close);
    assert_eq!(names(&out.events), ["error"]);
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

async fn manager_with_script(script: PathBuf, dir: &std::path::Path) -> Manager {
    let store = Store::open(&dir.join("w.db")).await.unwrap();
    let driver = Arc::new(AgyDriver::with_command(
        "sh",
        vec![script.to_string_lossy().into_owned()],
    ));
    Manager::new(Config::default(), store, vec![driver], Launcher::direct())
}

fn run_spec(dir: &std::path::Path) -> RunSpec {
    RunSpec {
        driver: Some(DriverKind::Agy),
        prompt: "hi".into(),
        cwd: dir.to_path_buf(),
        ..Default::default()
    }
}

#[tokio::test]
async fn end_to_end_turn_through_a_mock_process() {
    let dir = tempfile::tempdir().unwrap();
    let script = replay_script(&load("agy", "basic"), dir.path());
    let m = manager_with_script(script, dir.path()).await;
    let id = m.run(run_spec(dir.path())).await.unwrap().session.id;
    wait_state(&m, &id, SessionState::Idle).await;
    let st = m.status(&id).await.unwrap();
    assert_eq!(st.session.last_result.as_deref(), Some("pong"));
    assert_eq!(st.session.driver_session_id.as_deref(), Some(CONVERSATION));
}

#[tokio::test]
async fn interrupt_ends_the_process_without_an_error() {
    // Behaves like agy: SIGINT prints an `interrupted` result and exits 1.
    let records = load("agy", "interrupt");
    let line = |event: &str| {
        let r = records.iter().find(|r| r.msg["event"] == event).unwrap();
        r.msg.to_string().replace('\'', "'\\''")
    };
    let dir = tempfile::tempdir().unwrap();
    let script = dir.path().join("agy.sh");
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\non_int() {{ printf '%s\\n' '{}'; exit 1; }}\ntrap on_int INT\nIFS= read -r _\nprintf '%s\\n' '{}'\nwhile :; do sleep 0.05; done\n",
            line("result"),
            line("init"),
        ),
    )
    .unwrap();
    let m = manager_with_script(script, dir.path()).await;
    let id = m.run(run_spec(dir.path())).await.unwrap().session.id;
    for _ in 0..500 {
        let s = m.status(&id).await.unwrap().session;
        if s.driver_session_id.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    m.interrupt(&id).await.unwrap();
    wait_state(&m, &id, SessionState::Done).await;
    let st = m.status(&id).await.unwrap();
    assert_eq!(st.session.last_error, None);
    assert_eq!(st.session.driver_session_id.as_deref(), Some(INTERRUPTED));
}

#[tokio::test]
async fn readonly_sessions_are_refused() {
    let dir = tempfile::tempdir().unwrap();
    let script = replay_script(&load("agy", "basic"), dir.path());
    let m = manager_with_script(script, dir.path()).await;
    let mut spec = run_spec(dir.path());
    spec.readonly = true;
    let err = m.run(spec).await.unwrap_err().to_string();
    assert!(err.contains("read-only"), "{err}");
    assert!(m.list(true).await.unwrap().is_empty());
}
