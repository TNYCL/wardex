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
  server), sqlx + SQLite (runtime queries, embedded migrations), clap, serde,
  tracing.
- **No guard layer:** no policy engine, approval layer or command filter.
  Safety is left to prompts and `CLAUDE.md` / `AGENTS.md` rules.
- **No roles:** one MCP endpoint, one token. Every client sees and manages
  every session. Sessions are told apart by `agent` and `task` labels.
- **Hands off CLI config:** Wardex only passes per-session flags (`--model`,
  `-c`, `--enable`, …). It never edits `~/.claude*` or `~/.codex/` and does
  not set a separate `CLAUDE_CONFIG_DIR` / `CODEX_HOME`, so sessions share the
  user's existing settings, auth and quota.
- **Generic:** nothing deployment-specific in the repo (URLs, keys, paths, bot
  names, host settings). Those live only in config and env. The repo ships
  `deploy/wardex.example.toml`.

## 1. Phase 1 — core daemon + CLI

### 1.1 Drivers

Protocol details and verified behavior: [`drivers.md`](drivers.md).

**Claude Code:** `claude -p --input-format stream-json --output-format
stream-json --include-partial-messages --verbose --permission-prompt-tool stdio`

- `--permission-mode bypassPermissions`; model via `--model`.
- Wardex picks the Claude session id up front with `--session-id <uuid>`.
- Questions: with `--permission-prompt-tool stdio`, `AskUserQuestion` arrives
  as a `can_use_tool` control request and the CLI blocks until Wardex answers
  with a `control_response` (verified in M1). Without that flag the tool is not
  available in `-p` mode at all.
- Interrupt with an `interrupt` control request; the resulting
  `error_during_execution` result is reported as `done` (interrupted).
- Resume with `--resume <session_id>`.
- Read-only: `--permission-mode plan` **and** the driver denies every
  permission request except `AskUserQuestion` (plan mode alone lets writes
  through when the host allows them).

**Codex:** `codex app-server` (stdio JSON-RPC).

- yolo equivalent: `approval_policy=never`, `sandbox=danger-full-access`.
- `model` and `reasoning_effort` passed through.
- Thread/turn management; approval requests become `approval` events.
- One app-server process per session. Resume via `thread/resume` with the
  thread id (`excludeTurns: true`). Interrupt via `turn/interrupt`. Read-only:
  `sandbox=read-only`.
- Questions (`item/tool/requestUserInput`) need the under-development feature
  `default_mode_request_user_input`. Opt-in via
  `drivers.codex.request_user_input` (default off); works in 0.163.0.
- The protocol changes often: pin the tested Codex version, derive types from
  the schema the app-server can generate, and keep contract tests against
  recorded JSON-RPC fixtures.

**Driver trait:** `start`, `send_message`, `answer`, `interrupt`, `stop`,
`events() -> Stream<Event>`. New CLIs (Gemini etc.) can be added later.

Tested CLI versions are listed in `docs/drivers.md`.

### 1.2 Event model

Events: `started`, `prompt`, `message` (delta and full text), `tool_call`
(name, short input, result summary), `question`, `approval`, `answered`,
`reminder`, `done`, `error`, `stuck`, `ended`.

Session states: `starting`, `running`, `idle` (turn done, process waiting for
the next message), `waiting_input`, and the resumable end states `done`,
`error`, `stopped`, `orphaned`. An idle process is closed after
`sessions.idle_shutdown_secs`; `send` to an ended session resumes it
transparently.

- `tool_call` is emitted when a tool starts and when it finishes (same
  `call_id`). `question` carries one or more question items with options.
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
  Failed batches remain in a bounded queue (4096 pending writes, including
  in-flight and quarantined writes) and retry in the background with
  exponential backoff from 25 ms to 2 s. Recovery clears the writer error and
  reopens new-work admission. A failed constraint/serialization batch is split
  to isolate the offending session; its later writes stay ordered in quarantine
  while other sessions continue. Quarantined writes also retry and consume
  queue capacity; inspect the reported constraint/record and repair its cause.
  A global database outage or full queue rejects new work before agent commands
  are sent. Existing driver output waits for bounded writer capacity. Flush
  reports errors instead of acknowledging uncommitted writes.
