use std::collections::BTreeMap;
use std::io::Read;
use std::process::ExitCode;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use clap::Parser;
use serde_json::Value;
use wardex_core::manager::{
    AnswerInput, Overview, RunOutcome, SessionStatus, SessionSummary, SessionView,
};
use wardex_core::store::QuestionRow;
use wardex_core::{Config, DriverKind, Event, RunSpec, SessionRecord};
use wardex_server::Client;
use wardex_server::api::{Request, Response};
use wardex_server::daemon::{Exit, Source};

mod cli;
mod output;

use cli::{Cli, Command, DaemonAction};

fn main() -> ExitCode {
    let cli = Cli::parse();
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("wardex: cannot start runtime: {e}");
            return ExitCode::FAILURE;
        }
    };
    match runtime.block_on(run(cli)) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("wardex: {e:#}");
            ExitCode::FAILURE
        }
    }
}

async fn run(cli: Cli) -> Result<ExitCode> {
    let mut config = Config::load(cli.config.as_deref())?;
    if let Some(socket) = &cli.socket {
        config.daemon.socket = socket.to_string_lossy().into_owned();
    }
    let client = Client::new(config.socket_path());
    let json = cli.json;

    match cli.command {
        Command::Daemon(args) => match args.action {
            None => {
                init_logging();
                let source = Source {
                    config_path: cli.config.clone(),
                    socket: cli
                        .socket
                        .as_ref()
                        .map(|s| s.to_string_lossy().into_owned()),
                };
                match wardex_server::daemon::run(source).await? {
                    Exit::Stopped => Ok(ExitCode::SUCCESS),
                    Exit::Restart => Err(reexec()),
                }
            }
            Some(DaemonAction::Stop { drain }) => {
                let before = daemon_info(&client).await?;
                let resp = client
                    .request(&Request::Shutdown {
                        drain_secs: drain,
                        restart: false,
                    })
                    .await?;
                if !resp.ok {
                    return finish(json, resp, |_| String::new());
                }
                let timeout = stop_timeout(&config, drain);
                wait_until(timeout, || async { !daemon_alive(&client, &before).await })
                    .await
                    .with_context(|| {
                        format!("the daemon did not exit within {}s", timeout.as_secs())
                    })?;
                finish(json, resp, |_| "daemon stopped\n".into())
            }
            Some(DaemonAction::Restart { drain }) => {
                let before = daemon_info(&client).await?;
                let resp = client
                    .request(&Request::Shutdown {
                        drain_secs: drain,
                        restart: true,
                    })
                    .await?;
                if !resp.ok {
                    return finish(json, resp, |_| String::new());
                }
                let timeout = stop_timeout(&config, drain) + Duration::from_secs(30);
                wait_until(timeout, || async {
                    matches!(daemon_info(&client).await, Ok(now) if now.started_ms > before.started_ms)
                })
                .await
                .with_context(|| {
                    format!("the daemon did not come back within {}s", timeout.as_secs())
                })?;
                finish(json, resp, |_| "daemon restarted\n".into())
            }
            Some(DaemonAction::Reload) => {
                let resp = client.request(&Request::Reload).await?;
                finish(json, resp, |v| {
                    let mut out = format!(
                        "reloaded: {} webhook(s), {} active\n",
                        v["webhooks"], v["webhooks_active"]
                    );
                    let restart: Vec<&str> = v["restart_needed"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(|s| s.as_str())
                        .collect();
                    if !restart.is_empty() {
                        out.push_str(&format!(
                            "changed sections that need `wardex daemon restart`: {}\n",
                            restart.join(", ")
                        ));
                    }
                    out
                })
            }
        },
        Command::Run(args) => {
            let prompt = match (&args.prompt, &args.prompt_file) {
                (Some(p), _) => p.clone(),
                (None, Some(path)) if path.as_os_str() == "-" => {
                    let mut s = String::new();
                    std::io::stdin().read_to_string(&mut s)?;
                    s
                }
                (None, Some(path)) => std::fs::read_to_string(path)
                    .with_context(|| format!("cannot read {}", path.display()))?,
                (None, None) => bail!("--prompt or --prompt-file is required"),
            };
            let cwd = match &args.cwd {
                Some(c) => c.clone(),
                None => std::env::current_dir()?,
            };
            let cwd = std::fs::canonicalize(&cwd)
                .with_context(|| format!("cannot use cwd {}", cwd.display()))?;
            let spec = RunSpec {
                driver: Some(match args.driver {
                    cli::Driver::Claude => DriverKind::Claude,
                    cli::Driver::Codex => DriverKind::Codex,
                }),
                prompt,
                model: args.model,
                reasoning: args.reasoning,
                cwd,
                worktree: args.worktree,
                branch: args.branch,
                remove_worktree: args.rm_worktree,
                readonly: args.readonly,
                agent: args.agent,
                task: args.task,
                slice: args.slice,
            };
            let resp = client.request(&Request::Run(spec)).await?;
            finish(json, resp, |v| {
                match serde_json::from_value::<RunOutcome>(v.clone()) {
                    // Only the id goes to stdout, so `id=$(wardex run ...)` works.
                    Ok(out) => {
                        for w in out.warnings {
                            eprintln!("warning: {w}");
                        }
                        format!("{}\n", out.session.id)
                    }
                    Err(_) => output::json(v),
                }
            })
        }
        Command::List { all } => {
            let resp = client.request(&Request::List { all }).await?;
            finish(json, resp, |v| {
                let views: Vec<SessionView> = serde_json::from_value(v.clone()).unwrap_or_default();
                output::list(&views)
            })
        }
        Command::Status { id } => {
            let resp = client.request(&Request::Status { id }).await?;
            finish(json, resp, |v| {
                match serde_json::from_value::<SessionStatus>(v.clone()) {
                    Ok(st) => output::status(&st),
                    Err(_) => output::json(v),
                }
            })
        }
        Command::Tail {
            id,
            follow,
            since_seq,
            lines,
            deltas,
            full,
            wait,
        } => {
            let req = Request::Tail {
                id,
                since_seq,
                last: lines,
                follow,
                deltas,
                wait_secs: wait,
                full,
            };
            if follow {
                let mut in_delta = false;
                let err = client
                    .stream(&req, |item| {
                        print_event(json, &item.event, &mut in_delta);
                    })
                    .await?;
                if in_delta {
                    println!();
                }
                match err {
                    Some(resp) if !resp.ok => Err(anyhow!(resp.error.unwrap_or_default())),
                    _ => Ok(ExitCode::SUCCESS),
                }
            } else {
                let resp = client.request(&req).await?;
                finish(json, resp, |v| {
                    let events: Vec<Event> = serde_json::from_value(v.clone()).unwrap_or_default();
                    events
                        .iter()
                        .map(|e| format!("{}\n", output::event(e)))
                        .collect()
                })
            }
        }
        Command::Summary { id } => {
            let resp = client.request(&Request::Summary { id }).await?;
            finish(json, resp, |v| {
                match serde_json::from_value::<SessionSummary>(v.clone()) {
                    Ok(s) => output::summary(&s),
                    Err(_) => output::json(v),
                }
            })
        }
        Command::Answer {
            id,
            text,
            question,
            items,
        } => {
            let input = if items.is_empty() {
                AnswerInput::Text(text.ok_or_else(|| anyhow!("give an answer or --item ID=TEXT"))?)
            } else {
                if text.is_some() {
                    bail!("use either a single answer or --item, not both");
                }
                let mut map = BTreeMap::new();
                for item in items {
                    let (k, v) = item
                        .split_once('=')
                        .ok_or_else(|| anyhow!("--item expects ID=TEXT, got {item:?}"))?;
                    map.insert(k.to_string(), v.to_string());
                }
                AnswerInput::Items(map)
            };
            let resp = client
                .request(&Request::Answer {
                    id,
                    question_id: question,
                    input,
                })
                .await?;
            finish(json, resp, |v| {
                format!("answered {}\n", v["question_id"].as_str().unwrap_or("-"))
            })
        }
        Command::Send { id, message } => {
            let resp = client.request(&Request::Send { id, message }).await?;
            finish(json, resp, |v| {
                if v["resumed"].as_bool() == Some(true) {
                    "sent (session resumed)\n".into()
                } else {
                    "sent\n".into()
                }
            })
        }
        Command::Stop { id } => {
            let resp = client.request(&Request::Stop { id }).await?;
            finish(json, resp, |_| "stopping\n".into())
        }
        Command::Interrupt { id } => {
            let resp = client.request(&Request::Interrupt { id }).await?;
            finish(json, resp, |_| "interrupt sent\n".into())
        }
        Command::Resume { id, message } => {
            let resp = client.request(&Request::Resume { id, message }).await?;
            finish(json, resp, |v| {
                let r: Option<SessionRecord> = serde_json::from_value(v.clone()).ok();
                format!(
                    "resumed {}\n",
                    r.map(|r| r.id.to_string()).unwrap_or_default()
                )
            })
        }
        Command::Pending => {
            let resp = client.request(&Request::Pending).await?;
            finish(json, resp, |v| {
                let rows: Vec<QuestionRow> = serde_json::from_value(v.clone()).unwrap_or_default();
                output::pending(&rows)
            })
        }
        Command::ToolOutput { id, seq } => {
            let resp = client.request(&Request::ToolOutput { id, seq }).await?;
            finish(json, resp, |v| {
                let mut out = format!(
                    "{} {} (events {}..{})\n",
                    v["name"].as_str().unwrap_or("-"),
                    v["status"].as_str().unwrap_or("running"),
                    v["started_seq"],
                    v["finished_seq"]
                );
                for key in ["input", "output"] {
                    out.push_str(&format!("--- {key}\n"));
                    match v[key].as_str() {
                        Some(text) => {
                            out.push_str(text);
                            if !text.ends_with('\n') {
                                out.push('\n');
                            }
                        }
                        None => out.push_str("(not recorded)\n"),
                    }
                }
                out
            })
        }
        Command::Mcp { stdio } => {
            if !stdio {
                bail!(
                    "only `wardex mcp --stdio` is supported; HTTP MCP is served by the daemon at /mcp"
                );
            }
            init_logging();
            wardex_server::mcp::serve_stdio(client).await?;
            Ok(ExitCode::SUCCESS)
        }
        Command::Overview { events } => {
            let resp = client
                .request(&Request::Overview {
                    events: Some(events),
                })
                .await?;
            finish(json, resp, |v| {
                match serde_json::from_value::<Overview>(v.clone()) {
                    Ok(o) => output::overview(&o),
                    Err(_) => output::json(v),
                }
            })
        }
    }
}

/// What `ping` tells about the running daemon.
struct DaemonInfo {
    pid: Option<u32>,
    started_ms: i64,
}

async fn daemon_info(client: &Client) -> Result<DaemonInfo> {
    let resp = client.request(&Request::Ping).await?;
    let data = resp.data.unwrap_or_default();
    Ok(DaemonInfo {
        pid: data["pid"].as_u64().map(|p| p as u32),
        started_ms: data["started_ms"].as_i64().unwrap_or(0),
    })
}

/// Whether the daemon `before` describes still runs: its process exists, or
/// (for a daemon that does not report its pid) its socket still answers.
async fn daemon_alive(client: &Client, before: &DaemonInfo) -> bool {
    match before.pid {
        Some(pid) => process_running(pid),
        None => client.request(&Request::Ping).await.is_ok(),
    }
}

/// Whether `pid` exists and is not a zombie (an exited child its parent has
/// not reaped yet).
fn process_running(pid: u32) -> bool {
    match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        // `pid (comm) state ...`; comm may contain spaces and parentheses.
        Ok(stat) => stat
            .rsplit_once(") ")
            .and_then(|(_, rest)| rest.chars().next())
            .is_some_and(|state| state != 'Z' && state != 'X'),
        Err(_) => false,
    }
}

