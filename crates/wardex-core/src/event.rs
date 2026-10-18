//! Unified event model. Every driver translates its native output into these
//! events. See `docs/drivers.md` for how each CLI's messages map onto them.

use serde::{Deserialize, Serialize};

use crate::SessionId;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DriverKind {
    Claude,
    Codex,
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
    Started {
        /// The driver's own session/thread id, used for resume.
        driver_session_id: Option<String>,
        model: Option<String>,
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
    Reminder {
        question_id: String,
        waited_secs: u64,
    },
    /// The current turn finished; the session waits for the next message.
    Done {
        result: Option<String>,
        /// The turn was ended by an interrupt Wardex sent.
        interrupted: bool,
    },
    Error {
        message: String,
    },
    Stuck {
        reason: StuckReason,
        idle_secs: u64,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StuckReason {
    /// A question or approval has been pending too long.
    WaitingInput,
    /// No output and no running tool for too long.
    NoOutput,
}

impl EventKind {
    /// Stable name used in config filters and webhook payloads.
    pub fn name(&self) -> &'static str {
        match self {
            Self::Started { .. } => "started",
            Self::Message { .. } => "message",
            Self::ToolCall { .. } => "tool_call",
            Self::Question { .. } => "question",
            Self::Approval { .. } => "approval",
            Self::Reminder { .. } => "reminder",
            Self::Done { .. } => "done",
            Self::Error { .. } => "error",
            Self::Stuck { .. } => "stuck",
        }
    }

    /// Events pushed to webhooks by default.
    pub fn is_notable(&self) -> bool {
        matches!(
            self,
            Self::Question { .. }
                | Self::Approval { .. }
                | Self::Reminder { .. }
                | Self::Done { .. }
                | Self::Error { .. }
                | Self::Stuck { .. }
        )
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
    }

    #[test]
    fn name_matches_serde_tag() {
        let ev = event(EventKind::ToolCall {
            call_id: "c1".into(),
            name: "Bash".into(),
            input_summary: "ls".into(),
            status: ToolStatus::Started,
            result_summary: None,
        });
        let json = serde_json::to_value(&ev).unwrap();
        assert_eq!(json["type"], ev.kind.name());
    }

    #[test]
    fn notable_events() {
        assert!(
            EventKind::Done {
                result: None,
                interrupted: false
            }
            .is_notable()
        );
        assert!(
            !EventKind::Message {
                text: String::new(),
                delta: true
            }
            .is_notable()
        );
    }
}
