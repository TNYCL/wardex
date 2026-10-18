use std::path::PathBuf;

use clap::{Args, Parser, Subcommand, ValueEnum};

#[derive(Debug, Parser)]
#[command(name = "wardex", version, about, propagate_version = true)]
pub struct Cli {
    /// Path to the config file.
    #[arg(long, global = true, env = "WARDEX_CONFIG")]
    pub config: Option<PathBuf>,

    /// Print machine-readable JSON.
    #[arg(long, global = true)]
    pub json: bool,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Run the daemon in the foreground.
    Daemon,
    /// Start a new agent session.
    Run(RunArgs),
    /// List sessions.
    List {
        /// Include finished sessions.
        #[arg(long)]
        all: bool,
    },
    /// Show a session's state and resource usage.
    Status { id: String },
    /// Print a session's events.
    Tail {
        id: String,
        /// Keep streaming new events.
        #[arg(short, long)]
        follow: bool,
        /// Only events after this sequence number.
        #[arg(long)]
        since_seq: Option<u64>,
    },
    /// Summarize a session: result, touched files, duration, counts.
    Summary { id: String },
    /// Answer a pending question or approval.
    Answer { id: String, text: String },
    /// Send a follow-up message to a session.
    Send { id: String, message: String },
    /// Stop a session.
    Stop { id: String },
    /// Resume a finished, stopped or orphaned session.
    Resume {
        id: String,
        /// Optional message to continue with.
        message: Option<String>,
    },
}

impl Command {
    pub fn name(&self) -> &'static str {
        match self {
            Self::Daemon => "daemon",
            Self::Run(_) => "run",
            Self::List { .. } => "list",
            Self::Status { .. } => "status",
            Self::Tail { .. } => "tail",
            Self::Summary { .. } => "summary",
            Self::Answer { .. } => "answer",
            Self::Send { .. } => "send",
            Self::Stop { .. } => "stop",
            Self::Resume { .. } => "resume",
        }
    }
}

#[derive(Debug, Args)]
pub struct RunArgs {
    #[arg(long, value_enum)]
    pub driver: Driver,
    #[arg(long)]
    pub model: Option<String>,
    /// Reasoning effort (passed through to the driver).
    #[arg(long)]
    pub reasoning: Option<String>,
    /// Working directory (defaults to the current directory).
    #[arg(long)]
    pub cwd: Option<PathBuf>,
    /// Run in a new git worktree under `.worktree/<name>`.
    #[arg(long, value_name = "NAME")]
    pub worktree: Option<String>,
    /// Branch name for the worktree.
    #[arg(long, requires = "worktree")]
    pub branch: Option<String>,
    /// Read-only session (plan / read-only sandbox mode).
    #[arg(long)]
    pub readonly: bool,
    #[arg(long)]
    pub agent: Option<String>,
    #[arg(long)]
    pub task: Option<String>,
    /// systemd slice to place the session in.
    #[arg(long)]
    pub slice: Option<String>,
    #[arg(
        long,
        conflicts_with = "prompt_file",
        required_unless_present = "prompt_file"
    )]
    pub prompt: Option<String>,
    #[arg(long)]
    pub prompt_file: Option<PathBuf>,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum Driver {
    Claude,
    Codex,
}

#[cfg(test)]
mod tests {
    use clap::CommandFactory;

    use super::*;

    #[test]
    fn cli_definition_is_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn run_requires_a_prompt() {
        assert!(Cli::try_parse_from(["wardex", "run", "--driver", "claude"]).is_err());
        assert!(
            Cli::try_parse_from(["wardex", "run", "--driver", "codex", "--prompt", "hi"]).is_ok()
        );
    }
}
