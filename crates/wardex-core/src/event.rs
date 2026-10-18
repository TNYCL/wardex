//! Unified event model. Every driver translates its native output into these
//! events. This is a first draft; it will be refined once the driver spikes
//! (see `docs/SCOPE.md`, milestone M1) have recorded real CLI output.

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
    ToolCall {
        name: String,
        input_summary: String,
        result_summary: Option<String>,
    },
    Question {
        question_id: String,
        text: String,
        options: Vec<String>,
    },
    Approval {
        request_id: String,
        summary: String,
    },
    Reminder {
        question_id: String,
        waited_secs: u64,
    },
    Done {
        result: Option<String>,
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
            text: "Proceed?".into(),
            options: vec!["yes".into(), "no".into()],
        });
        let json = serde_json::to_value(&ev).unwrap();
        assert_eq!(json["type"], "question");
        assert_eq!(json["payload"]["question_id"], "q1");
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
            name: "Bash".into(),
            input_summary: "ls".into(),
            result_summary: None,
        });
        let json = serde_json::to_value(&ev).unwrap();
        assert_eq!(json["type"], ev.kind.name());
    }

    #[test]
    fn notable_events() {
        assert!(EventKind::Done { result: None }.is_notable());
        assert!(
            !EventKind::Message {
                text: String::new(),
                delta: true
            }
            .is_notable()
        );
    }
}