fn stop_timeout(config: &Config, drain: Option<u64>) -> Duration {
    Duration::from_secs(drain.unwrap_or(0) + config.sessions.stop_grace_secs + 30)
}

async fn wait_until<F, Fut>(timeout: Duration, mut done: F) -> Result<()>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let start = std::time::Instant::now();
    while !done().await {
        if start.elapsed() > timeout {
            bail!("timed out");
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    Ok(())
}

/// Replace this process with a fresh `wardex` run with the same arguments.
/// Only returns on failure.
fn reexec() -> anyhow::Error {
    use std::os::unix::process::CommandExt;
    let exe = match std::env::current_exe() {
        Ok(p) => p,
        Err(e) => return anyhow!("cannot find the wardex binary to restart: {e}"),
    };
    // Linux reports a replaced binary as "<path> (deleted)"; run the new one.
    let exe = exe
        .to_str()
        .and_then(|s| s.strip_suffix(" (deleted)"))
        .map(std::path::PathBuf::from)
        .unwrap_or(exe);
    let err = std::process::Command::new(&exe)
        .args(std::env::args_os().skip(1))
        .exec();
    anyhow!("cannot restart {}: {err}", exe.display())
}

/// Print a response: JSON as-is with `--json`, otherwise via `human`.
fn finish(json: bool, resp: Response, human: impl FnOnce(&Value) -> String) -> Result<ExitCode> {
    if !resp.ok {
        if json {
            println!("{}", serde_json::to_string_pretty(&resp)?);
            return Ok(ExitCode::FAILURE);
        }
        bail!(resp.error.unwrap_or_else(|| "request failed".into()));
    }
    let data = resp.data.unwrap_or(Value::Null);
    if json {
        println!("{}", output::json(&data));
    } else {
        print!("{}", human(&data));
    }
    Ok(ExitCode::SUCCESS)
}

fn print_event(json: bool, e: &Event, in_delta: &mut bool) {
    if json {
        println!("{}", serde_json::to_string(e).unwrap_or_default());
        return;
    }
    if e.kind.is_delta() {
        if let wardex_core::EventKind::Message { text, .. } = &e.kind {
            print!("{text}");
            use std::io::Write;
            let _ = std::io::stdout().flush();
            *in_delta = true;
        }
        return;
    }
    if *in_delta {
        println!();
        *in_delta = false;
        // The full message follows the deltas; skip repeating it.
        if e.kind.name() == "message" {
            return;
        }
    }
    println!("{}", output::event(e));
}

fn init_logging() {
    use tracing_subscriber::EnvFilter;
    let filter = EnvFilter::try_from_default_env()
        // rmcp logs every MCP request at info.
        .unwrap_or_else(|_| EnvFilter::new("info,rmcp=warn"));
    let json = std::env::var("WARDEX_LOG_FORMAT").is_ok_and(|v| v == "json");
    let ansi = std::io::IsTerminal::is_terminal(&std::io::stderr());
    let builder = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .with_ansi(ansi)
        .with_writer(std::io::stderr);
    if json {
        builder.json().init();
    } else {
        builder.init();
    }
}
