//! Grok Build driver: `grok agent stdio`, the Agent Client Protocol (ACP,
//! JSON-RPC 2.0 over stdio). One agent process per session. See
//! `docs/drivers.md`.

use std::collections::{HashMap, VecDeque};

use serde_json::{Value, json};
use wardex_core::config::{GrokApproval, GrokConfig};
use wardex_core::driver::{
    Driver, DriverCommand, DriverError, DriverHandle, SpawnSpec, cap_tool_text, summarize,
};
use wardex_core::process::{LaunchRequest, Launcher};
use wardex_core::{Answer, DriverKind, EventKind, QuestionItem, QuestionOption, ToolStatus};

use crate::runner::{Output, Protocol, spawn_with};

const RESULT_SUMMARY: usize = 300;
const INPUT_SUMMARY: usize = 200;
const PERMISSION: &str = "session/request_permission";
const QUESTION: &str = "_x.ai/ask_user_question";
const MODEL_OPTION: &str = "model";
const EFFORT_OPTION: &str = "reasoning_effort";

pub struct GrokDriver {
    config: GrokConfig,
    prefix_args: Vec<String>,
}

impl GrokDriver {
    pub fn new(config: GrokConfig) -> Self {
        Self {
            config,
            prefix_args: Vec::new(),
        }
    }

    /// Run `program prefix_args... <grok args>` instead of `grok`.
    pub fn with_command(program: impl Into<String>, prefix_args: Vec<String>) -> Self {
        Self {
            config: GrokConfig {
                bin: program.into(),
                ..GrokConfig::default()
            },
            prefix_args,
        }
    }

    pub fn args(&self) -> Vec<String> {
        let mut a = self.prefix_args.clone();
        // A shared leader process would run the session outside this
        // process's scope.
        a.extend(["agent".into(), "--no-leader".into()]);
        if self.config.approval == GrokApproval::AlwaysApprove {
            a.push("--always-approve".into());
        }
        a.extend(self.config.extra_args.iter().cloned());
        a.push("stdio".into());
        a
    }
}

impl Driver for GrokDriver {
    fn kind(&self) -> DriverKind {
        DriverKind::Grok
    }

