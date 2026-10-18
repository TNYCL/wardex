# Wardex

> **Status: pre-alpha.** Nothing works yet. The scope and milestones live in
> [`docs/SCOPE.md`](docs/SCOPE.md).

Wardex is a single-binary daemon and CLI, written in Rust, that starts,
watches and manages [Claude Code](https://docs.claude.com/en/docs/claude-code)
and [Codex](https://github.com/openai/codex) sessions. It is built to run
around 30 concurrent agents on one Linux host, and to let other agents or bots
drive those sessions over MCP.

- One event model for every agent CLI: `started`, `message`, `tool_call`,
  `question`, `approval`, `reminder`, `done`, `error`, `stuck`.
- Questions are surfaced as events and answered with `wardex answer`.
- State lives in SQLite and survives daemon restarts; lost sessions can be
  resumed.
- Important events are pushed to webhooks; details are pulled over MCP.
- Each session runs in its own systemd scope, optionally inside a slice.

Wardex has **no policy or approval layer**. Agents run with full permissions
(`bypassPermissions` / `danger-full-access`). Safety is left to your prompts
and to `CLAUDE.md` / `AGENTS.md` rules. Only run it on hosts where that is
acceptable, and always protect the MCP endpoint with a token.

## Planned CLI

```text
wardex daemon
wardex run --driver claude|codex --prompt "..." [--model M] [--reasoning R]
           [--cwd DIR] [--worktree NAME] [--readonly] [--agent A] [--task T]
wardex list | status <id> | tail <id> [-f] | summary <id>
wardex answer <id> <text> | send <id> <message> | stop <id> | resume <id>
```

Every command accepts `--json`.

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT license ([LICENSE-MIT](LICENSE-MIT))

at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted
for inclusion in the work by you, as defined in the Apache-2.0 license, shall be
dual licensed as above, without any additional terms or conditions.
