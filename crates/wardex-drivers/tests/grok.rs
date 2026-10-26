//! Grok Build driver tests against fixtures recorded from Grok 1.0.70
//! (`grok agent stdio`, ACP).
//!
//! The recordings use request ids in the driver's order (initialize,
//! session/new or session/resume, set_config_option..., session/prompt), so
//! recorded responses line up with the driver's requests. Permission requests
//! (`session/request_permission`) were not observed live: on the recording
//! host Grok's own auto mode decided them. Those tests use messages built
//! from the ACP schema and are marked as such.

mod common;

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use common::{Record, collect, load, names, replay_script};
use serde_json::{Value, json};
use wardex_core::config::{GrokApproval, GrokConfig};
use wardex_core::driver::{Driver, DriverCommand, SpawnSpec};
use wardex_core::manager::{AnswerInput, Manager};
use wardex_core::process::Launcher;
use wardex_core::store::Store;
use wardex_core::{
    Answer, Config, DriverKind, EventKind, RunSpec, SessionId, SessionState, ToolStatus,
};
use wardex_drivers::GrokDriver;
use wardex_drivers::grok::GrokProtocol;
use wardex_drivers::runner::Protocol;

const SESSION: &str = "01a175c1-2855-7233-9fbf-1b85df3ca899";

fn spec(model: Option<&str>, reasoning: Option<&str>, resume: Option<&str>) -> SpawnSpec {
    SpawnSpec {
        session_id: SessionId("abcd1234".into()),
        cwd: PathBuf::from("/work"),
        model: model.map(String::from),
        reasoning: reasoning.map(String::from),
        readonly: false,
        resume: resume.map(String::from),
        fork: false,
        slice: None,
        unit: "wardex-abcd1234-1".into(),
    }
}

fn protocol(s: &SpawnSpec) -> GrokProtocol {
    GrokProtocol::new(s, GrokApproval::Inherit)
}

/// Start, queue `prompts`, then feed every `out` record.
fn drive(
    p: &mut GrokProtocol,
    records: &[Record],
    prompts: &[&str],
) -> (Vec<EventKind>, Vec<Value>) {
    let mut events = Vec::new();
    let mut writes = Vec::new();
    collect(p.on_start(), &mut events, &mut writes);
    for text in prompts {
        collect(
            p.on_command(DriverCommand::Send((*text).into())),
            &mut events,
            &mut writes,
        );
    }
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

fn recorded_inputs(records: &[Record]) -> Vec<Value> {
    records
        .iter()
        .filter(|r| r.dir == "in")
        .map(|r| r.msg.clone())
        .collect()
}

fn position(records: &[Record], pred: impl Fn(&Value) -> bool) -> usize {
    records
        .iter()
        .position(|r| r.dir == "out" && pred(&r.msg))
        .unwrap()
}

#[test]
fn args_never_select_a_shared_leader() {
    let d = GrokDriver::new(GrokConfig::default());
    assert_eq!(d.args(), ["agent", "--no-leader", "stdio"]);
    let cfg = GrokConfig {
        approval: GrokApproval::AlwaysApprove,
        extra_args: vec!["--no-subagents".into()],
        ..GrokConfig::default()
    };
    assert_eq!(
        GrokDriver::new(cfg).args(),
        [
            "agent",
            "--no-leader",
            "--always-approve",
            "--no-subagents",
            "stdio"
        ]
    );
    assert_eq!(d.kind(), DriverKind::Grok);
    assert!(!d.supports_readonly());
}

#[test]
fn applies_model_and_effort_and_reports_what_grok_uses() {
    let records = load("grok", "basic");
    let mut p = protocol(&spec(Some("grok-4.7"), Some("low"), None));
    let (events, writes) = drive(
        &mut p,
        &records,
        &["Reply with exactly: pong", "Reply with exactly: ping"],
    );
    assert_eq!(
        methods(&writes),
        [
            "initialize",
            "session/new",
            "session/set_config_option",
            "session/set_config_option",
            "session/prompt",
            "session/prompt"
        ]
    );
    // The same requests (ids, methods, parameters) as the recording.
    let recorded = recorded_inputs(&records);
    for (w, r) in writes.iter().zip(&recorded).skip(1) {
        let mut r = r.clone();
        if r["params"].get("cwd").is_some() {
            r["params"]["cwd"] = w["params"]["cwd"].clone();
        }
        assert_eq!(w, &r);
    }
    assert!(writes[1]["params"].get("_meta").is_none());

    assert_eq!(
        names(&events),
        ["started", "message", "done", "message", "done"]
    );
    match &events[0] {
        EventKind::Started {
            driver_session_id,
            model,
            reasoning,
        } => {
            assert_eq!(driver_session_id.as_deref(), Some(SESSION));
            assert_eq!(model.as_deref(), Some("grok-4.7"));
            assert_eq!(reasoning.as_deref(), Some("low"));
        }
        e => panic!("{e:?}"),
    }
    let results: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            EventKind::Done {
                result,
                interrupted,
                ..
            } => {
                assert!(!interrupted);
                result.clone()
            }
            _ => None,
        })
        .collect();
    assert_eq!(results, ["pong", "ping"]);
}