- Persistence failure does not disable `list`, `status`, `stop` or `interrupt`.
  Live/cached status remains available with `persisted: false` and warnings
  when SQLite cannot be read or writes are uncommitted. Inspect database access
  and free disk space; retries do not require a restart. To exit during an
  outage, use `daemon stop` without `--drain`: shutdown completes even when its
  final flush fails and reports the durability warning. Uncommitted writes are
  only in memory and can be lost on exit. `run`, `send` and `answer` distinguish
  an applied action from a durable one: a post-action write failure returns
  success with `applied: true`, `persisted: false` and a do-not-resend warning.
  An admission failure reports that no action was applied. On degraded CLI/MCP
  reads, array payloads use a `data` wrapper to carry warnings and durability.
- Before opening SQLite or recovering sessions, the daemon takes a
  non-blocking exclusive file lock on `<database>.lock` and binds its Unix
  socket, rejecting a live socket and replacing a stale one. The lock is held
  until shutdown completes, so another daemon using the same database is
  rejected even with a different socket. The lock file stays on disk; the
  operating system releases the lock when the daemon exits, including crashes.
- Agent stdio pipes are owned by the daemon, so a daemon restart loses live
  sessions. On start, sessions whose process is gone are marked
  `error: orphaned` and can be continued with `wardex resume` (driver resume
  id). `auto_resume_orphaned` (default off) resumes them automatically.
- **Ordinary shutdown** (`daemon stop` / `restart` without `--drain`, or a
  termination signal): close session stdin, wait for the shutdown grace
  period, then force-kill remaining processes/scopes. Sessions become
  orphaned with persisted resume ids; pending protocol requests are cancelled.
- **Graceful drain** (`daemon stop --drain SECS` / `restart --drain SECS`):
  reject new runs, resumes and follow-ups while waiting for existing starts,
  all turns and
  question/approval answers, allowing answers to existing requests. If the
  deadline expires, refuse shutdown and report the blocking session ids in
  CLI errors and JSON `blocked_sessions`; every live session and pending
  request remains available. No turns are interrupted (`interrupted_sessions`
  is empty). Errors include `closed_sessions`, `blocked_sessions` and three
  `next_steps`: (1) `answer` or `stop <id>`, (2) retry `--drain`, (3) explicitly
  use `--force`. A session that never reports a resume id requires `stop`.
  Once all sessions are idle, flush state and resume ids, close
  stdin, and wait up to `sessions.stop_grace_secs + 5` seconds for exit.
  Drain never force-kills an idle leader that refuses EOF, including after
  stdout EOF. After the leader exits, clean its scope with SIGTERM and a grace
  period, then SIGKILL if needed, and verify that no descendants remain before
  drain succeeds. Recover and manual/automatic resume also clean any old unit
  recorded on an orphan awaiting resume; cleanup failure prevents a new spawn.
  If a process refuses EOF, refuse shutdown and report its id. Other sessions
  may already have closed; the remaining process has closed stdin and rejects
  new messages. Clear its drain flag so a later natural exit is classified
  normally. Wait for it to exit or explicitly `stop` it before retrying.
  Closed sessions are orphaned and can be resumed manually or through
  `auto_resume_orphaned`, without an extra continuation prompt. A successful
  JSON response lists `closed_sessions`; it is sent only after persistence
  and graceful process exit complete.
- **Forced drain** (`daemon restart --drain SECS --force`, also supported on
  `daemon stop`): try normal drain first; if it cannot finish, use ordinary
  shutdown. Responses and human CLI output list `interrupted_sessions`; each
  interrupted session receives a `done` event with `interrupted: true` and an
  explicit force/shutdown note. Pending questions/approvals are cancelled.
  The response reports `persisted: false` if those records cannot be committed.
  `--force` requires an explicit drain deadline.
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

