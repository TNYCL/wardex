//! Codex driver: `codex app-server`, newline-delimited JSON-RPC over stdio.
//! One app-server process per session. See `docs/drivers.md`.

use std::collections::{HashMap, VecDeque};

use serde_json::{Value, json};
use wardex_core::config::CodexConfig;
use wardex_core::driver::{
    Driver, DriverCommand, DriverError, DriverHandle, SpawnSpec, cap_tool_text, summarize,
};
use wardex_core::process::{LaunchRequest, Launcher};
use wardex_core::{Answer, DriverKind, EventKind, QuestionItem, QuestionOption, ToolStatus};

use crate::runner::{Output, Protocol, spawn_with};

const RESULT_SUMMARY: usize = 300;
const INPUT_SUMMARY: usize = 200;
const USER_INPUT: &str = "item/tool/requestUserInput";

pub struct CodexDriver {
    config: CodexConfig,
    prefix_args: Vec<String>,
}

impl CodexDriver {
    pub fn new(config: CodexConfig) -> Self {
        Self {
            config,
            prefix_args: Vec::new(),
        }
    }

    /// Run `program prefix_args... <codex args>` instead of `codex`.
    pub fn with_command(program: impl Into<String>, prefix_args: Vec<String>) -> Self {
        Self {
            config: CodexConfig {
                bin: program.into(),
                ..CodexConfig::default()
            },
            prefix_args,
        }
    }

    pub fn args(&self) -> Vec<String> {
        let mut a = self.prefix_args.clone();
        a.push("app-server".into());
        if self.config.request_user_input {
            a.push("--enable".into());
            a.push("default_mode_request_user_input".into());
        }
        a.extend(self.config.extra_args.iter().cloned());
        a
    }
}

impl Driver for CodexDriver {
    fn kind(&self) -> DriverKind {
        DriverKind::Codex
    }

