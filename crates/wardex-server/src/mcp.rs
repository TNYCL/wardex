//! MCP server (Streamable HTTP, stateless). Every tool maps to a daemon API
//! request, so MCP clients and the CLI share one implementation. Served under
//! `/mcp` behind the bearer token (see `http.rs`).

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, ContentBlock, Implementation, ServerCapabilities, ServerConfig};
use rmcp::{ServerHandler, tool, tool_handler, tool_router};
use schemars::JsonSchema;
use serde::Deserialize;
use wardex_core::manager::{AnswerInput, Manager};
use wardex_core::{DriverKind, RunSpec};

use crate::api::{self, DEFAULT_OVERVIEW_EVENTS, Request, Response};

/// Upper bound for `tail`'s `wait_secs`; MCP clients time tool calls out.
pub const MAX_WAIT_SECS: u64 = 60;

const INSTRUCTIONS: &str = "\
Wardex runs and supervises Claude Code and Codex agent sessions on one host. \
Typical flow: `run` starts a session and returns its id (any unique id prefix \
works everywhere). The agent works on its own; when it asks something, the \
session goes to `waiting_input` and the question shows up in `pending_questions`, \
`overview` and as a `question` or `approval` event in `tail`. Reply with \
`answer`. When a turn ends the session is `idle` and the result is in `summary` \
or the `done` event; `send` gives it the next instruction and also resumes \
ended sessions. `overview` shows every live session in one call. `tail` pages \
events with `since_seq`; with `wait_secs` it waits for the next event. \
Webhooks (if configured) announce questions, results, errors and stuck sessions, \
so poll only when you have no webhook.";

/// Inlined in the schema: some MCP clients do not resolve `$ref`.
#[derive(Debug, Clone, Copy, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
#[schemars(inline)]
pub enum DriverArg {
    Claude,
    Codex,
}