`daemon` (+ `daemon stop`), `run`, `list`, `status <id>`, `tail <id> [-f]`,
`summary <id>`, `answer <id> <text>`, `send <id> <message>`, `stop <id>`,
`interrupt <id>`, `resume <id>`, `pending`.

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
| M0 | Repo skeleton, licenses, CI, example config, scope (done) |
| M1 | Spikes: Claude question flow, Codex app-server handshake; record fixtures (done, see `drivers.md`) |
| M2 | Event model, store, session manager (done) |
| M3 | Claude driver (done) |
| M4 | CLI + Unix socket API (done) |
| M5 | Codex driver (done) |
| M6 | Stuck/reminder detection, webhooks (done, see `webhooks.md`) |
| M7 | systemd scopes, resource usage (done) |
| M8 | Restart/orphan/resume, 10+ session load test, acceptance (done, see below) |

### Phase 1 acceptance results (2026-10-18)

Run against Claude Code 2.1.302 (haiku) and Codex 0.163.0 (gpt-6-luna) with a
temporary daemon, systemd scopes on and a local webhook receiver:

| # | Criterion | Result |
|---|---|---|
| 1 | Claude and Codex sessions start from the CLI; `tail -f` streams live | pass |
| 2 | A question is a `question` event; `answer` continues the session | pass (Claude and Codex) |
| 3 | `done`, `error`, `stuck`, `reminder` reach the webhook | pass (31 deliveries, all signatures valid) |
| 4 | After a daemon restart, `list` and history are intact; orphans resume | pass (both drivers resumed with their context; auto-resume continued a mid-turn session; `--drain` waited for a running turn) |
| 5 | 10+ concurrent sessions | pass (21 live sessions, 12-session load batch all correct) |
| 6 | Mock-process integration tests; CI with test, clippy, fmt | pass |

Also verified: read-only mode (no files written by either driver) and
interrupts (`done` interrupted on both drivers).

## 2. Phase 2 — MCP + tunnel

### 2.1 MCP server (rmcp, Streamable HTTP)

- Every CLI capability is an MCP tool: `run`, `list`, `status`, `tail`
  (paged with `since_seq`, optional long-poll with `wait_secs`), `summary`,
  `answer`, `send`, `interrupt`, `stop`, `resume`, `pending_questions`.
  Results are the CLI's `--json` output. Reference: [`mcp.md`](mcp.md).
- Stateless Streamable HTTP with JSON responses (no MCP session to lose on a
  daemon restart).
- `overview`: all live sessions with state, pending questions, latest events,
  duration and resource usage in one call.
- **Auth:** a bearer token from env is **always required** for HTTP/MCP; the
  daemon refuses to serve HTTP without one. Tunnels forward traffic to
  localhost, so "localhost without token" would be open to the internet.
  Tokenless access exists only on the local Unix socket. Constant-time token
  comparison and a rate limit on failed authentications
  (`http.auth_failures_per_minute`). `/mcp` checks the `Host` header against
  localhost and `http.allowed_hosts` (DNS rebinding protection).
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

### Phase 2 milestones

| | Milestone |
|---|---|
| M9 | MCP server, `overview`, HTTP auth/rate limit/host check, tunnel guide, acceptance (done, see below) |

### Phase 2 acceptance results (2026-10-18)

Run against Claude Code 2.1.302 (haiku) and Codex 0.163.0 (gpt-6-luna) with a
temporary daemon (MCP on a separate port, systemd scopes on) and a local
webhook receiver acting as a bot:

| # | Criterion | Result |
|---|---|---|
| 1 | A bot starts a session over MCP, sees and answers its question, reads the result | pass: Claude Code (haiku) as the MCP client ran `run`, `tail` with `wait_secs`, `answer`, `summary`, `overview` on its own |
| 2 | `overview` shows all sessions correctly in one call | pass: 3 live sessions (2 Claude, 1 Codex; running, running tool, waiting_input) matched `status` for each; restarted daemon's orphans listed as recently ended |
| 3 | A webhook wakes a bot and the bot pulls details over MCP | pass: `question` and `reminder` webhooks woke the bot, it answered via `pending_questions` + `answer` and read `summary` on `done` (17 deliveries, all signatures valid) |

Also verified over MCP with real agents: `send` to an idle session,
`interrupt` of a running Codex command, `stop` then `resume` with context,
token rejection, and a `tools/call` from plain `curl` without `initialize`.

## 3. Out of scope (Phase 3)

Web panel, token and cost tracking, daily reports, queue/priority and
concurrency limits, metrics, auto-restart, per-session pipe shim. A policy or
role layer will never be built.
