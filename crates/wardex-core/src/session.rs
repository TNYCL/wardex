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

/// Which sessions `list` returns. Without flags: live sessions only.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListFilter {
    /// Live and ended sessions.
    #[serde(default)]
    pub all: bool,
    /// Only sessions whose agent label matches this glob (`*`, `?`).
    #[serde(default)]
    pub agent: Option<String>,
    /// Only sessions whose task label matches this glob.
    #[serde(default)]
    pub task: Option<String>,
    /// Only ended sessions (implies `all`).
    #[serde(default)]
    pub ended: bool,
    /// Only ended sessions `send` can continue: a resume id is known, no
    /// scope cleanup is pending and the working directory still exists.
    #[serde(default)]
    pub resumable: bool,
}

impl ListFilter {
    /// Whether `record` passes, apart from the working directory check of
    /// `resumable`, which needs the filesystem.
    pub fn matches(&self, record: &SessionRecord) -> bool {
        let terminal = record.state.is_terminal();
        let state_ok = if self.ended || self.resumable {
            terminal
        } else {
            self.all || !terminal
        };
        let label_ok = |pattern: &Option<String>, value: &Option<String>| match pattern {
            None => true,
            Some(p) => value.as_deref().is_some_and(|v| crate::glob::glob(p, v)),
        };
        state_ok
            && (!self.resumable || (record.driver_session_id.is_some() && !record.cleanup_failed))
            && label_ok(&self.agent, &record.agent)
            && label_ok(&self.task, &record.task)
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

#[cfg(test)]
mod tests {
    use super::*;

    fn record(state: SessionState, agent: Option<&str>, task: Option<&str>) -> SessionRecord {
        SessionRecord {
            id: SessionId("s1".into()),
            driver: DriverKind::Codex,
            driver_session_id: Some("thread-1".into()),
            state,
            agent: agent.map(String::from),
            task: task.map(String::from),
            model: None,
            reasoning: None,
            cwd: PathBuf::from("/work"),
            origin_cwd: None,
            worktree: None,
            branch: None,
            remove_worktree: false,
            readonly: false,
            slice: None,
            unit: None,
            pid: None,
            created_ms: 1,
            updated_ms: 1,
            ended_ms: None,
            last_seq: 0,
            last_result: None,
            last_error: None,
            spawn_count: 1,
            resume_pending: false,
            resume_with_prompt: false,
            cleanup_failed: false,
            ended_reason: None,
        }
    }

    #[test]
    fn list_filter_states() {
        let live = record(SessionState::Idle, None, None);
        let ended = record(SessionState::Done, None, None);
        let plain = ListFilter::default();
        assert!(plain.matches(&live) && !plain.matches(&ended));
        let all = ListFilter {
            all: true,
            ..Default::default()
        };
        assert!(all.matches(&live) && all.matches(&ended));
        for only_ended in [
            ListFilter {
                ended: true,
                ..Default::default()
            },
            ListFilter {
                ended: true,
                all: true,
                ..Default::default()
            },
        ] {
            assert!(!only_ended.matches(&live) && only_ended.matches(&ended));
        }
    }

    #[test]
    fn list_filter_resumable() {
        let resumable = ListFilter {
            resumable: true,
            ..Default::default()
        };
        assert!(resumable.matches(&record(SessionState::Stopped, None, None)));
        assert!(!resumable.matches(&record(SessionState::Idle, None, None)));
        let mut no_id = record(SessionState::Error, None, None);
        no_id.driver_session_id = None;
        assert!(!resumable.matches(&no_id));
        let mut quarantined = record(SessionState::Orphaned, None, None);
        quarantined.cleanup_failed = true;
        assert!(!resumable.matches(&quarantined));
    }

    #[test]
    fn list_filter_labels() {
        let r = record(SessionState::Idle, Some("agent-a"), Some("fix-login"));
        let by = |agent: Option<&str>, task: Option<&str>| ListFilter {
            agent: agent.map(String::from),
            task: task.map(String::from),
            ..Default::default()
        };
        assert!(by(Some("agent-*"), None).matches(&r));
        assert!(by(Some("agent-a"), Some("fix-*")).matches(&r));
        assert!(!by(Some("agent-b"), None).matches(&r));
        assert!(!by(None, Some("feat-*")).matches(&r));
        // A pattern never matches a missing label, not even `*`.
        assert!(!by(Some("*"), None).matches(&record(SessionState::Idle, None, None)));
    }
}
