# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and the project uses
[Semantic Versioning](https://semver.org/).

## [Unreleased]

### Added

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

- `run` prints warnings on stderr so stdout holds only the session id.
- Daemon logs use colors only on a terminal.
- Question rows are written in order with events, so `pending` never misses
  a question that was just asked.

### Changed

- Event model: `tool_call` has a `call_id` and start/finish status,
  `question` carries question items with options, `approval` has a kind,
  `done` reports interrupts.

## [0.0.1] - 2026-10-18

### Added

- Workspace skeleton: `wardex`, `wardex-core`, `wardex-drivers`, `wardex-server`.
- First draft of the unified event model and session states.
- CLI surface (all commands are stubs).
- CI: fmt, clippy, tests, secret scanning.
- Example config, systemd user unit and tunnel guide.