    fn spawn(&self, spec: SpawnSpec, launcher: &Launcher) -> Result<DriverHandle, DriverError> {
        let args = self.args();
        let req = LaunchRequest {
            unit: &spec.unit,
            slice: spec.slice.as_deref(),
            program: &self.config.bin,
            args: &args,
            cwd: &spec.cwd,
        };
        spawn_with(launcher, &req, CodexProtocol::new(&spec))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Rpc {
    Initialize,
    ThreadStart,
    ThreadResume,
    TurnStart,
    TurnInterrupt,
}

struct ServerRequest {
    id: Value,
    method: String,
    params: Value,
}

/// Codex app-server protocol state.
pub struct CodexProtocol {
    cwd: String,
    model: Option<String>,
    reasoning: Option<String>,
    readonly: bool,
    resume: Option<String>,
    next_id: u64,
    rpcs: HashMap<u64, Rpc>,
    thread_id: Option<String>,
    turn_id: Option<String>,
    turn_active: bool,
    queue: VecDeque<String>,
    requests: HashMap<String, ServerRequest>,
    tools: HashMap<String, String>,
    last_final: Option<String>,
    interrupting: bool,
}

impl CodexProtocol {
    pub fn new(spec: &SpawnSpec) -> Self {
        Self {
            cwd: spec.cwd.to_string_lossy().into_owned(),
            model: spec.model.clone(),
            reasoning: spec.reasoning.clone(),
            readonly: spec.readonly,
            resume: spec.resume.clone(),
            next_id: 0,
            rpcs: HashMap::new(),
            thread_id: None,
            turn_id: None,
            turn_active: false,
            queue: VecDeque::new(),
            requests: HashMap::new(),
            tools: HashMap::new(),
            last_final: None,
            interrupting: false,
        }
    }

    fn request(&mut self, out: &mut Output, rpc: Rpc, method: &str, params: Value) {
        self.next_id += 1;
        self.rpcs.insert(self.next_id, rpc);
        out.writes
            .push(json!({"id": self.next_id, "method": method, "params": params}).to_string());
    }

    fn sandbox(&self) -> &'static str {
        if self.readonly {
            "read-only"
        } else {
            "danger-full-access"
        }
    }

    fn open_thread(&mut self, out: &mut Output) {
        let mut params = json!({
            "cwd": self.cwd,
            "approvalPolicy": "never",
            "sandbox": self.sandbox(),
        });
        if let Some(m) = &self.model {
            params["model"] = json!(m);
        }
        match self.resume.clone() {
            Some(thread_id) => {
                params["threadId"] = json!(thread_id);
                params["excludeTurns"] = json!(true);
                self.request(out, Rpc::ThreadResume, "thread/resume", params);
            }
            None => self.request(out, Rpc::ThreadStart, "thread/start", params),
        }
    }

    fn start_turn(&mut self, out: &mut Output, text: String) {
        let Some(thread_id) = self.thread_id.clone() else {
            self.queue.push_back(text);
            return;
        };
        let mut params = json!({
            "threadId": thread_id,
            "input": [{"type": "text", "text": text}],
        });
        if let Some(r) = &self.reasoning {
            params["effort"] = json!(r);
        }
        if let Some(m) = &self.model {
            params["model"] = json!(m);
        }
        self.turn_active = true;
        self.turn_id = None;
        self.last_final = None;
        self.request(out, Rpc::TurnStart, "turn/start", params);
    }

    fn next_queued(&mut self, out: &mut Output) {
        if !self.turn_active
            && self.thread_id.is_some()
            && let Some(text) = self.queue.pop_front()
        {
            self.start_turn(out, text);
        }
    }

    fn on_response(&mut self, msg: &Value, out: &mut Output) {
        let Some(id) = msg["id"].as_u64() else { return };
        let Some(rpc) = self.rpcs.remove(&id) else {
            return;
        };
        if let Some(err) = msg.get("error") {
            let message = err["message"]
                .as_str()
                .unwrap_or("request failed")
                .to_string();
            match rpc {
                Rpc::Initialize | Rpc::ThreadStart | Rpc::ThreadResume => {
                    out.events.push(EventKind::Error {
                        message: format!("codex {rpc:?} failed: {message}"),
                    });
                    out.close = true;
                }
                Rpc::TurnStart => {
                    self.turn_active = false;
                    out.events.push(EventKind::Error {
                        message: format!("codex turn failed to start: {message}"),
                    });
                    self.next_queued(out);
                }
                Rpc::TurnInterrupt => {
                    tracing::debug!("codex interrupt failed: {message}");
                }
            }
            return;
        }
        let result = &msg["result"];
        match rpc {
            Rpc::Initialize => {
                out.writes
                    .push(json!({"method": "initialized"}).to_string());
                self.open_thread(out);
            }
            Rpc::ThreadStart | Rpc::ThreadResume => {
                let thread_id = result["thread"]["id"].as_str().map(String::from);
                self.thread_id = thread_id.clone();
                out.events.push(EventKind::Started {
                    driver_session_id: thread_id,
                    model: result["model"]
                        .as_str()
                        .or(result["thread"]["model"].as_str())
                        .map(String::from),
                    reasoning: result["reasoningEffort"].as_str().map(String::from),
                });
                self.next_queued(out);
            }
            Rpc::TurnStart => {
                if let Some(turn) = result["turn"]["id"].as_str() {
                    self.turn_id = Some(turn.to_string());
                }
            }
            Rpc::TurnInterrupt => {}
        }
    }

    fn on_server_request(&mut self, msg: &Value, out: &mut Output) {
        let method = msg["method"].as_str().unwrap_or_default().to_string();
        let id = msg["id"].clone();
        let key = match &id {
            Value::String(s) => s.clone(),
            other => other.to_string(),
        };
        let params = msg["params"].clone();
        if method == USER_INPUT {
            let items = params["questions"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|q| QuestionItem {
                    id: q["id"].as_str().unwrap_or_default().to_string(),
                    header: q["header"].as_str().map(String::from),
                    text: q["question"].as_str().unwrap_or_default().to_string(),
                    options: q["options"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .map(|o| QuestionOption {
                            label: o["label"].as_str().unwrap_or_default().to_string(),
                            description: o["description"].as_str().map(String::from),
                        })
                        .collect(),
                    multi_select: false,
                    allow_other: q["isOther"].as_bool().unwrap_or(false),
                })
                .collect();
            self.requests
                .insert(key.clone(), ServerRequest { id, method, params });
            out.events.push(EventKind::Question {
                question_id: key,
                items,
            });
            return;
        }
        let Some(kind) = approval_kind(&method) else {
            out.writes.push(
                json!({"id": id, "error": {"code": -32601, "message": format!("{method} is not supported by wardex")}})
                    .to_string(),
            );
            return;
        };
        let request = ServerRequest { id, method, params };
        if self.readonly {
            out.writes.push(approval_response(&request, false));
            return;
        }
        let summary = summarize(
            request.params["command"]
                .as_str()
                .or(request.params["reason"].as_str())
                .or(request.params["message"].as_str())
                .unwrap_or(&request.params.to_string()),
            INPUT_SUMMARY,
        );
        self.requests.insert(key.clone(), request);
        out.events.push(EventKind::Approval {
            request_id: key,
            kind: kind.into(),
            summary,
        });
    }

    fn on_notification(&mut self, msg: &Value, out: &mut Output) {
        let params = &msg["params"];
        match msg["method"].as_str().unwrap_or_default() {
            "item/agentMessage/delta" => {
                if let Some(text) = params["delta"].as_str() {
                    out.events.push(EventKind::Message {
                        text: text.into(),
                        delta: true,
                    });
                }
            }
            "item/started" => {
                let item = &params["item"];
                if let Some(name) = tool_name(item) {
                    let id = item["id"].as_str().unwrap_or_default().to_string();
                    self.tools.insert(id.clone(), name.clone());
                    let input = tool_input(item);
                    out.events.push(EventKind::ToolCall {
                        call_id: id,
                        name,
                        input_summary: summarize(&input, INPUT_SUMMARY),
                        status: ToolStatus::Started,
                        result_summary: None,
                        input: Some(cap_tool_text(&input)),
                        output: None,
                    });
                }
            }
            "item/completed" => {
                let item = &params["item"];
                if item["type"] == "agentMessage" {
                    let text = item["text"].as_str().unwrap_or_default().to_string();
                    if item["phase"] == "final_answer" || self.last_final.is_none() {
                        self.last_final = Some(text.clone());
                    }
                    if !text.trim().is_empty() {
                        out.events.push(EventKind::Message { text, delta: false });
                    }
                    return;
                }
                let id = item["id"].as_str().unwrap_or_default();
                if let Some(name) = self.tools.remove(id) {
                    let status = item["status"].as_str().unwrap_or("completed");
                    let failed = matches!(status, "failed" | "declined")
                        || item["exitCode"].as_i64().is_some_and(|c| c != 0);
                    let result = item["aggregatedOutput"]
                        .as_str()
                        .map(String::from)
                        .unwrap_or_else(|| status.to_string());
                    out.events.push(EventKind::ToolCall {
                        call_id: id.to_string(),
                        name,
                        input_summary: String::new(),
                        status: if failed {
                            ToolStatus::Failed
                        } else {
                            ToolStatus::Completed
                        },
                        result_summary: Some(summarize(&result, RESULT_SUMMARY)),
                        input: None,
                        output: Some(cap_tool_text(&result)),
                    });
                }
            }
            "turn/completed" => {
                let turn = &params["turn"];
                self.turn_active = false;
                self.turn_id = None;
                self.tools.clear();
                let interrupted = self.interrupting;
                self.interrupting = false;
                match turn["status"].as_str().unwrap_or_default() {
                    "interrupted" => out.events.push(EventKind::Done {
                        result: None,
                        interrupted: true,
                        note: None,
                        git: None,
                    }),
                    "failed" => out.events.push(EventKind::Error {
                        message: turn["error"]["message"]
                            .as_str()
                            .unwrap_or("turn failed")
                            .to_string(),
                    }),
                    _ => out.events.push(EventKind::Done {
                        result: self.last_final.take(),
                        interrupted,
                        note: None,
                        git: None,
                    }),
                }
                self.next_queued(out);
            }
            "error" => {
                tracing::debug!("codex error notification: {}", params);
            }
            _ => {}
        }
    }
}

impl Protocol for CodexProtocol {
    fn on_start(&mut self) -> Output {
        let mut out = Output::default();
        self.request(
            &mut out,
            Rpc::Initialize,
            "initialize",
            json!({
                "clientInfo": {"name": "wardex", "version": env!("CARGO_PKG_VERSION")},
                "capabilities": {"experimentalApi": true},
            }),
        );
        out
    }

    fn on_line(&mut self, line: &str) -> Output {
        let mut out = Output::default();
        let Ok(msg) = serde_json::from_str::<Value>(line) else {
            tracing::debug!("non-JSON line from codex: {line}");
            return out;
        };
        let has_method = msg.get("method").is_some();
        let has_id = msg.get("id").is_some_and(|v| !v.is_null());
        match (has_method, has_id) {
            (false, true) => self.on_response(&msg, &mut out),
            (true, true) => self.on_server_request(&msg, &mut out),
            (true, false) => self.on_notification(&msg, &mut out),
            (false, false) => {}
        }
        out
    }

    fn on_command(&mut self, cmd: DriverCommand) -> Output {
        let mut out = Output::default();
        match cmd {
            DriverCommand::Send(text) => {
                if self.turn_active || self.thread_id.is_none() {
                    self.queue.push_back(text);
                } else {
                    self.start_turn(&mut out, text);
                }
            }
            DriverCommand::Answer {
                question_id,
                answer,
            } => {
                let Some(req) = self.requests.remove(&question_id) else {
                    tracing::warn!(question_id, "answer for unknown codex request");
                    return out;
                };
                let line = match answer {
                    Answer::Question { items } if req.method == USER_INPUT => {
                        let answers: serde_json::Map<String, Value> = items
                            .into_iter()
                            .map(|(k, v)| (k, json!({"answers": v})))
                            .collect();
                        json!({"id": req.id, "result": {"answers": answers}}).to_string()
                    }
                    Answer::Approval { allow, .. } => approval_response(&req, allow),
                    Answer::Question { .. } => approval_response(&req, false),
                };
                out.writes.push(line);
            }
            DriverCommand::Interrupt => {
                if let (Some(thread), Some(turn)) = (self.thread_id.clone(), self.turn_id.clone()) {
                    self.interrupting = true;
                    self.request(
                        &mut out,
                        Rpc::TurnInterrupt,
                        "turn/interrupt",
                        json!({"threadId": thread, "turnId": turn}),
                    );
                }
            }
            DriverCommand::Shutdown | DriverCommand::Kill => {}
        }
        out
    }
}

fn approval_kind(method: &str) -> Option<&'static str> {
    Some(match method {
        "item/commandExecution/requestApproval" => "command",
        "item/fileChange/requestApproval" => "file_change",
        "item/permissions/requestApproval" => "permissions",
        "mcpServer/elicitation/request" => "mcp_elicitation",
        "applyPatchApproval" => "apply_patch",
        "execCommandApproval" => "exec_command",
        _ => return None,
    })
}

fn approval_response(req: &ServerRequest, allow: bool) -> String {
    let result = match req.method.as_str() {
        "applyPatchApproval" | "execCommandApproval" => {
            json!({"decision": if allow { "approved" } else { "denied" }})
        }
        "item/permissions/requestApproval" => {
            if allow {
                json!({"permissions": req.params["permissions"]})
            } else {
                json!({"permissions": {}})
            }
        }
        "mcpServer/elicitation/request" => {
            json!({"action": if allow { "accept" } else { "decline" }})
        }
        _ => json!({"decision": if allow { "accept" } else { "decline" }}),
    };
    json!({"id": req.id, "result": result}).to_string()
}

fn tool_name(item: &Value) -> Option<String> {
    Some(match item["type"].as_str()? {
        "commandExecution" => "shell".into(),
        "fileChange" => "file_change".into(),
        "mcpToolCall" => format!(
            "mcp:{}/{}",
            item["server"].as_str().unwrap_or("?"),
            item["tool"].as_str().unwrap_or("?")
        ),
        "dynamicToolCall" => item["tool"].as_str().unwrap_or("dynamic_tool").into(),
        "webSearch" => "web_search".into(),
        "collabAgentToolCall" => "agent".into(),
        "imageGeneration" => "image_generation".into(),
        _ => return None,
    })
}

fn tool_input(item: &Value) -> String {
    if let Some(c) = item["command"].as_str() {
        return c.to_string();
    }
    if let Some(changes) = item["changes"].as_array() {
        let paths: Vec<&str> = changes.iter().filter_map(|c| c["path"].as_str()).collect();
        return paths.join(", ");
    }
    if let Some(q) = item["query"].as_str() {
        return q.to_string();
    }
    item["arguments"].to_string()
}
