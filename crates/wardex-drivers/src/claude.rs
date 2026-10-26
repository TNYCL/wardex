//! Claude Code driver: `claude -p` with stream-json in and out, and the SDK
//! control protocol (`--permission-prompt-tool stdio`) for questions,
//! permission requests and interrupts. See `docs/drivers.md`.

use std::collections::HashMap;

use serde_json::{Value, json};
use wardex_core::config::ClaudeConfig;
use wardex_core::driver::{
    Driver, DriverCommand, DriverError, DriverHandle, SpawnSpec, cap_tool_text, summarize,
};
use wardex_core::process::{LaunchRequest, Launcher};
use wardex_core::{Answer, DriverKind, EventKind, QuestionItem, QuestionOption, ToolStatus};

use crate::runner::{Output, Protocol, spawn_with};

const ASK_TOOL: &str = "AskUserQuestion";
const RESULT_SUMMARY: usize = 300;
const INPUT_SUMMARY: usize = 200;

pub struct ClaudeDriver {
    config: ClaudeConfig,
    /// Arguments placed before the generated ones (tests run `sh -c ...`).
    prefix_args: Vec<String>,
}

impl ClaudeDriver {
    pub fn new(config: ClaudeConfig) -> Self {
        Self {
            config,
            prefix_args: Vec::new(),
        }
    }

    /// Run `program prefix_args... <claude args>` instead of `claude`.
    pub fn with_command(program: impl Into<String>, prefix_args: Vec<String>) -> Self {
        Self {
            config: ClaudeConfig {
                bin: program.into(),
                ..ClaudeConfig::default()
            },
            prefix_args,
        }
    }

    /// Arguments for `spec`. A new session or a fork gets `new_id` as its
    /// `--session-id`.
    pub fn args(&self, spec: &SpawnSpec, new_id: &str) -> Vec<String> {
        let mut a: Vec<String> = self.prefix_args.clone();
        a.extend(
            [
                "-p",
                "--input-format",
                "stream-json",
                "--output-format",
                "stream-json",
                "--verbose",
                "--include-partial-messages",
                "--permission-prompt-tool",
                "stdio",
                "--permission-mode",
            ]
            .map(String::from),
        );
        a.push(
            if spec.readonly {
                "plan"
            } else {
                "bypassPermissions"
            }
            .into(),
        );
        match &spec.resume {
            Some(id) if spec.fork => {
                // Claude accepts --session-id with --resume only together
                // with --fork-session, so the fork's id is known up front.
                a.push("--resume".into());
                a.push(id.clone());
                a.push("--fork-session".into());
                a.push("--session-id".into());
                a.push(new_id.to_string());
            }
            Some(id) => {
                a.push("--resume".into());
                a.push(id.clone());
            }
            None => {
                a.push("--session-id".into());
                a.push(new_id.to_string());
            }
        }
        if let Some(m) = &spec.model {
            a.push("--model".into());
            a.push(m.clone());
        }
        if let Some(r) = &spec.reasoning {
            a.push("--effort".into());
            a.push(r.clone());
        }
        a.extend(self.config.extra_args.iter().cloned());
        a
    }
}

impl Driver for ClaudeDriver {
    fn kind(&self) -> DriverKind {
        DriverKind::Claude
    }

    fn spawn(&self, spec: SpawnSpec, launcher: &Launcher) -> Result<DriverHandle, DriverError> {
        let new_id = uuid::Uuid::new_v4().to_string();
        let args = self.args(&spec, &new_id);
        let req = LaunchRequest {
            unit: &spec.unit,
            slice: spec.slice.as_deref(),
            program: &self.config.bin,
            args: &args,
            cwd: &spec.cwd,
            env: &[],
        };
        let mut handle = spawn_with(launcher, &req, ClaudeProtocol::new(spec.readonly))?;
        if spec.resume.is_none() || spec.fork {
            handle.driver_session_id = Some(new_id);
        }
        Ok(handle)
    }
}

struct PendingRequest {
    tool: String,
    input: Value,
}

/// Claude stream-json protocol state.
pub struct ClaudeProtocol {
    readonly: bool,
    started: bool,
    /// tool_use id -> tool name, for tool results.
    tools: HashMap<String, String>,
    /// control request id -> request, for answers.
    pending: HashMap<String, PendingRequest>,
    note: Option<String>,
    interrupting: bool,
    interrupts: u32,
}

impl ClaudeProtocol {
    pub fn new(readonly: bool) -> Self {
        Self {
            readonly,
            started: false,
            tools: HashMap::new(),
            pending: HashMap::new(),
            note: None,
            interrupting: false,
            interrupts: 0,
        }
    }