#[test]
fn reports_unchanged_values_without_setting_them() {
    let records = load("grok", "basic");
    let created = position(&records, |m| m["id"] == 2);
    let mut p = protocol(&spec(None, None, None));
    let (events, writes) = drive(&mut p, &records[..=created], &["hi"]);
    assert_eq!(
        methods(&writes),
        ["initialize", "session/new", "session/prompt"]
    );
    match &events[0] {
        EventKind::Started {
            model, reasoning, ..
        } => {
            // What the user's Grok settings select.
            assert_eq!(model.as_deref(), Some("grok-4.7-build-fast"));
            assert_eq!(reasoning.as_deref(), Some("xhigh"));
        }
        e => panic!("{e:?}"),
    }
}

#[test]
fn an_effort_grok_does_not_apply_fails_the_session() {
    let records = load("grok", "basic");
    let set_effort = position(&records, |m| m["id"] == 4);
    let mut p = protocol(&spec(Some("grok-4.7"), Some("low"), None));
    let (_, _) = drive(&mut p, &records[..set_effort], &["hi"]);
    // The agent answers but keeps another effort (e.g. a model without
    // reasoning effort support drops it).
    let mut reply = records[set_effort].msg.clone();
    for o in reply["result"]["configOptions"].as_array_mut().unwrap() {
        if o["id"] == "reasoning_effort" {
            o["currentValue"] = json!("xhigh");
        }
    }
    let out = p.on_line(&reply.to_string());
    assert!(out.close);
    assert!(out.writes.is_empty(), "no prompt with the wrong effort");
    match &out.events[..] {
        [EventKind::Error { message }] => assert!(message.contains("reasoning_effort")),
        e => panic!("{e:?}"),
    }
    // A rejected value fails too.
    let mut p = protocol(&spec(Some("grok-4.7"), Some("low"), None));
    drive(&mut p, &records[..set_effort], &["hi"]);
    let out = p.on_line(
        &json!({"jsonrpc":"2.0","id":4,"error":{"code":-32602,"message":"unknown value"}})
            .to_string(),
    );
    assert!(out.close);
    assert_eq!(names(&out.events), ["error"]);
}

#[test]
fn resume_continues_without_replaying_history() {
    let records = load("grok", "resume");
    let mut p = protocol(&spec(None, None, Some(SESSION)));
    let (events, writes) = drive(
        &mut p,
        &records,
        &["What word did you reply with in the previous turn? Reply with only that word."],
    );
    assert_eq!(
        methods(&writes),
        ["initialize", "session/resume", "session/prompt"]
    );
    assert_eq!(writes[1]["params"]["sessionId"], SESSION);
    assert_eq!(names(&events), ["started", "message", "done"]);
    match (&events[0], events.last().unwrap()) {
        (
            EventKind::Started {
                driver_session_id,
                model,
                reasoning,
            },
            EventKind::Done { result, .. },
        ) => {
            assert_eq!(driver_session_id.as_deref(), Some(SESSION));
            // The session kept the model and effort it was set to.
            assert_eq!(model.as_deref(), Some("grok-4.7"));
            assert_eq!(reasoning.as_deref(), Some("low"));
            assert_eq!(result.as_deref(), Some("ping"));
        }
        e => panic!("{e:?}"),
    }
}

#[test]
fn ask_mode_turns_always_approve_off_per_session() {
    let s = spec(None, None, None);
    let mut p = GrokProtocol::new(&s, GrokApproval::Ask);
    let init = load("grok", "basic")
        .into_iter()
        .find(|r| r.dir == "out" && r.msg["id"] == 1)
        .unwrap();
    p.on_start();
    let out = p.on_line(&init.msg.to_string());
    let w: Value = serde_json::from_str(&out.writes[0]).unwrap();
    assert_eq!(w["method"], "session/new");
    assert_eq!(w["params"]["_meta"], json!({"yoloMode": false}));
    // Also on resume.
    let mut p = GrokProtocol::new(&spec(None, None, Some(SESSION)), GrokApproval::Ask);
    p.on_start();
    let out = p.on_line(&init.msg.to_string());
    let w: Value = serde_json::from_str(&out.writes[0]).unwrap();
    assert_eq!(w["method"], "session/resume");
    assert_eq!(w["params"]["_meta"], json!({"yoloMode": false}));
}

