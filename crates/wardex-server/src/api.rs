//! The daemon API: requests, responses and their handling. Transport-neutral;
//! used by the Unix socket server and the MCP tools.

use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use wardex_core::manager::{AnswerInput, Manager, ManagerError, drain_next_steps};
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
        /// Include the full tool input/output on `tool_call` events.
        #[serde(default)]
        full: bool,
    },
    /// Full input and output of the tool call that event `seq` belongs to.
    ToolOutput {
        id: String,
        seq: u64,
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
    /// running turns and pending answers to finish. Drain refuses shutdown
    /// on timeout or if an idle process will not exit gracefully; it never
    /// interrupts a turn. With `restart`, the daemon starts again in
    /// the same process (same PID) after stopping.
    Shutdown {
        #[serde(default)]
        drain_secs: Option<u64>,
        #[serde(default)]
        restart: bool,
        #[serde(default)]
        force: bool,
    },
    /// Re-read the config and env files and apply the webhook settings.
    Reload,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub persisted: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub applied: Option<bool>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
}

impl Response {
    pub fn ok(data: Value) -> Self {
        Self {
            ok: true,
            data: Some(data),
            error: None,
            code: None,
            persisted: None,
            applied: None,
            warnings: Vec::new(),
        }
    }

    pub fn err(code: &str, message: impl Into<String>) -> Self {
        Self {
            ok: false,
            data: None,
            error: Some(message.into()),
            code: Some(code.into()),
            persisted: None,
            applied: Some(false),
            warnings: Vec::new(),
        }
    }

    /// Keep ordinary successful payloads compatible. Degraded responses carry
    /// durability metadata through CLI/MCP too, including array payloads.
    pub fn client_data(&self) -> Value {
        let data = self.data.clone().unwrap_or(Value::Null);
        if self.warnings.is_empty() {
            return data;
        }
        let mut data = match data {
            Value::Object(object) => object,
            other => serde_json::Map::from_iter([("data".into(), other)]),
        };
        data.insert("persisted".into(), json!(self.persisted.unwrap_or(false)));
        let mut warnings = data
            .remove("warnings")
            .and_then(|value| value.as_array().cloned())
            .unwrap_or_default();
        warnings.extend(self.warnings.iter().map(|warning| json!(warning)));
        data.insert("warnings".into(), json!(warnings));
        if let Some(applied) = self.applied {
            data.insert("applied".into(), json!(applied));
        }
        Value::Object(data)
    }
}

impl From<ManagerError> for Response {
    fn from(e: ManagerError) -> Self {
        if let ManagerError::DrainBlocked {
            ref sessions,
            ref closed_sessions,
            ref cleanup_failed,
            ..
        } = e
        {
            let mut response = Response::err("conflict", e.to_string());
            response.data = Some(json!({
                "shutting_down": false,
                "blocked_sessions": sessions,
                "closed_sessions": closed_sessions,
                "interrupted_sessions": [],
                "cleanup_failed": cleanup_failed,
                "next_steps": drain_next_steps(sessions, cleanup_failed),
            }));
            response.applied = Some(!closed_sessions.is_empty());
            return response;
        }
        let code = match &e {
            ManagerError::NotFound(_) => "not_found",
            ManagerError::Ambiguous(_) => "ambiguous",
            ManagerError::Invalid(_) => "invalid",
            ManagerError::Conflict(_) => "conflict",
            ManagerError::DrainBlocked { .. } => "conflict",
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
    let action = matches!(
        &req,
        Request::Run(_)
            | Request::Send { .. }
            | Request::Answer { .. }
            | Request::Resume { .. }
            | Request::Stop { .. }
            | Request::Interrupt { .. }
    );
    match handle_inner(manager, req).await {
        Ok(v) => {
            let persistence = manager.persistence();
            let mut response = Response::ok(v);
            response.persisted = Some(persistence.persisted);
            response.warnings = persistence.warnings;
            response.applied = action.then_some(true);
            if action && !persistence.persisted {
                response.warnings.insert(0, "Action applied but not yet committed; do not resend it. Accepted writes are retained for background retries.".into());
            }
            response
        }
        Err(e) => e.into(),
    }
}

async fn handle_inner(m: &Manager, req: Request) -> Result<Value, ManagerError> {
    Ok(match req {
        Request::Ping => json!({
            "version": env!("CARGO_PKG_VERSION"),
            "pid": std::process::id(),
            "started_ms": m.started_ms(),
        }),
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
            full,
            ..
        } => {
            let id = m.resolve(&id).await?;
            if let (Some(seq), Some(wait)) = (since_seq, wait_secs.filter(|w| *w > 0)) {
                let wait = Duration::from_secs(wait.min(MAX_WAIT_SECS));
                m.wait_for_event(&id, seq, wait).await?;
            }
            let mut events = initial_events(m, &id, since_seq, last).await?;
            if !full {
                events.iter_mut().for_each(|e| e.kind.strip_tool_text());
            }
            to_value(&events)
        }
        Request::ToolOutput { id, seq } => {
            let id = m.resolve(&id).await?;
            to_value(&m.tool_output(&id, seq).await?)
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
        Request::Shutdown { .. } => {
            return Err(ManagerError::Invalid(
                "shutdown is handled by the daemon's socket server".into(),
            ));
        }
        Request::Reload => {
            return Err(ManagerError::Invalid(
                "reload is handled by the daemon's socket server".into(),
            ));
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
