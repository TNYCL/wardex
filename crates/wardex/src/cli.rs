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
        /// Include each tool call's full input and output.
        #[arg(long)]
        full: bool,
        /// With --since-seq: if there is nothing newer yet, wait up to SECS
        /// (max 60) for the next event.
        #[arg(
            long,
            value_name = "SECS",
            requires = "since_seq",
            conflicts_with = "follow"
        )]
        wait: Option<u64>,
    },
    /// Summarize a session: result, changes, duration, counts.
    Summary { id: String },
    /// Full input and output of a tool call, by the `seq` of its event.
    ToolOutput { id: String, seq: u64 },
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
    /// Serve the MCP tools on stdin/stdout, forwarding to the running daemon
    /// (e.g. `ssh host wardex mcp --stdio` as an MCP client command).
    Mcp {
        /// Use stdin/stdout (the only transport here; HTTP MCP is served by
        /// the daemon at /mcp).
        #[arg(long)]
        stdio: bool,
    },
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
    /// Shut the running daemon down and wait until it has exited. Live
    /// sessions become orphaned (resumable, automatically with
    /// `sessions.auto_resume_orphaned`).
    Stop {
        /// Wait for turns and pending answers, then close idle sessions
        /// gracefully. Refuse shutdown on timeout; never force-kill in drain.
        #[arg(long, value_name = "SECS")]
        drain: Option<u64>,
        /// If drain cannot finish, interrupt remaining turns and shut down.
        #[arg(long, requires = "drain")]
        force: bool,
    },
    /// Stop the daemon and start it again in the same process (same PID, so
    /// systemd keeps tracking it), running the currently installed binary.
    /// Waits until the new daemon answers.
    Restart {
        /// Wait for turns and pending answers, then close idle sessions
        /// gracefully. Refuse restart on timeout; never force-kill in drain.
        #[arg(long, value_name = "SECS")]
        drain: Option<u64>,
        /// If drain cannot finish, interrupt remaining turns and restart.
        #[arg(long, requires = "drain")]
        force: bool,
    },
    /// Re-read the config and env files and apply webhook changes without a
    /// restart (same as SIGHUP / `systemctl --user reload wardex`).
    Reload,
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

    #[test]
    fn force_requires_an_explicit_drain() {
        assert!(Cli::try_parse_from(["wardex", "daemon", "restart", "--force"]).is_err());
        assert!(
            Cli::try_parse_from(["wardex", "daemon", "restart", "--drain", "0", "--force"]).is_ok()
        );
    }
}