#[test]
fn fork_is_refused() {
    let mut s = spec(None, None, Some(SESSION));
    s.fork = true;
    let mut p = protocol(&s);
    let init = load("grok", "basic")
        .into_iter()
        .find(|r| r.dir == "out" && r.msg["id"] == 1)
        .unwrap();
    p.on_start();
    let out = p.on_line(&init.msg.to_string());
    assert!(out.close && out.writes.is_empty());
    assert_eq!(names(&out.events), ["error"]);
}

#[test]
fn cancel_ends_the_turn_as_interrupted() {
    let records = load("grok", "cancel");
    let cancel_at = records
        .iter()
        .position(|r| r.dir == "in" && r.msg["method"] == "session/cancel")
        .unwrap();
    let mut p = protocol(&spec(None, None, None));
    let (mut events, mut writes) = drive(
        &mut p,
        &records[..cancel_at],
        &["Run the shell command `sleep 60 && echo done` and reply with its output."],
    );
    collect(
        p.on_command(DriverCommand::Interrupt),
        &mut events,
        &mut writes,
    );
    assert_eq!(writes.last().unwrap(), &records[cancel_at].msg);
    for r in records[cancel_at..].iter().filter(|r| r.dir == "out") {
        collect(p.on_line(&r.msg.to_string()), &mut events, &mut writes);
    }
    assert_eq!(names(&events), ["started", "message", "tool_call", "done"]);
    match events.last().unwrap() {
        EventKind::Done {
            interrupted,
            result,
            ..
        } => {
            assert!(interrupted);
            assert!(result.is_none());
        }
        e => panic!("{e:?}"),
    }
}

#[test]
fn question_is_answered_by_question_text() {
    let records = load("grok", "question");
    let ask = position(&records, |m| m["method"] == "_x.ai/ask_user_question");
    let mut p = protocol(&spec(None, None, None));
    let (events, _) = drive(&mut p, &records[..=ask], &["ask"]);
    let (qid, items) = match events.last().unwrap() {
        EventKind::Question { question_id, items } => (question_id.clone(), items.clone()),
        e => panic!("{e:?}"),
    };
    assert_eq!(qid, "0");
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].id, "Which color do you prefer?");
    let labels: Vec<_> = items[0].options.iter().map(|o| o.label.as_str()).collect();
    assert_eq!(labels, ["red", "blue"]);

    let out = p.on_command(DriverCommand::Answer {
        question_id: qid,
        answer: Answer::Question {
            items: BTreeMap::from([(items[0].id.clone(), vec!["blue".into()])]),
        },
    });
    let answer: Value = serde_json::from_str(&out.writes[0]).unwrap();
    let recorded = records.iter().skip(ask).find(|r| r.dir == "in").unwrap();
    assert_eq!(answer, recorded.msg);

    let mut events = Vec::new();
    let mut writes = Vec::new();
    for r in records[ask + 1..].iter().filter(|r| r.dir == "out") {
        collect(p.on_line(&r.msg.to_string()), &mut events, &mut writes);
    }
    match events.last().unwrap() {
        EventKind::Done { result, .. } => assert_eq!(result.as_deref(), Some("blue")),
        e => panic!("{e:?}"),
    }
}

#[test]
fn grok_auto_mode_decisions_do_not_reach_wardex() {
    let records = load("grok", "auto_blocked");
    let mut p = protocol(&spec(None, None, None));
    let (events, writes) = drive(&mut p, &records, &["push"]);
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, EventKind::Approval { .. } | EventKind::Question { .. }))
    );
    assert!(
        !writes.iter().any(|w| w.get("result").is_some()),
        "nothing was answered"
    );
    let failed = events.iter().any(|e| {
        matches!(
            e,
            EventKind::ToolCall {
                status: ToolStatus::Failed,
                ..
            }
        )
    });
    assert!(failed);
    assert_eq!(events.last().unwrap().name(), "done");
}

/// Built from the ACP `session/request_permission` schema; not recorded.
fn permission_request(options: Value) -> String {
    json!({
        "jsonrpc": "2.0", "id": 7, "method": "session/request_permission",
        "params": {
            "sessionId": "s",
            "toolCall": {"toolCallId": "c1", "title": "Execute `make deploy`", "kind": "execute"},
            "options": options,
        }
    })
    .to_string()
}

