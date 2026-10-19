//! MCP server. Every tool maps to a daemon API request, so MCP clients and
//! the CLI share one implementation. Two transports: Streamable HTTP inside
//! the daemon (`/mcp` behind the bearer token, see `http.rs`), and stdio
//! (`wardex mcp --stdio`), which forwards requests to the daemon's Unix
//! socket, e.g. for clients that connect with `ssh host wardex mcp --stdio`.

use std::collections::BTreeMap;
use std::path::PathBuf;

use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, ContentBlock, Implementation, ServerCapabilities, ServerConfig};
use rmcp::{ServerHandler, ServiceExt, tool, tool_handler, tool_router};
use schemars::JsonSchema;
use serde::Deserialize;
use wardex_core::external::ToolDetail;
use wardex_core::manager::{AnswerInput, Manager};
use wardex_core::{DriverKind, RunSpec, SessionOrigin};

use crate::Client;
use crate::api::{self, DEFAULT_OVERVIEW_EVENTS, Request, Response};

const INSTRUCTIONS: &str = "\
Wardex runs and supervises Claude Code and Codex agent sessions on one host. \
Typical flow: `run` starts a session and returns its id (any unique id prefix \
works everywhere). The agent works on its own; when it asks something, the \
session goes to `waiting_input` and the question shows up in `pending_questions`, \
`overview` and as a `question` or `approval` event in `tail`. Reply with \
`answer`. When a turn ends the session is `idle` and the result is in `summary` \
or the `done` event; `send` gives it the next instruction and also resumes \
ended sessions. Do not `stop` a session when its work is done: an idle session \
closes itself after a while (see `shutdown_in_ms` in `overview`) and `send` \
brings it back. `overview` shows every live session in one call. `tail` pages \
events with `since_seq`; with `wait_secs` it waits for the next event; \
`tool_output` has a tool call's full output. \
Webhooks (if configured) announce questions, results, errors and stuck sessions, \
so poll only when you have no webhook.";

/// Tools for sessions started outside Wardex; listed only with
/// `external.enabled` and `external.mcp`.
const EXTERNAL_TOOLS: &[&str] = &["external_list", "history", "adopt"];

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
    /// Only sessions whose `agent` label matches this glob (`*`, `?`).
    #[serde(default)]
    pub agent: Option<String>,
    /// Only sessions whose `task` label matches this glob (`*`, `?`).
    #[serde(default)]
    pub task: Option<String>,
    /// Only ended sessions (implies `all`).
    #[serde(default)]
    pub ended: bool,
    /// Only ended sessions that `send` can continue (resume id known, no
    /// pending scope cleanup, working directory still there).
    #[serde(default)]
    pub resumable: bool,
    /// Only sessions of this origin.
    #[serde(default)]
    pub origin: Option<OriginArg>,
}

/// Inlined in the schema: some MCP clients do not resolve `$ref`.
#[derive(Debug, Clone, Copy, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
#[schemars(inline)]
pub enum OriginArg {
    /// Started by Wardex.
    Wardex,
    /// An external session linked with `adopt`.
    Adopted,
    /// A fork of an external session.
    Forked,
}