    /// `grok agent` has no read-only mode Wardex can select per session.
    fn supports_readonly(&self) -> bool {
        false
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
        spawn_with(
            launcher,
            &req,
            GrokProtocol::new(&spec, self.config.approval),
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Rpc {
    Initialize,
    SessionNew,
    SessionResume,
    SetConfig(&'static str),
    Prompt,
}

struct AgentRequest {
    id: Value,
    method: String,
    params: Value,
}

/// ACP protocol state for one Grok session.
pub struct GrokProtocol {
    cwd: String,
    model: Option<String>,
    reasoning: Option<String>,
    resume: Option<String>,
    fork: bool,
    approval: GrokApproval,
    next_id: u64,
    rpcs: HashMap<u64, Rpc>,
    session_id: Option<String>,
    /// Options still to apply before the first prompt: (config id, value).
    pending_config: VecDeque<(&'static str, String)>,
    /// Values the agent reports for `model` and `reasoning_effort`.
    observed: HashMap<String, String>,
    ready: bool,
    turn_active: bool,
    queue: VecDeque<String>,
    requests: HashMap<String, AgentRequest>,
    tools: HashMap<String, String>,
    /// Agent text since the last tool call, and the last finished segment.
    text: String,
    last_text: Option<String>,
    interrupting: bool,
}

impl GrokProtocol {
    pub fn new(spec: &SpawnSpec, approval: GrokApproval) -> Self {
        Self {
            cwd: spec.cwd.to_string_lossy().into_owned(),
            model: spec.model.clone(),
            reasoning: spec.reasoning.clone(),
            resume: spec.resume.clone(),
            fork: spec.fork,
            approval,
            next_id: 0,
            rpcs: HashMap::new(),
            session_id: None,
            pending_config: VecDeque::new(),
            observed: HashMap::new(),
            ready: false,
            turn_active: false,
            queue: VecDeque::new(),
            requests: HashMap::new(),
            tools: HashMap::new(),
            text: String::new(),
            last_text: None,
            interrupting: false,
        }
    }

    fn request(&mut self, out: &mut Output, rpc: Rpc, method: &str, params: Value) {
        self.next_id += 1;
        self.rpcs.insert(self.next_id, rpc);
        out.writes.push(
            json!({"jsonrpc": "2.0", "id": self.next_id, "method": method, "params": params})
                .to_string(),
        );
    }

    fn fail(&mut self, out: &mut Output, message: String) {
        out.events.push(EventKind::Error { message });
        out.close = true;
    }

    fn open_session(&mut self, out: &mut Output, init: &Value) {
        if self.fork {
            return self.fail(out, "the grok driver cannot fork sessions".into());
        }
        let mut params = json!({"cwd": self.cwd, "mcpServers": []});
        if self.approval == GrokApproval::Ask {
            params["_meta"] = json!({"yoloMode": false});
        }
        match self.resume.clone() {
            Some(id) => {
                // `session/resume` continues without replaying the history
                // (`session/load` would stream it again).
                if init["agentCapabilities"]["sessionCapabilities"]
                    .get("resume")
                    .is_none()
                {
                    return self.fail(out, "this grok version cannot resume sessions".into());
                }
                params["sessionId"] = json!(id);
                self.session_id = Some(id);
                self.request(out, Rpc::SessionResume, "session/resume", params);
            }
            None => self.request(out, Rpc::SessionNew, "session/new", params),
        }
    }

    /// Record `configOptions` current values (`model`, `reasoning_effort`).
    fn observe(&mut self, options: &Value) {
        for o in options.as_array().into_iter().flatten() {
            if let (Some(id), Some(v)) = (o["id"].as_str(), o["currentValue"].as_str()) {
                self.observed.insert(id.to_string(), v.to_string());
            }
        }
    }

    /// Apply the requested model and effort, then report `started`.
    fn configure(&mut self, out: &mut Output) {
        while let Some((id, value)) = self.pending_config.pop_front() {
            if self.observed.get(id) == Some(&value) {
                continue;
            }
            let session = self.session_id.clone().unwrap_or_default();
            self.request(
                out,
                Rpc::SetConfig(id),
                "session/set_config_option",
                json!({"sessionId": session, "configId": id, "value": value}),
            );
            return;
        }
        // The requested values must be the ones in effect.
        let wanted = [
            (MODEL_OPTION, &self.model),
            (EFFORT_OPTION, &self.reasoning),
        ];
        let mismatch = wanted.into_iter().find_map(|(id, wanted)| {
            let wanted = wanted.as_ref()?;
            let seen = self.observed.get(id);
            (seen != Some(wanted)).then(|| {
                format!(
                    "grok did not apply {id} {wanted:?} (it reports {:?})",
                    seen.map(String::as_str).unwrap_or("none")
                )
            })
        });
        if let Some(message) = mismatch {
            return self.fail(out, message);
        }
        self.ready = true;
        out.events.push(EventKind::Started {
            driver_session_id: self.session_id.clone(),
            model: self.observed.get(MODEL_OPTION).cloned(),
            reasoning: self.observed.get(EFFORT_OPTION).cloned(),
        });
        self.next_queued(out);
    }

    fn start_turn(&mut self, out: &mut Output, text: String) {
        let session = self.session_id.clone().unwrap_or_default();
        self.turn_active = true;
        self.text.clear();
        self.last_text = None;
        self.request(
            out,
            Rpc::Prompt,
            "session/prompt",
            json!({"sessionId": session, "prompt": [{"type": "text", "text": text}]}),
        );
    }

    fn next_queued(&mut self, out: &mut Output) {
        if self.ready
            && !self.turn_active
            && let Some(text) = self.queue.pop_front()
        {
            self.start_turn(out, text);
        }
    }

    /// End the current text segment (before a tool call or at turn end).
    fn flush_text(&mut self, out: &mut Output) {
        let text = std::mem::take(&mut self.text);
        if !text.trim().is_empty() {
            out.events.push(EventKind::Message {
                text: text.clone(),
                delta: false,
            });
            self.last_text = Some(text);
        }
    }

    fn end_turn(&mut self, out: &mut Output, stop_reason: &str) {
        self.flush_text(out);
        self.turn_active = false;
        self.tools.clear();
        // Requests belong to the turn; the agent drops them when it ends.
        self.requests.clear();
        let interrupted = std::mem::take(&mut self.interrupting);
        match stop_reason {
            "end_turn" => out.events.push(EventKind::Done {
                result: self.last_text.take(),
                interrupted,
                note: None,
                git: None,
            }),
            "cancelled" => out.events.push(EventKind::Done {
                result: None,
                interrupted: true,
                note: None,
                git: None,
            }),
            other => out.events.push(EventKind::Error {
                message: format!("grok turn stopped: {other}"),
            }),
        }
        self.next_queued(out);
    }

    fn on_response(&mut self, msg: &Value, out: &mut Output) {
        let Some(id) = msg["id"].as_u64() else { return };
        let Some(rpc) = self.rpcs.remove(&id) else {
            return;
        };
        if let Some(err) = msg.get("error") {
            let message = err["message"].as_str().unwrap_or("request failed");
            match rpc {
                Rpc::Prompt => {
                    self.flush_text(out);
                    self.turn_active = false;
                    self.interrupting = false;
                    self.requests.clear();
                    out.events.push(EventKind::Error {
                        message: format!("grok prompt failed: {message}"),
                    });
                    self.next_queued(out);
                }
                Rpc::SetConfig(option) => {
                    self.fail(out, format!("grok rejected {option}: {message}"));
                }
                other => self.fail(out, format!("grok {other:?} failed: {message}")),
            }
            return;
        }
        let result = &msg["result"];
        match rpc {
            Rpc::Initialize => self.open_session(out, result),
            Rpc::SessionNew | Rpc::SessionResume => {
                if let Some(id) = result["sessionId"].as_str() {
                    self.session_id = Some(id.to_string());
                }
                self.observe(&result["configOptions"]);
                if let Some(m) = &self.model {
                    self.pending_config.push_back((MODEL_OPTION, m.clone()));
                }
                if let Some(r) = &self.reasoning {
                    self.pending_config.push_back((EFFORT_OPTION, r.clone()));
                }
                self.configure(out);
            }
            Rpc::SetConfig(_) => {
                self.observe(&result["configOptions"]);
                self.configure(out);
            }
            Rpc::Prompt => {
                let reason = result["stopReason"]
                    .as_str()
                    .unwrap_or("end_turn")
                    .to_string();
                self.end_turn(out, &reason);
            }
        }
    }

    fn on_agent_request(&mut self, msg: &Value, out: &mut Output) {
        let method = msg["method"].as_str().unwrap_or_default().to_string();
        let id = msg["id"].clone();
        let key = match &id {
            Value::String(s) => s.clone(),
            other => other.to_string(),
        };
        let params = msg["params"].clone();
        match method.as_str() {
            PERMISSION => {
                let call = &params["toolCall"];
                let kind = call["kind"]
                    .as_str()
                    .or(call["_meta"]["x.ai/tool"]["kind"].as_str())
                    .unwrap_or("permission")
                    .to_string();
                let summary = summarize(
                    call["title"]
                        .as_str()
                        .unwrap_or(&call["rawInput"].to_string()),
                    INPUT_SUMMARY,
                );
                self.requests
                    .insert(key.clone(), AgentRequest { id, method, params });
                out.events.push(EventKind::Approval {
                    request_id: key,
                    kind,
                    summary,
                });
            }
            QUESTION => {
                let items = params["questions"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|q| {
                        let text = q["question"].as_str().unwrap_or_default().to_string();
                        QuestionItem {
                            // Grok keys answers by question text.
                            id: text.clone(),
                            header: q["header"].as_str().map(String::from),
                            text,
                            options: q["options"]
                                .as_array()
                                .into_iter()
                                .flatten()
                                .map(|o| QuestionOption {
                                    label: o["label"].as_str().unwrap_or_default().to_string(),
                                    description: o["description"].as_str().map(String::from),
                                })
                                .collect(),
                            multi_select: q["multiSelect"].as_bool().unwrap_or(false),
                            allow_other: false,
                        }
                    })
                    .collect();
                self.requests
                    .insert(key.clone(), AgentRequest { id, method, params });
                out.events.push(EventKind::Question {
                    question_id: key,
                    items,
                });
            }
            _ => out.writes.push(
                json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32601, "message": format!("{method} is not supported by wardex")}})
                    .to_string(),
            ),
        }
    }

    fn on_update(&mut self, update: &Value, out: &mut Output) {
        match update["sessionUpdate"].as_str().unwrap_or_default() {
            "agent_message_chunk" => {
                if let Some(text) = update["content"]["text"].as_str() {
                    self.text.push_str(text);
                    out.events.push(EventKind::Message {
                        text: text.into(),
                        delta: true,
                    });
                }
            }
            "tool_call" => {
                self.flush_text(out);
                let call_id = update["toolCallId"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string();
                let name = update["_meta"]["x.ai/tool"]["name"]
                    .as_str()
                    .or(update["title"].as_str())
                    .unwrap_or("tool")
                    .to_string();
                self.tools.insert(call_id.clone(), name.clone());
                let input = tool_input(&update["rawInput"]);
                out.events.push(EventKind::ToolCall {
                    call_id,
                    name,
                    input_summary: summarize(&input, INPUT_SUMMARY),
                    status: ToolStatus::Started,
                    result_summary: None,
                    input: Some(cap_tool_text(&input)),
                    output: None,
                });
            }
            "tool_call_update" => {
                let status = update["status"].as_str().unwrap_or_default();
                if !matches!(status, "completed" | "failed") {
                    return;
                }
                let call_id = update["toolCallId"].as_str().unwrap_or_default();
                let Some(name) = self.tools.remove(call_id) else {
                    return;
                };
                let result = tool_output(update);
                out.events.push(EventKind::ToolCall {
                    call_id: call_id.to_string(),
                    name,
                    input_summary: String::new(),
                    status: if status == "failed" {
                        ToolStatus::Failed
                    } else {
                        ToolStatus::Completed
                    },
                    result_summary: Some(summarize(&result, RESULT_SUMMARY)),
                    input: None,
                    output: Some(cap_tool_text(&result)),
                });
            }
            "config_option_update" => self.observe(&update["configOptions"]),
            _ => {}
        }
    }

    fn answer(&mut self, req: AgentRequest, answer: Answer) -> String {
        let result = match (req.method.as_str(), answer) {
            (PERMISSION, Answer::Approval { allow, .. }) => permission_outcome(&req.params, allow),
            (PERMISSION, Answer::Question { .. }) => permission_outcome(&req.params, false),
            (_, Answer::Question { items }) => {
                let answers: serde_json::Map<String, Value> = items
                    .into_iter()
                    .map(|(k, mut v)| {
                        let v = if v.len() == 1 {
                            json!(v.remove(0))
                        } else {
                            json!(v)
                        };
                        (k, v)
                    })
                    .collect();
                json!({"outcome": "accepted", "answers": answers})
            }
            (_, Answer::Approval { .. }) => json!({"outcome": "cancelled"}),
        };
        json!({"jsonrpc": "2.0", "id": req.id, "result": result}).to_string()
    }
}

/// Allow once or reject. Wardex never picks an "always" option: that would
/// widen the grant beyond the request that was answered.
fn permission_outcome(params: &Value, allow: bool) -> Value {
    let options = params["options"].as_array().cloned().unwrap_or_default();
    let pick = |kinds: &[&str]| {
        kinds.iter().find_map(|kind| {
            options
                .iter()
                .find(|o| o["kind"] == *kind)
                .and_then(|o| o["optionId"].as_str())
        })
    };
    let choice = if allow {
        pick(&["allow_once"])
    } else {
        pick(&["reject_once", "reject_always"])
    };
    match choice {
        Some(id) => json!({"outcome": {"outcome": "selected", "optionId": id}}),
        None => json!({"outcome": {"outcome": "cancelled"}}),
    }
}

fn tool_input(raw: &Value) -> String {
    for key in ["command", "file_path", "path", "query", "url"] {
        if let Some(v) = raw[key].as_str() {
            return v.to_string();
        }
    }
    if raw.is_null() {
        String::new()
    } else {
        raw.to_string()
    }
}

fn tool_output(update: &Value) -> String {
    let raw = &update["rawOutput"];
    if let Some(s) = raw["output_for_prompt"].as_str() {
        return s.to_string();
    }
    let text: Vec<&str> = update["content"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|c| c["content"]["text"].as_str())
        .filter(|t| !t.is_empty())
        .collect();
    if !text.is_empty() {
        return text.join("\n");
    }
    update["status"].as_str().unwrap_or_default().to_string()
}

impl Protocol for GrokProtocol {
    fn on_start(&mut self) -> Output {
        let mut out = Output::default();
        self.request(
            &mut out,
            Rpc::Initialize,
            "initialize",
            json!({
                "protocolVersion": 1,
                "clientCapabilities": {
                    "fs": {"readTextFile": false, "writeTextFile": false},
                    "terminal": false,
                },
                "clientInfo": {"name": "wardex", "version": env!("CARGO_PKG_VERSION")},
            }),
        );
        out
    }

    fn on_line(&mut self, line: &str) -> Output {
        let mut out = Output::default();
        let Ok(msg) = serde_json::from_str::<Value>(line) else {
            tracing::debug!("non-JSON line from grok: {line}");
            return out;
        };
        let has_method = msg.get("method").is_some();
        let has_id = msg.get("id").is_some_and(|v| !v.is_null());
        match (has_method, has_id) {
            (false, true) => self.on_response(&msg, &mut out),
            (true, true) => self.on_agent_request(&msg, &mut out),
            (true, false) if msg["method"] == "session/update" => {
                self.on_update(&msg["params"]["update"], &mut out)
            }
            _ => {}
        }
        out
    }

    fn on_command(&mut self, cmd: DriverCommand) -> Output {
        let mut out = Output::default();
        match cmd {
            DriverCommand::Send(text) => {
                if self.ready && !self.turn_active {
                    self.start_turn(&mut out, text);
                } else {
                    self.queue.push_back(text);
                }
            }
            DriverCommand::Answer {
                question_id,
                answer,
            } => match self.requests.remove(&question_id) {
                Some(req) => out.writes.push(self.answer(req, answer)),
                None => tracing::warn!(question_id, "answer for unknown grok request"),
            },
            DriverCommand::Interrupt => {
                if let (true, Some(session)) = (self.turn_active, self.session_id.clone()) {
                    self.interrupting = true;
                    out.writes.push(
                        json!({"jsonrpc": "2.0", "method": "session/cancel", "params": {"sessionId": session}})
                            .to_string(),
                    );
                    // ACP: pending requests of a cancelled turn are answered
                    // with `cancelled`.
                    for (key, req) in std::mem::take(&mut self.requests) {
                        let result = if req.method == PERMISSION {
                            json!({"outcome": {"outcome": "cancelled"}})
                        } else {
                            json!({"outcome": "cancelled"})
                        };
                        out.writes.push(
                            json!({"jsonrpc": "2.0", "id": req.id, "result": result}).to_string(),
                        );
                        out.events.push(EventKind::Resolved { question_id: key });
                    }
                }
            }
            DriverCommand::Shutdown | DriverCommand::Drain | DriverCommand::Kill => {}
        }
        out
    }
}