fn all_options() -> Value {
    json!([
        {"optionId": "a1", "name": "Allow", "kind": "allow_once"},
        {"optionId": "a2", "name": "Always allow", "kind": "allow_always"},
        {"optionId": "r1", "name": "Reject", "kind": "reject_once"},
    ])
}

fn ready_protocol() -> GrokProtocol {
    let records = load("grok", "basic");
    let created = position(&records, |m| m["id"] == 2);
    let mut p = protocol(&spec(None, None, None));
    drive(&mut p, &records[..=created], &["hi"]);
    p
}

#[test]
fn permission_requests_are_approvals_answered_once() {
    for (allow, option) in [(true, "a1"), (false, "r1")] {
        let mut p = ready_protocol();
        let out = p.on_line(&permission_request(all_options()));
        match &out.events[..] {
            [
                EventKind::Approval {
                    request_id,
                    kind,
                    summary,
                },
            ] => {
                assert_eq!(request_id, "7");
                assert_eq!(kind, "execute");
                assert_eq!(summary, "Execute `make deploy`");
            }
            e => panic!("{e:?}"),
        }
        assert!(out.writes.is_empty(), "never answered on its own");
        let out = p.on_command(DriverCommand::Answer {
            question_id: "7".into(),
            answer: Answer::Approval {
                allow,
                message: None,
            },
        });
        let w: Value = serde_json::from_str(&out.writes[0]).unwrap();
        assert_eq!(
            w,
            json!({"jsonrpc":"2.0","id":7,"result":{"outcome":{"outcome":"selected","optionId":option}}})
        );
    }
    // Without a one-time allow option, an approval is not widened to "always".
    let mut p = ready_protocol();
    p.on_line(&permission_request(json!([
        {"optionId": "a2", "name": "Always allow", "kind": "allow_always"},
        {"optionId": "r1", "name": "Reject", "kind": "reject_once"},
    ])));
    let out = p.on_command(DriverCommand::Answer {
        question_id: "7".into(),
        answer: Answer::Approval {
            allow: true,
            message: None,
        },
    });
    let w: Value = serde_json::from_str(&out.writes[0]).unwrap();
    assert_eq!(w["result"]["outcome"], json!({"outcome": "cancelled"}));
}

#[test]
fn interrupt_cancels_pending_requests() {
    let mut p = ready_protocol();
    p.on_line(&permission_request(all_options()));
    let out = p.on_command(DriverCommand::Interrupt);
    let writes: Vec<Value> = out
        .writes
        .iter()
        .map(|w| serde_json::from_str(w).unwrap())
        .collect();
    assert_eq!(writes[0]["method"], "session/cancel");
    assert_eq!(
        writes[1],
        json!({"jsonrpc":"2.0","id":7,"result":{"outcome":{"outcome":"cancelled"}}})
    );
    assert_eq!(names(&out.events), ["resolved"]);
    // A late answer is not sent.
    let out = p.on_command(DriverCommand::Answer {
        question_id: "7".into(),
        answer: Answer::Approval {
            allow: true,
            message: None,
        },
    });
    assert!(out.writes.is_empty());
}

#[test]
fn unknown_agent_requests_get_an_error() {
    let mut p = ready_protocol();
    let out = p.on_line(
        &json!({"jsonrpc":"2.0","id":9,"method":"fs/write_text_file","params":{}}).to_string(),
    );
    let w: Value = serde_json::from_str(&out.writes[0]).unwrap();
    assert_eq!(w["id"], 9);
    assert_eq!(w["error"]["code"], -32601);
    assert!(out.events.is_empty());
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
    let script = replay_script(&load("grok", fixture), dir);
    let store = Store::open(&dir.join("w.db")).await.unwrap();
    let driver = Arc::new(GrokDriver::with_command(
        "sh",
        vec![script.to_string_lossy().into_owned()],
    ));
    Manager::new(Config::default(), store, vec![driver], Launcher::direct())
}

fn run_spec(dir: &std::path::Path) -> RunSpec {
    RunSpec {
        driver: Some(DriverKind::Grok),
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
        Some("01a175c2-b809-7ad2-8589-5c30aaa7969d")
    );
}

#[tokio::test]
async fn readonly_sessions_are_refused() {
    let dir = tempfile::tempdir().unwrap();
    let m = manager_with("basic", dir.path()).await;
    let mut spec = run_spec(dir.path());
    spec.readonly = true;
    let err = m.run(spec).await.unwrap_err().to_string();
    assert!(err.contains("read-only"), "{err}");
    assert!(m.list(true).await.unwrap().is_empty());
}
