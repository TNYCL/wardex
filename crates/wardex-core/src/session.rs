use std::fmt;

use serde::{Deserialize, Serialize};

/// Wardex-assigned session identifier. Independent of the driver's own id
/// (Claude `session_id`, Codex `thread_id`), which is stored alongside it so a
/// session can be resumed.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SessionId(pub String);

impl fmt::Display for SessionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Lifecycle state of a session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionState {
    Starting,
    Running,
    /// Blocked on a question or approval.
    WaitingInput,
    Done,
    Error,
    Stopped,
    /// The process was lost (e.g. daemon restart); resumable via the driver id.
    Orphaned,
}

impl SessionState {
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Done | Self::Error | Self::Stopped | Self::Orphaned
        )
    }
}
