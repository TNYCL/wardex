# Wardex scope (Phase 1 + Phase 2)

Wardex is an open-source orchestrator daemon, written in Rust, that starts,
watches and manages Claude Code and Codex sessions. Bots and other agents reach
those sessions over MCP; important events are pushed via webhooks. Target
scale: ~30 concurrent agents on one Linux host. It replaces ad-hoc setups built
on tmux, `send-keys` and output scraping.

## 0. Ground rules

- **Name:** `wardex` (crates and binary). Repo: `TNYCL/wardex`.
- **License:** MIT OR Apache-2.0.
- **Visibility:** public from day one; pre-alpha until v0.1.
- **Stack:** Rust, single binary. tokio, axum (HTTP and MCP), rmcp (MCP
  server), sqlx + SQLite (offline mode, `.sqlx/` committed), clap, serde,
  tracing.
- **No guard layer:** no policy engine, approval layer or command filter.
  Safety is left to prompts and `CLAUDE.md` / `AGENTS.md` rules.
- **No roles:** one MCP endpoint, one token. Every client sees and manages
  every session. Sessions are told apart by `agent` and `task` labels.
- **Generic:** nothing deployment-specific in the repo (URLs, keys, paths, bot
  names, host settings). Those live only in config and env. The repo ships
  `deploy/wardex.example.toml`.

## 1. Phase 1 — core daemon + CLI

### 1.1 Drivers

**Claude Code:** `claude -p --input-format stream-json --output-format
stream-json --include-partial-messages --verbose`

- `--permission-mode bypassPermissions`; model via `--model`.
- Questions: `AskUserQuestion` is caught as a `tool_use` and turned into a
  `question` event. The answer is written to stdin as a stream-json user
  message. **Must be verified by a spike first (M1)**; if headless mode does
  not block on it, route questions through `--permission-prompt-tool` to a
  Wardex MCP tool (relay only, no filtering).
- Resume with `--resume <session_id>`.
- Read-only: `--permission-mode plan`.

**Codex:** `codex app-server` (stdio JSON-RPC).

- yolo equivalent: `approval_policy=never`, `sandbox=danger-full-access`.
- `model` and `reasoning_effort` passed through.
- Thread/turn management; approval requests become `approval` events.
- Resume via thread id. Read-only: `sandbox=read-only`.
- The protocol changes often: pin the tested Codex version, derive types from
  the schema the app-server can generate, and keep contract tests against
  recorded JSON-RPC fixtures.

**Driver trait:** `start`, `send_message`, `answer`, `interrupt`, `stop`,
`events() -> Stream<Event>`. New CLIs (Gemini etc.) can be added later.

Tested CLI versions are listed in `docs/drivers.md`.

### 1.2 Event model

Events: `started`, `message` (delta and full text), `tool_call` (name, short
input, result summary), `question`, `approval`, `reminder`, `done`, `error`,
`stuck`.

- Every event carries `session_id`, `seq`, `ts`, `agent`, `task`, `driver`,
  `payload`.
- Message deltas go only to the in-memory broadcast (`tail -f`, live MCP
  reads). SQLite stores full messages, tool calls and lifecycle events.
- **Stuck detection:** a session waiting on a question gets a `reminder` at
  3 min and `stuck` at 8 min. A session with no output gets `stuck` at 8 min,
  but **only while no tool is running** (a long build is not stuck);
  optionally the scope's CPU usage is checked too. All timings are
  configurable.

### 1.3 Sessions and workspace

- Default: the main checkout. `--cwd` runs directly in the given directory.
- Optional `--worktree <name>`: a git worktree under `.worktree/<name>`,
  optional branch name, optional cleanup on close.
- `--readonly`: the driver's plan / read-only mode plus a prompt prefix as a
  fallback.
- Concurrent sessions in the same cwd are allowed with a warning (no repo
  lock).
- Each session stores both the Wardex id and the driver's id (Claude
  `session_id`, Codex `thread_id`) so it can always be resumed.
- The initial prompt and every follow-up message are stored as events.
- On close, git state is recorded (touched files, branch, uncommitted
  changes) and included in `summary` and the `done` webhook.

### 1.4 Persistence and restarts

- SQLite (WAL, single writer task, batched writes): `sessions`, `events`,
  `questions`. History and state survive daemon restarts.
- Agent stdio pipes are owned by the daemon, so a daemon restart loses live
  sessions. On start, sessions whose process is gone are marked
  `error: orphaned` and can be continued with `wardex resume` (driver resume
  id). `auto_resume_orphaned` (default off) resumes them automatically.
