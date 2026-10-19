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
    /// List sessions (live ones unless filtered otherwise), newest first.
    List {
        /// Include ended sessions.
        #[arg(short, long)]
        all: bool,
        /// Only sessions whose agent label matches this glob (`*`, `?`).
        #[arg(long, value_name = "GLOB")]
        agent: Option<String>,
        /// Only sessions whose task label matches this glob (`*`, `?`).
        #[arg(long, value_name = "GLOB")]
        task: Option<String>,
        /// Only ended sessions.
        #[arg(long)]
        ended: bool,
        /// Only ended sessions that `send` can continue.
        #[arg(long)]
        resumable: bool,
        /// Only sessions started by Wardex, adopted or forked.
        #[arg(long, value_enum)]
        origin: Option<Origin>,
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
    /// Send a follow-up message (resumes an ended session):
    /// `send <ID> <MESSAGE>` or `send --latest --agent GLOB [--task GLOB] <MESSAGE>`.
    Send {
        /// Session id or unique prefix (omit with --latest).
        #[arg(value_name = "ID")]
        id: Option<String>,
        message: Option<String>,
        /// Send to the newest session (live or ended) whose labels match
        /// --agent and/or --task.
        #[arg(long)]
        latest: bool,
        /// With --latest: glob for the session's agent label.
        #[arg(long, value_name = "GLOB", requires = "latest")]
        agent: Option<String>,
        /// With --latest: glob for the session's task label.
        #[arg(long, value_name = "GLOB", requires = "latest")]
        task: Option<String>,
    },
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
    /// Sessions started outside Wardex (needs `external.enabled`).
    External {
        #[command(subcommand)]
        action: ExternalAction,
    },
    /// Print a session's native conversation, read from the CLI's
    /// transcript. ID: a Wardex session id, or an external session id
    /// (`claude:<uuid>`, `codex:<uuid>`, a uuid or a prefix of 8+ characters).
    History(HistoryArgs),
    /// Take over a session started outside Wardex: a Wardex session bound to
    /// the same native id, continued by `send`/`resume`. Prints its id.
    Adopt(AdoptArgs),
}

