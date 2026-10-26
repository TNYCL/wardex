//! Google Antigravity CLI driver: `agy` in headless stream-json mode. One
//! process per session; prompts are JSON lines on stdin, events JSON lines on
//! stdout. See `docs/drivers.md`.

use std::collections::{HashMap, VecDeque};

use serde_json::{Value, json};
use wardex_core::config::{AgyApproval, AgyConfig};
use wardex_core::driver::{
    Driver, DriverCommand, DriverError, DriverHandle, SpawnSpec, cap_tool_text, summarize,
};
use wardex_core::process::{LaunchRequest, Launcher};
use wardex_core::{DriverKind, EventKind, ToolStatus};

use crate::runner::{Output, Protocol, spawn_with};

const RESULT_SUMMARY: usize = 300;
const INPUT_SUMMARY: usize = 200;
/// Wardex starts `agy` for each session; an update in the middle of one
/// would replace the binary under it. Update `agy` yourself.
const ENV: &[(&str, &str)] = &[("AGY_CLI_DISABLE_AUTO_UPDATE", "1")];

pub struct AgyDriver {
    config: AgyConfig,
    prefix_args: Vec<String>,
}

impl AgyDriver {
    pub fn new(config: AgyConfig) -> Self {
        Self {
            config,
            prefix_args: Vec::new(),
        }
    }

    /// Run `program prefix_args... <agy args>` instead of `agy`.
    pub fn with_command(program: impl Into<String>, prefix_args: Vec<String>) -> Self {
        Self {
            config: AgyConfig {
                bin: program.into(),
                ..AgyConfig::default()
            },
            prefix_args,
        }
    }

    pub fn args(&self, spec: &SpawnSpec) -> Vec<String> {
        let mut a = self.prefix_args.clone();
        a.extend([
            "--input-format".into(),
            "stream-json".into(),
            "--output-format".into(),
            "stream-json".into(),
        ]);
        if let Some(model) = &spec.model {
            a.extend(["--model".into(), model.clone()]);
        }
        if let Some(effort) = &spec.reasoning {
            a.extend(["--effort".into(), effort.clone()]);
        }
        if let Some(id) = spec.resume.as_ref().filter(|_| !spec.fork) {
            a.extend(["--conversation".into(), id.clone()]);
        }
        if self.config.approval == AgyApproval::AlwaysApprove {
            a.push("--dangerously-skip-permissions".into());
        }
        a.extend(self.config.extra_args.iter().cloned());
        a
    }
}

impl Driver for AgyDriver {
    fn kind(&self) -> DriverKind {
        DriverKind::Agy
    }

    /// `--mode plan` was not shown to block writes; there is no read-only
    /// mode Wardex can rely on.
    fn supports_readonly(&self) -> bool {
        false
    }

    fn spawn(&self, spec: SpawnSpec, launcher: &Launcher) -> Result<DriverHandle, DriverError> {
        let args = self.args(&spec);
        let req = LaunchRequest {
            unit: &spec.unit,
            slice: spec.slice.as_deref(),
            program: &self.config.bin,
            args: &args,
            cwd: &spec.cwd,
            env: ENV,
        };
        spawn_with(launcher, &req, AgyProtocol::new(&spec))
    }
}

/// stream-json protocol state for one `agy` process.
pub struct AgyProtocol {
    fork: bool,
    started: bool,
    turn_active: bool,
    queue: VecDeque<String>,
    /// Agent text per `agent_response` step.
    text: HashMap<u64, String>,
    interrupting: bool,
}

impl AgyProtocol {
    pub fn new(spec: &SpawnSpec) -> Self {
        Self {
            fork: spec.fork,
            started: false,
            turn_active: false,
            queue: VecDeque::new(),
            text: HashMap::new(),
            interrupting: false,
        }
    }

    fn prompt(text: &str) -> String {
        json!({"event": "user", "message": {"content": text}}).to_string()
    }

    fn on_step(&mut self, step: &Value, out: &mut Output) {
        let index = step["step_index"].as_u64().unwrap_or_default();
        let done = step["state"] == "DONE";
        match step["step_type"].as_str() {
            Some("agent_response") => {
                if let Some(delta) = step["text_delta"].as_str().filter(|d| !d.is_empty()) {
                    self.text.entry(index).or_default().push_str(delta);
                    out.events.push(EventKind::Message {
                        text: delta.to_string(),
                        delta: true,
                    });
                }
                if done
                    && let Some(text) = self.text.remove(&index)
                    && !text.trim().is_empty()
                {
                    out.events.push(EventKind::Message {
                        text: text.trim_end().to_string(),
                        delta: false,
                    });
                }
            }
            Some("tool") => {
                let info = &step["tool_info"];
                let name = info["name"]
                    .as_str()
                    .or(step["tool_name"].as_str())
                    .unwrap_or("tool")
                    .to_string();
                let call_id = format!("step-{index}");
                if !done {
                    let input = tool_input(&info["parameters"]);
                    out.events.push(EventKind::ToolCall {
                        call_id,
                        name,
                        input_summary: summarize(&input, INPUT_SUMMARY),
                        status: ToolStatus::Started,
                        result_summary: None,
                        input: Some(cap_tool_text(&input)),
                        output: None,
                    });
                    return;
                }
                let error = info["error"].as_str().filter(|e| !e.is_empty());
                let output = error.or(info["output"].as_str()).map(String::from);
                out.events.push(EventKind::ToolCall {
                    call_id,
                    name,
                    input_summary: String::new(),
                    status: if error.is_some() {
                        ToolStatus::Failed
                    } else {
                        ToolStatus::Completed
                    },
                    result_summary: output.as_deref().map(|o| summarize(o, RESULT_SUMMARY)),
                    input: None,
                    output: output.as_deref().map(cap_tool_text),
                });
            }
            _ => {}
        }
    }