impl From<DriverArg> for DriverKind {
    fn from(d: DriverArg) -> Self {
        match d {
            DriverArg::Claude => DriverKind::Claude,
            DriverArg::Codex => DriverKind::Codex,
        }
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct RunParams {
    /// The task for the agent.
    pub prompt: String,
    /// Absolute working directory on the Wardex host.
    pub cwd: String,
    /// Agent CLI.
    pub driver: DriverArg,
    /// Model name passed to the CLI. Default: the driver's configured default.
    #[serde(default)]
    pub model: Option<String>,
    /// Reasoning effort (e.g. low, medium, high).
    #[serde(default)]
    pub reasoning: Option<String>,
    /// Run in a new git worktree with this name (under the repo's worktree dir).
    #[serde(default)]
    pub worktree: Option<String>,
    /// Branch for the worktree. Default: the worktree name.
    #[serde(default)]
    pub branch: Option<String>,
    /// Remove the worktree when the session ends cleanly.
    #[serde(default)]
    pub remove_worktree: bool,
    /// Read-only: the agent may look but not change files.
    #[serde(default)]
    pub readonly: bool,
    /// Label: who started the session.
    #[serde(default)]
    pub agent: Option<String>,
    /// Label: what the session is for.
    #[serde(default)]
    pub task: Option<String>,
    /// systemd slice from the configured list.
    #[serde(default)]
    pub slice: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ListParams {
    /// Include ended sessions (up to 500, newest first).
    #[serde(default)]
    pub all: bool,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct IdParams {
    /// Session id or unique prefix.
    pub id: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct TailParams {
    /// Session id or unique prefix.
    pub id: String,
    /// Return events after this sequence number (paging). Without it, the
    /// last `last` events.
    #[serde(default)]
    pub since_seq: Option<u64>,
    /// Maximum number of events. Default: 20 without `since_seq`, all with it.
    #[serde(default)]
    pub last: Option<i64>,
    /// With `since_seq`: if there are no newer events yet, wait up to this many
    /// seconds (max 60) for one.
    #[serde(default)]
    pub wait_secs: Option<u64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct AnswerParams {
    /// Session id or unique prefix.
    pub id: String,
    /// Question to answer. Default: the session's oldest pending question.
    #[serde(default)]
    pub question_id: Option<String>,
    /// The answer: free text or an option label for a single question, or
    /// allow/deny for an approval.
    #[serde(default)]
    pub text: Option<String>,
    /// Answers per question item id, for questions with several items.
    #[serde(default)]
    pub answers: Option<BTreeMap<String, String>>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SendParams {
    /// Session id or unique prefix.
    pub id: String,
    /// The next instruction. Queued if a turn is running; resumes an ended
    /// session.
    pub message: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ResumeParams {
    /// Session id or unique prefix.
    pub id: String,
    /// Optional message to send after resuming.
    #[serde(default)]
    pub message: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct OverviewParams {
    /// Latest events per live session. Default: 3.
    #[serde(default)]
    pub events: Option<i64>,
}

#[derive(Clone)]
pub struct WardexMcp {
    manager: Manager,
    tool_router: ToolRouter<Self>,
}

impl WardexMcp {
    pub fn new(manager: Manager) -> Self {
        Self {
            manager,
            tool_router: Self::tool_router(),
        }
    }

    async fn call(&self, req: Request) -> CallToolResult {
        respond(api::handle(&self.manager, req).await)
    }
}

/// Successful results are the same JSON the CLI prints with `--json`.
fn respond(resp: Response) -> CallToolResult {
    if resp.ok {
        let data = resp.data.unwrap_or(serde_json::Value::Null);
        CallToolResult::success(vec![ContentBlock::text(data.to_string())])
    } else {
        CallToolResult::error(vec![ContentBlock::text(format!(
            "{}: {}",
            resp.code.as_deref().unwrap_or("error"),
            resp.error.unwrap_or_default()
        ))])
    }
}

fn invalid(message: &str) -> CallToolResult {
    respond(Response::err("invalid", message))
}

#[tool_router]
impl WardexMcp {
    #[tool(
        description = "Start a Claude Code or Codex session. Returns the session record \
                          (its `id` is what every other tool takes) and warnings."
    )]
    async fn run(&self, Parameters(p): Parameters<RunParams>) -> CallToolResult {
        self.call(Request::Run(RunSpec {
            driver: Some(p.driver.into()),
            prompt: p.prompt,
            model: p.model,
            reasoning: p.reasoning,
            cwd: PathBuf::from(p.cwd),
            worktree: p.worktree,
            branch: p.branch,
            remove_worktree: p.remove_worktree,
            readonly: p.readonly,
            agent: p.agent,
            task: p.task,
            slice: p.slice,
        }))
        .await
    }

    #[tool(
        description = "List live sessions (or all with `all`), newest first.",
        annotations(read_only_hint = true)
    )]
    async fn list(&self, Parameters(p): Parameters<ListParams>) -> CallToolResult {
        self.call(Request::List { all: p.all }).await
    }

    #[tool(
        description = "Every live session with state, pending questions, running tools, \
                       resource usage and latest events, plus sessions ended in the last 24h.",
        annotations(read_only_hint = true)
    )]
    async fn overview(&self, Parameters(p): Parameters<OverviewParams>) -> CallToolResult {
        self.call(Request::Overview {
            events: Some(p.events.unwrap_or(DEFAULT_OVERVIEW_EVENTS)),
        })
        .await
    }

    #[tool(
        description = "One session's record, pending questions, running tools and resource usage.",
        annotations(read_only_hint = true)
    )]
    async fn status(&self, Parameters(p): Parameters<IdParams>) -> CallToolResult {
        self.call(Request::Status { id: p.id }).await
    }

    #[tool(
        description = "A session's events, oldest first. Page with `since_seq` (pass the last \
                       `seq` you saw); add `wait_secs` to wait for the next event.",
        annotations(read_only_hint = true)
    )]
    async fn tail(&self, Parameters(p): Parameters<TailParams>) -> CallToolResult {
        if let (Some(seq), Some(wait)) = (p.since_seq, p.wait_secs.filter(|w| *w > 0)) {
            let id = match self.manager.resolve(&p.id).await {
                Ok(id) => id,
                Err(e) => return respond(e.into()),
            };
            let wait = Duration::from_secs(wait.min(MAX_WAIT_SECS));
            if let Err(e) = self.manager.wait_for_event(&id, seq, wait).await {
                return respond(e.into());
            }
        }
        self.call(Request::Tail {
            id: p.id,
            since_seq: p.since_seq,
            last: p.last,
            follow: false,
            deltas: false,
        })
        .await
    }

    #[tool(
        description = "Result of a session: final text, git state (branch, changed files), \
                       duration, turn, event and tool counts.",
        annotations(read_only_hint = true)
    )]
    async fn summary(&self, Parameters(p): Parameters<IdParams>) -> CallToolResult {
        self.call(Request::Summary { id: p.id }).await
    }

    #[tool(
        description = "Open questions and approvals across all sessions.",
        annotations(read_only_hint = true)
    )]
    async fn pending_questions(&self) -> CallToolResult {
        self.call(Request::Pending).await
    }

    #[tool(
        description = "Answer a session's pending question or approval. Give `text` for a \
                          single question (an option label or free text) or allow/deny for \
                          an approval, or `answers` (item id -> answer) for several items."
    )]
    async fn answer(&self, Parameters(p): Parameters<AnswerParams>) -> CallToolResult {
        let input = match (p.text, p.answers) {
            (Some(text), None) => AnswerInput::Text(text),
            (None, Some(items)) if !items.is_empty() => AnswerInput::Items(items),
            _ => return invalid("give exactly one of `text` or `answers`"),
        };
        self.call(Request::Answer {
            id: p.id,
            question_id: p.question_id,
            input,
        })
        .await
    }

    #[tool(
        description = "Send the next message to a session. Queued while a turn runs; \
                          an ended session is resumed first."
    )]
    async fn send(&self, Parameters(p): Parameters<SendParams>) -> CallToolResult {
        self.call(Request::Send {
            id: p.id,
            message: p.message,
        })
        .await
    }

    #[tool(
        description = "Interrupt the running turn. The session stays open and takes new messages.",
        annotations(destructive_hint = false)
    )]
    async fn interrupt(&self, Parameters(p): Parameters<IdParams>) -> CallToolResult {
        self.call(Request::Interrupt { id: p.id }).await
    }

    #[tool(description = "Stop a session's process. It can be resumed later.")]
    async fn stop(&self, Parameters(p): Parameters<IdParams>) -> CallToolResult {
        self.call(Request::Stop { id: p.id }).await
    }

    #[tool(
        description = "Resume an ended session with its previous context, optionally \
                          sending a message."
    )]
    async fn resume(&self, Parameters(p): Parameters<ResumeParams>) -> CallToolResult {
        self.call(Request::Resume {
            id: p.id,
            message: p.message,
        })
        .await
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for WardexMcp {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("wardex", env!("CARGO_PKG_VERSION")))
            .with_instructions(INSTRUCTIONS)
    }
}
