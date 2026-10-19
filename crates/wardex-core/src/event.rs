//! Unified event model. Every driver translates its native output into these
//! events. See `docs/drivers.md` for how each CLI's messages map onto them.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::{EndReason, SessionId, SessionState};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DriverKind {
    Claude,
    Codex,
}

impl DriverKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "claude" => Some(Self::Claude),
            "codex" => Some(Self::Codex),
            _ => None,
        }
    }
}

/// One event in a session's stream.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Event {
    pub session_id: SessionId,
    /// Monotonic per-session sequence number, starting at 1.
    pub seq: u64,
    /// Unix timestamp in milliseconds.
    pub ts_ms: i64,
    pub agent: Option<String>,
    pub task: Option<String>,
    pub driver: DriverKind,
    #[serde(flatten)]
    pub kind: EventKind,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "payload", rename_all = "snake_case")]
pub enum EventKind {
    /// The agent process is up. Emitted once per process (also on resume).
    Started {
        /// The driver's own session/thread id, used for resume.
        driver_session_id: Option<String>,
        model: Option<String>,
        /// Reasoning effort the CLI reports it uses (Codex; Claude does not
        /// report it).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reasoning: Option<String>,
    },
    /// A message sent to the agent (initial prompt or follow-up).
    Prompt {
        text: String,
    },
    Message {
        text: String,
        /// `true` for streaming deltas (broadcast only, not persisted).
        delta: bool,
    },
    /// Emitted when a tool starts and again when it finishes, with the same
    /// `call_id`. A session with a started but unfinished tool is not idle.
    ToolCall {
        call_id: String,
        name: String,
        input_summary: String,
        status: ToolStatus,
        result_summary: Option<String>,
        /// Full input (on `started`), up to `TOOL_TEXT_LIMIT` bytes. Stored,
        /// but left out of `tail` unless asked for and out of webhooks.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        input: Option<String>,
        /// Full output (on `completed`/`failed`), up to `TOOL_TEXT_LIMIT`
        /// bytes. Same visibility as `input`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        output: Option<String>,
    },
    /// One pending request from the agent; it may hold several questions.
    Question {
        question_id: String,
        items: Vec<QuestionItem>,
    },
    Approval {
        request_id: String,
        /// Driver-specific kind, e.g. `command`, `file_change`, `exit_plan`.
        kind: String,
        summary: String,
    },
    /// A question or approval was answered.
    Answered {
        question_id: String,
        answer: Answer,
    },
    Reminder {
        question_id: String,
        waited_secs: u64,
    },
    /// The current turn finished; the session waits for the next message.
    Done {
        result: Option<String>,
        /// The turn was ended by an interrupt Wardex sent.
        interrupted: bool,
        /// Short turn summary written by the agent CLI, when it offers one.
        note: Option<String>,
        git: Option<GitState>,
    },
    Error {
        message: String,
    },
    Stuck {
        reason: StuckReason,
        idle_secs: u64,
    },
    /// The agent process ended.
    Ended {
        state: SessionState,
        exit_code: Option<i32>,
        /// Why it ended. Absent in events recorded by older versions.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<EndReason>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolStatus {
    Started,
    Completed,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuestionItem {
    /// Stable key used when answering (Codex question id; Claude question
    /// text).
    pub id: String,
    pub header: Option<String>,
    pub text: String,
    pub options: Vec<QuestionOption>,
    pub multi_select: bool,
    /// Free-text answers besides the options are accepted.
    pub allow_other: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuestionOption {
    pub label: String,
    pub description: Option<String>,
}

/// Answer to a pending question or approval.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Answer {
    /// Answers per question item id.
    Question {
        items: BTreeMap<String, Vec<String>>,
    },
    Approval {
        allow: bool,
        message: Option<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitState {
    pub branch: Option<String>,
    /// Paths with uncommitted changes (`git status --porcelain`).
    pub changed_files: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StuckReason {
    /// A question or approval has been pending too long.
    WaitingInput,
    /// No output and no running tool for too long.
    NoOutput,
}

/// Every event type name.
pub const EVENT_NAMES: &[&str] = &[
    "started",
    "prompt",
    "message",
    "tool_call",
    "question",
    "approval",
    "answered",
    "reminder",
    "done",
    "error",
    "stuck",
    "ended",
];

/// Event type names that are pushed to webhooks by default.
pub const NOTABLE_EVENTS: &[&str] = &["question", "approval", "reminder", "done", "error", "stuck"];

impl EventKind {
    /// Stable name used in config filters and webhook payloads.
    pub fn name(&self) -> &'static str {
        match self {
            Self::Started { .. } => "started",
            Self::Prompt { .. } => "prompt",
            Self::Message { .. } => "message",
            Self::ToolCall { .. } => "tool_call",
            Self::Question { .. } => "question",
            Self::Approval { .. } => "approval",
            Self::Answered { .. } => "answered",
            Self::Reminder { .. } => "reminder",
            Self::Done { .. } => "done",
            Self::Error { .. } => "error",
            Self::Stuck { .. } => "stuck",
            Self::Ended { .. } => "ended",
        }
    }

    /// Events pushed to webhooks by default.
    pub fn is_notable(&self) -> bool {
        NOTABLE_EVENTS.contains(&self.name())
    }

    /// Streaming deltas are broadcast live but never persisted.
    pub fn is_delta(&self) -> bool {
        matches!(self, Self::Message { delta: true, .. })
    }

    /// Drop the full tool input/output, keeping the summaries.
    pub fn strip_tool_text(&mut self) {
        if let Self::ToolCall { input, output, .. } = self {
            *input = None;
            *output = None;
        }
    }

    /// One-line human-readable description, used by `tail`.
    pub fn describe(&self) -> String {
        match self {
            Self::Started {
                driver_session_id,
                model,
                ..
            } => format!(
                "started (model {}, id {})",
                model.as_deref().unwrap_or("default"),
                driver_session_id.as_deref().unwrap_or("-")
            ),
            Self::Prompt { text } => format!("> {text}"),
            Self::Message { text, .. } => text.clone(),
            Self::ToolCall {
                name,
                input_summary,
                status,
                result_summary,
                ..
            } => match status {
                ToolStatus::Started => format!("{name}: {input_summary}"),
                ToolStatus::Completed => {
                    format!("{name} done: {}", result_summary.as_deref().unwrap_or(""))
                }
                ToolStatus::Failed => {
                    format!("{name} failed: {}", result_summary.as_deref().unwrap_or(""))
                }
            },
            Self::Question { question_id, items } => {
                let mut s = format!("question {question_id}:");
                for item in items {
                    s.push_str(&format!(" {}", item.text));
                    if !item.options.is_empty() {
                        let labels: Vec<&str> =
                            item.options.iter().map(|o| o.label.as_str()).collect();
                        s.push_str(&format!(" [{}]", labels.join(" / ")));
                    }
                }
                s
            }
            Self::Approval {
                request_id,
                kind,
                summary,
            } => format!("approval {request_id} ({kind}): {summary}"),
            Self::Answered {
                question_id,
                answer,
            } => match answer {
                Answer::Question { items } => {
                    let parts: Vec<String> = items.values().map(|v| v.join(", ")).collect();
                    format!("answered {question_id}: {}", parts.join("; "))
                }
                Answer::Approval { allow, .. } => format!(
                    "answered {question_id}: {}",
                    if *allow { "allow" } else { "deny" }
                ),
            },
            Self::Reminder {
                question_id,
                waited_secs,
            } => format!("reminder: {question_id} waiting for {waited_secs}s"),
            Self::Done {
                result,
                interrupted,
                ..
            } => {
                if *interrupted {
                    "done (interrupted)".to_string()
                } else {
                    format!("done: {}", result.as_deref().unwrap_or(""))
                }
            }
            Self::Error { message } => format!("error: {message}"),
            Self::Stuck { reason, idle_secs } => {
                format!("stuck ({reason:?}) for {idle_secs}s")
            }
            Self::Ended {
                state,
                exit_code,
                reason,
            } => {
                let mut text = format!("ended: {}", state.as_str());
                if let Some(reason) = reason.filter(|r| r.as_str() != state.as_str()) {
                    text.push_str(&format!(" ({})", reason.as_str()));
                }
                if let Some(code) = exit_code {
                    text.push_str(&format!(" (exit {code})"));
                }
                text
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(kind: EventKind) -> Event {
        Event {
            session_id: SessionId("s1".into()),
            seq: 7,
            ts_ms: 1_700_000_000_000,
            agent: Some("agent-a".into()),
            task: None,
            driver: DriverKind::Claude,
            kind,
        }
    }

    #[test]
    fn serializes_with_type_and_payload() {
        let ev = event(EventKind::Question {
            question_id: "q1".into(),
            items: vec![QuestionItem {
                id: "color".into(),
                header: Some("Color".into()),
                text: "Which color?".into(),
                options: vec![QuestionOption {
                    label: "Blue".into(),
                    description: None,
                }],
                multi_select: false,
                allow_other: true,
            }],
        });
        let json = serde_json::to_value(&ev).unwrap();
        assert_eq!(json["type"], "question");
        assert_eq!(json["payload"]["question_id"], "q1");
        assert_eq!(json["payload"]["items"][0]["options"][0]["label"], "Blue");
        assert_eq!(json["driver"], "claude");
        assert_eq!(json["seq"], 7);
    }

    #[test]
    fn roundtrips() {
        let ev = event(EventKind::Stuck {
            reason: StuckReason::NoOutput,
            idle_secs: 480,
        });
        let json = serde_json::to_string(&ev).unwrap();
        assert_eq!(serde_json::from_str::<Event>(&json).unwrap(), ev);

        let ev = event(EventKind::Answered {
            question_id: "q1".into(),
            answer: Answer::Approval {
                allow: true,
                message: None,
            },
        });
        let json = serde_json::to_string(&ev).unwrap();
        assert_eq!(serde_json::from_str::<Event>(&json).unwrap(), ev);
    }

    #[test]
    fn ended_reason_is_optional_on_the_wire() {
        let ev = event(EventKind::Ended {
            state: SessionState::Done,
            exit_code: Some(0),
            reason: Some(EndReason::IdleShutdown),
        });
        let json = serde_json::to_value(&ev).unwrap();
        assert_eq!(json["payload"]["reason"], "idle_shutdown");
        assert_eq!(ev.kind.describe(), "ended: done (idle_shutdown) (exit 0)");
        assert_eq!(serde_json::from_value::<Event>(json).unwrap(), ev);

        // Events stored by older versions have no reason.
        let old = serde_json::json!({
            "session_id": "s1", "seq": 3, "ts_ms": 1, "agent": null, "task": null,
            "driver": "codex", "type": "ended",
            "payload": {"state": "stopped", "exit_code": null},
        });
        let ev: Event = serde_json::from_value(old).unwrap();
        assert!(matches!(ev.kind, EventKind::Ended { reason: None, .. }));
        assert_eq!(ev.kind.describe(), "ended: stopped");
        let json = serde_json::to_value(&ev).unwrap();
        assert!(json["payload"].get("reason").is_none());
    }

    #[test]
    fn name_matches_serde_tag() {
        let ev = event(EventKind::ToolCall {
            call_id: "c1".into(),
            name: "Bash".into(),
            input_summary: "ls".into(),
            status: ToolStatus::Started,
            result_summary: None,
            input: Some("{\"command\":\"ls\"}".into()),
            output: None,
        });
        let json = serde_json::to_value(&ev).unwrap();
        assert_eq!(json["type"], ev.kind.name());
    }

    #[test]
    fn notable_and_delta() {
        let done = EventKind::Done {
            result: None,
            interrupted: false,
            note: None,
            git: None,
        };
        assert!(done.is_notable());
        let delta = EventKind::Message {
            text: String::new(),
            delta: true,
        };
        assert!(!delta.is_notable());
        assert!(delta.is_delta());
    }
}
