use std::collections::BTreeMap;
use std::io::Read;
use std::process::ExitCode;

use anyhow::{Context, Result, anyhow, bail};
use clap::Parser;
use serde_json::Value;
use wardex_core::manager::{AnswerInput, RunOutcome, SessionStatus, SessionSummary, SessionView};
use wardex_core::store::QuestionRow;
use wardex_core::{Config, DriverKind, Event, RunSpec, SessionRecord};
use wardex_server::Client;
use wardex_server::api::{Request, Response};

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
                wardex_server::daemon::run(config).await?;
                Ok(ExitCode::SUCCESS)
            }
            Some(DaemonAction::Stop { drain }) => {
                let resp = client
                    .request(&Request::Shutdown { drain_secs: drain })
                    .await?;
                finish(json, resp, |_| "daemon is shutting down\n".into())
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
                    Ok(out) => {
                        let mut s = format!("{}\n", out.session.id);
                        for w in out.warnings {
                            s.push_str(&format!("warning: {w}\n"));
                        }
                        s
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
        } => {
            let req = Request::Tail {
                id,
                since_seq,
                last: lines,
                follow,
                deltas,
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
    }
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
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let json = std::env::var("WARDEX_LOG_FORMAT").is_ok_and(|v| v == "json");
    let builder = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false);
    if json {
        builder.json().init();
    } else {
        builder.init();
    }
}
