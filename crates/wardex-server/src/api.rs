//! The daemon API: requests, responses and their handling. Transport-neutral;
//! used by the Unix socket server (and later by MCP).

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use wardex_core::manager::{AnswerInput, Manager, ManagerError};
use wardex_core::{Event, EventKind, RunSpec};

/// Default number of events `tail` shows.
pub const DEFAULT_TAIL: i64 = 20;
const MAX_EVENTS: i64 = 10_000;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Request {
    Ping,
    Run(RunSpec),
    List {
        #[serde(default)]
        all: bool,
    },
    Status {
        id: String,
    },
    /// Persisted events. With `since_seq`, events after it; otherwise the last
    /// `last` (default 20). With `follow`, keeps streaming live events.
    Tail {
        id: String,
        #[serde(default)]
        since_seq: Option<u64>,
        #[serde(default)]
        last: Option<i64>,
        #[serde(default)]
        follow: bool,
        /// Also stream message deltas while following.
        #[serde(default)]
        deltas: bool,
    },
    Summary {
        id: String,
    },
    Answer {
        id: String,
        #[serde(default)]
        question_id: Option<String>,
        input: AnswerInput,
    },
    Send {
        id: String,
        message: String,
    },
    Stop {
        id: String,
    },
    Interrupt {
        id: String,
    },
    Resume {
        id: String,
        #[serde(default)]
        message: Option<String>,
    },
    Pending,
    Shutdown,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Response {
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Machine-readable error kind: `not_found`, `ambiguous`, `invalid`,
    /// `conflict`, `internal`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
}

impl Response {
    pub fn ok(data: Value) -> Self {
        Self {
            ok: true,
            data: Some(data),
            error: None,
            code: None,
        }
    }

    pub fn err(code: &str, message: impl Into<String>) -> Self {
        Self {
            ok: false,
            data: None,
            error: Some(message.into()),
            code: Some(code.into()),
        }
    }
}

impl From<ManagerError> for Response {
    fn from(e: ManagerError) -> Self {
        let code = match &e {
            ManagerError::NotFound(_) => "not_found",
            ManagerError::Ambiguous(_) => "ambiguous",
            ManagerError::Invalid(_) => "invalid",
            ManagerError::Conflict(_) => "conflict",
            ManagerError::Store(_) | ManagerError::Driver(_) => "internal",
        };
        Response::err(code, e.to_string())
    }
}

/// A line in a `tail --follow` stream.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamItem {
    pub event: Event,
}

fn to_value<T: Serialize>(v: &T) -> Value {
    serde_json::to_value(v).unwrap_or(Value::Null)
}

/// Handle a request that produces a single response. `Tail` with `follow`
/// and `Shutdown` are handled by the transport.
pub async fn handle(manager: &Manager, req: Request) -> Response {
    match handle_inner(manager, req).await {
        Ok(v) => Response::ok(v),
        Err(e) => e.into(),
    }
}

async fn handle_inner(m: &Manager, req: Request) -> Result<Value, ManagerError> {
    Ok(match req {
        Request::Ping => json!({"version": env!("CARGO_PKG_VERSION")}),
        Request::Run(spec) => to_value(&m.run(spec).await?),
        Request::List { all } => to_value(&m.list(all).await?),
        Request::Status { id } => {
            let id = m.resolve(&id).await?;
            to_value(&m.status(&id).await?)
        }
        Request::Tail {
            id,
            since_seq,
            last,
            ..
        } => {
            let id = m.resolve(&id).await?;
            to_value(&initial_events(m, &id, since_seq, last).await?)
        }
        Request::Summary { id } => {
            let id = m.resolve(&id).await?;
            to_value(&m.summary(&id).await?)
        }
        Request::Answer {
            id,
            question_id,
            input,
        } => {
            let id = m.resolve(&id).await?;
            let qid = m.answer(&id, question_id.as_deref(), input).await?;
            json!({"session_id": id, "question_id": qid})
        }
        Request::Send { id, message } => {
            let id = m.resolve(&id).await?;
            let resumed = m.send(&id, message).await?;
            json!({"session_id": id, "resumed": resumed})
        }
        Request::Stop { id } => {
            let id = m.resolve(&id).await?;
            m.stop(&id).await?;
            json!({"session_id": id})
        }
        Request::Interrupt { id } => {
            let id = m.resolve(&id).await?;
            m.interrupt(&id).await?;
            json!({"session_id": id})
        }
        Request::Resume { id, message } => {
            let id = m.resolve(&id).await?;
            to_value(&m.resume(&id, message).await?)
        }
        Request::Pending => to_value(&m.pending_questions().await?),
        Request::Shutdown => json!({"shutting_down": true}),
    })
}

/// Events a `tail` starts with.
pub async fn initial_events(
    m: &Manager,
    id: &wardex_core::SessionId,
    since_seq: Option<u64>,
    last: Option<i64>,
) -> Result<Vec<Event>, ManagerError> {
    match since_seq {
        Some(seq) => {
            m.events(id, seq, last.unwrap_or(MAX_EVENTS).min(MAX_EVENTS))
                .await
        }
        None => {
            m.last_events(id, last.unwrap_or(DEFAULT_TAIL).min(MAX_EVENTS))
                .await
        }
    }
}

/// Whether a followed stream should end after this event.
pub fn ends_stream(event: &Event) -> bool {
    matches!(event.kind, EventKind::Ended { .. })
}