    fn on_result(&mut self, result: &Value, out: &mut Output) {
        self.turn_active = false;
        self.text.clear();
        let interrupting = std::mem::take(&mut self.interrupting);
        let status = result["status"].as_str().unwrap_or("");
        let error = result["error"].as_str().filter(|e| !e.is_empty());
        let response = result["response"]
            .as_str()
            .map(str::trim_end)
            .filter(|r| !r.is_empty())
            .map(String::from);
        let denied = denied_actions(&result["denied_actions"]);
        match status {
            "SUCCESS" if denied.is_empty() => out.events.push(EventKind::Done {
                result: response,
                interrupted: false,
                note: None,
                git: None,
            }),
            // Headless agy denies what it would have to ask about and still
            // reports success. Wardex does not call that turn done.
            "SUCCESS" => out.events.push(EventKind::Error {
                message: format!(
                    "agy denied actions that need review ({}); headless agy cannot ask. \
                     Allow them in the Antigravity settings or set drivers.agy.approval",
                    denied.join(", ")
                ),
            }),
            "CANCELED" | "INTERRUPTED" => out.events.push(interrupted()),
            "ERROR" if interrupting => out.events.push(interrupted()),
            _ => out.events.push(EventKind::Error {
                message: format!(
                    "agy turn ended with status {}: {}",
                    if status.is_empty() { "unknown" } else { status },
                    error.unwrap_or("no error message")
                ),
            }),
        }
        if interrupting {
            // The interrupt ends the agy process; later messages never reach it.
            if !self.queue.is_empty() {
                out.events.push(EventKind::Error {
                    message: format!(
                        "{} queued message(s) were not delivered: the agy process ends on \
                         interrupt; send them again",
                        self.queue.len()
                    ),
                });
                self.queue.clear();
            }
        } else if let Some(next) = self.queue.pop_front() {
            self.turn_active = true;
            out.writes.push(Self::prompt(&next));
        }
    }
}

fn interrupted() -> EventKind {
    EventKind::Done {
        result: None,
        interrupted: true,
        note: None,
        git: None,
    }
}

fn denied_actions(v: &Value) -> Vec<String> {
    v.as_array()
        .into_iter()
        .flatten()
        .map(|a| {
            let name = a["display_name"].as_str().or(a["action"].as_str());
            name.unwrap_or("action").to_string()
        })
        .collect()
}

fn tool_input(params: &Value) -> String {
    for key in ["CommandLine", "AbsolutePath", "TargetFile", "Query", "Url"] {
        if let Some(v) = params[key].as_str() {
            return v.to_string();
        }
    }
    match params {
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

impl Protocol for AgyProtocol {
    fn on_start(&mut self) -> Output {
        if self.fork {
            let mut out = Output::default();
            out.events.push(EventKind::Error {
                message: "agy cannot fork a conversation".into(),
            });
            out.close = true;
            return out;
        }
        Output::default()
    }

    fn on_line(&mut self, line: &str) -> Output {
        let mut out = Output::default();
        let Ok(msg) = serde_json::from_str::<Value>(line) else {
            tracing::debug!("non-JSON agy output: {line}");
            return out;
        };
        match msg["event"].as_str() {
            Some("init") if !self.started => {
                self.started = true;
                let id = msg["conversation_id"].as_str().filter(|s| !s.is_empty());
                out.events.push(EventKind::Started {
                    driver_session_id: id.map(String::from),
                    model: msg["init"]["model"].as_str().map(String::from),
                    reasoning: None,
                });
            }
            Some("step_update") => self.on_step(&msg["step_update"], &mut out),
            Some("result") => self.on_result(&msg["result"], &mut out),
            _ => {}
        }
        out
    }

    fn on_command(&mut self, cmd: DriverCommand) -> Output {
        let mut out = Output::default();
        match cmd {
            DriverCommand::Send(text) => {
                if self.turn_active || self.fork {
                    self.queue.push_back(text);
                } else {
                    self.turn_active = true;
                    out.writes.push(Self::prompt(&text));
                }
            }
            DriverCommand::Answer { question_id, .. } => {
                tracing::warn!(question_id, "agy sessions have no questions or approvals");
            }
            DriverCommand::Interrupt => {
                if self.turn_active && !self.interrupting {
                    self.interrupting = true;
                    out.sigint = true;
                }
            }
            DriverCommand::Shutdown | DriverCommand::Drain | DriverCommand::Kill => {}
        }
        out
    }
}