    fn control_response(request_id: &str, response: Value) -> String {
        json!({
            "type": "control_response",
            "response": {"subtype": "success", "request_id": request_id, "response": response}
        })
        .to_string()
    }

    fn on_control_request(&mut self, msg: &Value) -> Output {
        let out = Output::default();
        let request_id = msg["request_id"].as_str().unwrap_or_default().to_string();
        let req = &msg["request"];
        if req["subtype"] != "can_use_tool" {
            let reply = json!({
                "type": "control_response",
                "response": {
                    "subtype": "error",
                    "request_id": request_id,
                    "error": format!("unsupported control request {}", req["subtype"]),
                }
            });
            return out.write(reply.to_string());
        }
        let tool = req["tool_name"].as_str().unwrap_or_default().to_string();
        let input = req["input"].clone();
        if tool == ASK_TOOL {
            let items = question_items(&input);
            self.pending
                .insert(request_id.clone(), PendingRequest { tool, input });
            return out.event(EventKind::Question {
                question_id: request_id,
                items,
            });
        }
        if self.readonly {
            // Plan mode routes writes (and ExitPlanMode) here; a read-only
            // session refuses them all.
            return out.write(Self::control_response(
                &request_id,
                json!({"behavior": "deny", "message": "Read-only session: this action is not allowed."}),
            ));
        }
        let kind = match tool.as_str() {
            "ExitPlanMode" => "exit_plan".to_string(),
            other => other.to_string(),
        };
        let summary = input_summary(&tool, &input);
        self.pending
            .insert(request_id.clone(), PendingRequest { tool, input });
        out.event(EventKind::Approval {
            request_id,
            kind,
            summary,
        })
    }
}

impl Protocol for ClaudeProtocol {
    fn on_line(&mut self, line: &str) -> Output {
        let mut out = Output::default();
        let Ok(msg) = serde_json::from_str::<Value>(line) else {
            tracing::debug!("non-JSON line from claude: {line}");
            return out;
        };
        let top_level = msg["parent_tool_use_id"].is_null();
        match msg["type"].as_str().unwrap_or_default() {
            "system" => match msg["subtype"].as_str().unwrap_or_default() {
                "init" if !self.started => {
                    self.started = true;
                    out.events.push(EventKind::Started {
                        driver_session_id: msg["session_id"].as_str().map(String::from),
                        model: msg["model"].as_str().map(String::from),
                        reasoning: None,
                    });
                }
                "post_turn_summary" => {
                    self.note = msg["status_detail"]
                        .as_str()
                        .filter(|s| !s.is_empty())
                        .map(String::from);
                }
                _ => {}
            },
            "stream_event" if top_level => {
                let ev = &msg["event"];
                if ev["type"] == "content_block_delta"
                    && ev["delta"]["type"] == "text_delta"
                    && let Some(text) = ev["delta"]["text"].as_str()
                {
                    out.events.push(EventKind::Message {
                        text: text.to_string(),
                        delta: true,
                    });
                }
            }
            "assistant" if top_level => {
                for block in msg["message"]["content"].as_array().into_iter().flatten() {
                    match block["type"].as_str().unwrap_or_default() {
                        "text" => {
                            let text = block["text"].as_str().unwrap_or_default();
                            if !text.trim().is_empty() {
                                out.events.push(EventKind::Message {
                                    text: text.to_string(),
                                    delta: false,
                                });
                            }
                        }
                        "tool_use" => {
                            let name = block["name"].as_str().unwrap_or_default().to_string();
                            if name == ASK_TOOL {
                                continue;
                            }
                            let id = block["id"].as_str().unwrap_or_default().to_string();
                            self.tools.insert(id.clone(), name.clone());
                            out.events.push(EventKind::ToolCall {
                                call_id: id,
                                input_summary: input_summary(&name, &block["input"]),
                                name,
                                status: ToolStatus::Started,
                                result_summary: None,
                                input: Some(cap_tool_text(&block["input"].to_string())),
                                output: None,
                            });
                        }
                        _ => {}
                    }
                }
            }
            "user" if top_level => {
                for block in msg["message"]["content"].as_array().into_iter().flatten() {
                    if block["type"] != "tool_result" {
                        continue;
                    }
                    let id = block["tool_use_id"].as_str().unwrap_or_default();
                    let Some(name) = self.tools.remove(id) else {
                        continue;
                    };
                    let failed = block["is_error"].as_bool().unwrap_or(false);
                    let text = content_text(&block["content"]);
                    out.events.push(EventKind::ToolCall {
                        call_id: id.to_string(),
                        name,
                        input_summary: String::new(),
                        status: if failed {
                            ToolStatus::Failed
                        } else {
                            ToolStatus::Completed
                        },
                        result_summary: Some(summarize(&text, RESULT_SUMMARY)),
                        input: None,
                        output: Some(cap_tool_text(&text)),
                    });
                }
            }
            "control_request" => {
                let o = self.on_control_request(&msg);
                out.events.extend(o.events);
                out.writes.extend(o.writes);
            }
            "result" => {
                self.tools.clear();
                let note = self.note.take();
                let result = msg["result"].as_str().map(String::from);
                if self.interrupting {
                    self.interrupting = false;
                    out.events.push(EventKind::Done {
                        result: None,
                        interrupted: true,
                        note,
                        git: None,
                    });
                } else if msg["is_error"].as_bool().unwrap_or(false) {
                    let message = result
                        .filter(|r| !r.is_empty())
                        .or_else(|| {
                            msg["errors"]
                                .as_array()
                                .map(|e| {
                                    e.iter()
                                        .filter_map(|x| x.as_str())
                                        .collect::<Vec<_>>()
                                        .join("; ")
                                })
                                .filter(|s| !s.is_empty())
                        })
                        .unwrap_or_else(|| {
                            format!(
                                "turn failed: {}",
                                msg["subtype"].as_str().unwrap_or("error")
                            )
                        });
                    out.events.push(EventKind::Error { message });
                } else {
                    out.events.push(EventKind::Done {
                        result,
                        interrupted: false,
                        note,
                        git: None,
                    });
                }
            }
            _ => {}
        }
        out
    }

