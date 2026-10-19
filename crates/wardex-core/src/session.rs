use std::fmt;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::DriverKind;

/// Wardex-assigned session identifier: 8 lowercase hex characters. Commands
/// accept any unique prefix. Independent of the driver's own id (Claude
/// `session_id`, Codex `thread_id`), which is stored alongside it so a
/// session can be resumed.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SessionId(pub String);

impl SessionId {
    pub fn generate() -> Self {
        Self(uuid::Uuid::new_v4().simple().to_string()[..8].to_string())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for SessionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Lifecycle state of a session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionState {
    Starting,
    /// A turn is in progress.
    Running,
    /// The last turn finished; the process waits for the next message.
    Idle,
    /// Blocked on a question or approval.
    WaitingInput,
    /// The process ended normally. Resumable.
    Done,
    /// The process failed. Resumable if the driver id is known.
    Error,
    /// Stopped on request. Resumable.
    Stopped,
    /// The process was lost (e.g. daemon restart). Resumable.
    Orphaned,
}

impl SessionState {
    pub const ALL: [SessionState; 8] = [
        Self::Starting,
        Self::Running,
        Self::Idle,
        Self::WaitingInput,
        Self::Done,
        Self::Error,
        Self::Stopped,
        Self::Orphaned,
    ];

    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Done | Self::Error | Self::Stopped | Self::Orphaned
        )
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Starting => "starting",
            Self::Running => "running",
            Self::Idle => "idle",
            Self::WaitingInput => "waiting_input",
            Self::Done => "done",
            Self::Error => "error",
            Self::Stopped => "stopped",
            Self::Orphaned => "orphaned",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|st| st.as_str() == s)
    }
}

/// Why a session's process ended. Set when it ends, cleared on resume.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EndReason {
    /// The agent process exited by itself between turns.
    Done,
    /// Stopped on request.
    Stopped,
    /// Idle for `sessions.idle_shutdown_secs`.
    IdleShutdown,
    /// Closed because more than `sessions.max_idle_live` sessions were idle.
    IdleEvicted,
    /// Closed because available memory fell below
    /// `sessions.min_mem_available_mb`.
    MemoryPressure,
    /// The process failed or could not be started.
    Error,
    /// Closed by `daemon stop/restart --drain`.
    Drain,
    /// Closed by a daemon stop or restart without drain.
    Shutdown,
    /// Found live after the daemon stopped unexpectedly (crash or kill).
    DaemonLost,
}

impl EndReason {
    pub const ALL: [EndReason; 9] = [
        Self::Done,
        Self::Stopped,
        Self::IdleShutdown,
        Self::IdleEvicted,
        Self::MemoryPressure,
        Self::Error,
        Self::Drain,
        Self::Shutdown,
        Self::DaemonLost,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Done => "done",
            Self::Stopped => "stopped",
            Self::IdleShutdown => "idle_shutdown",
            Self::IdleEvicted => "idle_evicted",
            Self::MemoryPressure => "memory_pressure",
            Self::Error => "error",
            Self::Drain => "drain",
            Self::Shutdown => "shutdown",
            Self::DaemonLost => "daemon_lost",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|r| r.as_str() == s)
    }

    /// Wardex closed an idle process to save resources; the session is
    /// unfinished work and `send` continues it.
    pub fn is_idle_close(self) -> bool {
        matches!(
            self,
            Self::IdleShutdown | Self::IdleEvicted | Self::MemoryPressure
        )
    }
}

/// Everything needed to start a new session.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RunSpec {
    pub driver: Option<DriverKind>,
    pub prompt: String,
    pub model: Option<String>,
    pub reasoning: Option<String>,
    /// Absolute working directory.
    pub cwd: PathBuf,
    /// Create a git worktree with this name under the configured worktree dir.
    pub worktree: Option<String>,
    /// Branch for the worktree.
    pub branch: Option<String>,
    /// Remove the worktree when the session ends cleanly.
    pub remove_worktree: bool,
    pub readonly: bool,
    pub agent: Option<String>,
    pub task: Option<String>,
    pub slice: Option<String>,
}

/// Persistent session row.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionRecord {
    pub id: SessionId,
    pub driver: DriverKind,
    pub driver_session_id: Option<String>,
    pub state: SessionState,
    pub agent: Option<String>,
    pub task: Option<String>,
    pub model: Option<String>,
    pub reasoning: Option<String>,
    /// Directory the agent runs in (the worktree, if one was created).
    pub cwd: PathBuf,
    /// Directory the session was started from, when it differs from `cwd`.
    pub origin_cwd: Option<PathBuf>,
    pub worktree: Option<String>,
    pub branch: Option<String>,
    pub remove_worktree: bool,
    pub readonly: bool,
    pub slice: Option<String>,
    pub unit: Option<String>,
    pub pid: Option<u32>,
    pub created_ms: i64,
    pub updated_ms: i64,
    pub ended_ms: Option<i64>,
    pub last_seq: u64,
    pub last_result: Option<String>,
    pub last_error: Option<String>,
    /// Number of processes spawned for this session (1 + resumes).
    pub spawn_count: u32,
    /// Orphaned and not resumed since (see `sessions.auto_resume_orphaned`).
    #[serde(default)]
    pub resume_pending: bool,
    /// Orphaned mid-turn: an automatic resume sends `sessions.resume_prompt`.
    #[serde(default)]
    pub resume_with_prompt: bool,
    /// The old scope could not be verified empty. Only an explicit resume
    /// may retry cleanup; automatic resume must leave this session alone.
    #[serde(default)]
    pub cleanup_failed: bool,
    /// Why the process ended (`None` while live and for sessions that
    /// ended before this was recorded).
    #[serde(default)]
    pub ended_reason: Option<EndReason>,
}
