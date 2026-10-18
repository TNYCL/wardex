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
- CLI: `daemon`, `daemon stop`, `run`, `list`, `status`, `tail`, `summary`,
  `answer`, `send`, `interrupt`, `stop`, `resume`, `pending`, all with
  `--json`; session id prefixes.

- `docs/drivers.md`: verified Claude Code and Codex protocol behavior (M1).
- Sanitized driver fixtures recorded from real sessions, plus recording and
  sanitizing tools in `tools/fixtures/`.

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
