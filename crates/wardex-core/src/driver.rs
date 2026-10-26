//! Interface between the session manager and agent CLI drivers.
//!
//! A driver spawns one agent process per call and talks to it through two
//! channels: commands in, events out. This keeps drivers free of manager
//! state and makes them easy to test against recorded fixtures.

use std::path::PathBuf;

use tokio::sync::mpsc;

use crate::process::Launcher;
use crate::{Answer, DriverKind, EventKind, SessionId};

/// Prepended to the first prompt of a read-only session, in addition to the
/// driver's own read-only mode.
pub const READONLY_PREFIX: &str = "[Read-only session] Do not create, modify or delete files and \
     do not run commands that change state. Only read, analyze and answer.\n\n";

#[derive(Debug, Clone)]
pub struct SpawnSpec {
    pub session_id: SessionId,
    pub cwd: PathBuf,
    pub model: Option<String>,
    pub reasoning: Option<String>,
    pub readonly: bool,
    /// Driver session id to resume instead of starting fresh.
    pub resume: Option<String>,
    /// With `resume`: start a new native session branched from it; the
    /// original is only read. The new id comes with `started` (or in the
    /// handle when the driver picks it up front).
    pub fork: bool,
    pub slice: Option<String>,
    /// Unique systemd unit name for this process (without `.scope`).
    pub unit: String,
}

#[derive(Debug, Clone)]
pub enum DriverCommand {
    /// Send a user message. Queued by the driver until it can be delivered.
    Send(String),
    Answer {
        question_id: String,
        answer: Answer,
    },
    /// Interrupt the running turn.
    Interrupt,
    /// End the process gracefully (close stdin).
    Shutdown,
    /// Close stdin for daemon drain, without an automatic force-kill fallback.
    Drain,
    /// Kill the process (and its scope).
    Kill,
}

#[derive(Debug, Clone)]
pub enum DriverEvent {
    Event(EventKind),
    /// The process exited. Always the last event.
    Exited {
        code: Option<i32>,
        stderr_tail: Vec<String>,
        /// The driver signalled the process to interrupt a turn (for CLIs
        /// whose interrupt ends the process). Once that turn is over, a
        /// non-zero exit code is not a failure.
        interrupted: bool,
    },
}

pub struct DriverHandle {
    pub pid: Option<u32>,
    pub unit: Option<String>,
    /// The driver's session id when it is chosen before the process starts
    /// (Claude `--session-id`), so it is recorded before the first event.
    pub driver_session_id: Option<String>,
    pub commands: mpsc::UnboundedSender<DriverCommand>,
    pub events: mpsc::UnboundedReceiver<DriverEvent>,
}

#[derive(Debug, thiserror::Error)]
pub enum DriverError {
    #[error("cannot start {program}: {source}")]
    Spawn {
        program: String,
        source: std::io::Error,
    },
}

pub trait Driver: Send + Sync + 'static {
    fn kind(&self) -> DriverKind;

    /// Whether the CLI itself can keep a session read-only. Drivers that
    /// cannot are refused for `readonly` sessions rather than relying on the
    /// prompt prefix alone.
    fn supports_readonly(&self) -> bool {
        true
    }

    /// Start the agent process. Must be called inside a Tokio runtime.
    fn spawn(&self, spec: SpawnSpec, launcher: &Launcher) -> Result<DriverHandle, DriverError>;
}

/// Cap for the full tool input/output stored on `tool_call` events.
pub const TOOL_TEXT_LIMIT: usize = 256 * 1024;

/// `text` cut to `TOOL_TEXT_LIMIT` bytes (at a char boundary), with a note
/// of how much was cut.
pub fn cap_tool_text(text: &str) -> String {
    if text.len() <= TOOL_TEXT_LIMIT {
        return text.to_string();
    }
    let mut end = TOOL_TEXT_LIMIT;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!(
        "{}\n[wardex: {} more bytes not stored]",
        &text[..end],
        text.len() - end
    )
}

/// Collapse whitespace and cut to `max` characters, for one-line summaries.
pub fn summarize(text: &str, max: usize) -> String {
    let mut out = String::new();
    let mut count = 0;
    let mut last_space = false;
    for c in text.chars() {
        let c = if c.is_whitespace() { ' ' } else { c };
        if c == ' ' {
            if last_space || out.is_empty() {
                continue;
            }
            last_space = true;
        } else {
            last_space = false;
        }
        if count == max {
            out.push('…');
            return out.trim_end().to_string();
        }
        out.push(c);
        count += 1;
    }
    out.trim_end().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summarize_collapses_and_truncates() {
        assert_eq!(summarize("  a\n\n b  c ", 10), "a b c");
        assert_eq!(summarize("abcdef", 3), "abc…");
        assert_eq!(summarize("", 3), "");
    }
}