impl From<OriginArg> for SessionOrigin {
    fn from(o: OriginArg) -> Self {
        match o {
            OriginArg::Wardex => SessionOrigin::Wardex,
            OriginArg::Adopted => SessionOrigin::Adopted,
            OriginArg::Forked => SessionOrigin::Forked,
        }
    }
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
    /// Include each tool call's full input and output (can be large; see
    /// also `tool_output`).
    #[serde(default)]
    pub full: bool,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ToolOutputParams {
    /// Session id or unique prefix.
    pub id: String,
    /// `seq` of the tool's `tool_call` event (its start or its end).
    pub seq: u64,
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
    /// Session id or unique prefix. Omit it with `latest`.
    #[serde(default)]
    pub id: Option<String>,
    /// The next instruction. Queued if a turn is running; resumes an ended
    /// session.
    pub message: String,
    /// Send to the newest session (live or ended) whose labels match `agent`
    /// and/or `task` instead of `id`.
    #[serde(default)]
    pub latest: bool,
    /// With `latest`: glob (`*`, `?`) for the session's `agent` label.
    #[serde(default)]
    pub agent: Option<String>,
    /// With `latest`: glob (`*`, `?`) for the session's `task` label.
    #[serde(default)]
    pub task: Option<String>,
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
pub struct ExternalListParams {
    /// Only this CLI's sessions.
    #[serde(default)]
    pub driver: Option<DriverArg>,
    /// Absolute directory on the Wardex host: only sessions started there.
    #[serde(default)]
    pub cwd: Option<String>,
    /// With `cwd`: also sessions in directories below it (e.g. worktrees).
    #[serde(default)]
    pub under: bool,
    /// Only sessions written in the last this many seconds.
    #[serde(default)]
    pub since_secs: Option<u64>,
    /// Maximum number of sessions (default 50, newest first).
    #[serde(default)]
    pub limit: Option<usize>,
    /// Also sessions this Wardex already manages.
    #[serde(default)]
    pub include_managed: bool,
}

/// Inlined in the schema: some MCP clients do not resolve `$ref`.
#[derive(Debug, Clone, Copy, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
#[schemars(inline)]
pub enum ToolsArg {
    None,
    Summary,
    Full,
}

impl From<ToolsArg> for ToolDetail {
    fn from(t: ToolsArg) -> Self {
        match t {
            ToolsArg::None => ToolDetail::None,
            ToolsArg::Summary => ToolDetail::Summary,
            ToolsArg::Full => ToolDetail::Full,
        }
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct HistoryParams {
    /// A Wardex session id, or an external session id: `claude:<uuid>`,
    /// `codex:<uuid>`, a full uuid or a unique prefix of 8+ characters.
    pub id: String,
    #[serde(default)]
    pub driver: Option<DriverArg>,
    /// Turns (prompts) per page. Default: 20.
    #[serde(default)]
    pub turns: Option<usize>,
    /// `next_before` of the previous page, for older turns.
    #[serde(default)]
    pub before: Option<String>,
    /// Tool calls: none, summary (default) or full (input and output).
    #[serde(default)]
    pub tools: Option<ToolsArg>,
    /// Byte budget for the page's items. Default: 65536.
    #[serde(default)]
    pub max_bytes: Option<usize>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct AdoptParams {
    /// External session id: `claude:<uuid>`, `codex:<uuid>`, a uuid or a
    /// unique prefix of 8+ characters (see `external_list`).
    pub id: String,
    #[serde(default)]
    pub driver: Option<DriverArg>,
    /// Label: who adopted the session.
    #[serde(default)]
    pub agent: Option<String>,
    /// Label: what the session is for.
    #[serde(default)]
    pub task: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub reasoning: Option<String>,
    /// Continue read-only (otherwise with Wardex's full permissions).
    #[serde(default)]
    pub readonly: bool,
    /// Resume the session with this message right away.
    #[serde(default)]
    pub message: Option<String>,
    /// Branch a new session off it instead of continuing it; the original
    /// is only read, so this works while it is open elsewhere.
    #[serde(default)]
    pub fork: bool,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct OverviewParams {
    /// Latest events per live session. Default: 3.
    #[serde(default)]
    pub events: Option<i64>,
}

/// Where tool calls go: the daemon's manager, or the daemon's socket.
#[derive(Clone)]
enum Backend {
    Local(Manager),
    Socket(Client),
}

#[derive(Clone)]
pub struct WardexMcp {
    backend: Backend,
    tool_router: ToolRouter<Self>,
}

impl WardexMcp {
    /// Tools served inside the daemon.
    pub fn new(manager: Manager) -> Self {
        let external = manager.config().external.over_mcp();
        Self {
            backend: Backend::Local(manager),
            tool_router: Self::router(external),
        }
    }

    /// Tools that forward to a running daemon over its Unix socket.
    /// `external`: the daemon exposes external sessions over MCP.
    pub fn remote(client: Client, external: bool) -> Self {
        Self {
            backend: Backend::Socket(client),
            tool_router: Self::router(external),
        }
    }

    fn router(external: bool) -> ToolRouter<Self> {
        let mut router = Self::tool_router();
        if !external {
            for name in EXTERNAL_TOOLS {
                router.remove_route(name);
            }
        }
        router
    }

    async fn call(&self, req: Request) -> CallToolResult {
        respond(match &self.backend {
            Backend::Local(m) => api::handle(m, req).await,
            Backend::Socket(c) => c.request(&req).await.unwrap_or_else(|e| {
                let mut message = e.to_string();
                if let Some(source) = std::error::Error::source(&e) {
                    message = format!("{message}: {source}");
                }
                Response::err("unavailable", message)
            }),
        })
    }
}

/// Serve MCP on stdin/stdout until the client disconnects, forwarding every
/// tool call to the daemon behind `client`.
pub async fn serve_stdio(client: Client) -> std::io::Result<()> {
    // The daemon decides; it also refuses the calls if this is stale.
    let external = match client.request(&Request::Ping).await {
        Ok(resp) => resp.data.is_some_and(|d| d["external_mcp"] == true),
        Err(_) => false,
    };
    let service = WardexMcp::remote(client, external)
        .serve(rmcp::transport::stdio())
        .await
        .map_err(std::io::Error::other)?;
    service.waiting().await.map_err(std::io::Error::other)?;
    Ok(())
}

/// Successful results are the same JSON the CLI prints with `--json`.
fn respond(resp: Response) -> CallToolResult {
    if resp.ok {
        let data = resp.client_data();
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
        description = "List live sessions (or all with `all`), newest first. Filter by \
                       `agent`/`task` glob; `ended` for ended sessions only, `resumable` \
                       for ended ones `send` can continue (see `ended_reason`).",
        annotations(read_only_hint = true)
    )]
    async fn list(&self, Parameters(p): Parameters<ListParams>) -> CallToolResult {
        self.call(Request::List {
            all: p.all,
            agent: p.agent,
            task: p.task,
            ended: p.ended,
            resumable: p.resumable,
            origin: p.origin.map(Into::into),
        })
        .await
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
        self.call(Request::Tail {
            id: p.id,
            since_seq: p.since_seq,
            last: p.last,
            follow: false,
            deltas: false,
            wait_secs: p.wait_secs,
            full: p.full,
        })
        .await
    }

    #[tool(
        description = "Full input and output of one tool call (tool_call events carry only \
                       300-character summaries). Output is stored up to 256 KiB.",
        annotations(read_only_hint = true)
    )]
    async fn tool_output(&self, Parameters(p): Parameters<ToolOutputParams>) -> CallToolResult {
        self.call(Request::ToolOutput {
            id: p.id,
            seq: p.seq,
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
        description = "Send the next message to a session (`id`), or with `latest` to the \
                          newest session matching `agent`/`task` globs. Queued while a turn \
                          runs; an ended session is resumed first."
    )]
    async fn send(&self, Parameters(p): Parameters<SendParams>) -> CallToolResult {
        self.call(Request::Send {
            id: p.id,
            message: p.message,
            latest: p.latest,
            agent: p.agent,
            task: p.task,
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
        description = "Sessions started outside Wardex (in a terminal or IDE) found in the \
                       Claude Code and Codex stores, newest first, with liveness (`live` = \
                       open in a process right now). Filter by `cwd` (`under` for subdirectories).",
        annotations(read_only_hint = true)
    )]
    async fn external_list(&self, Parameters(p): Parameters<ExternalListParams>) -> CallToolResult {
        self.call(Request::ExternalList {
            driver: p.driver.map(Into::into),
            cwd: p.cwd.map(PathBuf::from),
            under: p.under,
            since_secs: p.since_secs,
            limit: p.limit,
            include_managed: p.include_managed,
            archived: false,
            include_subagents: false,
            via_mcp: true,
        })
        .await
    }

    #[tool(
        description = "A session's native conversation (prompts, messages, tool calls, \
                       compactions), read from the CLI's transcript: the last `turns` turns; \
                       page back with `before` = `next_before`. Secrets are masked on a best-effort \
                       basis. Works for Wardex session ids and external session ids.",
        annotations(read_only_hint = true)
    )]
    async fn history(&self, Parameters(p): Parameters<HistoryParams>) -> CallToolResult {
        self.call(Request::History {
            id: p.id,
            driver: p.driver.map(Into::into),
            turns: p.turns,
            before: p.before,
            tools: p.tools.map(Into::into),
            max_bytes: p.max_bytes,
            via_mcp: true,
        })
        .await
    }

    #[tool(
        description = "Take over a session started outside Wardex: creates a Wardex session \
                       (state stopped, origin adopted) bound to the same native id; `send` or \
                       `message` continues it in a Wardex process. Refused while the session is \
                       open elsewhere (live) or was written very recently (recent); `fork` \
                       branches a new session off it instead, which is always allowed (with \
                       `message`, like `run` on a fork). Unless `readonly`, it continues with \
                       full permissions."
    )]
    async fn adopt(&self, Parameters(p): Parameters<AdoptParams>) -> CallToolResult {
        self.call(Request::Adopt {
            id: p.id,
            driver: p.driver.map(Into::into),
            agent: p.agent,
            task: p.task,
            model: p.model,
            reasoning: p.reasoning,
            readonly: p.readonly,
            message: p.message,
            force: false,
            fork: p.fork,
            via_mcp: true,
        })
        .await
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