#[derive(Debug, Args)]
pub struct AdoptArgs {
    /// `claude:<uuid>`, `codex:<uuid>`, a uuid or a unique prefix.
    pub id: String,
    #[arg(long, value_enum)]
    pub driver: Option<Driver>,
    #[arg(long)]
    pub agent: Option<String>,
    #[arg(long)]
    pub task: Option<String>,
    #[arg(long)]
    pub model: Option<String>,
    #[arg(long)]
    pub reasoning: Option<String>,
    /// Continue read-only (otherwise with Wardex's full permissions).
    #[arg(long)]
    pub readonly: bool,
    /// Resume the session with this message right away.
    #[arg(long, conflicts_with = "prompt_file")]
    pub message: Option<String>,
    /// Read the message from a file (`-` for stdin).
    #[arg(long)]
    pub prompt_file: Option<PathBuf>,
    /// Adopt even if the session was written recently or its liveness is
    /// unknown (never when it is live).
    #[arg(long)]
    pub force: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Origin {
    Wardex,
    Adopted,
    Forked,
}

#[derive(Debug, Subcommand)]
pub enum ExternalAction {
    /// List sessions found in the Claude Code and Codex stores, newest first.
    List(ExternalListArgs),
}

#[derive(Debug, Args)]
pub struct ExternalListArgs {
    #[arg(long, value_enum)]
    pub driver: Option<Driver>,
    /// Only sessions that started in DIR.
    #[arg(long, value_name = "DIR", conflicts_with = "here")]
    pub cwd: Option<PathBuf>,
    /// Only sessions that started in the current directory.
    #[arg(long)]
    pub here: bool,
    /// With --cwd or --here: also sessions in directories below it.
    #[arg(long)]
    pub under: bool,
    /// Only sessions written within AGE (e.g. 90s, 30m, 12h, 7d).
    #[arg(long, value_name = "AGE", value_parser = parse_age)]
    pub since: Option<u64>,
    /// Maximum number of sessions.
    #[arg(short = 'n', long, default_value_t = 50)]
    pub limit: usize,
    /// Also sessions this Wardex already manages.
    #[arg(long)]
    pub include_managed: bool,
    /// Also archived sessions (Codex).
    #[arg(long)]
    pub archived: bool,
    /// Also subagent threads (Codex).
    #[arg(long)]
    pub include_subagents: bool,
}

#[derive(Debug, Args)]
pub struct HistoryArgs {
    pub id: String,
    #[arg(long, value_enum)]
    pub driver: Option<Driver>,
    /// Turns (prompts) to show.
    #[arg(long, default_value_t = 20)]
    pub turns: usize,
    /// Show the turns before this cursor (`next_before` of a page).
    #[arg(long, value_name = "CURSOR")]
    pub before: Option<String>,
    /// How much of each tool call to show.
    #[arg(long, value_enum, default_value_t = ToolsArg::Summary)]
    pub tools: ToolsArg,
    /// `markdown` is a context summary another agent can take as a prompt.
    #[arg(long, value_enum, default_value_t = Format::Text)]
    pub format: Format,
    /// Byte budget for the shown items (default 64 KiB).
    #[arg(long, value_name = "BYTES")]
    pub max_bytes: Option<usize>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum ToolsArg {
    None,
    Summary,
    Full,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Format {
    Text,
    Markdown,
    Json,
}

/// `90`, `90s`, `30m`, `12h` or `7d` as seconds.
fn parse_age(s: &str) -> Result<u64, String> {
    let s = s.trim();
    let (num, unit) = match s.find(|c: char| !c.is_ascii_digit()) {
        Some(i) => s.split_at(i),
        None => (s, "s"),
    };
    let n: u64 = num.parse().map_err(|_| format!("invalid age {s:?}"))?;
    let mult = match unit {
        "s" => 1,
        "m" => 60,
        "h" => 3600,
        "d" => 86_400,
        "w" => 7 * 86_400,
        _ => return Err(format!("invalid age {s:?}; use s, m, h, d or w")),
    };
    Ok(n.saturating_mul(mult))
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
    fn list_filters_and_send_latest() {
        let cli = Cli::try_parse_from([
            "wardex",
            "list",
            "--agent",
            "agent-*",
            "--task",
            "t?",
            "--ended",
            "--resumable",
        ])
        .unwrap();
        match cli.command {
            Command::List {
                agent,
                task,
                ended,
                resumable,
                all,
                ..
            } => {
                assert_eq!(agent.as_deref(), Some("agent-*"));
                assert_eq!(task.as_deref(), Some("t?"));
                assert!(ended && resumable && !all);
            }
            c => panic!("{c:?}"),
        }
        let cli =
            Cli::try_parse_from(["wardex", "send", "--latest", "--agent", "a*", "next"]).unwrap();
        match cli.command {
            Command::Send {
                id,
                message,
                latest,
                agent,
                task,
            } => {
                // The single positional is the message; main.rs sorts it out.
                assert_eq!(id.as_deref(), Some("next"));
                assert!(message.is_none() && latest && task.is_none());
                assert_eq!(agent.as_deref(), Some("a*"));
            }
            c => panic!("{c:?}"),
        }
        assert!(Cli::try_parse_from(["wardex", "send", "ab12", "next"]).is_ok());
        // Label patterns select a session only with --latest.
        assert!(Cli::try_parse_from(["wardex", "send", "--agent", "a", "ab12", "next"]).is_err());
    }

    #[test]
    fn external_list_and_history() {
        let cli = Cli::try_parse_from([
            "wardex", "external", "list", "--here", "--under", "--since", "7d", "-n", "5",
        ])
        .unwrap();
        match cli.command {
            Command::External {
                action: ExternalAction::List(a),
            } => {
                assert!(a.here && a.under);
                assert_eq!(a.since, Some(7 * 86_400));
                assert_eq!(a.limit, 5);
            }
            c => panic!("{c:?}"),
        }
        assert!(
            Cli::try_parse_from(["wardex", "external", "list", "--here", "--cwd", "/x"]).is_err()
        );
        assert!(Cli::try_parse_from(["wardex", "external", "list", "--since", "3y"]).is_err());
        let cli = Cli::try_parse_from([
            "wardex",
            "history",
            "claude:abcd1234",
            "--tools",
            "full",
            "--format",
            "markdown",
        ])
        .unwrap();
        match cli.command {
            Command::History(h) => {
                assert_eq!(h.id, "claude:abcd1234");
                assert_eq!(h.tools, ToolsArg::Full);
                assert_eq!(h.format, Format::Markdown);
                assert_eq!(h.turns, 20);
            }
            c => panic!("{c:?}"),
        }
        assert_eq!(parse_age("90"), Ok(90));
        assert_eq!(parse_age("30m"), Ok(1800));
    }

    #[test]
    fn adopt_and_origin_filter() {
        let cli = Cli::try_parse_from([
            "wardex",
            "adopt",
            "claude:abcd1234",
            "--readonly",
            "--message",
            "go on",
            "--force",
        ])
        .unwrap();
        match cli.command {
            Command::Adopt(a) => {
                assert_eq!(a.id, "claude:abcd1234");
                assert!(a.readonly && a.force);
                assert_eq!(a.message.as_deref(), Some("go on"));
            }
            c => panic!("{c:?}"),
        }
        assert!(
            Cli::try_parse_from([
                "wardex",
                "adopt",
                "x",
                "--message",
                "m",
                "--prompt-file",
                "f"
            ])
            .is_err()
        );
        let cli = Cli::try_parse_from(["wardex", "list", "-a", "--origin", "adopted"]).unwrap();
        assert!(matches!(
            cli.command,
            Command::List {
                origin: Some(Origin::Adopted),
                ..
            }
        ));
    }

    #[test]
    fn force_requires_an_explicit_drain() {
        assert!(Cli::try_parse_from(["wardex", "daemon", "restart", "--force"]).is_err());
        assert!(
            Cli::try_parse_from(["wardex", "daemon", "restart", "--drain", "0", "--force"]).is_ok()
        );
    }
}
