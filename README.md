# Wardex

> **Status: pre-alpha.** Under active development; interfaces may change.
> The scope and milestones live in [`docs/SCOPE.md`](docs/SCOPE.md).

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

## CLI

```text
wardex daemon                      # run the daemon (see deploy/wardex.service)
wardex daemon stop [--drain S]     # shut it down and wait; live sessions become resumable
wardex daemon restart [--drain S]  # same PID, current binary; sessions resume
wardex daemon reload               # re-read config + env file (webhooks), no restart
wardex run --driver claude|codex --prompt "..." [--prompt-file F|-]
           [--model M] [--reasoning R] [--cwd DIR] [--readonly]
           [--worktree NAME [--branch B] [--rm-worktree]]
           [--agent A] [--task T] [--slice S]
wardex list [-a] [--agent GLOB] [--task GLOB] [--ended] [--resumable]
wardex status <id> | summary <id> | pending
wardex tail <id> [-f] [-n N] [--since-seq N [--wait SECS]] [--deltas] [--full]
wardex tool-output <id> <seq>      # full input/output of one tool call
wardex answer <id> <text> | answer <id> --item ID=TEXT ... [--question Q]
wardex send <id> <message> | interrupt <id> | stop <id> | resume <id> [message]
wardex send --latest --agent GLOB [--task GLOB] <message>   # newest matching session
wardex overview [-n EVENTS]        # live sessions, questions, resources at a glance
wardex mcp --stdio                 # MCP server on stdin/stdout (e.g. over ssh)
wardex external list [--cwd DIR [--under] | --here]   # sessions started outside Wardex
wardex history <id> [--turns N] [--format text|markdown|json]  # native conversation
wardex adopt <id> [--fork] [--readonly] [--message TEXT] [--force]  # continue it in Wardex
wardex run --fork-from <id> --prompt "..."                     # fork it and continue
```

Session ids can be shortened to any unique prefix. Every command accepts
`--json`. An idle session closes after 12 h by default (or earlier when too
many are idle or memory runs low, see `[sessions]` in
`deploy/wardex.example.toml`); find it again with
`wardex list --resumable --agent A` and continue it with `send`. The CLI talks to the daemon over a Unix socket (mode 0600).

## MCP

The same capabilities are MCP tools: `run`, `overview`, `list`, `status`,
`tail`, `summary`, `pending_questions`, `answer`, `send`, `interrupt`, `stop`,
`resume`, `tool_output` (plus `external_list`, `history` and `adopt` when
`[external]` enables them, see [`docs/external-sessions.md`](docs/external-sessions.md)).
Two transports: `wardex mcp --stdio` (forwards to the daemon's
socket; remote clients use `ssh <host> wardex mcp --stdio`), and, with a token
set (`WARDEX_TOKEN`), Streamable HTTP at `http://127.0.0.1:7878/mcp`. See
[`docs/mcp.md`](docs/mcp.md) for the tools and the bot loop, and
[`deploy/tunnel.md`](deploy/tunnel.md) for exposing it through Cloudflare
Tunnel or Tailscale Funnel.

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT license ([LICENSE-MIT](LICENSE-MIT))

at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted
for inclusion in the work by you, as defined in the Apache-2.0 license, shall be
dual licensed as above, without any additional terms or conditions.
