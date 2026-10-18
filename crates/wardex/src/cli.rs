use std::path::PathBuf;

use clap::{Args, Parser, Subcommand, ValueEnum};

#[derive(Debug, Parser)]
#[command(name = "wardex", version, about, propagate_version = true)]
pub struct Cli {
    /// Path to the config file (default: $XDG_CONFIG_HOME/wardex/wardex.toml).
    #[arg(long, global = true, env = "WARDEX_CONFIG")]
    pub config: Option<PathBuf>,

    /// Daemon socket (overrides config and WARDEX_SOCKET).
    #[arg(long, global = true)]
    pub socket: Option<PathBuf>,

    /// Print machine-readable JSON.
    #[arg(long, global = true)]
    pub json: bool,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Run the daemon in the foreground.
    Daemon(DaemonArgs),
    /// Start a new agent session.
    Run(RunArgs),
    /// List sessions.
    List {
        /// Include ended sessions.
        #[arg(short, long)]
        all: bool,
    },
    /// Show a session's state, pending questions and resource usage.
    Status { id: String },
    /// Print a session's events.
    Tail {
        id: String,
        /// Keep streaming new events until the session ends.
        #[arg(short, long)]
        follow: bool,
        /// Only events after this sequence number.
        #[arg(long)]
        since_seq: Option<u64>,
        /// Number of recent events to show.
        #[arg(short = 'n', long)]
        lines: Option<i64>,
        /// While following, also print streamed message text as it arrives.
        #[arg(long)]
        deltas: bool,
    },
    /// Summarize a session: result, changes, duration, counts.
    Summary { id: String },
    /// Answer a pending question or approval.
    Answer {
        id: String,
        /// The answer (or allow/deny for an approval).
        text: Option<String>,
        /// Which pending question (default: the oldest).
        #[arg(long)]
        question: Option<String>,
        /// Answer one item of a multi-question request: ITEM_ID=TEXT.
        #[arg(long = "item", value_name = "ID=TEXT")]
        items: Vec<String>,
    },
    /// Send a follow-up message (resumes an ended session).
    Send { id: String, message: String },
    /// Stop a session.
    Stop { id: String },
    /// Interrupt the running turn of a session.
    Interrupt { id: String },
    /// Resume an ended, stopped or orphaned session.
    Resume {
        id: String,
        /// Optional message to continue with.
        message: Option<String>,
    },
    /// List pending questions and approvals of all sessions.
    Pending,
    /// Live sessions with questions, resources and latest events, plus
    /// sessions ended in the last 24 hours.
    Overview {
        /// Latest events per live session.
        #[arg(short = 'n', long, default_value_t = 3)]
        events: i64,
    },
}

#[derive(Debug, Args)]
pub struct DaemonArgs {
    #[command(subcommand)]
    pub action: Option<DaemonAction>,
}

#[derive(Debug, Subcommand)]
pub enum DaemonAction {
    /// Ask the running daemon to shut down. Live sessions become orphaned
    /// (resumable, automatically with `sessions.auto_resume_orphaned`).
    Stop {
        /// First wait up to this many seconds for running turns to finish.
        #[arg(long, value_name = "SECS")]
        drain: Option<u64>,
    },
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
    /// Branch for the worktree (created if missing).
    #[arg(long, requires = "worktree")]
    pub branch: Option<String>,
    /// Remove the worktree when the session ends cleanly.
    #[arg(long, requires = "worktree")]
    pub rm_worktree: bool,
    /// Read-only session (plan / read-only sandbox mode).
    #[arg(long)]
    pub readonly: bool,
    #[arg(long)]
    pub agent: Option<String>,
    #[arg(long)]
    pub task: Option<String>,
    /// systemd slice to place the session in (must be in resources.slices).
    #[arg(long)]
    pub slice: Option<String>,
    #[arg(
        long,
        conflicts_with = "prompt_file",
        required_unless_present = "prompt_file"
    )]
    pub prompt: Option<String>,
    /// Read the prompt from a file (`-` for stdin).
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

    #[test]
    fn answer_items_and_daemon_stop() {
        let cli = Cli::try_parse_from(["wardex", "answer", "ab", "--item", "a=1", "--item", "b=2"])
            .unwrap();
        match cli.command {
            Command::Answer { items, text, .. } => {
                assert_eq!(items, ["a=1", "b=2"]);
                assert!(text.is_none());
            }
            c => panic!("{c:?}"),
        }
        assert!(Cli::try_parse_from(["wardex", "daemon", "stop"]).is_ok());
        assert!(Cli::try_parse_from(["wardex", "--json", "list", "-a"]).is_ok());
    }
}