    fn on_command(&mut self, cmd: DriverCommand) -> Output {
        let out = Output::default();
        match cmd {
            DriverCommand::Send(text) => out.write(
                json!({"type": "user", "message": {"role": "user", "content": text}}).to_string(),
            ),
            DriverCommand::Answer {
                question_id,
                answer,
            } => {
                let Some(req) = self.pending.remove(&question_id) else {
                    tracing::warn!(question_id, "answer for unknown claude request");
                    return out;
                };
                let response = match answer {
                    Answer::Question { items } if req.tool == ASK_TOOL => {
                        let mut answers = serde_json::Map::new();
                        for q in req.input["questions"].as_array().into_iter().flatten() {
                            let text = q["question"].as_str().unwrap_or_default();
                            if let Some(a) = items.get(text) {
                                answers.insert(text.to_string(), Value::String(a.join(", ")));
                            }
                        }
                        let mut input = req.input.clone();
                        input["answers"] = Value::Object(answers);
                        json!({"behavior": "allow", "updatedInput": input})
                    }
                    Answer::Approval { allow: true, .. } => {
                        json!({"behavior": "allow", "updatedInput": req.input})
                    }
                    Answer::Approval {
                        allow: false,
                        message,
                    } => json!({
                        "behavior": "deny",
                        "message": message.unwrap_or_else(|| "Denied by the operator.".into()),
                    }),
                    Answer::Question { .. } => json!({
                        "behavior": "deny",
                        "message": "Unexpected answer type.",
                    }),
                };
                out.write(Self::control_response(&question_id, response))
            }
            DriverCommand::Interrupt => {
                self.interrupting = true;
                self.interrupts += 1;
                out.write(
                    json!({
                        "type": "control_request",
                        "request_id": format!("wardex-interrupt-{}", self.interrupts),
                        "request": {"subtype": "interrupt"},
                    })
                    .to_string(),
                )
            }
            DriverCommand::Shutdown | DriverCommand::Drain | DriverCommand::Kill => out,
        }
    }
}

fn question_items(input: &Value) -> Vec<QuestionItem> {
    input["questions"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|q| {
            let text = q["question"].as_str().unwrap_or_default().to_string();
            QuestionItem {
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
                allow_other: true,
            }
        })
        .collect()
}

/// Short description of a tool input.
pub fn input_summary(tool: &str, input: &Value) -> String {
    let field = match tool {
        "Bash" => "command",
        "Read" | "Write" | "Edit" | "MultiEdit" | "NotebookEdit" => "file_path",
        "Glob" | "Grep" => "pattern",
        "WebFetch" => "url",
        "WebSearch" => "query",
        "Task" | "Agent" => "description",
        "ExitPlanMode" => "plan",
        _ => "",
    };
    let text = input[field]
        .as_str()
        .map(String::from)
        .unwrap_or_else(|| input.to_string());
    summarize(&text, INPUT_SUMMARY)
}

pub(crate) fn content_text(content: &Value) -> String {
    match content {
        Value::String(s) => s.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter_map(|p| p["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n"),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}
