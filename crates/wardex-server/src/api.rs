//! The daemon API: requests, responses and their handling. Transport-neutral;
//! used by the Unix socket server and the MCP tools.

use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use wardex_core::manager::{AnswerInput, Manager, ManagerError};
use wardex_core::{Event, EventKind, RunSpec};

/// Default number of events `tail` shows.
pub const DEFAULT_TAIL: i64 = 20;
const MAX_EVENTS: i64 = 10_000;
/// Upper bound for `tail`'s `wait_secs`; MCP clients time tool calls out.
pub const MAX_WAIT_SECS: u64 = 60;
/// Default number of latest events per session in `overview`.
pub const DEFAULT_OVERVIEW_EVENTS: i64 = 3;
const MAX_OVERVIEW_EVENTS: i64 = 50;

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
        /// With `since_seq` and without `follow`: if there are no newer
        /// events yet, wait up to this many seconds (max 60) for one.
        #[serde(default)]
        wait_secs: Option<u64>,
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
    /// Live sessions with their latest `events` events, plus recently ended
    /// sessions.
    Overview {
        #[serde(default)]
        events: Option<i64>,
    },
    /// Stop the daemon. With `drain_secs`, first wait up to that long for
    /// running turns to finish.
    Shutdown {
        #[serde(default)]
        drain_secs: Option<u64>,
    },
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
            wait_secs,
            ..
        } => {
            let id = m.resolve(&id).await?;
            if let (Some(seq), Some(wait)) = (since_seq, wait_secs.filter(|w| *w > 0)) {
                let wait = Duration::from_secs(wait.min(MAX_WAIT_SECS));
                m.wait_for_event(&id, seq, wait).await?;
            }
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
        Request::Overview { events } => to_value(
            &m.overview(
                events
                    .unwrap_or(DEFAULT_OVERVIEW_EVENTS)
                    .clamp(0, MAX_OVERVIEW_EVENTS),
            )
            .await?,
        ),
        Request::Shutdown { drain_secs } => {
            json!({"shutting_down": true, "drain_secs": drain_secs})
        }
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
