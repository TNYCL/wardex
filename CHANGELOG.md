# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and the project uses
[Semantic Versioning](https://semver.org/).

## [Unreleased]

## [0.1.0] - 2026-10-20

First functional release (Phases 1 and 2 of `docs/SCOPE.md`). 0.0.1 was a
skeleton with stub commands.

### Added

- External sessions (`docs/external-sessions.md`, off by default): with
  `[external] enabled = true`, `wardex external list [--cwd DIR [--under] |
  --here]` lists Claude Code and Codex sessions started outside Wardex from
  the CLIs' own stores, with liveness (`live`, `recent`, `inactive`,
  `unknown`) detected from `/proc` and the CLIs' registry files. `wardex
  history <id>` prints a session's conversation (prompts, messages, tool
  calls, compactions) read from its transcript on every call, paged by turns,
  capped in bytes and with best-effort secret redaction (`text`, `markdown`
  or `json`). The MCP tools `external_list` and `history` additionally need
  `external.mcp = true`. Wardex never writes, locks or deletes anything in
  the CLIs' stores, and history is not copied into its database.
- `wardex adopt <id> [--readonly] [--message TEXT] [--force]` and the MCP
  tool `adopt`: link an external session to a new Wardex session (`stopped`,
  `ended_reason: adopted`, `origin: adopted`) under the same native id, so
  `send`/`resume` continue it. Refused while the session is open in a process
  (`live`); `recent` or `unknown` sessions need `--force`, which exists only
  in the CLI. Adopting again returns the same session. Every resume of an
  adopted session checks again that no CLI process has it open. New `adopted`
  event (not sent to webhooks by default), `origin`, `forked_from` and
  `adopted_ms` on session records, and `list --origin wardex|adopted|forked`
  (MCP `list` `origin`).
- Forks: `wardex adopt --fork <id>`, `wardex run --fork-from <id> --prompt …`
  and MCP `adopt` with `fork` create a session (`origin: forked`) that
  branches off an external session when it first runs (Claude
  `--resume <id> --fork-session --session-id <new>`, Codex `thread/fork`); the
  original is only read, so this works while it is open elsewhere. Every fork
  is a new session, and later resumes continue the fork.
- `wardex list --agent GLOB --task GLOB --ended --resumable` and the same
  filters on the MCP `list` tool (`agent`, `task`, `ended`, `resumable`), to
  find a closed session again. `list` shows when and why a session ended.
- `wardex send --latest --agent GLOB [--task GLOB] <message>` and `latest`
  with `agent`/`task` on the MCP `send` tool (`id` is now optional): send to
  the newest session with matching labels, resuming it if it ended.
- Memory protection for idle sessions: `sessions.max_idle_live` (default 8)
  closes the longest idle agent process above that many idle ones, and
  `sessions.min_mem_available_mb` (default 1024) closes idle processes, longest
  idle first and one at a time, while the system's `MemAvailable` is below
  it. Running turns and open questions are never closed; `send` resumes a
  closed session. 0 turns either check off.
- `sessions.auto_resume_idle` (default false): with `auto_resume_orphaned`,
  also restart sessions that were idle when the daemon stopped.
- `ended_reason` on session records (`list`, `status`, `overview`) and
  `reason` on `ended` events: `done`, `stopped`, `idle_shutdown`,
  `idle_evicted`, `memory_pressure`, `error`, `drain`, `shutdown`,
  `daemon_lost`. The field is optional, so older events still parse.
- Webhook routing: `agents` and `tasks` glob filters per `[[webhooks]]`
  table, `escalate` event types that go to a target for every session (e.g.
  errors and stuck sessions to an on-call bot), and `name` for logs. Unknown
  event types in `events`/`escalate` are rejected.
- `wardex daemon reload`, SIGHUP and `ExecReload` in the systemd unit: re-read
  the config and the env file (`daemon.env_file`, now read by the daemon
  itself) and switch webhooks over without a restart; other changed sections
  are reported as `restart_needed`.
- `wardex daemon restart [--drain S]`: stop and start again in the same
  process (same PID, currently installed binary) and wait until it answers.
- Full tool input/output on `tool_call` events (up to 256 KiB each), shown by
  `tail --full`, `wardex tool-output <id> <seq>` and the MCP tool
  `tool_output`; left out of plain `tail` and webhooks.
- `summary`: `last_turn_ended_ms` and `last_turn_ms`. `overview`:
  `idle_since_ms` and `shutdown_in_ms` per session. `ping` reports the
  daemon's pid and start time.
- The effort Codex reports on `thread/start` is recorded as the session's
  `reasoning` when none was given.
- `wardex mcp --stdio`: the MCP tools on stdin/stdout, forwarding to the
  daemon's Unix socket, so remote clients can connect with
  `ssh <host> wardex mcp --stdio` without HTTP, tunnel or token.
- `tail --wait SECS` (with `--since-seq`) and `wait_secs` in the socket API:
  the daemon waits for the next event.
- MCP server at `/mcp` (rmcp, stateless Streamable HTTP with JSON
  responses): `run`, `overview`, `list`, `status`, `tail` (paging with
  `since_seq`, long-poll with `wait_secs`), `summary`, `pending_questions`,
  `answer`, `send`, `interrupt`, `stop`, `resume`. Results match the CLI's
  `--json` output (`docs/mcp.md`).
- `overview` (MCP tool and `wardex overview`): live sessions with state,
  pending questions, running tools, resources and latest events, plus
  sessions ended in the last 24 hours.
- `[http]` config: `allowed_hosts` (Host header check for `/mcp`, env
  `WARDEX_ALLOWED_HOSTS`) and `auth_failures_per_minute` (rate limit on
  failed authentications).
- Session manager: run, send, answer, stop, interrupt, resume, list, status,
  summary; stuck and reminder detection; idle shutdown with transparent
  resume; orphan recovery on startup; retention.
- SQLite store with an ordered single-writer queue for events and session
  updates.
- Config loading (file, env, flags) with path expansion.
- Process launcher with optional systemd scopes and resource sampling.
- Git helpers for change summaries and `--worktree`.
- Claude Code driver: stream-json + SDK control protocol (questions,
  permission requests, interrupts), resume via `--resume`, read-only via plan
  mode with permission requests denied.
- Daemon with a Unix socket API (one JSON request per connection, streamed
  `tail --follow`), `/healthz` over HTTP when a token is set, SIGTERM/SIGINT
  handling and orphan recovery on start.
- Codex driver: `codex app-server` JSON-RPC (initialize, thread start and
  resume with `excludeTurns`, turns with model and effort, queued follow-ups,
  `turn/interrupt`), questions via `item/tool/requestUserInput` (opt-in),
  approvals for command, file change, permission and MCP elicitation requests
  (declined automatically in read-only sessions).
- Webhook dispatcher: per-webhook ordered delivery, event filter, HMAC-SHA256
  signature, extra headers from env, retry with exponential backoff, stable
  delivery ids; URLs and secrets are never logged (`docs/webhooks.md`).
- Sessions run in transient systemd user scopes (`--collect`), optionally in
  a slice; scopes are killed when a session ends so no tool processes linger.
- Recovery: sessions orphaned by a daemon stop or crash are flagged; with
  `sessions.auto_resume_orphaned` the next daemon resumes them, sending
  `sessions.resume_prompt` to those that were mid-turn. Leftover scopes from
  a crashed daemon are killed first. `daemon stop --drain SECS` waits for
  running turns.
- CLI: `daemon`, `daemon stop`, `run`, `list`, `status`, `tail`, `summary`,
  `answer`, `send`, `interrupt`, `stop`, `resume`, `pending`, all with
  `--json`; session id prefixes.

- `docs/drivers.md`: verified Claude Code and Codex protocol behavior (M1).
- Sanitized driver fixtures recorded from real sessions, plus recording and
  sanitizing tools in `tools/fixtures/`.

### Fixed

- The Claude session id Wardex chooses with `--session-id` is recorded when
  the process starts, not only when `system/init` arrives, so a session whose
  process dies before its first event can still be resumed or inspected.
- Isolate unverifiable old scopes during recovery so one session cannot
  prevent daemon startup. Persist `cleanup_failed`, keep its scope recorded,
  skip automatic/implicit resume and expose the warning in status/list/overview.
  Explicit `resume` retries cleanup before allowing a new process; drain
  errors recommend `resume` or `--force` for these orphans.
- Reset both drain exit flags after a refused/cancelled drain, retaining the
  closed-stdin guard and any independent idle shutdown.
- Retain accepted SQLite writes in a bounded queue and retry with exponential
  background backoff. Split constraint failures to quarantine the offending
  session while other sessions continue; clear errors after recovery.
- Keep live/cached status, stop and interrupt available during database errors.
  Shutdown exits even if the final flush fails. Applied actions report
  `applied: true`, `persisted: false` and warnings instead of implying they
  should be resent; a full write queue rejects new work before agent commands.
- Reserve session starts atomically so concurrent resumes spawn one process.
  Delayed stop timers and driver events target the original process generation.
- Reap scope descendants after drain with SIGTERM then SIGKILL as necessary,
  and clean old orphan scopes before manual/automatic resume. Failed cleanup
  prevents a new generation from spawning alongside old descendants.
- Report closed sessions on a partially completed drain and clear remaining
  drain flags. Wait for existing starts until the drain deadline and give
  explicit stop instructions when an agent never reports a resume id.
- Reject duplicate daemons before opening the database or recovering sessions
  by holding an exclusive database lock for the daemon's lifetime and checking
  the socket first. A second start, even with a different socket, no longer
  orphans live sessions or cancels their pending questions.
- `wardex daemon stop` returned before the daemon had exited, so an
  immediate `systemctl --user start wardex` could fail on the still-bound
  socket. It now waits for the process to exit.
- `summary.duration_ms` counted up to the time of the `summary` call for idle
  sessions; it now ends with the last turn.
- Without `XDG_RUNTIME_DIR` (e.g. a non-interactive SSH command), the default
  socket path falls back to `/run/user/<uid>` instead of the temp dir, so the
  CLI finds a daemon running under systemd.
- `run` prints warnings on stderr so stdout holds only the session id.
- Daemon logs use colors only on a terminal.
- Question rows are written in order with events, so `pending` never misses
  a question that was just asked.

### Changed (breaking)

- Migration 0005 adds `sessions.origin`, `forked_from` and `adopted_ms` and an
  index on `(driver, driver_session_id)`; after it is applied, older binaries
  can no longer open the database. Back up `wardex.db` first.
- Migration 0004 adds `sessions.ended_reason`; after it is applied, older
  binaries can no longer open the database. Back up `wardex.db` first.
- After migration 0003 is applied, older binaries can no longer open this
  database; back up `wardex.db` before installing.
- `daemon stop/restart --drain SECS` refuses shutdown when turns, answers or
  graceful leader exit do not finish. Existing questions/approvals stay live;
  idle leaders are not force-killed. Use `answer`/`stop`, retry `--drain`, or
  explicitly choose `--drain SECS --force` to fall back to ordinary shutdown.
  Forced shutdown reports interrupted session ids in CLI/JSON and session
  notes. Error responses include `closed_sessions` and three operator steps.
- Degraded CLI/MCP array results use a `data` wrapper to carry persistence
  warnings; ordinary healthy payloads keep their existing shape.

### Changed

- README rewritten for first-time users: requirements, install, quick start,
  MCP client setup and an architecture diagram, with absolute links so it
  renders on crates.io. Published crates contain only sources, migrations
  and license files.
- `sessions.idle_shutdown_secs` now defaults to 43200 (12 h) instead of 900,
  so a follow-up `send` usually reaches a process that still has its context
  loaded. Each idle agent process keeps its memory meanwhile (roughly
  100–300 MB per session, partly swapped out over time); set
  `idle_shutdown_secs = 900` to keep the old behaviour. Values above 30 days
  are rejected (0 still keeps idle sessions forever).
- `timeouts.idle_stuck_secs = 0` disables `stuck` (`no_output`) detection
  instead of flagging every turn at once.
- With `auto_resume_orphaned`, only sessions that were mid-turn or waiting
  for an answer are restarted when the daemon starts; idle ones (all of them
  after `daemon restart --drain`) stay orphaned until `send` resumes them.
  Set `sessions.auto_resume_idle = true` for the old behaviour.
- `--rm-worktree` / `remove_worktree` no longer removes the worktree when an
  idle session is closed for idleness or memory; the worktree is removed on
  `stop` or when the agent exits by itself, so `send` can continue the work.
- `send`/`resume` of a session whose working directory is gone fail with a
  clear `conflict` instead of a spawn error.
- Event model: `tool_call` has a `call_id` and start/finish status,
  `question` carries question items with options, `approval` has a kind,
  `done` reports interrupts.
- Minimum Rust version is 1.88 (required by rmcp).
- `/healthz` reports `live_sessions` only to requests with the token.

## [0.0.1] - 2026-10-18

### Added

- Workspace skeleton: `wardex`, `wardex-core`, `wardex-drivers`, `wardex-server`.
- First draft of the unified event model and session states.
- CLI surface (all commands are stubs).
- CI: fmt, clippy, tests, secret scanning.
- Example config, systemd user unit and tunnel guide.