- **Graceful shutdown:** on stop the daemon persists state and resume ids and
  exits without killing sessions; `wardex daemon stop --drain` interrupts
  sessions explicitly.
- A per-session shim that keeps pipes alive across restarts is Phase 3.
- Event retention and size limits are configurable.

### 1.5 Webhooks

- Notable events (`question`, `approval`, `reminder`, `done`, `error`,
  `stuck`) are POSTed to configured URLs.
- URLs, headers and keys come from env. Optional HMAC-SHA256 signature header.
- Retry with backoff; event-type filter per webhook.
- No secret is ever logged.

### 1.6 Resources (Linux)

- Each session runs in its own `systemd-run --user --scope`, optionally in a
  configured slice (e.g. build/tests).
- Per-session RSS and CPU are read and shown in `status`.
- Requires `loginctl enable-linger` to survive logout (documented).

### 1.7 CLI

`daemon`, `run`, `list`, `status <id>`, `tail <id> [-f]`, `summary <id>`,
`answer <id> <text>`, `send <id> <message>`, `stop <id>`, `resume <id>`.

- All accept `--json`. The CLI talks to the daemon over a local Unix socket.
- `run`: `--driver claude|codex`, `--model`, `--reasoning`, `--cwd`,
  `--worktree`, `--branch`, `--readonly`, `--agent`, `--task`, `--slice`,
  `--prompt` or `--prompt-file`.
- `summary`: final result text, touched files, duration, event/tool counts.
  No extra LLM call.

### 1.8 Deployment

- systemd user unit (`deploy/wardex.service`) running an installed release
  binary, never a working checkout.
- `deploy/wardex.example.toml`. Precedence: file < env < CLI flags.
- Structured logs via tracing; `/healthz`.

### Phase 1 acceptance

1. Claude and Codex sessions start from the CLI; `tail -f` streams them live.
2. A question from Claude shows up as a `question` event; `answer` resumes
   the session.
3. `done`, `error`, `stuck` and `reminder` reach the webhook.
4. After a daemon restart, `list` and history are intact; orphaned sessions
   resume.
5. 10+ concurrent sessions run without issues.
6. Driver integration tests with mock processes and recorded fixtures. CI runs
   `cargo test`, `clippy`, `fmt` and a secret scan.

### Milestones

| | Milestone |
|---|---|
| M0 | Repo skeleton, licenses, CI, example config, scope |
| M1 | Spikes: Claude question flow, Codex app-server handshake; record fixtures |
| M2 | Event model, store, session manager |
| M3 | Claude driver |
| M4 | CLI + Unix socket API |
| M5 | Codex driver |
| M6 | Stuck/reminder detection, webhooks |
| M7 | systemd scopes, resource usage |
| M8 | Restart/orphan/resume, 10+ session load test, acceptance |

## 2. Phase 2 — MCP + tunnel

### 2.1 MCP server (rmcp, Streamable HTTP)

- Every CLI capability is an MCP tool: `run`, `list`, `status`, `tail`
  (paged with `since_seq`), `summary`, `answer`, `send`, `stop`, `resume`,
  `pending_questions`.
- `overview`: all live sessions with state, pending questions, latest events,
  duration and resource usage in one call.
- **Auth:** a bearer token from env is **always required** for HTTP/MCP; the
  daemon refuses to serve HTTP without one. Tunnels forward traffic to
  localhost, so "localhost without token" would be open to the internet.
  Tokenless access exists only on the local Unix socket. Constant-time token
  comparison and basic rate limiting.
- MCP is pull-based. Webhooks wake clients; clients pull details over MCP.

### 2.2 Exposure

- Cloudflare Tunnel or Tailscale Funnel to `https://<host>/mcp`. Generic guide
  in `deploy/tunnel.md`.

### 2.3 Migration (operations, not in this repo)

Installing Wardex, connecting bots as MCP clients, adapting webhook consumers
and retiring a previous tmux-based setup after running both in parallel.

### Phase 2 acceptance

1. A bot starts a session over MCP, sees and answers its question, reads the
   result.
2. `overview` shows all sessions correctly in one call.
3. A webhook wakes a bot and the bot pulls details over MCP.

## 3. Out of scope (Phase 3)

Web panel, token and cost tracking, daily reports, queue/priority and
concurrency limits, metrics, auto-restart, per-session pipe shim. A policy or
role layer will never be built.
